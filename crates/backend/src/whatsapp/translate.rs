//! Incoming translation: library events and messages become the
//! crate's own `LiveChat` / `LiveMessage` values, so nothing from
//! the library crosses the backend boundary.

use whatsapp_rust::prelude::*;
use whatsapp_rust::wacore::types::events::Event;

use crate::{
    BackendEvent, EventSink, LiveChat, LiveChatKind, LiveMessage, LiveMessageKind, LivePreview,
    ReceiptKind,
};

/// Maps the library's privacy-settings response into our
/// category → value spelling map. Every category the library
/// returns is carried, including the ones the Privacy page has
/// no row for (`calladd`, `messages`, `defense`): silently
/// discarding fetched account state would lose settings the
/// account actually has. A category or value this build does
/// not know is carried under its wire spelling (the library's
/// `Other` fallback), so nothing the server sent is dropped.
pub(super) fn privacy_map(
    resp: &whatsapp_rust::privacy_settings::PrivacySettingsResponse,
) -> std::collections::BTreeMap<String, String> {
    use whatsapp_rust::privacy_settings::{PrivacyCategory, PrivacyValue};
    let mut out = std::collections::BTreeMap::new();
    for setting in &resp.settings {
        let category = match &setting.category {
            PrivacyCategory::Last => "last",
            PrivacyCategory::Online => "online",
            PrivacyCategory::Profile => "profile",
            PrivacyCategory::Status => "status",
            PrivacyCategory::GroupAdd => "groupadd",
            PrivacyCategory::ReadReceipts => "readreceipts",
            PrivacyCategory::CallAdd => "calladd",
            PrivacyCategory::Messages => "messages",
            PrivacyCategory::DefenseMode => "defense",
            PrivacyCategory::Other(name) => name.as_str(),
        };
        let value = match &setting.value {
            PrivacyValue::All => "all",
            PrivacyValue::Contacts => "contacts",
            PrivacyValue::None => "none",
            PrivacyValue::ContactBlacklist => "contact_blacklist",
            PrivacyValue::MatchLastSeen => "match_last_seen",
            PrivacyValue::Known => "known",
            PrivacyValue::Off => "off",
            PrivacyValue::OnStandard => "on_standard",
            PrivacyValue::Other(v) => v.as_str(),
        };
        out.insert(category.to_owned(), value.to_owned());
    }
    out
}

/// Handles message types that map to dedicated backend events
/// (reactions, edits, revokes) instead of message upserts. Returns
/// true when the message was fully handled.
pub(super) fn translate_special(
    ctx: &MessageContext,
    sink: &EventSink,
    book: &IdentityBook,
) -> bool {
    let source = &ctx.info.source;
    let chat_jid = source.chat.to_string();
    let actor = {
        let push = ctx.info.push_name.trim();
        if source.is_from_me {
            "Me".to_owned()
        } else {
            book.sender_label(&source.sender.to_string(), push)
        }
    };
    let msg = &ctx.message;
    {
        // Non-content payloads (P2-04): sender-key distribution
        // and limit-sharing records are protocol housekeeping,
        // not chat content. When one of them is the message's
        // ONLY payload (no text, media, contact, location, poll
        // or interactive field rides along), consume it here so
        // it can never surface as a fake "Message" bubble, the
        // same treatment protocolMessage's non-content types
        // get below. Reachability through the library's own
        // dispatch is unverified; the arm is harmless if the
        // library consumes them first.
        let base = msg.get_base_message();
        let non_content = base.sender_key_distribution_message.as_option().is_some()
            || base.limit_sharing_message.as_option().is_some();
        let has_content = base.conversation.as_deref().is_some_and(|c| !c.is_empty())
            || base.extended_text_message.as_option().is_some()
            || base.image_message.as_option().is_some()
            || base.video_message.as_option().is_some()
            || base.ptv_message.as_option().is_some()
            || base.audio_message.as_option().is_some()
            || base.document_message.as_option().is_some()
            || base.document_with_caption_message.as_option().is_some()
            || base.sticker_message.as_option().is_some()
            || base.lottie_sticker_message.as_option().is_some()
            || base.contact_message.as_option().is_some()
            || base.contacts_array_message.as_option().is_some()
            || base.location_message.as_option().is_some()
            || base.live_location_message.as_option().is_some()
            || base.poll_creation_message.as_option().is_some()
            || base.interactive_message.as_option().is_some()
            || base.list_message.as_option().is_some()
            || base.event_message.as_option().is_some()
            || base.group_invite_message.as_option().is_some()
            || base.product_message.as_option().is_some()
            || base.order_message.as_option().is_some();
        if non_content && !has_content {
            return true;
        }
    }
    if let Some(pin) = msg.pin_in_chat_message.as_option() {
        // The pin state change is an event, not the message:
        // emit it, then let the message itself translate too,
        // so its classifier arm renders the "Pinned a message"
        // system line in the stream.
        if let Some(target_id) = pin.key.as_option().and_then(|k| k.id.as_deref()) {
            sink.emit(BackendEvent::MessagePinChanged {
                chat_jid: chat_jid.clone(),
                message_id: target_id.to_owned(),
                pinned: !matches!(
                    pin.r#type,
                    Some(
                        whatsapp_rust::prelude::wa::message::pin_in_chat_message::Type::UnpinForAll
                    )
                ),
            });
        }
    }
    if let Some(reaction) = msg.reaction_message.as_option() {
        if let Some(key) = reaction.key.as_option() {
            if let Some(target_id) = key.id.as_deref() {
                sink.emit(BackendEvent::ReactionUpdate {
                    chat_jid,
                    message_id: target_id.to_owned(),
                    actor,
                    emoji: reaction.text.clone().unwrap_or_default(),
                });
                return true;
            }
        }
    }
    if let Some(protocol) = msg.protocol_message.as_option() {
        use whatsapp_rust::prelude::wa::message::protocol_message::Type;
        let target_id =
            protocol.key.as_option().and_then(|k| k.id.as_deref()).map(ToOwned::to_owned);
        match protocol.r#type {
            Some(Type::EPHEMERAL_SETTING) => {
                // A disappearing-messages change in a direct chat
                // (ours from another device, or the contact's):
                // surface it as state, never as a chat message.
                sink.emit(BackendEvent::DisappearingChanged {
                    chat_jid,
                    duration_secs: protocol.ephemeral_expiration.unwrap_or(0),
                });
                return true;
            }
            Some(Type::REVOKE) => {
                if let Some(target_id) = target_id {
                    sink.emit(BackendEvent::MessageRevoked { chat_jid, message_id: target_id });
                    return true;
                }
            }
            Some(Type::MESSAGE_EDIT) => {
                if let (Some(target_id), Some(edited)) =
                    (target_id, protocol.edited_message.as_option())
                {
                    // An edited event carries a fresh event
                    // payload, not text: it reports as an event
                    // edit with the whole new field set (a
                    // cancellation is this same shape with the
                    // canceled flag set).
                    if let Some(event) = edited.event_message.as_option() {
                        sink.emit(event_fields(event).into_edited(chat_jid, target_id));
                        return true;
                    }
                    sink.emit(BackendEvent::MessageEdited {
                        chat_jid,
                        message_id: target_id,
                        new_body: edited.text_content().unwrap_or_default().to_owned(),
                    });
                    return true;
                }
            }
            // Any other protocol message (history sync
            // notifications the library consumes itself, peer
            // data operations, group settings records) is not
            // chat content: it is handled here so it can never
            // surface as a message bubble.
            Some(_) => return true,
            None => {}
        }
    }
    if let Some(keep) = msg.keep_in_chat_message.as_option() {
        // A keep (or un-keep) of a disappearing message, made
        // from any device: it is an add-on about another
        // message, never a bubble of its own. The library
        // dispatches no event for it, so the state crosses here.
        if let Some(target_id) =
            keep.key.as_option().and_then(|k| k.id.as_deref()).map(ToOwned::to_owned)
        {
            sink.emit(BackendEvent::MessageKept {
                chat_jid,
                message_id: target_id,
                kept: keep.keep_type == Some(whatsapp_rust::prelude::wa::KeepType::KeepForAll),
            });
            return true;
        }
    }
    false
}

/// Chats whose titles translation could not resolve, reported
/// so the session can heal them from the client's own stores:
/// (chat JID, chat kind) pairs.
pub(super) type TitleHeals = Vec<(String, LiveChatKind)>;

/// Media messages translation saw, reported so the session can
/// record where each can be re-addressed for the media-retry
/// flow: (chat JID, message ID, mark) triples.
pub(super) type RetryMarks = Vec<(String, String, super::util::RetryMark)>;

/// Maps a library event the typed handlers do not cover.
/// `own_jids` are this account's own JIDs (phone number and
/// LID, non-AD spellings): the disappearing-mode notification
/// uses the same event for per-chat timers and for the
/// account-wide default, told apart only by whose JID the
/// change came from.
/// The user-facing wording for a pairing-code failure, by
/// category: the library's structured rejection decides what
/// the user should do next (wait out a throttle, fall back to
/// the QR code, fix the connection), and the raw error text
/// never reaches the banner.
pub(super) fn pairing_error_message(
    err: &whatsapp_rust::wacore::types::events::PairingCodeError,
) -> String {
    use whatsapp_rust::wacore::pair_code::PairCodeRejection;
    match &err.rejection {
        Some(PairCodeRejection::FeatureNotAvailable) => {
            "Pairing by code is not available for this account. Use the QR code instead.".to_owned()
        }
        Some(rejection) if rejection.is_throttled() => {
            let wait = err
                .backoff
                .map(|d| format!(" Wait about {} seconds before trying again.", d.as_secs()))
                .unwrap_or_default();
            format!("WhatsApp is limiting pairing attempts for this number right now.{wait}")
        }
        Some(PairCodeRejection::InternalServerError) => {
            "WhatsApp's server could not complete the pairing. Try again in a little while."
                .to_owned()
        }
        Some(_) => {
            "WhatsApp refused the pairing code. Request a new code and try again.".to_owned()
        }
        None => "The pairing code request did not complete. Check your connection and try again."
            .to_owned(),
    }
}

pub(super) fn translate_event(
    event: &Event,
    sink: &EventSink,
    own_jids: &[String],
    book: &mut IdentityBook,
) -> (TitleHeals, RetryMarks) {
    let mut unresolved_titles: TitleHeals = Vec::new();
    let mut retry_marks: RetryMarks = Vec::new();
    match event {
        Event::Receipt(receipt) => {
            use whatsapp_rust::wacore::types::presence::ReceiptType;
            let kind = match receipt.r#type {
                ReceiptType::Delivered => Some(ReceiptKind::Delivered),
                ReceiptType::Sent => Some(ReceiptKind::Sent),
                ReceiptType::Read | ReceiptType::ReadSelf => Some(ReceiptKind::Read),
                ReceiptType::Played | ReceiptType::PlayedSelf => Some(ReceiptKind::Played),
                _ => None,
            };
            if let Some(kind) = kind {
                sink.emit(BackendEvent::Receipt {
                    chat_jid: receipt.source.chat.to_string(),
                    message_ids: receipt.message_ids.iter().map(ToString::to_string).collect(),
                    kind,
                    at: receipt.timestamp.timestamp(),
                    actor: Some(receipt.source.sender.to_string()),
                });
            }
        }
        Event::DisappearingModeChanged(change) => {
            // Group timer changes arrive as notifications (direct
            // chats come through the protocol-message path in
            // translate_special instead). The same notification
            // carries the account-wide default timer when it comes
            // from our own JID (an account_sync change made on
            // another device, the phone included); that is not a
            // per-chat change and must not land on a chat keyed by
            // our own JID.
            let from = change.from.to_string();
            if own_jids.iter().any(|own| own == &from) {
                sink.emit(BackendEvent::DefaultDisappearingTimerChanged {
                    duration_secs: change.duration,
                });
            } else {
                sink.emit(BackendEvent::DisappearingChanged {
                    chat_jid: from,
                    duration_secs: change.duration,
                });
            }
        }
        Event::MuteUpdate(update) => {
            // App-state sync reporting the account's mute state
            // for one chat: a mute or unmute from another device,
            // or the state as a full sync delivers it. This is
            // the only wire path that knows mute; message arrivals
            // do not carry it.
            sink.emit(BackendEvent::MuteChanged {
                chat_jid: update.jid.to_string(),
                muted: mute_action_state(&update.action, now_millis()),
                mute_until: mute_action_until(&update.action),
            });
        }
        Event::PinUpdate(update) => {
            // App-state sync reporting the account's pin state
            // for one chat: a pin or unpin from another device,
            // or the state as a full sync delivers it. This is
            // the only wire path that knows pin; message arrivals
            // do not carry it.
            sink.emit(BackendEvent::PinChanged {
                chat_jid: update.jid.to_string(),
                pinned: pin_action_state(&update.action),
            });
        }
        Event::ArchiveUpdate(update) => {
            // App-state sync reporting the account's archive
            // state for one chat: an archive or unarchive from
            // another device, or the state as a full sync
            // delivers it. This is the only wire path that knows
            // archive; message arrivals do not carry it. The
            // action's message range (which messages the archive
            // spans) is not chat-list state and has no
            // counterpart in the app's model, so only the flag
            // crosses the boundary.
            sink.emit(BackendEvent::ArchiveChanged {
                chat_jid: update.jid.to_string(),
                archived: archive_action_state(&update.action),
            });
        }
        Event::StarUpdate(update) => {
            // App-state sync reporting the account's star state
            // for one message: a star or unstar from another
            // device, or the state as a full sync delivers it
            // (the link-time import of the phone's pre-existing
            // stars rides this path). History-sync arrivals carry
            // the same state per message (WebMessageInfo.starred,
            // read in history_message); live arrivals do not.
            sink.emit(BackendEvent::StarChanged {
                chat_jid: update.chat_jid.to_string(),
                message_id: update.message_id.clone(),
                starred: star_action_state(&update.action),
            });
        }
        Event::MarkChatAsReadUpdate(update) => {
            // App-state sync reporting the account's read state
            // for one chat: marked read or unread from another
            // device, or the state as a full sync delivers it.
            // The device that made the change already sent its
            // read receipts (this sync action is distinct from
            // the receipt IQ), so the app adopts the state and
            // sends nothing.
            let (read, unread_count) = mark_read_action_state(&update.action);
            sink.emit(BackendEvent::ChatReadChanged {
                chat_jid: update.jid.to_string(),
                read,
                unread_count,
            });
        }
        Event::DeleteChatUpdate(update) => {
            // App-state sync reporting a chat deleted on
            // another device. The action carries only a message
            // range (the messages the deletion spans, on the
            // phone the whole conversation) and the event adds a
            // delete-media flag; neither crosses the boundary.
            // The app has no partial-delete concept (clearing a
            // chat is a separate sync action), so a deletion here
            // removes the chat and its archive whole, and the app
            // does not track downloaded media per chat, so there
            // is nothing for the media flag to delete.
            sink.emit(BackendEvent::ChatDeleted { chat_jid: update.jid.to_string() });
        }
        Event::ClearChatUpdate(update) => {
            // App-state sync reporting a chat cleared on
            // another device: the messages go, the chat stays.
            // The starred policy lives in the mutation index,
            // not the action, and the event carries it as a
            // flag. The deletion bound is the action's message
            // range (its last-message timestamp); the range's
            // message list enumerates what the clearing device
            // held and does not narrow the bound further. The
            // delete-media flag does not cross: the app does
            // not track downloaded media per chat, so there is
            // nothing for it to delete (the same call as chat
            // deletion). One wire nuance cannot cross either:
            // the mutation timestamp, which WA Web reads only
            // to spare a message starred after the clear was
            // made; the app keeps no star times, so it cannot
            // tell such a message from one starred before.
            sink.emit(BackendEvent::ChatCleared {
                chat_jid: update.jid.to_string(),
                delete_starred: update.delete_starred,
                last_message_timestamp: clear_chat_bound(&update.action),
            });
        }
        Event::DeleteMessageForMeUpdate(update) => {
            // App-state sync reporting one message deleted
            // for the account on another device. The identity
            // is the (chat, protocol ID) pair every other
            // per-message sync uses; the participant and
            // from-me fields stay behind, as with star. The
            // action's delete-media flag does not cross: the
            // app keeps no per-message media registry to
            // delete from, and its message timestamp is not
            // needed once the message is identified.
            sink.emit(BackendEvent::MessageDeletedForMe {
                chat_jid: update.chat_jid.to_string(),
                message_id: update.message_id.clone(),
            });
        }
        Event::ChatPresence(update) => {
            use whatsapp_rust::wacore::types::presence::ChatActivity;
            sink.emit(BackendEvent::TypingReceived {
                chat_jid: update.source.chat.to_string(),
                typing: !matches!(update.state, ChatActivity::Idle),
            });
        }
        Event::Presence(update) => {
            use whatsapp_rust::wacore::types::presence::PresenceStatus;
            // The library only delivers these for contacts this
            // client subscribed to (plus its own resubscribes
            // after a reconnect), so every report is live data
            // about a watched contact, never a cached guess.
            sink.emit(BackendEvent::PresenceReceived {
                chat_jid: update.from.to_string(),
                online: matches!(update.status, PresenceStatus::Available),
                last_seen: update.last_seen.map(|at| at.timestamp()),
            });
        }
        Event::TemporaryBan(ban) => {
            sink.emit(BackendEvent::TemporaryBan {
                code: u32::try_from(ban.code.code()).unwrap_or(0),
            });
        }
        Event::Disconnected(_) => {
            sink.emit(BackendEvent::ConnectionLost {
                reason: "Connection lost, the client will reconnect.".to_owned(),
            });
        }
        Event::PairingCodeError(err) => {
            sink.emit(BackendEvent::Error { message: pairing_error_message(err) });
        }
        Event::PairingQrCodesExhausted(_) => {
            // The library issues no further codes for this session
            // and (unless a pair-code flow is outstanding) closes
            // the connection: a QR still on screen is dead, and the
            // app must stop presenting it as scannable.
            sink.emit(BackendEvent::QrExpired);
        }
        Event::HistorySync(lazy) => {
            // The roster itself: after pairing, the phone sends
            // the account's conversations and their recent
            // messages as history sync chunks. This event is the
            // ONLY channel that carries them; without a handler
            // the list stays empty except for chats a live message
            // happens to create. Progress rides the same event.
            sink.emit(BackendEvent::SyncProgress { percent: lazy.progress() });
            if let Some(sync) = lazy.get() {
                (unresolved_titles, retry_marks) = translate_history_sync(sync, sink, book);
            }
        }
        Event::ContactUpdate(update) => {
            // App-state sync reporting a saved contact's name
            // (set or changed on any device, or delivered by a
            // full sync). The action carries the name plus both
            // of the contact's JID forms, so the resolution is
            // broadcast for both: the host rewrites raw sender
            // labels and a raw chat title wherever they are still
            // stored, and its guards keep resolved names safe.
            let action = &update.action;
            let name = action
                .full_name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .or_else(|| action.first_name.clone().filter(|n| !n.trim().is_empty()))
                .or_else(|| action.username.clone().filter(|n| !n.trim().is_empty()));
            if let Some(name) = name {
                let mut jids = vec![update.jid.to_string()];
                if let Some(lid) = &action.lid_jid {
                    if !jids.contains(lid) {
                        jids.push(lid.clone());
                    }
                }
                if let Some(pn) = &action.pn_jid {
                    if !jids.contains(pn) {
                        jids.push(pn.clone());
                    }
                }
                // The name joins the session identity book as a
                // saved name under every JID form, and the forms
                // are recorded as counterparts, so live traffic
                // from this contact resolves from here on.
                for jid in &jids {
                    book.note_saved(jid, &name);
                }
                for pair in jids.windows(2) {
                    book.note_mapping(&pair[0], &pair[1]);
                }
                for jid in jids {
                    sink.emit(BackendEvent::SenderNamed {
                        sender_jid: jid.clone(),
                        name: name.clone(),
                    });
                    // A saved name set or changed on a device is
                    // an authoritative rename: it replaces even a
                    // resolved title, it does not only heal a raw
                    // one.
                    sink.emit(BackendEvent::ChatRenamed {
                        chat_jid: jid,
                        title: name.clone(),
                        authoritative: true,
                    });
                }
            }
        }
        Event::OfflineSyncPreview(_) => {
            sink.emit(BackendEvent::SyncProgress { percent: None });
        }
        Event::OfflineSyncCompleted(_) => {
            sink.emit(BackendEvent::SyncCompleted);
        }
        Event::OfflineSyncInterrupted(_) => {
            // The library contract is explicit: the drain did
            // not finish and the client is not caught up. This
            // is never a completion (QC1614-A0-5 W1).
            sink.emit(BackendEvent::SyncInterrupted);
        }
        Event::PictureUpdate(update) => {
            // A picture was set, changed, or removed: any cached
            // copy for this identity is stale.
            sink.emit(BackendEvent::AvatarChanged { jid: update.jid.to_string() });
        }
        Event::ContactUpdated(update) => {
            // The signal WA Web refreshes the profile picture
            // on (the library's own docs say so).
            sink.emit(BackendEvent::AvatarChanged { jid: update.jid.to_string() });
        }
        Event::GroupUpdate(update) => {
            use whatsapp_rust::wacore::stanza::groups::GroupNotificationAction;
            let chat_jid = update.group_jid.to_string();
            if let GroupNotificationAction::Subject { subject, .. } = &*update.action {
                if !subject.trim().is_empty() {
                    // A subject change on the wire is an
                    // authoritative rename: the group's current
                    // subject replaces whatever title the chat
                    // carries, resolved ones included.
                    sink.emit(BackendEvent::ChatRenamed {
                        chat_jid: chat_jid.clone(),
                        title: subject.clone(),
                        authoritative: true,
                    });
                }
            }
            // Whatever changed (members, modes, subject), the
            // app's cached member list for this group is stale.
            sink.emit(BackendEvent::GroupChanged { chat_jid });
        }
        Event::CallLogSync(sync) => {
            if let Some(record) = call_record_of(
                &sync.record,
                Some(sync.call_id.clone()),
                Some(sync.from_me),
                Some(sync.call_creator_jid.to_string()),
                sync.record.start_time.filter(|t| *t > 0).or(Some(sync.timestamp.timestamp())),
            ) {
                sink.emit(BackendEvent::CallHistoryRecord { record });
            }
        }
        Event::CallLogHistory(history) => {
            if let Some(record) = call_record_of(
                &history.record,
                None,
                history.from_me,
                history.call_creator_jid.as_ref().map(ToString::to_string),
                history.timestamp.map(|t| t.timestamp()),
            ) {
                sink.emit(BackendEvent::CallHistoryRecord { record });
            }
        }
        Event::FavoritesUpdate(update) => {
            // The phone's favorite chats, complete: the app's
            // favorite set is replaced, not merged.
            let chat_jids = update
                .action
                .favorites
                .iter()
                .filter_map(|entry| entry.id.clone())
                .filter(|id| !id.is_empty())
                .collect();
            sink.emit(BackendEvent::FavoritesSync { chat_jids });
        }
        Event::FavoriteStickerUpdate(update) => {
            // Favorite-sticker sync (total audit P1-01): the
            // events are public variants on the bus this match
            // already consumes; the earlier register claim that
            // they were crate-private dispatch was wrong.
            sink.emit(BackendEvent::StickerFavoriteChanged {
                filehash: update.filehash.clone(),
                favorite: update.action.is_favorite.unwrap_or(false),
            });
        }
        Event::RemoveRecentStickerUpdate(update) => {
            sink.emit(BackendEvent::RecentStickerRemoved { filehash: update.filehash.clone() });
        }
        Event::AppStateSyncFailed(failed) => {
            // A fatal collection refusal is the case WA Web
            // treats as grounds to tell the user (the library
            // deliberately keeps the session alive); retryable
            // and skipped collections stay silent.
            if !failed.fatal.is_empty() {
                sink.emit(BackendEvent::Error {
                    message: "Some account data could not sync from your phone. If chats or \
                         settings look out of date, check the WhatsApp app on your phone."
                        .to_owned(),
                });
            }
        }
        Event::NewsletterLiveUpdate(update) => {
            // Channel live updates carry counters only
            // (reactions, poll tallies, forwards), keyed by
            // the update's server ID; post bodies arrive
            // through the message path. Forward each counter
            // set as its own event so the app can settle the
            // one update it names.
            let jid = update.newsletter_jid.to_string();
            for counters in &update.messages {
                sink.emit(BackendEvent::ChannelCountersUpdated {
                    counters: crate::LiveChannelCounters {
                        jid: jid.clone(),
                        server_id: counters.server_id,
                        reactions: counters
                            .reactions
                            .iter()
                            .map(|r| (r.code.clone(), u32::try_from(r.count).unwrap_or(u32::MAX)))
                            .collect(),
                        poll_votes: counters
                            .votes
                            .iter()
                            .map(|v| (v.option_hash, u32::try_from(v.count).unwrap_or(u32::MAX)))
                            .collect(),
                    },
                });
            }
        }
        Event::UndecryptableMessage(failed) => {
            use whatsapp_rust::wacore::types::wire_enums::DecryptFailMode;
            // Only the wire's show mode renders a placeholder;
            // hide mode means the sender's client expects the
            // gap to stay invisible.
            if failed.decrypt_fail_mode == DecryptFailMode::Show {
                let info = &failed.info;
                sink.emit(BackendEvent::MessageUnavailable {
                    chat_jid: info.source.chat.to_string(),
                    message_id: info.id.to_string(),
                    sender_jid: Some(info.source.sender.to_string()),
                    from_me: info.source.is_from_me,
                    timestamp: info.timestamp.timestamp(),
                });
            }
        }
        _ => {}
    }
    (unresolved_titles, retry_marks)
}

/// Builds the app's call record from one wire call-log record
/// (the pairing-time history and app-state mutations share the
/// record shape). Direction comes from the event's own creator
/// comparison (`from_me`), never from the record's `is_incoming`
/// flag, which the library documents as unreliable in both
/// directions. Records without a call ID are skipped: history
/// chunks replay, and an ID-less record could not be told apart
/// from its own replay.
fn call_record_of(
    record: &whatsapp_rust::prelude::wa::CallLogRecord,
    call_id_override: Option<String>,
    from_me: Option<bool>,
    creator_jid: Option<String>,
    fallback_started_at: Option<i64>,
) -> Option<crate::CallRecord> {
    use whatsapp_rust::prelude::wa::call_log_record::CallResult;
    let call_id = call_id_override
        .filter(|id| !id.is_empty())
        .or_else(|| record.call_id.clone().filter(|id| !id.is_empty()))?;
    let creator = creator_jid
        .filter(|j| !j.is_empty())
        .or_else(|| record.call_creator_jid.clone().filter(|j| !j.is_empty()));
    let outgoing = from_me.unwrap_or(false);
    let chat_jid = record.group_jid.clone().filter(|j| !j.is_empty()).or_else(|| {
        if outgoing {
            // The chat is the other party: the first
            // participant who is not the creator.
            record
                .participants
                .iter()
                .filter_map(|p| p.user_jid.clone())
                .find(|jid| Some(jid) != creator.as_ref())
                .or_else(|| creator.clone())
        } else {
            creator.clone()
        }
    })?;
    let missed = !outgoing && record.call_result == Some(CallResult::Missed);
    Some(crate::CallRecord {
        id: 0,
        call_id,
        chat_jid,
        video: record.is_video.unwrap_or(false),
        outgoing,
        missed,
        duration_secs: u32::try_from(record.duration.unwrap_or(0).max(0)).unwrap_or(0),
        started_at: record.start_time.filter(|t| *t > 0).or(fallback_started_at).unwrap_or(0),
    })
}

/// Translates one decoded history sync payload into chat and
/// message upserts: the roster as the phone reported it, with each
/// conversation's carried messages translated by the same
/// classifier and body rules as live traffic, so history and live
/// messages are indistinguishable downstream.
///
/// Every identity source the payload carries is gathered first
/// into an [`IdentityBook`]: the saved contact names from the
/// inline contacts, the push names, and the LID-to-phone-number
/// mappings in both directions. Names resolve against all of
/// them, never just one. The return value lists the chats whose
/// titles still fell back to the raw identifier, so the session
/// can heal them from the client's own stores (group metadata,
/// the learned LID-to-phone cache).
pub(super) fn translate_history_sync(
    sync: &whatsapp_rust::prelude::wa::HistorySync,
    sink: &EventSink,
    book: &mut IdentityBook,
) -> (TitleHeals, RetryMarks) {
    // The payload's own sources join the session book first, so
    // this payload resolves against everything the session has
    // learned so far, and later payloads and live messages
    // resolve against this payload in turn.
    book.absorb(IdentityBook::from_sync(sync));
    let mut resolved_senders: Vec<(String, String)> = Vec::new();
    let mut unresolved: TitleHeals = Vec::new();
    let mut retry_marks: RetryMarks = Vec::new();
    for conv in &sync.conversations {
        if let Some(item) = translate_history_conversation(
            conv,
            book,
            &mut resolved_senders,
            &mut retry_marks,
            sink,
        ) {
            unresolved.push(item);
        }
    }
    // A name learned for a sender in this payload is valid in
    // every chat, including ones other payloads delivered: tell
    // the host, which rewrites any stored labels that still show
    // the raw identifier. Resolved labels are never downgraded
    // (the host's store guards enforce that), so broadcasting is
    // safe.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (sender_jid, name) in &resolved_senders {
        if seen.insert(sender_jid.as_str()) {
            sink.emit(BackendEvent::SenderNamed {
                sender_jid: sender_jid.clone(),
                name: name.clone(),
            });
        }
    }
    (unresolved, retry_marks)
}

/// The identity sources a session has learned, indexed for
/// resolution. A person appears under two JID forms (phone number
/// and LID); sources are keyed by whichever form they used, so
/// every lookup tries the JID itself and its counterpart form.
/// The session owns one book for its whole life: history payloads
/// are absorbed into it, live messages register their sender's
/// push name and counterpart mapping into it, and contact updates
/// land in it as saved names, so the live translation path
/// resolves sender labels, quoted authors and direct-chat titles
/// from the same sources the history path uses, in the same
/// order (saved contact name, push name, formatted phone number).
#[derive(Default)]
pub(super) struct IdentityBook {
    /// Saved contact names (inline contacts: full name, else
    /// first name, else username), by JID in either form.
    saved: std::collections::HashMap<String, String>,
    /// Push names, by JID in either form.
    push: std::collections::HashMap<String, String>,
    /// LID JID to phone-number JID.
    lid_to_pn: std::collections::HashMap<String, String>,
    /// Phone-number JID to LID JID.
    pn_to_lid: std::collections::HashMap<String, String>,
}

impl IdentityBook {
    fn from_sync(sync: &whatsapp_rust::prelude::wa::HistorySync) -> Self {
        let mut saved = std::collections::HashMap::new();
        for contact in &sync.inline_contacts {
            let name = contact
                .full_name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .or_else(|| contact.first_name.clone().filter(|n| !n.trim().is_empty()))
                .or_else(|| contact.username.clone().filter(|n| !n.trim().is_empty()));
            if let Some(name) = name {
                if let Some(pn) = &contact.pn_jid {
                    saved.insert(pn.clone(), name.clone());
                }
                if let Some(lid) = &contact.lid_jid {
                    saved.insert(lid.clone(), name);
                }
            }
        }
        let push: std::collections::HashMap<String, String> = sync
            .pushnames
            .iter()
            .filter_map(|p| Some((p.id.clone()?, p.pushname.clone()?)))
            .filter(|(_, name)| !name.trim().is_empty())
            .collect();
        let mut lid_to_pn = std::collections::HashMap::new();
        let mut pn_to_lid = std::collections::HashMap::new();
        for mapping in &sync.phone_number_to_lid_mappings {
            if let (Some(pn), Some(lid)) = (&mapping.pn_jid, &mapping.lid_jid) {
                lid_to_pn.insert(lid.clone(), pn.clone());
                pn_to_lid.insert(pn.clone(), lid.clone());
            }
        }
        Self { saved, push, lid_to_pn, pn_to_lid }
    }

    /// Merges another book (one history payload's sources) into
    /// this one. Later sources win within their own map; the
    /// resolution order keeps the maps apart, so a push name can
    /// never downgrade a saved name no matter the merge order.
    fn absorb(&mut self, other: IdentityBook) {
        self.saved.extend(other.saved);
        self.push.extend(other.push);
        self.lid_to_pn.extend(other.lid_to_pn);
        self.pn_to_lid.extend(other.pn_to_lid);
    }

    /// Records a saved contact name for a JID (a contact update,
    /// or a history payload's inline contact).
    pub(super) fn note_saved(&mut self, jid: &str, name: &str) {
        if !name.trim().is_empty() {
            self.saved.insert(jid.to_owned(), name.to_owned());
        }
    }

    /// Records a push name for a JID (a live message carries its
    /// sender's current push name).
    pub(super) fn note_push(&mut self, jid: &str, name: &str) {
        if !name.trim().is_empty() {
            self.push.insert(jid.to_owned(), name.to_owned());
        }
    }

    /// Records that two JIDs are the same person's two forms
    /// (phone number and LID), from whichever source learned it:
    /// a payload mapping, a contact update's JID pair, or a live
    /// message's alternate sender address.
    pub(super) fn note_mapping(&mut self, a: &str, b: &str) {
        if a == b {
            return;
        }
        if a.ends_with("@lid") && b.ends_with("@s.whatsapp.net") {
            self.lid_to_pn.insert(a.to_owned(), b.to_owned());
            self.pn_to_lid.insert(b.to_owned(), a.to_owned());
        } else if b.ends_with("@lid") && a.ends_with("@s.whatsapp.net") {
            self.lid_to_pn.insert(b.to_owned(), a.to_owned());
            self.pn_to_lid.insert(a.to_owned(), b.to_owned());
        }
    }

    /// The same person's other JID form, when the payload's
    /// mappings know it.
    fn counterpart(&self, jid: &str) -> Option<&str> {
        self.lid_to_pn.get(jid).or_else(|| self.pn_to_lid.get(jid)).map(String::as_str)
    }

    /// The best display name the payload knows for a JID: a saved
    /// contact name first, then a push name, each tried under the
    /// JID itself and under its counterpart form.
    pub(super) fn name_for(&self, jid: &str) -> Option<String> {
        if let Some(name) = self.saved.get(jid) {
            return Some(name.clone());
        }
        if let Some(other) = self.counterpart(jid) {
            if let Some(name) = self.saved.get(other) {
                return Some(name.clone());
            }
        }
        if let Some(name) = self.push.get(jid) {
            return Some(name.clone());
        }
        if let Some(other) = self.counterpart(jid) {
            if let Some(name) = self.push.get(other) {
                return Some(name.clone());
            }
        }
        None
    }

    /// The display label for a live message's sender: the book's
    /// best name (saved contact name first), then the push name
    /// this message itself carries, then the formatted phone
    /// number, and the raw user part only when nothing better
    /// exists in any source.
    pub(super) fn sender_label(&self, sender_jid: &str, own_push: &str) -> String {
        if let Some(name) = self.name_for(sender_jid) {
            return name;
        }
        if !own_push.trim().is_empty() {
            return own_push.to_owned();
        }
        if let Some(phone) = self.phone_for(sender_jid) {
            return phone;
        }
        user_part(sender_jid)
    }

    /// The display label for an author named only by JID (a
    /// quoted message's author, a reaction's actor): the book's
    /// best name, then the formatted phone number, then the raw
    /// user part.
    pub(super) fn author_label(&self, author_jid: &str) -> String {
        self.name_for(author_jid)
            .or_else(|| self.phone_for(author_jid))
            .unwrap_or_else(|| user_part(author_jid))
    }

    /// The title a live arrival gives its direct chat: the book's
    /// name for the chat itself first (a name recorded under the
    /// counterpart JID form counts), then the sender's label,
    /// then the chat's formatted phone number, then the raw user
    /// part.
    pub(super) fn dm_title(&self, chat_jid: &str, sender_label: Option<&str>) -> String {
        self.name_for(chat_jid)
            .or_else(|| sender_label.map(ToOwned::to_owned))
            .or_else(|| self.phone_for(chat_jid))
            .unwrap_or_else(|| user_part(chat_jid))
    }

    /// The person's phone number, formatted for display, when the
    /// payload's mappings (or the JID itself, when it already is
    /// a phone-number JID) provide one.
    fn phone_for(&self, jid: &str) -> Option<String> {
        let pn = if jid.ends_with("@lid") {
            self.lid_to_pn.get(jid)?
        } else if jid.ends_with("@s.whatsapp.net") {
            jid
        } else {
            return None;
        };
        Some(format!("+{}", user_part(pn)))
    }
}

/// Whether a display string is a raw identifier rather than a
/// name: nine or more characters, all digits. LIDs, phone numbers
/// and group ids all qualify; a person or group actually named
/// with a long digit string would be misread, which is accepted:
/// the cost is one unrenamed chat, never a wrong name.
pub(super) fn is_raw_identifier(text: &str) -> bool {
    text.len() >= 9 && text.bytes().all(|b| b.is_ascii_digit())
}

fn translate_history_conversation(
    conv: &whatsapp_rust::prelude::wa::Conversation,
    book: &IdentityBook,
    resolved_senders: &mut Vec<(String, String)>,
    retry_marks: &mut RetryMarks,
    sink: &EventSink,
) -> Option<(String, LiveChatKind)> {
    let chat_jid = conv.id.clone();
    if chat_jid.is_empty() {
        return None;
    }
    if chat_jid == STATUS_BROADCAST_JID {
        // The status broadcast "conversation" in a history
        // payload is not a chat: see STATUS_BROADCAST_JID. Its
        // payloads are the statuses the server still holds,
        // redelivered at link time; each one feeds the Updates
        // feed as a received status and nothing becomes a chat
        // or enters the archive.
        for entry in &conv.messages {
            if let Some(info) = entry.message.as_option() {
                if let Some(status) = history_status(info, book) {
                    sink.emit(BackendEvent::StatusReceived { status: Box::new(status) });
                }
            }
        }
        return None;
    }
    let kind = kind_of(&chat_jid);
    let mut messages: Vec<LiveMessage> = Vec::new();
    for entry in &conv.messages {
        if let Some(info) = entry.message.as_option() {
            if let Some(msg) = history_message(&chat_jid, info, book, resolved_senders) {
                messages.push(msg);
            }
        }
    }
    // Every history message's media-retry addressing is recorded
    // by the session: history media is exactly the media whose
    // CDN copies expire, and the retry flow needs the author's
    // identity, which only this payload knows.
    for msg in &messages {
        retry_marks.push((
            chat_jid.clone(),
            msg.id.clone(),
            super::util::RetryMark { from_me: msg.from_me, sender_jid: msg.sender_jid.clone() },
        ));
    }
    messages.sort_by_key(|m| m.timestamp);
    let last = messages.last();
    // Title resolution, in the owner's order of preference: the
    // conversation's own name (a group subject or a saved contact
    // name), then the payload's saved/push names for this JID in
    // either form, then (for a direct chat) the last sender's
    // already-resolved name. The formatted phone number from the
    // LID-to-phone mapping is only the displayed fallback, never
    // a resolution: a chat titled by its number is reported
    // unresolved, so the heal pass can still bring the saved
    // contact name the payload did not carry. The raw identifier
    // is the last resort.
    let named_title = conv
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| conv.display_name.clone().filter(|n| !n.trim().is_empty()))
        .or_else(|| book.name_for(&chat_jid))
        .or_else(|| match kind {
            LiveChatKind::Dm => {
                last.and_then(|m| m.sender.clone()).filter(|name| !is_raw_identifier(name))
            }
            _ => None,
        });
    let title_unresolved = named_title.is_none();
    let title = named_title
        .or_else(|| match kind {
            LiveChatKind::Dm => book.phone_for(&chat_jid),
            // A broadcast list has no contact name to heal and
            // its raw id is never a title (P2-06).
            LiveChatKind::Broadcast => Some("Broadcast list".to_owned()),
            _ => None,
        })
        .unwrap_or_else(|| user_part(&chat_jid));
    let timestamp = conv
        .last_msg_timestamp
        .map(|t| i64::try_from(t).unwrap_or(0))
        .filter(|t| *t > 0)
        .or_else(|| last.map(|m| m.timestamp))
        .unwrap_or(0);
    let now_secs = now_millis() / 1000;
    let chat = LiveChat {
        jid: chat_jid.clone(),
        title,
        kind,
        preview: last.map(|m| core::model::flatten_preview(&m.body)).unwrap_or_default(),
        preview_sender: if kind == LiveChatKind::Group || kind == LiveChatKind::Broadcast {
            last.and_then(|m| m.sender.clone()).map(|s| core::model::flatten_preview(&s))
        } else {
            None
        },
        timestamp,
        unread: conv.unread_count.unwrap_or(0),
        archived: conv.archived.unwrap_or(false),
        pinned: conv.pinned.is_some_and(|p| p > 0),
        ephemeral_secs: conv.ephemeral_expiration.unwrap_or(0),
        muted: conv.mute_end_time.is_some_and(|end| i64::try_from(end).unwrap_or(0) > now_secs),
        mute_until: conv
            .mute_end_time
            .map(|end| i64::try_from(end).unwrap_or(0))
            .filter(|end| *end > now_secs)
            .unwrap_or(0),
        parent_jid: None,
        is_parent: false,
    };
    sink.emit(BackendEvent::ChatUpsert(chat));
    for msg in messages {
        sink.emit(BackendEvent::MessageUpsert(msg));
    }
    if title_unresolved
        && matches!(kind, LiveChatKind::Dm | LiveChatKind::Group | LiveChatKind::Channel)
    {
        Some((chat_jid, kind))
    } else {
        None
    }
}

/// Translates one history-carried message. Stub entries and
/// messages the phone marked to ignore carry no displayable
/// payload and are skipped. Sender names resolve through the
/// payload's identity book (saved contact name, then push name,
/// then the formatted phone number from the LID-to-phone
/// mapping); a name resolved from the book is reported back in
/// `resolved_senders`, because it is valid for that sender in
/// every chat, not only this one.
fn history_message(
    chat_jid: &str,
    info: &whatsapp_rust::prelude::wa::WebMessageInfo,
    book: &IdentityBook,
    resolved_senders: &mut Vec<(String, String)>,
) -> Option<LiveMessage> {
    if info.ignore == Some(true) {
        return None;
    }
    let proto = info.message.as_option()?;
    let key = info.key.as_option()?;
    let id = key.id.clone()?;
    let from_me = key.from_me.unwrap_or(false);
    let timestamp = i64::try_from(info.message_timestamp.unwrap_or(0)).unwrap_or(0);
    let text = proto.text_content().map(ToOwned::to_owned).or_else(|| structured_text(proto));
    let (mut kind, view_once) = classify_message(proto);
    if matches!(kind, LiveMessageKind::Text) && text.is_none() {
        kind = LiveMessageKind::Other(unhandled_label(proto));
    }
    let body = body_for(&kind, view_once, text.as_deref());
    let sender_jid = key.participant.clone().or_else(|| info.participant.clone()).or_else(|| {
        // A 1:1 payload names no participant: the author of an
        // incoming direct message is the conversation's other
        // party, named by the key's remote JID. Without this the
        // author collapses to None, which the app's model reads
        // as "mine", so the message would render as the owner's
        // own outgoing message.
        (!from_me && kind_of(chat_jid) == LiveChatKind::Dm)
            .then(|| key.remote_jid.clone().unwrap_or_else(|| chat_jid.to_owned()))
    });
    let sender_name = if from_me {
        None
    } else {
        // The book first (it prefers the saved contact name
        // over any push name), then this payload's push name,
        // then the formatted phone number: a saved name must
        // never be downgraded by a push name the same payload
        // happens to carry.
        let own_push_name = info.push_name.clone().filter(|n| !n.trim().is_empty());
        let resolved = sender_jid
            .as_deref()
            .and_then(|jid| book.name_for(jid))
            .or(own_push_name)
            .or_else(|| sender_jid.as_deref().and_then(|jid| book.phone_for(jid)));
        if let (Some(jid), Some(name)) = (&sender_jid, &resolved) {
            if !is_raw_identifier(name) {
                resolved_senders.push((jid.clone(), name.clone()));
            }
        }
        resolved.or_else(|| sender_jid.as_deref().map(user_part))
    };
    let mut msg = LiveMessage::text(id, chat_jid.to_owned(), sender_name, body, timestamp, from_me);
    // The history payload carries the account's star state for
    // this message (WebMessageInfo.starred): a message the phone
    // had starred before linking arrives already starred, so the
    // archive and the Starred panel tell the truth after a fresh
    // link without depending on the app-state sync's timing. The
    // app-state path (StarUpdate) remains the authoritative one
    // for later changes; the two agree by construction, and the
    // app's flag resolution never lets an uninformed upsert clear
    // a star either path set.
    msg.starred = info.starred.unwrap_or(false);
    // The wire's authoritative per-message status restores the
    // delivery ticks a fresh link would otherwise lose: history
    // translation reads WebMessageInfo.status for outgoing rows.
    if from_me {
        use whatsapp_rust::prelude::wa::web_message_info::Status as WireStatus;
        msg.delivery = match info.status {
            Some(WireStatus::Read) | Some(WireStatus::Played) => Some(crate::DeliveryState::Read),
            Some(WireStatus::DeliveryAck) => Some(crate::DeliveryState::Delivered),
            Some(WireStatus::ServerAck) | Some(WireStatus::Pending) => {
                Some(crate::DeliveryState::Sent)
            }
            _ => None,
        };
    }
    msg.kind = kind;
    msg.view_once = view_once;
    msg.photo_ref = super::util::photo_ref_of(proto);
    msg.voice_ref = super::util::voice_ref_of(proto);
    msg.video_ref = super::util::video_ref_of(proto);
    msg.video_thumbnail = super::util::video_thumbnail_of(proto);
    msg.poster_ref = super::util::poster_ref_of(proto);
    if let Some((width, height)) = super::util::video_dimensions_of(proto) {
        msg.video_width = Some(width);
        msg.video_height = Some(height);
    }
    msg.voice_waveform = super::util::voice_waveform_of(proto);
    msg.photo_thumbnail = super::util::photo_thumbnail_of(proto);
    msg.sticker_ref = super::util::sticker_ref_of(proto);
    carry_media_extras(&mut msg, proto);
    msg.mentions = super::util::mentioned_jids_of(proto);
    // History sync states the sender's verified business name
    // on the message itself when the sender is a verified
    // business; the app badges the chat from it.
    msg.sender_verified_name = info.verified_biz_name.clone().filter(|n| !n.trim().is_empty());
    msg.sender_jid = sender_jid;
    if matches!(msg.kind, LiveMessageKind::Poll { .. }) {
        if let Some((secret, _names)) = super::util::received_poll_secret(proto) {
            msg.poll_secret = Some(secret);
            msg.poll_creator = msg.sender_jid.clone();
        }
    }
    if matches!(msg.kind, LiveMessageKind::Event { .. }) {
        if let Some(secret) = super::util::received_event_secret(proto) {
            msg.event_secret = Some(secret);
            msg.event_creator = msg.sender_jid.clone();
        }
    }
    Some(msg)
}

/// Carries the media extras both translation paths (history and
/// live) attach to a message: the GIF flag, the sticker's
/// animation flags and static frame, the document's download
/// reference, name, page count and thumbnail, and the location's
/// map snapshot. Each extractor yields nothing for payloads of
/// other kinds, so this runs unconditionally.
fn carry_media_extras(msg: &mut LiveMessage, proto: &whatsapp_rust::prelude::wa::Message) {
    msg.gif_playback = super::util::gif_playback_of(proto);
    let (animated, lottie) = super::util::sticker_flags_of(proto);
    msg.sticker_animated = animated;
    msg.sticker_lottie = lottie;
    msg.sticker_thumbnail = super::util::sticker_thumbnail_of(proto);
    msg.document_ref = super::util::document_ref_of(proto);
    msg.document_name = super::util::document_name_of(proto);
    msg.document_page_count = super::util::document_page_count_of(proto);
    msg.document_thumbnail = super::util::document_thumbnail_of(proto);
    msg.location_thumbnail = super::util::location_thumbnail_of(proto);
    // A document without a caption keeps its file name as its
    // body (search and previews name the file, not the bare
    // kind label); a captioned document's body is its caption
    // already, through body_for.
    if msg.body == "Document" {
        if let Some(name) = &msg.document_name {
            msg.body = name.clone();
        }
    }
}

/// The display body for one translated message: its own text when
/// it has any, otherwise a typed summary from its kind (a photo
/// reads "Photo", a document its caption or the bare label,
/// a poll its question). A view-once caption stays sealed: it is
/// part of the protected payload and is never promoted to the
/// body, where previews, notifications and the archive would pick
/// it up as ordinary text. No bracketed placeholder is ever
/// produced: the old "[non-text message]" literal rendered as a
/// fake text bubble for every unwrapped media payload.
pub(super) fn body_for(kind: &LiveMessageKind, view_once: bool, text: Option<&str>) -> String {
    if let Some(text) = text {
        if !text.is_empty() {
            return text.to_owned();
        }
    }
    match kind {
        LiveMessageKind::Media { media, caption, .. } => {
            if !view_once && !caption.is_empty() {
                caption.clone()
            } else {
                match media {
                    crate::MediaKind::Photo => "Photo",
                    crate::MediaKind::Video => "Video",
                    crate::MediaKind::Voice => "Voice message",
                    crate::MediaKind::Audio => "Audio",
                    crate::MediaKind::Document => "Document",
                    crate::MediaKind::Sticker => "Sticker",
                }
                .to_owned()
            }
        }
        LiveMessageKind::Poll { question, .. } => {
            if question.is_empty() {
                "Poll".to_owned()
            } else {
                question.clone()
            }
        }
        LiveMessageKind::Event { name, canceled, .. } => {
            if *canceled {
                "Event canceled".to_owned()
            } else if name.is_empty() {
                "Event".to_owned()
            } else {
                name.clone()
            }
        }
        LiveMessageKind::Location { name, .. } => name.clone(),
        LiveMessageKind::Contact { name, .. } => name.clone(),
        LiveMessageKind::Carousel { body, .. } => body.clone(),
        LiveMessageKind::Other(label) => label.clone(),
        LiveMessageKind::Text => "Message".to_owned(),
    }
}

/// The body text a structured payload carries in its own
/// fields: template, buttons, list, product, order, and
/// non-carousel interactive messages are text messages wearing
/// structure, and their words live outside the two slots
/// `text_content` reads. Recovering them here means previews,
/// bubbles, search, and exports show the sender's words
/// instead of a placeholder. A template whose hydrated text
/// still carries an unsubstituted parameter marker is not
/// recovered: showing raw markers would be worse than the
/// typed label the fallthrough gives it.
pub(super) fn structured_text(msg: &whatsapp_rust::prelude::wa::Message) -> Option<String> {
    let base = msg.get_base_message();
    let clean = |s: Option<String>| s.filter(|t| !t.trim().is_empty());
    // A channel comment's words live in its inner message
    // (audit P0-09): recover them from there, the same
    // descent the classifier makes.
    if let Some(inner) = base.comment_message.as_option().and_then(|c| c.message.as_option()) {
        if let Some(text) = clean(inner.text_content().map(ToOwned::to_owned)) {
            return Some(text);
        }
        if let Some(text) = structured_text(inner) {
            return Some(text);
        }
    }
    if let Some(template) = base.template_message.as_option() {
        if let Some(text) = clean(
            template.hydrated_template.as_option().and_then(|t| t.hydrated_content_text.clone()),
        ) {
            if !text.contains("{{") {
                return Some(text);
            }
        }
    }
    // An AI rich response carries its words in its
    // submessages' text parts (audit P0-09): joined, they
    // are the response's body. A response with no text
    // parts keeps its typed label in the fallthrough.
    if let Some(rich) = base.rich_response_message.as_option() {
        let joined = rich
            .submessages
            .iter()
            .filter_map(|s| s.message_text.clone())
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(text) = clean(Some(joined)) {
            return Some(text);
        }
    }
    if let Some(buttons) = base.buttons_message.as_option() {
        if let Some(text) = clean(buttons.content_text.clone()) {
            return Some(text);
        }
    }
    if let Some(list) = base.list_message.as_option() {
        if let Some(text) = clean(list.title.clone()).or_else(|| clean(list.description.clone())) {
            return Some(text);
        }
    }
    if let Some(product) = base.product_message.as_option() {
        let snapshot = product.product.as_option();
        if let Some(text) = clean(product.body.clone())
            .or_else(|| clean(snapshot.and_then(|p| p.title.clone())))
            .or_else(|| clean(snapshot.and_then(|p| p.description.clone())))
        {
            return Some(text);
        }
    }
    if let Some(order) = base.order_message.as_option() {
        if let Some(text) =
            clean(order.message.clone()).or_else(|| clean(order.order_title.clone()))
        {
            return Some(text);
        }
    }
    if let Some(interactive) = base.interactive_message.as_option() {
        if let Some(text) = clean(interactive.body.as_option().and_then(|b| b.text.clone())) {
            return Some(text);
        }
    }
    // Replies to structured messages carry the words the
    // sender picked: the tapped button's label, the chosen
    // list row, the answer text. Those words are the message;
    // without this recovery they fell through to a generic
    // label, and a conversation lost what was actually said.
    if let Some(reply) = base.template_button_reply_message.as_option() {
        if let Some(text) = clean(reply.selected_display_text.clone()) {
            return Some(text);
        }
    }
    if let Some(reply) = base.buttons_response_message.as_option() {
        if let Some(
            whatsapp_rust::prelude::wa::message::buttons_response_message::Response::SelectedDisplayText(
                text,
            ),
        ) = &reply.response
        {
            if let Some(text) = clean(Some(text.clone())) {
                return Some(text);
            }
        }
    }
    if let Some(reply) = base.list_response_message.as_option() {
        if let Some(text) = clean(reply.title.clone()).or_else(|| clean(reply.description.clone()))
        {
            return Some(text);
        }
    }
    if let Some(reply) = base.interactive_response_message.as_option() {
        if let Some(text) = clean(reply.body.as_option().and_then(|b| b.text.clone())) {
            return Some(text);
        }
    }
    if let Some(reply) = base.question_response_message.as_option() {
        if let Some(text) = clean(reply.text.clone()) {
            return Some(text);
        }
    }
    // A poll result snapshot carries the whole result in the
    // clear: the poll's name and every option's vote count.
    // The summary is the message; nothing about it needs a
    // label-only rendering.
    if let Some(snapshot) = base
        .poll_result_snapshot_message
        .as_option()
        .or_else(|| base.poll_result_snapshot_message_v3.as_option())
    {
        let mut parts: Vec<String> = Vec::new();
        if let Some(name) = clean(snapshot.name.clone()) {
            parts.push(name);
        }
        for vote in &snapshot.poll_votes {
            if let (Some(name), Some(count)) =
                (clean(vote.option_name.clone()), vote.option_vote_count)
            {
                parts.push(format!("{name}: {count}"));
            }
        }
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    // An added poll option names itself.
    if let Some(added) = base.poll_add_option_message.as_option() {
        if let Some(option) = added.add_option.as_option() {
            if let Some(text) = clean(option.option_name.clone()) {
                return Some(text);
            }
        }
    }
    // A scheduled call carries its title.
    if let Some(call) = base.scheduled_call_creation_message.as_option() {
        if let Some(text) = clean(call.title.clone()) {
            return Some(text);
        }
    }
    // A sticker pack carries its name (and publisher).
    if let Some(pack) = base.sticker_pack_message.as_option() {
        if let Some(name) = clean(pack.name.clone()) {
            return Some(match clean(pack.publisher.clone()) {
                Some(publisher) => format!("{name} · {publisher}"),
                None => name,
            });
        }
    }
    // A music share carries the song's title and author.
    if let Some(music) = base.music_message.as_option() {
        if let Some(embedded) = music.embedded_music.as_option() {
            if let Some(title) = clean(embedded.title.clone()) {
                return Some(match clean(embedded.author.clone()) {
                    Some(author) => format!("{title} · {author}"),
                    None => title,
                });
            }
        }
    }
    None
}

/// The typed label for the readable-but-not-media kinds whose
/// carried text [`structured_text`] recovers (a poll result
/// snapshot, an added poll option, a scheduled call, a sticker
/// pack, a music share): they classify as typed summaries with
/// the recovered text as the body, on both the live and the
/// history path.
pub(super) fn typed_summary_label(
    msg: &whatsapp_rust::prelude::wa::Message,
) -> Option<&'static str> {
    let base = msg.get_base_message();
    if base.poll_result_snapshot_message.as_option().is_some()
        || base.poll_result_snapshot_message_v3.as_option().is_some()
    {
        Some("Poll results")
    } else if base.poll_add_option_message.as_option().is_some() {
        Some("Poll option added")
    } else if base.scheduled_call_creation_message.as_option().is_some() {
        Some("Scheduled call")
    } else if base.sticker_pack_message.as_option().is_some() {
        Some("Sticker pack")
    } else if base.music_message.as_option().is_some() {
        Some("Music")
    } else {
        None
    }
}

/// The truthful label for a payload no classifier arm claims:
/// each remaining kind names itself, so an untranslated message
/// is summarized as what it is and never impersonates a text
/// message with the single word "Message". Only a payload with
/// nothing recognizable at all keeps the neutral word.
pub(super) fn unhandled_label(msg: &whatsapp_rust::prelude::wa::Message) -> String {
    let base = msg.get_base_message();
    let label = if base.comment_message.as_option().is_some()
        || base.enc_comment_message.as_option().is_some()
    {
        "Channel comment"
    } else if base.newsletter_follower_invite_message_v2.as_option().is_some() {
        "Channel invite"
    } else if base.newsletter_admin_profile_message.as_option().is_some()
        || base.newsletter_admin_profile_status_message.as_option().is_some()
    {
        "Channel update"
    } else if base.sticker_pack_message.as_option().is_some() {
        "Sticker pack"
    } else if base.poll_result_snapshot_message.as_option().is_some()
        || base.poll_result_snapshot_message_v3.as_option().is_some()
    {
        "Poll results"
    } else if base.secret_encrypted_message.as_option().is_some() {
        "Encrypted message"
    } else if base.enc_reaction_message.as_option().is_some() {
        "Encrypted reaction"
    } else if base.enc_event_response_message.as_option().is_some() {
        "Encrypted response"
    } else if base.placeholder_message.as_option().is_some() {
        "Unavailable message"
    } else if base.poll_add_option_message.as_option().is_some() {
        "Poll option added"
    } else if base.template_button_reply_message.as_option().is_some()
        || base.buttons_response_message.as_option().is_some()
        || base.list_response_message.as_option().is_some()
        || base.interactive_response_message.as_option().is_some()
        || base.question_response_message.as_option().is_some()
    {
        "Reply"
    } else if base.template_message.as_option().is_some() {
        "Template message"
    } else if base.scheduled_call_creation_message.as_option().is_some() {
        "Scheduled call"
    } else if base.request_phone_number_message.as_option().is_some() {
        "Phone number request"
    } else if base.request_location_message.as_option().is_some() {
        "Location request"
    } else if base.split_payment_message.as_option().is_some()
        || base.send_payment_message.as_option().is_some()
    {
        // The payment family is parity-excluded by design; an
        // honest typed placeholder is its end state (P2-02).
        "Payment"
    } else if base.request_payment_message.as_option().is_some() {
        "Payment request"
    } else if base.cancel_payment_request_message.as_option().is_some() {
        "Payment request cancelled"
    } else if base.decline_payment_request_message.as_option().is_some() {
        "Payment request declined"
    } else if base.payment_invite_message.as_option().is_some() {
        "Payment invite"
    } else if base.payment_reminder_message.as_option().is_some() {
        "Payment reminder"
    } else if base.invoice_message.as_option().is_some() {
        "Invoice"
    } else if base.conditional_reveal_message.as_option().is_some() {
        // Rare and status-surface kinds get their own typed
        // labels instead of the generic fallthrough (P2-03).
        "Conditionally revealed message"
    } else if base.status_mention_message.as_option().is_some()
        || base.group_status_mention_message.as_option().is_some()
    {
        "Status mention"
    } else if base.group_status_message.as_option().is_some() {
        "Group status"
    } else if base.question_message.as_option().is_some() {
        "Question"
    } else if base.question_reply_message.as_option().is_some() {
        "Question answer"
    } else if base.scheduled_call_edit_message.as_option().is_some() {
        "Scheduled call edited"
    } else if base.music_message.as_option().is_some() {
        "Music"
    } else if base.message_history_bundle.as_option().is_some()
        || base.message_history_notice.as_option().is_some()
    {
        "Shared chat history"
    } else if base.rich_response_message.as_option().is_some() {
        "AI response"
    } else if base.bcall_message.as_option().is_some() {
        "Call"
    } else {
        "Message"
    };
    label.to_owned()
}

/// Translates one incoming message context into a chat + message
/// upsert, in Zaprust terms. Sender labels, the quoted author and
/// the direct-chat title resolve through the session identity
/// book, in the documented order (saved contact name, push name,
/// formatted phone number), so live traffic shows the same names
/// the history path resolved; the raw identifier is the last
/// resort only.
/// The phone number inside a vCard (P2-10): the value after
/// the LAST colon of the first TEL line, with a `tel:` URI
/// scheme stripped. Param-heavy lines
/// (`TEL;VALUE=uri:tel:+62...`, `TEL;waid=123:+62...`) used to
/// yield the literal word "tel" or the parameter text as the
/// phone; taking the final segment reads every common shape.
fn vcard_phone(card: &str) -> Option<String> {
    card.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix("TEL")?;
        let value = rest.rsplit(':').next()?.trim();
        let value = value.strip_prefix("tel:").unwrap_or(value).trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

/// The display name inside a vCard's FN line, for cards whose
/// message carries no display name of its own (P2-10).
fn vcard_fn_name(card: &str) -> Option<String> {
    card.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix("FN")?;
        let value = rest.split(':').nth(1)?.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

pub(super) fn translate_message(
    ctx: &MessageContext,
    book: &IdentityBook,
) -> Option<(LiveChat, LiveMessage)> {
    let source = &ctx.info.source;
    let chat_jid = source.chat.to_string();
    if chat_jid == STATUS_BROADCAST_JID {
        // A status post, not a message in a conversation: see
        // STATUS_BROADCAST_JID. No chat, no message.
        return None;
    }
    let kind = kind_of(&chat_jid);
    let text =
        ctx.message.text_content().map(ToOwned::to_owned).or_else(|| structured_text(&ctx.message));
    let (mut msg_kind, view_once) = classify_kind(ctx);
    if matches!(msg_kind, LiveMessageKind::Text) && text.is_none() {
        // A payload with no text and no classifier arm: it is
        // not a text message and must not wear a text body. The
        // label names the kind it actually is.
        msg_kind = LiveMessageKind::Other(unhandled_label(&ctx.message));
    }
    let body = body_for(&msg_kind, view_once, text.as_deref());
    let push_name = ctx.info.push_name.trim();
    let sender_jid = source.sender.to_string();
    let sender_name = if source.is_from_me {
        None
    } else {
        // The book prefers the saved contact name over any push
        // name, this message's included: a saved name is never
        // downgraded by a push name arriving later.
        Some(book.sender_label(&sender_jid, push_name))
    };
    let title = match kind {
        LiveChatKind::Dm => book.dm_title(&chat_jid, sender_name.as_deref()),
        LiveChatKind::Broadcast => "Broadcast list".to_owned(),
        _ => user_part(&chat_jid),
    };
    let timestamp = ctx.info.timestamp.timestamp();
    let chat = LiveChat {
        jid: chat_jid.clone(),
        title,
        kind,
        preview: core::model::flatten_preview(&body),
        preview_sender: if kind == LiveChatKind::Group || kind == LiveChatKind::Broadcast {
            sender_name.clone().map(|s| core::model::flatten_preview(&s))
        } else {
            None
        },
        timestamp,
        unread: 0,
        archived: false,
        pinned: false,
        // A message arrival does not carry the chat's timer;
        // the app keeps the value it already has (see the field
        // docs on LiveChat).
        ephemeral_secs: 0,
        muted: false,
        mute_until: 0,
        parent_jid: None,
        is_parent: false,
    };
    let mut msg = LiveMessage::text(
        ctx.info.id.to_string(),
        chat_jid,
        sender_name,
        body,
        timestamp,
        source.is_from_me,
    );
    // The wire's per-message expiration becomes an absolute
    // expiry second, the only form the archive and the app's
    // expiry pass need.
    if let Some(expiration) = ctx.ephemeral_expiration {
        if expiration > 0 {
            msg.expires_at = timestamp + i64::from(expiration);
        }
    }
    msg.kind = msg_kind;
    msg.view_once = view_once;
    // A photo's re-download reference travels with the message so
    // the app can archive it: the photo's bytes stay fetchable
    // after this session's protocol cache is gone. Non-photos and
    // incomplete payloads yield no reference.
    msg.photo_ref = super::util::photo_ref_of(&ctx.message);
    // A voice message's reference travels the same way, so the
    // note stays fetchable for playback in later sessions.
    msg.voice_ref = super::util::voice_ref_of(&ctx.message);
    // A video's reference and its embedded thumbnail travel the
    // same way: the reference keeps the video fetchable for
    // playback in later sessions, and the thumbnail (a real
    // frame of the video, made by the sender's client) is the
    // video's visual identity until its bytes are played.
    msg.video_ref = super::util::video_ref_of(&ctx.message);
    msg.video_thumbnail = super::util::video_thumbnail_of(&ctx.message);
    msg.poster_ref = super::util::poster_ref_of(&ctx.message);
    if let Some((width, height)) = super::util::video_dimensions_of(&ctx.message) {
        msg.video_width = Some(width);
        msg.video_height = Some(height);
    }
    // A photo's embedded thumbnail and a sticker's reference
    // travel the same way, and so do the message's mentions: the
    // app resolves each mentioned JID to a display name at
    // render time.
    msg.photo_thumbnail = super::util::photo_thumbnail_of(&ctx.message);
    msg.sticker_ref = super::util::sticker_ref_of(&ctx.message);
    carry_media_extras(&mut msg, &ctx.message);
    msg.mentions = super::util::mentioned_jids_of(&ctx.message);
    // Quote context, when the message carries one. The quoted
    // author resolves through the same book as the sender: the
    // wire carries only the author's JID, and painting its raw
    // user part would head every quote with a bare number.
    if let Some(quoted) = quoted_context(ctx) {
        msg.quoted_sender = quoted.0.map(|author_jid| book.author_label(&author_jid));
        msg.quoted_body = quoted.1;
    }
    // A link preview the sender's client attached travels with
    // the message; surface it as carried, never fetch it here.
    msg.link_preview = embedded_link_preview(&ctx.message);
    // The author JID travels with every message: replies quote it
    // as the context participant, group reactions target it.
    msg.sender_jid = Some(source.sender.to_string());
    // A poll creation's secret travels with the message so the
    // app can archive it with the poll and hand it back in later
    // sessions; without it the poll could neither be voted on nor
    // tallied after a restart. The creator is the message author.
    // A voice note's waveform travels from its wire payload,
    // never from a local guess (see voice_waveform_of).
    msg.voice_waveform = super::util::voice_waveform_of(&ctx.message);
    if matches!(msg.kind, LiveMessageKind::Poll { .. }) {
        if let Some((secret, _names)) = super::util::received_poll_secret(&ctx.message) {
            msg.poll_secret = Some(secret);
            msg.poll_creator = Some(source.sender.to_string());
        }
    }
    // An event creation's secret travels the same way, for the
    // same reason: responses and edits are encrypted under it.
    if matches!(msg.kind, LiveMessageKind::Event { .. }) {
        if let Some(secret) = super::util::received_event_secret(&ctx.message) {
            msg.event_secret = Some(secret);
            msg.event_creator = Some(source.sender.to_string());
        }
    }
    Some((chat, msg))
}

/// Translates one channel history entry (from the library's
/// newsletter history fetch) into a message upsert for the
/// channel's chat. The classifier and body rules are the same
/// ones chat messages use; the envelope differs: the entry
/// carries the server ID channel reactions and votes key on,
/// the server's own reaction counts and poll tallies, and no
/// sender JID (a channel post's author is the channel; an
/// admin profile name rides along when the channel uses one).
/// An entry whose body the server withheld (a revoked update
/// keeps its envelope and loses its payload) renders as a
/// deleted-update placeholder, never as a blank bubble.
pub(super) fn translate_newsletter_message(
    channel_jid: &str,
    entry: &whatsapp_rust::NewsletterMessage,
) -> LiveMessage {
    let (mut kind, view_once, body) = match entry.message.as_ref() {
        Some(proto) => {
            let text =
                proto.text_content().map(ToOwned::to_owned).or_else(|| structured_text(proto));
            let (mut kind, view_once) = classify_message(proto);
            if matches!(kind, LiveMessageKind::Text) && text.is_none() {
                kind = LiveMessageKind::Other(unhandled_label(proto));
            }
            let body = body_for(&kind, view_once, text.as_deref());
            (kind, view_once, body)
        }
        None => {
            let kind = LiveMessageKind::Other("Deleted update".to_owned());
            let body = body_for(&kind, false, None);
            (kind, false, body)
        }
    };
    // The server's poll tallies key on the SHA-256 of the
    // option name; the creation payload the classifier read
    // holds the names, so the counts land on their options.
    if let LiveMessageKind::Poll { options, .. } = &mut kind {
        for (name, count) in options.iter_mut() {
            let hash = whatsapp_rust::wacore::poll::compute_option_hash(name);
            if let Some(vote) = entry.votes.iter().find(|v| v.option_hash == hash) {
                *count = u32::try_from(vote.count).unwrap_or(u32::MAX);
            }
        }
    }
    let id = entry
        .message_id
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("server:{}", entry.server_id));
    let timestamp = i64::try_from(entry.original_timestamp.unwrap_or(entry.timestamp))
        .unwrap_or(entry.timestamp.min(i64::MAX as u64) as i64);
    let mut msg = LiveMessage::text(
        id,
        channel_jid.to_owned(),
        entry.admin_profile.as_ref().map(|profile| profile.name.clone()),
        body,
        timestamp,
        entry.is_sender,
    );
    msg.server_id = Some(entry.server_id);
    msg.kind = kind;
    msg.view_once = view_once;
    msg.edited =
        matches!(entry.edit, whatsapp_rust::wacore::types::message::EditAttribute::AdminEdit);
    msg.forwarded = entry.forwards_count.is_some_and(|count| count > 0);
    msg.reactions = entry
        .reactions
        .iter()
        .map(|r| (r.code.clone(), u32::try_from(r.count).unwrap_or(u32::MAX)))
        .collect();
    if let Some(proto) = entry.message.as_ref() {
        msg.photo_ref = super::util::photo_ref_of(proto);
        msg.voice_ref = super::util::voice_ref_of(proto);
        msg.video_ref = super::util::video_ref_of(proto);
        msg.video_thumbnail = super::util::video_thumbnail_of(proto);
        msg.poster_ref = super::util::poster_ref_of(proto);
        if let Some((width, height)) = super::util::video_dimensions_of(proto) {
            msg.video_width = Some(width);
            msg.video_height = Some(height);
        }
        msg.photo_thumbnail = super::util::photo_thumbnail_of(proto);
        msg.sticker_ref = super::util::sticker_ref_of(proto);
        msg.voice_waveform = super::util::voice_waveform_of(proto);
        carry_media_extras(&mut msg, proto);
        msg.link_preview = embedded_link_preview(proto);
    }
    msg
}

/// Translates one live message addressed to `status@broadcast`
/// into a [`crate::LiveStatus`]. The classifier and body rules
/// are the same ones chat messages use, so a status reads
/// exactly like the same payload would in a conversation; only
/// the envelope differs (no chat, no archive, the author is the
/// identity). Returns `None` for payloads that carry no post at
/// all (sender-key distribution and other bookkeeping, the same
/// payloads [`translate_message`] drops).
pub(super) fn translate_status(
    ctx: &MessageContext,
    book: &IdentityBook,
) -> Option<crate::LiveStatus> {
    let source = &ctx.info.source;
    let (kind, view_once) = classify_kind(ctx);
    if matches!(kind, LiveMessageKind::Other(_)) && ctx.message.text_content().is_none() {
        // Bookkeeping payloads (sender-key distribution arrives
        // as an unclassified protocol payload) are not posts.
        return None;
    }
    let text =
        ctx.message.text_content().map(ToOwned::to_owned).or_else(|| structured_text(&ctx.message));
    let mut kind = kind;
    if matches!(kind, LiveMessageKind::Text) && text.is_none() {
        kind = LiveMessageKind::Other(unhandled_label(&ctx.message));
    }
    let body = body_for(&kind, view_once, text.as_deref());
    let author_jid = source.sender.to_string();
    let author_name = if source.is_from_me {
        None
    } else {
        Some(book.sender_label(&author_jid, ctx.info.push_name.trim()))
    };
    Some(status_from_parts(
        ctx.info.id.to_string(),
        author_jid,
        author_name,
        body,
        ctx.info.timestamp.timestamp(),
        source.is_from_me,
        kind,
        &ctx.message,
    ))
}

/// Translates one history-sync payload from the status
/// conversation into a [`crate::LiveStatus`]: the server
/// redelivers the statuses it still holds at link time, which
/// is how the feed repopulates each session. Same drop rules
/// as the live path.
fn history_status(
    info: &whatsapp_rust::prelude::wa::WebMessageInfo,
    book: &IdentityBook,
) -> Option<crate::LiveStatus> {
    if info.ignore == Some(true) {
        return None;
    }
    let proto = info.message.as_option()?;
    let key = info.key.as_option()?;
    let id = key.id.clone()?;
    let from_me = key.from_me.unwrap_or(false);
    let (kind, view_once) = classify_message(proto);
    if matches!(kind, LiveMessageKind::Other(_)) && proto.text_content().is_none() {
        return None;
    }
    let text = proto.text_content().map(ToOwned::to_owned).or_else(|| structured_text(proto));
    let mut kind = kind;
    if matches!(kind, LiveMessageKind::Text) && text.is_none() {
        kind = LiveMessageKind::Other(unhandled_label(proto));
    }
    let body = body_for(&kind, view_once, text.as_deref());
    let author_jid = key
        .participant
        .clone()
        .or_else(|| info.participant.clone())
        .or_else(|| key.remote_jid.clone())?;
    let author_name = if from_me {
        None
    } else {
        let own_push_name = info.push_name.clone().filter(|n| !n.trim().is_empty());
        Some(
            book.name_for(&author_jid)
                .or(own_push_name)
                .or_else(|| book.phone_for(&author_jid))
                .unwrap_or_else(|| user_part(&author_jid)),
        )
    };
    let timestamp = i64::try_from(info.message_timestamp.unwrap_or(0)).unwrap_or(0);
    Some(status_from_parts(id, author_jid, author_name, body, timestamp, from_me, kind, proto))
}

/// Assembles a [`crate::LiveStatus`] from translated parts and
/// the wire payload: the shared tail of the live and history
/// status translators (media references, the text background,
/// the video's stated duration).
#[allow(clippy::too_many_arguments)]
fn status_from_parts(
    id: String,
    author_jid: String,
    author_name: Option<String>,
    body: String,
    timestamp: i64,
    from_me: bool,
    kind: LiveMessageKind,
    proto: &whatsapp_rust::prelude::wa::Message,
) -> crate::LiveStatus {
    let (video_width, video_height) = super::util::video_dimensions_of(proto)
        .map(|(w, h)| (Some(w), Some(h)))
        .unwrap_or((None, None));
    crate::LiveStatus {
        id,
        author_jid,
        author_name,
        body,
        timestamp,
        from_me,
        kind,
        background_argb: proto
            .extended_text_message
            .as_option()
            .and_then(|ext| ext.background_argb),
        reactions: Vec::new(),
        my_reaction: None,
        photo_ref: super::util::photo_ref_of(proto),
        photo_thumbnail: super::util::photo_thumbnail_of(proto),
        video_ref: super::util::video_ref_of(proto),
        video_thumbnail: super::util::video_thumbnail_of(proto),
        poster_ref: super::util::poster_ref_of(proto),
        video_width,
        video_height,
    }
}

/// Classifies a message's rich kind from its protocol payload.
/// The fields of one event payload, in the backend's own
/// shape. Three consumers share this extraction: the
/// classifier (a received event becomes
/// [`LiveMessageKind::Event`]), the edit path (an edited event
/// becomes [`BackendEvent::EventEdited`]), and the session's
/// encrypted-edit decryption. The location is a label: the
/// place name when the payload states one, else its address,
/// else the formatted coordinates when that is all it states.
pub(super) struct EventFields {
    /// Event name.
    pub name: String,
    /// Free-text details (may be empty).
    pub description: String,
    /// Start, unix seconds (0 when the payload stated none).
    pub start_time: i64,
    /// End, unix seconds (0 when the event has no end).
    pub end_time: i64,
    /// Place label (may be empty).
    pub location: String,
    /// Join/call link (may be empty).
    pub join_link: String,
    /// The event is a scheduled call.
    pub scheduled_call: bool,
    /// Extra guests are allowed.
    pub extra_guests: bool,
    /// The organizer cancelled the event.
    pub canceled: bool,
}

impl EventFields {
    /// The fields as a fresh message kind: no responses yet,
    /// no answer of mine.
    pub(super) fn into_kind(self) -> LiveMessageKind {
        LiveMessageKind::Event {
            name: self.name,
            description: self.description,
            start_time: self.start_time,
            end_time: self.end_time,
            location: self.location,
            join_link: self.join_link,
            scheduled_call: self.scheduled_call,
            extra_guests: self.extra_guests,
            canceled: self.canceled,
            my_rsvp: None,
            going: 0,
            maybe: 0,
            not_going: 0,
        }
    }

    /// The fields as an edit report for one message.
    pub(super) fn into_edited(self, chat_jid: String, message_id: String) -> BackendEvent {
        BackendEvent::EventEdited {
            chat_jid,
            message_id,
            name: self.name,
            description: self.description,
            start_time: self.start_time,
            end_time: self.end_time,
            location: self.location,
            join_link: self.join_link,
            scheduled_call: self.scheduled_call,
            extra_guests: self.extra_guests,
            canceled: self.canceled,
        }
    }
}

/// Extracts the backend's event fields from a wire event
/// payload. An absent name stays empty here; the card renders
/// the neutral fallback, the data never invents one.
pub(super) fn event_fields(
    event: &whatsapp_rust::prelude::wa::message::EventMessage,
) -> EventFields {
    let location = event
        .location
        .as_option()
        .map(|loc| {
            let name = loc.name.clone().filter(|n| !n.trim().is_empty());
            let address = loc.address.clone().filter(|a| !a.trim().is_empty());
            match (name, address, loc.degrees_latitude, loc.degrees_longitude) {
                (Some(name), _, _, _) => name,
                (None, Some(address), _, _) => address,
                (None, None, Some(lat), Some(lng)) => format!("{lat:.5}, {lng:.5}"),
                _ => String::new(),
            }
        })
        .unwrap_or_default();
    EventFields {
        name: event.name.clone().unwrap_or_default(),
        description: event.description.clone().unwrap_or_default(),
        start_time: event.start_time.unwrap_or(0),
        end_time: event.end_time.unwrap_or(0),
        location,
        join_link: event.join_link.clone().unwrap_or_default(),
        scheduled_call: event.is_schedule_call.unwrap_or(false),
        extra_guests: event.extra_guests_allowed.unwrap_or(false),
        canceled: event.is_canceled.unwrap_or(false),
    }
}

/// Poll *votes* arrive encrypted as separate protocol messages;
/// they never reach this classifier (the session decrypts them
/// and reports them as `BackendEvent::PollVote`), so only poll
/// creations map here.
pub(super) fn classify_kind(ctx: &MessageContext) -> (LiveMessageKind, bool) {
    classify_message(&ctx.message)
}

/// Classifies one protocol message; the second return value is the
/// view-once flag (set when the message arrived view-once wrapped).
pub(super) fn classify_message(
    msg: &whatsapp_rust::prelude::wa::Message,
) -> (LiveMessageKind, bool) {
    // Wrapper layers are peeled before classifying: a photo sent
    // from the phone arrives inside a device-sent wrapper, media
    // in a disappearing chat inside an ephemeral wrapper, and
    // classifying the wrapper instead of the base made every such
    // message fall through to the Text fallback. The library's
    // base-message walk peels those layers; the view-once flag is
    // read off the wrapper walk separately, because the base walk
    // steps over that layer without exposing it.
    let view_once = super::util::proto_is_view_once(msg);
    let (kind, _) = classify_unwrapped(msg.get_base_message());
    (kind, view_once)
}

/// Classifies one already-unwrapped protocol message.
fn classify_unwrapped(msg: &whatsapp_rust::prelude::wa::Message) -> (LiveMessageKind, bool) {
    classify_unwrapped_deep(msg, 0)
}

/// The recursive worker for [`classify_unwrapped`]: payloads
/// wrapped in a future-proof envelope (bot invocations, lottie
/// stickers, album children, questions, the v4 poll creation,
/// mention and spoiler wrappers) carry the real message inside
/// the envelope, so the classifier descends and classifies what
/// is actually there instead of labeling the envelope. The
/// depth bound keeps a pathological envelope-within-envelope
/// payload from recursing without limit.
fn classify_unwrapped_deep(
    msg: &whatsapp_rust::prelude::wa::Message,
    depth: u32,
) -> (LiveMessageKind, bool) {
    if depth < 4 {
        // A channel comment wraps the comment's own body in
        // its `message` field (the library's comments feature
        // builds exactly this shape): classify the body, not
        // the envelope, so a comment renders as the text or
        // media it actually is instead of a kind label
        // (audit P0-09).
        if let Some(inner) = msg.comment_message.as_option().and_then(|c| c.message.as_option()) {
            return classify_unwrapped_deep(inner.get_base_message(), depth + 1);
        }
        // An encrypted comment that reaches the classifier
        // still sealed has no readable body on this device:
        // the library decrypts the comments it can upstream
        // and delivers those as plain comments. It keeps its
        // truthful label here instead of falling through to
        // the Text fallback (audit P0-09).
        if msg.enc_comment_message.as_option().is_some() {
            return (LiveMessageKind::Other("Channel comment".to_owned()), false);
        }
        let envelope = msg
            .bot_invoke_message
            .as_option()
            .or_else(|| msg.bot_task_message.as_option())
            .or_else(|| msg.bot_forwarded_message.as_option())
            .or_else(|| msg.lottie_sticker_message.as_option())
            .or_else(|| msg.associated_child_message.as_option())
            .or_else(|| msg.question_message.as_option())
            .or_else(|| msg.question_reply_message.as_option())
            .or_else(|| msg.poll_creation_message_v4.as_option())
            .or_else(|| msg.group_mentioned_message.as_option())
            .or_else(|| msg.status_mention_message.as_option())
            .or_else(|| msg.group_status_mention_message.as_option())
            .or_else(|| msg.spoiler_message.as_option())
            .or_else(|| msg.limit_sharing_message.as_option())
            .or_else(|| msg.audio_sticker_message.as_option())
            .or_else(|| msg.event_cover_image.as_option())
            .or_else(|| msg.poll_creation_option_image_message.as_option())
            .or_else(|| msg.status_add_yours.as_option())
            .or_else(|| msg.newsletter_scheduled_message.as_option());
        if let Some(inner) = envelope.and_then(|e| e.message.as_option()) {
            return classify_unwrapped_deep(inner.get_base_message(), depth + 1);
        }
    }
    if let Some(poll) = msg
        .poll_creation_message
        .as_option()
        .or_else(|| msg.poll_creation_message_v3.as_option())
        .or_else(|| msg.poll_creation_message_v2.as_option())
        .or_else(|| msg.poll_creation_message_v5.as_option())
        .or_else(|| msg.poll_creation_message_v6.as_option())
    {
        let options =
            poll.options.iter().map(|o| (o.option_name.clone().unwrap_or_default(), 0)).collect();
        let selectable = poll.selectable_options_count.unwrap_or(1);
        return (
            LiveMessageKind::Poll {
                question: poll.name.clone().unwrap_or_default(),
                options,
                multiple: selectable != 1,
                my_votes: Vec::new(),
                correct: None,
            },
            false,
        );
    }
    if let Some(location) = msg.location_message.as_option() {
        let name = location
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| location.address.clone())
            .unwrap_or_else(|| "Shared location".to_owned());
        return (
            LiveMessageKind::Location {
                name,
                latitude: location.degrees_latitude.unwrap_or(0.0),
                longitude: location.degrees_longitude.unwrap_or(0.0),
            },
            false,
        );
    }
    if let Some(contact) = msg.contact_message.as_option() {
        // The phone lives inside the vCard text (a TEL line); parse
        // it when present, keep the neutral label otherwise.
        let phone = contact
            .vcard
            .as_deref()
            .and_then(vcard_phone)
            .unwrap_or_else(|| "Contact card".to_owned());
        let name = contact
            .display_name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| contact.vcard.as_deref().and_then(vcard_fn_name))
            .unwrap_or_default();
        return (LiveMessageKind::Contact { name, phone }, false);
    }
    if let Some(interactive) = msg.interactive_message.as_option() {
        use whatsapp_rust::prelude::wa::__buffa::oneof::message::interactive_message::InteractiveMessage as InteractiveOneof;
        if let Some(InteractiveOneof::CarouselMessage(carousel)) = &interactive.interactive_message
        {
            let cards = carousel
                .cards
                .iter()
                .map(|card| {
                    let body =
                        card.body.as_option().and_then(|b| b.text.clone()).unwrap_or_default();
                    let title = card
                        .header
                        .as_option()
                        .and_then(|h| h.title.clone())
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| body.lines().next().unwrap_or_default().to_owned());
                    let buttons = match &card.interactive_message {
                        Some(InteractiveOneof::NativeFlowMessage(flow)) => {
                            flow.buttons.iter().filter_map(|b| b.name.clone()).collect()
                        }
                        _ => Vec::new(),
                    };
                    crate::LiveCard { title, body, image: None, buttons }
                })
                .collect();
            let body =
                interactive.body.as_option().and_then(|b| b.text.clone()).unwrap_or_default();
            return (LiveMessageKind::Carousel { body, cards }, false);
        }
    }
    if let Some(image) =
        msg.image_message.as_option().or_else(|| msg.instant_image_message.as_option())
    {
        // An instant image is an ImageMessage in its own slot
        // (field 135): same struct, same presentation, same
        // fetch rails as a regular photo.
        return (
            LiveMessageKind::Media {
                media: crate::MediaKind::Photo,
                caption: image.caption.clone().unwrap_or_default(),
                seconds: 0,
                size_bytes: image.file_length.unwrap_or(0),
            },
            false,
        );
    }
    if let Some(video) = msg.video_message.as_option().or_else(|| msg.ptv_message.as_option()) {
        // A PTV (a round video note) is a VideoMessage in its
        // own slot: same struct, same presentation, same fetch
        // rails.
        return (
            LiveMessageKind::Media {
                media: crate::MediaKind::Video,
                caption: video.caption.clone().unwrap_or_default(),
                seconds: video.seconds.unwrap_or(0),
                size_bytes: video.file_length.unwrap_or(0),
            },
            false,
        );
    }
    if let Some(live_location) = msg.live_location_message.as_option() {
        let name = live_location
            .caption
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "Live location".to_owned());
        return (
            LiveMessageKind::Location {
                name,
                latitude: live_location.degrees_latitude.unwrap_or(0.0),
                longitude: live_location.degrees_longitude.unwrap_or(0.0),
            },
            false,
        );
    }
    if let Some(array) = msg.contacts_array_message.as_option() {
        // A multi-contact share: the card shows the array's own
        // display name and the first contact's phone; the count
        // stands in the phone slot when the array carries
        // several contacts, so the card never pretends to be a
        // single person it is not.
        let first = array.contacts.first();
        let name = array
            .display_name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| first.and_then(|c| c.display_name.clone()))
            .unwrap_or_default();
        let phone = if array.contacts.len() > 1 {
            format!("{} contacts", array.contacts.len())
        } else {
            first
                .and_then(|c| c.vcard.as_deref())
                .and_then(vcard_phone)
                .unwrap_or_else(|| "Contact card".to_owned())
        };
        return (LiveMessageKind::Contact { name, phone }, false);
    }
    if msg.album_message.as_option().is_some() {
        // An album header: its photos and videos arrive as
        // their own associated messages and group in the
        // stream; the header itself is a typed summary, never
        // a fake text.
        return (LiveMessageKind::Other("Album".to_owned()), false);
    }
    if let Some(label) = typed_summary_label(msg) {
        // Readable summary kinds (poll results, an added poll
        // option, a scheduled call, a sticker pack, a music
        // share): typed, with the text they carry as the body.
        return (LiveMessageKind::Other(label.to_owned()), false);
    }
    if let Some(event) = msg.event_message.as_option() {
        // An event renders as its own card kind, with the
        // payload's fields carried whole (row 49): before this
        // arm existed, a received event collapsed to a generic
        // summary line carrying only its name.
        return (event_fields(event).into_kind(), false);
    }
    if let Some(invite) = msg.group_invite_message.as_option() {
        let label = invite
            .group_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Group invite".to_owned());
        return (LiveMessageKind::Other(label), false);
    }
    if let Some(pin) = msg.pin_in_chat_message.as_option() {
        // A pin/unpin notice: the state change itself travels
        // as a MessagePinChanged event (see translate_special);
        // the message renders as the neutral system line that
        // names the act, like WhatsApp's own in-stream notice.
        let unpin = matches!(
            pin.r#type,
            Some(whatsapp_rust::prelude::wa::message::pin_in_chat_message::Type::UnpinForAll)
        );
        return (
            LiveMessageKind::Other(if unpin {
                "Unpinned a message".to_owned()
            } else {
                "Pinned a message".to_owned()
            }),
            false,
        );
    }
    if let Some(invite) = msg.newsletter_admin_invite_message.as_option() {
        let label = invite
            .newsletter_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Channel invite".to_owned());
        return (LiveMessageKind::Other(label), false);
    }
    if let Some(audio) = msg.audio_message.as_option() {
        // The ptt flag is the whole difference between a voice
        // note and an audio file: a payload with ptt set is a
        // voice note (waveform bubble, voice-note rails); a
        // payload without it is a file the sender attached (a
        // song, a recording), and it renders as a file card
        // with a duration, never as a voice note it is not.
        if audio.ptt.unwrap_or(false) {
            return (
                LiveMessageKind::Media {
                    media: crate::MediaKind::Voice,
                    caption: String::new(),
                    seconds: audio.seconds.unwrap_or(0),
                    size_bytes: audio.file_length.unwrap_or(0),
                },
                false,
            );
        }
        return (
            LiveMessageKind::Media {
                media: crate::MediaKind::Audio,
                caption: audio_format_label(audio.mimetype.as_deref()),
                seconds: audio.seconds.unwrap_or(0),
                size_bytes: audio.file_length.unwrap_or(0),
            },
            false,
        );
    }
    if let Some(document) = msg.document_message.as_option() {
        // The caption slot holds the sender's caption only;
        // the file's name, page count and thumbnail travel in
        // the message's own document fields (see
        // carry_media_extras).
        return (
            LiveMessageKind::Media {
                media: crate::MediaKind::Document,
                caption: document.caption.clone().unwrap_or_default(),
                seconds: 0,
                size_bytes: document.file_length.unwrap_or(0),
            },
            false,
        );
    }
    if let Some(sticker) = msg.sticker_message.as_option() {
        return (
            LiveMessageKind::Media {
                media: crate::MediaKind::Sticker,
                caption: String::new(),
                seconds: 0,
                size_bytes: sticker.file_length.unwrap_or(0),
            },
            false,
        );
    }
    (LiveMessageKind::Text, false)
}

/// Extracts the link preview a sender's client attached to an
/// extended-text message: the page title and description it
/// generated, plus the URL it matched. This is the only source of
/// link previews: the receiving side never fetches a URL itself,
/// so a sender cannot learn the recipient's address or timing
/// from a planted link. A message whose extended text carries no
/// preview fields (an ordinary text or reply) yields `None`.
pub(super) fn embedded_link_preview(
    msg: &whatsapp_rust::prelude::wa::Message,
) -> Option<LivePreview> {
    let extended = msg.get_base_message().extended_text_message.as_option()?;
    let title = extended.title.clone().unwrap_or_default();
    let description = extended.description.clone().unwrap_or_default();
    if title.is_empty() && description.is_empty() {
        return None;
    }
    Some(LivePreview {
        title,
        description,
        url: extended.matched_text.clone().unwrap_or_default(),
        thumbnail: super::util::preview_thumbnail_of(msg),
    })
}

/// The display label of an audio file, from the payload's MIME
/// type: an audio payload carries no file name, so the honest
/// label is its format. An absent or unrecognized type is just
/// an audio file.
fn audio_format_label(mimetype: Option<&str>) -> String {
    let subtype = mimetype
        .and_then(|m| m.split('/').nth(1))
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase());
    let format = match subtype.as_deref() {
        Some("mpeg") => Some("MP3"),
        Some("mp4") | Some("m4a") | Some("x-m4a") => Some("M4A"),
        Some("ogg") | Some("opus") => Some("OGG"),
        Some("wav") | Some("x-wav") => Some("WAV"),
        Some("flac") => Some("FLAC"),
        Some("aac") => Some("AAC"),
        Some("webm") => Some("WebM"),
        _ => None,
    };
    match format {
        Some(format) => format!("{format} audio file"),
        None => "Audio file".to_owned(),
    }
}

/// Extracts (author JID, excerpt) when a message quotes another
/// one: the context info of an extended-text message carries the
/// stanza id + participant + quoted payload. The author crosses
/// as its JID; the caller resolves it to a display name through
/// the identity book, like any sender.
pub(super) fn quoted_context(ctx: &MessageContext) -> Option<(Option<String>, Option<String>)> {
    let extended = ctx.message.get_base_message().extended_text_message.as_option()?;
    let context = extended.context_info.as_option()?;
    context.stanza_id.as_ref()?;
    let excerpt =
        context.quoted_message.as_option().and_then(|q| q.text_content().map(ToOwned::to_owned));
    let sender = context.participant.clone();
    Some((sender, excerpt))
}

/// The effective mute state of a sync `MuteAction` at
/// `now_ms`. The action's flag alone is not the whole story: a
/// timed mute carries a positive end timestamp and stops being
/// a mute once it passes. The indefinite spelling is `-1` (what
/// the library's own `mute_chat` sends); a missing end is read
/// the same way.
pub(super) fn mute_action_state(
    action: &whatsapp_rust::prelude::wa::sync_action_value::MuteAction,
    now_ms: i64,
) -> bool {
    if action.muted != Some(true) {
        return false;
    }
    match action.mute_end_timestamp {
        Some(end) if end > 0 => end > now_ms,
        _ => true,
    }
}

/// The end of a sync `MuteAction` as unix seconds, for the app
/// to show and to lift the mute locally when it passes. `0`
/// when the action is not a timed mute: unmuted, indefinite
/// (the `-1` spelling), or an end already in the past (the
/// state helper reports those unmuted, so no end survives).
pub(super) fn mute_action_until(
    action: &whatsapp_rust::prelude::wa::sync_action_value::MuteAction,
) -> i64 {
    if action.muted != Some(true) {
        return 0;
    }
    match action.mute_end_timestamp {
        Some(end) if end > 0 => end / 1000,
        _ => 0,
    }
}

/// The pin state of a sync `PinAction`. The flag is the whole
/// story: only an explicit `true` pins; `false` and an absent
/// flag both report unpinned.
pub(super) fn pin_action_state(
    action: &whatsapp_rust::prelude::wa::sync_action_value::PinAction,
) -> bool {
    action.pinned == Some(true)
}

/// The archive state of a sync `ArchiveChatAction`. As with
/// pin, only an explicit `true` archives; `false` and an absent
/// flag both report unarchived. The action's message range is
/// deliberately not read: it spans which messages the archive
/// covers, not whether the chat is archived.
pub(super) fn archive_action_state(
    action: &whatsapp_rust::prelude::wa::sync_action_value::ArchiveChatAction,
) -> bool {
    action.archived == Some(true)
}

/// The star state of a sync `StarAction`. As with pin, only
/// an explicit `true` stars; `false` and an absent flag both
/// report unstarred.
pub(super) fn star_action_state(
    action: &whatsapp_rust::prelude::wa::sync_action_value::StarAction,
) -> bool {
    action.starred == Some(true)
}

/// The read state of a sync `MarkChatAsReadAction`, as
/// `(read, unread_count)`. Only an explicit `read: true` marks
/// the chat read, and then the count is 0. Anything else is the
/// mark-as-unread form, whose count is the number of messages
/// the action's message range lists as unread (0 when the
/// range is absent or lists none); how a zero count renders as
/// a badge is the app's decision, not the wire's.
pub(super) fn mark_read_action_state(
    action: &whatsapp_rust::prelude::wa::sync_action_value::MarkChatAsReadAction,
) -> (bool, u32) {
    if action.read == Some(true) {
        return (true, 0);
    }
    let listed = action
        .message_range
        .as_option()
        .map_or(0, |range| u32::try_from(range.messages.len()).unwrap_or(u32::MAX));
    (false, listed)
}

/// The bound of a sync `ClearChatAction`: the message
/// range's last-message timestamp (Unix seconds, the unit
/// message timestamps use). Messages stamped after it are
/// not the clear's to remove. `None` when the action carries
/// no range, or the range carries no bound; the clear then
/// covers the whole chat.
pub(super) fn clear_chat_bound(
    action: &whatsapp_rust::prelude::wa::sync_action_value::ClearChatAction,
) -> Option<i64> {
    action.message_range.as_option().and_then(|range| range.last_message_timestamp)
}

/// Wall-clock milliseconds since the Unix epoch (0 if the
/// system clock is set before it).
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The broadcast address WhatsApp delivers status posts under.
/// It is not a conversation: no chat row may ever be built from
/// it. Status posts are translated into `LiveStatus` values for
/// the Updates feed at this boundary (live arrivals in the
/// session's message handler, history payloads in
/// `translate_history_conversation`), never filed as a fake
/// direct chat titled "status" and never shelved in the chat
/// archive; history sync redelivers the ones the server still
/// holds at every link.
pub(super) const STATUS_BROADCAST_JID: &str = "status@broadcast";

pub(super) fn kind_of(jid: &str) -> LiveChatKind {
    if jid.ends_with("@g.us") {
        LiveChatKind::Group
    } else if jid.ends_with("@newsletter") {
        LiveChatKind::Channel
    } else if jid.ends_with("@broadcast") {
        // Broadcast lists (status@broadcast never reaches
        // here: it is filtered before classification).
        LiveChatKind::Broadcast
    } else {
        LiveChatKind::Dm
    }
}

pub(super) fn user_part(jid: &str) -> String {
    jid.split('@').next().unwrap_or(jid).to_owned()
}

/// The display name the server rendered for a group participant:
/// the participant details' display name, else the username.
/// Empty when the server provided neither. The raw phone number
/// or LID is never substituted here: the app resolves a label
/// from its own data or masks the identifier, because a bare
/// number must not be the rendered name (privacy finding D5).
pub(super) fn participant_name(p: &whatsapp_rust::GroupParticipant) -> String {
    if let Some(details) = &p.details {
        if let Some(name) = &details.display_name {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_owned();
            }
        }
    }
    if let Some(username) = &p.username {
        let username = username.trim();
        if !username.is_empty() {
            return username.to_owned();
        }
    }
    String::new()
}

#[cfg(test)]
mod mute_action_tests {
    use super::mute_action_state;
    use whatsapp_rust::prelude::wa::sync_action_value::MuteAction;

    const NOW_MS: i64 = 1_700_000_000_000;

    fn action(muted: Option<bool>, mute_end_timestamp: Option<i64>) -> MuteAction {
        let mut action = MuteAction::default();
        action.muted = muted;
        action.mute_end_timestamp = mute_end_timestamp;
        action
    }

    #[test]
    fn indefinite_mute_is_muted() {
        // -1 is the indefinite spelling the library itself sends.
        assert!(mute_action_state(&action(Some(true), Some(-1)), NOW_MS));
        // A missing end reads as indefinite too.
        assert!(mute_action_state(&action(Some(true), None), NOW_MS));
    }

    #[test]
    fn timed_mute_lasts_until_its_end() {
        assert!(mute_action_state(&action(Some(true), Some(NOW_MS + 3_600_000)), NOW_MS));
        assert!(!mute_action_state(&action(Some(true), Some(NOW_MS - 1)), NOW_MS));
    }

    #[test]
    fn unmute_and_absent_flag_are_not_muted() {
        assert!(!mute_action_state(&action(Some(false), Some(0)), NOW_MS));
        assert!(!mute_action_state(&action(None, None), NOW_MS));
    }
}

#[cfg(test)]
mod pin_archive_action_tests {
    use super::{archive_action_state, pin_action_state};
    use whatsapp_rust::prelude::wa::sync_action_value::{ArchiveChatAction, PinAction};

    fn pin(pinned: Option<bool>) -> PinAction {
        let mut action = PinAction::default();
        action.pinned = pinned;
        action
    }

    fn archive(archived: Option<bool>) -> ArchiveChatAction {
        let mut action = ArchiveChatAction::default();
        action.archived = archived;
        action
    }

    #[test]
    fn only_an_explicit_true_pins() {
        assert!(pin_action_state(&pin(Some(true))));
        assert!(!pin_action_state(&pin(Some(false))));
        assert!(!pin_action_state(&pin(None)));
    }

    #[test]
    fn only_an_explicit_true_archives() {
        assert!(archive_action_state(&archive(Some(true))));
        assert!(!archive_action_state(&archive(Some(false))));
        assert!(!archive_action_state(&archive(None)));
    }
}

#[cfg(test)]
mod star_mark_read_action_tests {
    use super::{mark_read_action_state, star_action_state};
    use whatsapp_rust::prelude::wa::sync_action_value::{
        MarkChatAsReadAction, StarAction, SyncActionMessage, SyncActionMessageRange,
    };

    fn star(starred: Option<bool>) -> StarAction {
        let mut action = StarAction::default();
        action.starred = starred;
        action
    }

    fn mark_read(read: Option<bool>, range_messages: Option<usize>) -> MarkChatAsReadAction {
        let mut action = MarkChatAsReadAction::default();
        action.read = read;
        if let Some(count) = range_messages {
            let mut range = SyncActionMessageRange::default();
            range.messages = (0..count).map(|_| SyncActionMessage::default()).collect();
            action.message_range = Some(range).into();
        }
        action
    }

    #[test]
    fn only_an_explicit_true_stars() {
        assert!(star_action_state(&star(Some(true))));
        assert!(!star_action_state(&star(Some(false))));
        assert!(!star_action_state(&star(None)));
    }

    #[test]
    fn explicit_true_marks_read_with_no_unread() {
        assert_eq!(mark_read_action_state(&mark_read(Some(true), None)), (true, 0));
        // A range riding on a read mark changes nothing: the
        // chat is read, the count is 0.
        assert_eq!(mark_read_action_state(&mark_read(Some(true), Some(4))), (true, 0));
    }

    #[test]
    fn mark_unread_counts_the_range_messages() {
        assert_eq!(mark_read_action_state(&mark_read(Some(false), Some(3))), (false, 3));
        // No range, or an empty one, lists no unread messages;
        // the badge floor is the app's decision, not the wire's.
        assert_eq!(mark_read_action_state(&mark_read(Some(false), None)), (false, 0));
        assert_eq!(mark_read_action_state(&mark_read(Some(false), Some(0))), (false, 0));
        assert_eq!(mark_read_action_state(&mark_read(None, Some(2))), (false, 2));
    }
}

#[cfg(test)]
mod clear_chat_action_tests {
    use super::clear_chat_bound;
    use whatsapp_rust::prelude::wa::sync_action_value::{ClearChatAction, SyncActionMessageRange};

    fn clear(range: Option<SyncActionMessageRange>) -> ClearChatAction {
        let mut action = ClearChatAction::default();
        if let Some(range) = range {
            action.message_range = Some(range).into();
        }
        action
    }

    #[test]
    fn the_bound_is_the_range_last_message_timestamp() {
        let mut range = SyncActionMessageRange::default();
        range.last_message_timestamp = Some(1_700_000_000);
        assert_eq!(clear_chat_bound(&clear(Some(range))), Some(1_700_000_000));
    }

    #[test]
    fn no_range_or_no_bound_means_unbounded() {
        assert_eq!(clear_chat_bound(&clear(None)), None);
        // A range that lists messages but stamps no bound
        // still bounds nothing: the list enumerates what the
        // clearing device held, it does not narrow the clear.
        let range = SyncActionMessageRange::default();
        assert_eq!(clear_chat_bound(&clear(Some(range))), None);
    }
}

#[cfg(test)]
mod embedded_link_preview_tests {
    use super::{embedded_link_preview, privacy_map};
    use whatsapp_rust::prelude::wa;
    use whatsapp_rust::prelude::{MessageBuilderExt, MessageField};

    fn extended_text(
        text: &str,
        matched: Option<&str>,
        title: Option<&str>,
        description: Option<&str>,
    ) -> wa::Message {
        let mut extended = wa::message::ExtendedTextMessage::default();
        extended.text = Some(text.to_owned());
        extended.matched_text = matched.map(ToOwned::to_owned);
        extended.title = title.map(ToOwned::to_owned);
        extended.description = description.map(ToOwned::to_owned);
        let mut msg = wa::Message::default();
        msg.extended_text_message = MessageField::some(extended);
        msg
    }

    #[test]
    fn an_attached_preview_is_carried_through() {
        let msg = extended_text(
            "look at this https://example.com/page",
            Some("https://example.com/page"),
            Some("Example Page"),
            Some("A page about examples"),
        );
        let preview = embedded_link_preview(&msg).expect("the wire carried a preview");
        assert_eq!(preview.title, "Example Page");
        assert_eq!(preview.description, "A page about examples");
        assert_eq!(preview.url, "https://example.com/page");
    }

    #[test]
    fn a_title_alone_still_counts_as_a_preview() {
        let msg = extended_text(
            "https://example.com",
            Some("https://example.com"),
            Some("Example"),
            None,
        );
        let preview = embedded_link_preview(&msg).expect("title-only preview");
        assert_eq!(preview.title, "Example");
        assert_eq!(preview.description, "");
    }

    #[test]
    fn extended_text_without_preview_fields_yields_none() {
        // A plain reply or link text: the extended text carries
        // no sender-generated preview, and the receiver must not
        // manufacture one by fetching.
        let msg = extended_text("just a reply with https://example.com inside", None, None, None);
        assert!(embedded_link_preview(&msg).is_none());
    }

    #[test]
    fn plain_conversation_text_yields_none() {
        let msg = wa::Message::text("hello https://example.com");
        assert!(embedded_link_preview(&msg).is_none());
    }

    /// Every category the library returns survives translation,
    /// including the ones the Privacy page has no row for and the
    /// value spellings only they use (audit finding D6). An
    /// unknown category or value is carried under its wire
    /// spelling instead of being dropped.
    #[test]
    fn privacy_map_carries_every_category() {
        use whatsapp_rust::privacy_settings::{
            PrivacyCategory, PrivacySetting, PrivacySettingsResponse, PrivacyValue,
        };
        let resp = PrivacySettingsResponse {
            settings: vec![
                PrivacySetting { category: PrivacyCategory::Last, value: PrivacyValue::Contacts },
                PrivacySetting { category: PrivacyCategory::CallAdd, value: PrivacyValue::Known },
                PrivacySetting { category: PrivacyCategory::Messages, value: PrivacyValue::All },
                PrivacySetting {
                    category: PrivacyCategory::DefenseMode,
                    value: PrivacyValue::OnStandard,
                },
                PrivacySetting {
                    category: PrivacyCategory::Other("futurecat".to_owned()),
                    value: PrivacyValue::Other("futureval".to_owned()),
                },
            ],
        };
        let map = privacy_map(&resp);
        assert_eq!(map.get("last").map(String::as_str), Some("contacts"));
        assert_eq!(map.get("calladd").map(String::as_str), Some("known"));
        assert_eq!(map.get("messages").map(String::as_str), Some("all"));
        assert_eq!(map.get("defense").map(String::as_str), Some("on_standard"));
        assert_eq!(map.get("futurecat").map(String::as_str), Some("futureval"));
        assert_eq!(map.len(), 5, "nothing the server sent is dropped");
    }
}

#[cfg(test)]
mod qr_expiry_translate_tests {
    use super::translate_event;
    use crate::{BackendEvent, EventSink};
    use whatsapp_rust::wacore::types::events::{Event, PairingQrCodesExhausted};

    /// Item 107: the library's rotation budget ending used to
    /// fall into the catch-all arm and vanish, leaving the last
    /// expired code on screen forever. It must surface as
    /// `BackendEvent::QrExpired` so the app can retire the code.
    #[test]
    fn qr_codes_exhausted_maps_to_qr_expired() {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        let event = Event::PairingQrCodesExhausted(
            PairingQrCodesExhausted::builder().disconnected(true).build(),
        );
        translate_event(&event, &sink, &[], &mut super::IdentityBook::default());
        assert!(matches!(rx.try_recv(), Ok(BackendEvent::QrExpired)));
    }
}

#[cfg(test)]
mod sticker_sync_translate_tests {
    use super::translate_event;
    use crate::{BackendEvent, EventSink};
    use whatsapp_rust::wacore::types::events::{
        Event, FavoriteStickerUpdate, RemoveRecentStickerUpdate,
    };

    fn sink() -> (EventSink, std::sync::mpsc::Receiver<BackendEvent>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        (EventSink { tx, wake: std::sync::Arc::new(|| {}) }, rx)
    }

    /// P1-01: a favorite-sticker sync from another device
    /// crosses the boundary as a typed backend event carrying
    /// the filehash and the direction.
    #[test]
    fn favorite_sticker_update_crosses_the_boundary() {
        let (sink, rx) = sink();
        let mut action = whatsapp_rust::prelude::wa::sync_action_value::StickerAction::default();
        action.is_favorite = Some(true);
        let at = whatsapp_rust::wacore::chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp");
        let event = Event::FavoriteStickerUpdate(
            FavoriteStickerUpdate::builder()
                .filehash("hash-a".to_owned())
                .timestamp(at)
                .action(Box::new(action))
                .from_full_sync(false)
                .build(),
        );
        translate_event(&event, &sink, &[], &mut super::IdentityBook::default());
        assert!(matches!(
            rx.try_recv(),
            Ok(BackendEvent::StickerFavoriteChanged { ref filehash, favorite: true })
                if filehash == "hash-a"
        ));
    }

    #[test]
    fn remove_recent_sticker_update_crosses_the_boundary() {
        let (sink, rx) = sink();
        let at = whatsapp_rust::wacore::chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp");
        let event = Event::RemoveRecentStickerUpdate(
            RemoveRecentStickerUpdate::builder()
                .filehash("hash-b".to_owned())
                .timestamp(at)
                .action(Box::new(
                    whatsapp_rust::prelude::wa::sync_action_value::RemoveRecentStickerAction::default(),
                ))
                .from_full_sync(false)
                .build(),
        );
        translate_event(&event, &sink, &[], &mut super::IdentityBook::default());
        assert!(matches!(
            rx.try_recv(),
            Ok(BackendEvent::RecentStickerRemoved { ref filehash }) if filehash == "hash-b"
        ));
    }
}

#[cfg(test)]
mod presence_translate_tests {
    use super::translate_event;
    use crate::{BackendEvent, EventSink};
    use whatsapp_rust::wacore::chrono::DateTime;
    use whatsapp_rust::wacore::types::events::{Event, PresenceUpdate};
    use whatsapp_rust::wacore::types::presence::PresenceStatus;
    use whatsapp_rust::wacore_binary::Jid;

    fn contact() -> Jid {
        "6281234567890@s.whatsapp.net".parse().expect("valid jid")
    }

    /// Item 111: a subscribed contact's availability report must
    /// cross the boundary with its online flag and last-seen
    /// instant intact; before this arm existed the event fell
    /// into the catch-all and the header had nothing real to
    /// show.
    #[test]
    fn presence_available_maps_to_online_without_last_seen() {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        let event = Event::Presence(
            PresenceUpdate::builder().from(contact()).status(PresenceStatus::Available).build(),
        );
        translate_event(&event, &sink, &[], &mut super::IdentityBook::default());
        assert!(matches!(
            rx.try_recv(),
            Ok(BackendEvent::PresenceReceived { ref chat_jid, online: true, last_seen: None })
                if chat_jid == "6281234567890@s.whatsapp.net"
        ));
    }

    #[test]
    fn presence_unavailable_carries_the_last_seen_instant() {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        let at = DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp");
        let event = Event::Presence(
            PresenceUpdate::builder()
                .from(contact())
                .status(PresenceStatus::Unavailable)
                .last_seen(at)
                .build(),
        );
        translate_event(&event, &sink, &[], &mut super::IdentityBook::default());
        assert!(matches!(
            rx.try_recv(),
            Ok(BackendEvent::PresenceReceived {
                online: false,
                last_seen: Some(1_700_000_000),
                ..
            })
        ));
    }
}

#[cfg(test)]
mod live_pairing_fix_tests {
    use super::{
        body_for, call_record_of, classify_message, pairing_error_message, structured_text,
        translate_history_sync, unhandled_label,
    };
    use crate::{BackendEvent, EventSink, LiveChatKind, LiveMessageKind, MediaKind};
    use whatsapp_rust::prelude::wa;
    use whatsapp_rust::prelude::{MessageBuilderExt, MessageField};

    fn image_proto(caption: Option<&str>) -> wa::Message {
        let mut image = wa::message::ImageMessage::default();
        image.caption = caption.map(ToOwned::to_owned);
        let mut msg = wa::Message::default();
        msg.image_message = MessageField::some(image);
        msg
    }

    fn wrap_device_sent(inner: wa::Message) -> wa::Message {
        let mut dsm = wa::message::DeviceSentMessage::default();
        dsm.message = MessageField::some(inner);
        let mut msg = wa::Message::default();
        msg.device_sent_message = MessageField::some(dsm);
        msg
    }

    /// The owner's first live run: every message he had sent from
    /// his phone arrived device-sent wrapped and rendered as a
    /// literal placeholder text bubble, because the classifier
    /// inspected the wrapper instead of the base message.
    #[test]
    fn device_sent_photo_classifies_as_media() {
        let (kind, view_once) = classify_message(&wrap_device_sent(image_proto(None)));
        assert!(!view_once);
        assert!(matches!(kind, LiveMessageKind::Media { media: MediaKind::Photo, .. }));
        assert_eq!(body_for(&kind, view_once, None), "Photo");
    }

    #[test]
    fn device_sent_photo_caption_becomes_the_body() {
        let (kind, view_once) = classify_message(&wrap_device_sent(image_proto(Some("beach day"))));
        assert_eq!(body_for(&kind, view_once, None), "beach day");
    }

    #[test]
    fn view_once_caption_stays_sealed() {
        let mut wrapper = wa::message::FutureProofMessage::default();
        wrapper.message = MessageField::some(image_proto(Some("secret")));
        let mut msg = wa::Message::default();
        msg.view_once_message = MessageField::some(wrapper);
        let (kind, view_once) = classify_message(&msg);
        assert!(view_once);
        assert_eq!(body_for(&kind, view_once, None), "Photo");
    }

    fn history_info(id: &str, from_me: bool, ts: u64, message: wa::Message) -> wa::WebMessageInfo {
        let mut key = wa::MessageKey::default();
        key.id = Some(id.to_owned());
        key.from_me = Some(from_me);
        let mut info = wa::WebMessageInfo::default();
        info.key = MessageField::some(key);
        info.message = MessageField::some(message);
        info.message_timestamp = Some(ts);
        info
    }

    /// The roster path that was missing entirely: a history sync
    /// payload must become a titled, unread-carrying chat upsert
    /// plus typed message upserts, with no placeholder body
    /// anywhere.
    #[test]
    fn history_sync_conversation_becomes_roster() {
        let mut text = wa::Message::default();
        text.conversation = Some("hello there".to_owned());
        let mut conv = wa::Conversation::default();
        conv.id = "12345@g.us".to_owned();
        conv.name = Some("Weekend Hikers".to_owned());
        conv.unread_count = Some(3);
        conv.archived = Some(true);
        conv.messages = vec![
            {
                let mut e = wa::HistorySyncMsg::default();
                e.message = MessageField::some(history_info("m1", false, 100, text));
                e
            },
            {
                let mut e = wa::HistorySyncMsg::default();
                e.message = MessageField::some(history_info(
                    "m2",
                    true,
                    200,
                    wrap_device_sent(image_proto(None)),
                ));
                e
            },
        ];
        let mut sync = wa::HistorySync::default();
        sync.conversations = vec![conv];

        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        translate_history_sync(&sync, &sink, &mut super::IdentityBook::default());
        let events: Vec<BackendEvent> = rx.try_iter().collect();

        let chat = events.iter().find_map(|e| match e {
            BackendEvent::ChatUpsert(c) => Some(c),
            _ => None,
        });
        let chat = chat.expect("a chat upsert is emitted");
        assert_eq!(chat.title, "Weekend Hikers");
        assert_eq!(chat.unread, 3);
        assert!(chat.archived);
        assert_eq!(chat.preview, "Photo");

        let bodies: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::MessageUpsert(m) => Some(m.body.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(bodies, vec!["hello there", "Photo"]);
        assert!(bodies.iter().all(|b| *b != "[non-text message]"));
    }

    /// The link-time star import's second path: a history
    /// message whose payload carries the account's star
    /// (WebMessageInfo.starred) arrives starred, so a message
    /// the phone had starred before linking is starred in the
    /// archive whichever sync lands first; a message without
    /// the flag arrives unstarred.
    /// QC1614-A0-1: history translation reads the wire's
    /// per-message status for outgoing messages, so a fresh
    /// link restores the ticks the peer already earned.
    #[test]
    fn history_sync_message_carries_its_delivery_status() {
        let mut text = wa::Message::default();
        text.conversation = Some("hello".to_owned());
        let mut read_info = history_info("r1", true, 100, text.clone());
        read_info.status = Some(wa::web_message_info::Status::Read);
        let mut sent_info = history_info("r2", true, 200, text);
        sent_info.status = Some(wa::web_message_info::Status::ServerAck);
        let mut conv = wa::Conversation::default();
        conv.id = "12345@s.whatsapp.net".to_owned();
        conv.messages = vec![
            {
                let mut e = wa::HistorySyncMsg::default();
                e.message = MessageField::some(read_info);
                e
            },
            {
                let mut e = wa::HistorySyncMsg::default();
                e.message = MessageField::some(sent_info);
                e
            },
        ];
        let mut sync = wa::HistorySync::default();
        sync.conversations = vec![conv];
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        translate_history_sync(&sync, &sink, &mut super::IdentityBook::default());
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        let deliveries: Vec<(&str, Option<crate::DeliveryState>)> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::MessageUpsert(m) => Some((m.id.as_str(), m.delivery)),
                _ => None,
            })
            .collect();
        assert_eq!(
            deliveries,
            vec![
                ("r1", Some(crate::DeliveryState::Read)),
                ("r2", Some(crate::DeliveryState::Sent)),
            ]
        );
    }

    #[test]
    fn history_sync_message_carries_its_star() {
        let mut text = wa::Message::default();
        text.conversation = Some("keep this one".to_owned());
        let mut starred_info = history_info("s1", false, 100, text.clone());
        starred_info.starred = Some(true);
        let plain_info = history_info("s2", false, 200, text);
        let mut conv = wa::Conversation::default();
        conv.id = "12345@s.whatsapp.net".to_owned();
        conv.messages = vec![
            {
                let mut e = wa::HistorySyncMsg::default();
                e.message = MessageField::some(starred_info);
                e
            },
            {
                let mut e = wa::HistorySyncMsg::default();
                e.message = MessageField::some(plain_info);
                e
            },
        ];
        let mut sync = wa::HistorySync::default();
        sync.conversations = vec![conv];

        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        translate_history_sync(&sync, &sink, &mut super::IdentityBook::default());
        let events: Vec<BackendEvent> = rx.try_iter().collect();

        let stars: Vec<(&str, bool)> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::MessageUpsert(m) => Some((m.id.as_str(), m.starred)),
                _ => None,
            })
            .collect();
        assert_eq!(stars, vec![("s1", true), ("s2", false)]);
    }

    /// Identity resolution across every source a payload carries:
    /// a group sender keyed by LID resolves through the mapping
    /// to the saved contact name (which beats the push name), a
    /// LID direct chat with only a mapping gets the formatted
    /// phone number, a push name filed under the phone-number form
    /// names the LID chat, and a group with no subject anywhere is
    /// reported unresolved so the session can heal it.
    #[test]
    fn history_sync_resolves_identities_from_every_payload_source() {
        let mut text = wa::Message::default();
        text.conversation = Some("halo semua".to_owned());
        let mut key = wa::MessageKey::default();
        key.id = Some("g1".to_owned());
        key.from_me = Some(false);
        key.participant = Some("57243458396358@lid".to_owned());
        let mut info = wa::WebMessageInfo::default();
        info.key = MessageField::some(key);
        info.message = MessageField::some(text);
        info.message_timestamp = Some(300);

        let mut group = wa::Conversation::default();
        group.id = "120363430176830545@g.us".to_owned();
        group.messages = vec![{
            let mut e = wa::HistorySyncMsg::default();
            e.message = MessageField::some(info);
            e
        }];

        let mut lid_dm = wa::Conversation::default();
        lid_dm.id = "128815045062660@lid".to_owned();

        let mut push_dm = wa::Conversation::default();
        push_dm.id = "21750217740322@lid".to_owned();

        let mut sync = wa::HistorySync::default();
        sync.conversations = vec![group, lid_dm, push_dm];
        sync.pushnames = vec![
            {
                let mut p = wa::Pushname::default();
                p.id = Some("6281234567890@s.whatsapp.net".to_owned());
                p.pushname = Some("Bima".to_owned());
                p
            },
            {
                let mut p = wa::Pushname::default();
                p.id = Some("628111222333@s.whatsapp.net".to_owned());
                p.pushname = Some("Sari".to_owned());
                p
            },
        ];
        sync.phone_number_to_lid_mappings = vec![
            {
                let mut m = wa::PhoneNumberToLIDMapping::default();
                m.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
                m.lid_jid = Some("57243458396358@lid".to_owned());
                m
            },
            {
                let mut m = wa::PhoneNumberToLIDMapping::default();
                m.pn_jid = Some("6289876543210@s.whatsapp.net".to_owned());
                m.lid_jid = Some("128815045062660@lid".to_owned());
                m
            },
            {
                let mut m = wa::PhoneNumberToLIDMapping::default();
                m.pn_jid = Some("628111222333@s.whatsapp.net".to_owned());
                m.lid_jid = Some("21750217740322@lid".to_owned());
                m
            },
        ];
        sync.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.lid_jid = Some("57243458396358@lid".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];

        let (tx, rx) = std::sync::mpsc::sync_channel(32);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        let (unresolved, _retry_marks) =
            translate_history_sync(&sync, &sink, &mut super::IdentityBook::default());
        let events: Vec<BackendEvent> = rx.try_iter().collect();

        let title_of = |jid: &str| {
            events.iter().find_map(|e| match e {
                BackendEvent::ChatUpsert(c) if c.jid == jid => Some(c.title.clone()),
                _ => None,
            })
        };
        assert_eq!(title_of("128815045062660@lid").as_deref(), Some("+6289876543210"));
        assert_eq!(title_of("21750217740322@lid").as_deref(), Some("Sari"));
        assert_eq!(title_of("120363430176830545@g.us").as_deref(), Some("120363430176830545"));

        let sender = events.iter().find_map(|e| match e {
            BackendEvent::MessageUpsert(m) => m.sender.clone(),
            _ => None,
        });
        assert_eq!(sender.as_deref(), Some("Bima Prasetyo"), "saved name beats push name");

        let named: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::SenderNamed { sender_jid, name } => {
                    Some((sender_jid.as_str(), name.as_str()))
                }
                _ => None,
            })
            .collect();
        assert!(named.contains(&("57243458396358@lid", "Bima Prasetyo")));

        assert_eq!(
            unresolved,
            vec![
                ("120363430176830545@g.us".to_owned(), LiveChatKind::Group),
                ("128815045062660@lid".to_owned(), LiveChatKind::Dm),
            ],
            "the subjectless group and the number-titled DM stay unresolved: \
             a formatted number is a displayed fallback, not a resolution"
        );
    }

    /// A 1:1 history payload names no participant: the author
    /// of an incoming message is the conversation's other party
    /// (the key's remote JID), resolved through the book like
    /// any other sender. Regressed on the owner's real account
    /// in v1.6.11: the author collapsed to None, which the app
    /// reads as "mine", so incoming DM history rendered as the
    /// owner's own outgoing messages.
    #[test]
    fn history_dm_messages_name_the_peer_as_author() {
        let dm_info = |id: &str, from_me: bool, ts: u64, body: &str| {
            let mut text = wa::Message::default();
            text.conversation = Some(body.to_owned());
            let mut key = wa::MessageKey::default();
            key.id = Some(id.to_owned());
            key.from_me = Some(from_me);
            key.remote_jid = Some("62812345@s.whatsapp.net".to_owned());
            let mut info = wa::WebMessageInfo::default();
            info.key = MessageField::some(key);
            info.message = MessageField::some(text);
            info.message_timestamp = Some(ts);
            info
        };
        let mut conv = wa::Conversation::default();
        conv.id = "62812345@s.whatsapp.net".to_owned();
        conv.messages = vec![
            {
                let mut e = wa::HistorySyncMsg::default();
                e.message = MessageField::some(dm_info("d1", false, 100, "from them"));
                e
            },
            {
                let mut e = wa::HistorySyncMsg::default();
                e.message = MessageField::some(dm_info("d2", true, 200, "from me"));
                e
            },
        ];
        let mut sync = wa::HistorySync::default();
        sync.conversations = vec![conv];
        sync.pushnames = vec![{
            let mut p = wa::Pushname::default();
            p.id = Some("62812345@s.whatsapp.net".to_owned());
            p.pushname = Some("Peer Name".to_owned());
            p
        }];

        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        translate_history_sync(&sync, &sink, &mut super::IdentityBook::default());
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        let msgs: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::MessageUpsert(m) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(msgs.len(), 2);
        let incoming = msgs.iter().find(|m| m.id == "d1").expect("the incoming message");
        assert!(!incoming.from_me);
        assert_eq!(incoming.sender_jid.as_deref(), Some("62812345@s.whatsapp.net"));
        assert_eq!(incoming.sender.as_deref(), Some("Peer Name"));
        let outgoing = msgs.iter().find(|m| m.id == "d2").expect("the outgoing message");
        assert!(outgoing.from_me);
        assert_eq!(outgoing.sender, None);
    }

    /// The same author fallback without any name source: the
    /// sender label is the formatted phone number, never None
    /// (None would render the message as the owner's own).
    #[test]
    fn history_dm_author_falls_back_to_the_formatted_number() {
        let mut text = wa::Message::default();
        text.conversation = Some("hello".to_owned());
        let mut key = wa::MessageKey::default();
        key.id = Some("d1".to_owned());
        key.from_me = Some(false);
        key.remote_jid = Some("62812345@s.whatsapp.net".to_owned());
        let mut info = wa::WebMessageInfo::default();
        info.key = MessageField::some(key);
        info.message = MessageField::some(text);
        info.message_timestamp = Some(100);
        let mut conv = wa::Conversation::default();
        conv.id = "62812345@s.whatsapp.net".to_owned();
        conv.messages = vec![{
            let mut e = wa::HistorySyncMsg::default();
            e.message = MessageField::some(info);
            e
        }];
        let mut sync = wa::HistorySync::default();
        sync.conversations = vec![conv];

        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        translate_history_sync(&sync, &sink, &mut super::IdentityBook::default());
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        let msg = events
            .iter()
            .find_map(|e| match e {
                BackendEvent::MessageUpsert(m) => Some(m),
                _ => None,
            })
            .expect("the message is emitted");
        assert_eq!(msg.sender.as_deref(), Some("+62812345"));
    }

    /// A direct chat whose only identity is its number keeps
    /// the formatted number as its displayed title but is
    /// reported unresolved, so the heal pass can still bring a
    /// saved name learned later; a chat with a name-grade title
    /// is settled. Regressed in v1.6.11: the number counted as
    /// a resolution, so saved contacts stayed titled by number.
    #[test]
    fn history_number_titled_dm_stays_unresolved() {
        let mut number_only = wa::Conversation::default();
        number_only.id = "62812345@s.whatsapp.net".to_owned();
        let mut named = wa::Conversation::default();
        named.id = "62899999@s.whatsapp.net".to_owned();
        named.name = Some("Named Friend".to_owned());
        let mut sync = wa::HistorySync::default();
        sync.conversations = vec![number_only, named];

        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let sink = EventSink { tx, wake: std::sync::Arc::new(|| {}) };
        let (unresolved, _marks) =
            translate_history_sync(&sync, &sink, &mut super::IdentityBook::default());
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        let title_of = |jid: &str| {
            events.iter().find_map(|e| match e {
                BackendEvent::ChatUpsert(c) if c.jid == jid => Some(c.title.clone()),
                _ => None,
            })
        };
        assert_eq!(title_of("62812345@s.whatsapp.net").as_deref(), Some("+62812345"));
        assert_eq!(title_of("62899999@s.whatsapp.net").as_deref(), Some("Named Friend"));
        assert!(unresolved.contains(&("62812345@s.whatsapp.net".to_owned(), LiveChatKind::Dm)));
        assert!(!unresolved.contains(&("62899999@s.whatsapp.net".to_owned(), LiveChatKind::Dm)));
    }

    fn call_log_record() -> whatsapp_rust::prelude::wa::CallLogRecord {
        use whatsapp_rust::prelude::wa::call_log_record::CallResult;
        let mut record = whatsapp_rust::prelude::wa::CallLogRecord::default();
        record.call_id = Some("call-1".to_owned());
        record.call_creator_jid = Some("628111@s.whatsapp.net".to_owned());
        record.call_result = Some(CallResult::Connected);
        record.is_video = Some(true);
        record.duration = Some(125);
        record.start_time = Some(1_700_000_000);
        record
    }

    /// A synced call record files under the other party for an
    /// incoming call, under the first non-creator participant for
    /// an outgoing one, and under the group for a group call.
    /// Direction comes from the event's own from_me, never from
    /// the record's unreliable is_incoming flag.
    #[test]
    fn call_record_files_under_the_right_chat() {
        let record = call_log_record();
        let incoming = call_record_of(&record, None, Some(false), None, None).unwrap();
        assert_eq!(incoming.chat_jid, "628111@s.whatsapp.net");
        assert!(!incoming.outgoing && !incoming.missed);
        assert!(incoming.video);
        assert_eq!(incoming.duration_secs, 125);
        assert_eq!(incoming.started_at, 1_700_000_000);

        let mut outgoing_record = call_log_record();
        let mut participant =
            whatsapp_rust::prelude::wa::call_log_record::ParticipantInfo::default();
        participant.user_jid = Some("628222@s.whatsapp.net".to_owned());
        outgoing_record.participants = vec![participant];
        let outgoing = call_record_of(&outgoing_record, None, Some(true), None, None).unwrap();
        assert_eq!(outgoing.chat_jid, "628222@s.whatsapp.net");
        assert!(outgoing.outgoing && !outgoing.missed);

        let mut group_record = call_log_record();
        group_record.group_jid = Some("120363@g.us".to_owned());
        let group = call_record_of(&group_record, None, Some(false), None, None).unwrap();
        assert_eq!(group.chat_jid, "120363@g.us");
    }

    /// Missed is an incoming-only outcome, and a record with no
    /// call id anywhere is dropped rather than filed under a
    /// guess.
    #[test]
    fn call_record_missed_and_identity_rules() {
        use whatsapp_rust::prelude::wa::call_log_record::CallResult;
        let mut record = call_log_record();
        record.call_result = Some(CallResult::Missed);
        let missed = call_record_of(&record, None, Some(false), None, None).unwrap();
        assert!(missed.missed);
        let outgoing = call_record_of(&record, None, Some(true), None, None).unwrap();
        assert!(!outgoing.missed, "an outgoing call is never missed");

        let mut anonymous = call_log_record();
        anonymous.call_id = None;
        assert!(call_record_of(&anonymous, None, Some(false), None, None).is_none());
        let rescued =
            call_record_of(&anonymous, Some("sync-id".to_owned()), Some(false), None, None)
                .unwrap();
        assert_eq!(rescued.call_id, "sync-id");
    }

    /// Defect round (23 Nov 2026): a PTV (round video note) is
    /// a VideoMessage in its own slot; before the arm existed
    /// it fell through to the fake text "Message".
    #[test]
    fn ptv_classifies_as_video() {
        let mut video = wa::message::VideoMessage::default();
        video.caption = Some("round note".to_owned());
        video.seconds = Some(9);
        let mut msg = wa::Message::default();
        msg.ptv_message = MessageField::some(video);
        let (kind, _) = classify_message(&msg);
        assert!(matches!(kind, LiveMessageKind::Media { media: MediaKind::Video, seconds: 9, .. }));
        assert_eq!(body_for(&kind, false, None), "round note");
    }

    /// The ptt flag is the whole difference between a voice
    /// note and an audio file: set, the payload is a voice
    /// note; unset, it is an audio file with a format label,
    /// never a voice note.
    #[test]
    fn audio_ptt_flag_decides_voice_note_vs_audio_file() {
        let build = |ptt: Option<bool>| {
            let mut audio = wa::message::AudioMessage::default();
            audio.ptt = ptt;
            audio.seconds = Some(83);
            audio.mimetype = Some("audio/mpeg".to_owned());
            let mut msg = wa::Message::default();
            msg.audio_message = MessageField::some(audio);
            msg
        };
        let (kind, _) = classify_message(&build(Some(true)));
        assert!(matches!(
            kind,
            LiveMessageKind::Media { media: crate::MediaKind::Voice, seconds: 83, .. }
        ));
        for ptt in [Some(false), None] {
            let (kind, _) = classify_message(&build(ptt));
            match kind {
                LiveMessageKind::Media {
                    media: crate::MediaKind::Audio, caption, seconds, ..
                } => {
                    assert_eq!(caption, "MP3 audio file");
                    assert_eq!(seconds, 83);
                }
                other => panic!("an audio file must not classify as {other:?}"),
            }
        }
    }

    /// Pairing failures reach the user as human categories,
    /// never as a Debug dump: no rejection (a local or timeout
    /// failure) reads as a connection problem, the
    /// feature-not-available rejection points at the QR code,
    /// and a throttle says to wait.
    #[test]
    fn pairing_errors_map_to_human_categories() {
        use whatsapp_rust::wacore::pair_code::PairCodeRejection;
        use whatsapp_rust::wacore::types::events::PairingCodeError;
        let local = PairingCodeError::builder().error("boom".to_owned()).build();
        let text = pairing_error_message(&local);
        assert!(text.contains("did not complete"), "{text}");
        assert!(!text.contains("boom"), "the raw error never reaches the user: {text}");
        let unavailable = PairingCodeError::builder()
            .error("boom".to_owned())
            .rejection(PairCodeRejection::FeatureNotAvailable)
            .build();
        assert!(pairing_error_message(&unavailable).contains("QR code"));
        let throttled = PairingCodeError::builder()
            .error("boom".to_owned())
            .rejection(PairCodeRejection::RateOverlimit)
            .build();
        assert!(pairing_error_message(&throttled).contains("limiting pairing attempts"));
    }

    /// A document's caption slot holds only the sender's
    /// caption; its file name is not stuffed into it (the name
    /// travels in the message's document fields).
    #[test]
    fn document_caption_is_the_caption_not_the_file_name() {
        let mut document = wa::message::DocumentMessage::default();
        document.file_name = Some("report.pdf".to_owned());
        document.caption = Some("the quarterly report".to_owned());
        document.file_length = Some(54_321);
        let mut msg = wa::Message::default();
        msg.document_message = MessageField::some(document);
        let (kind, _) = classify_message(&msg);
        match kind {
            LiveMessageKind::Media {
                media: crate::MediaKind::Document,
                caption,
                size_bytes,
                ..
            } => {
                assert_eq!(caption, "the quarterly report");
                assert_eq!(size_bytes, 54_321);
            }
            other => panic!("expected a document, got {other:?}"),
        }
    }

    /// A poll result snapshot classifies as a typed summary
    /// whose body is the result itself (name plus every
    /// option's count), and an added poll option names itself:
    /// readable payloads are consumed, not merely labeled.
    #[test]
    fn poll_snapshot_and_added_option_are_consumed() {
        let mut snapshot = wa::message::PollResultSnapshotMessage::default();
        snapshot.name = Some("Lunch?".to_owned());
        let mut vote_a = wa::message::poll_result_snapshot_message::PollVote::default();
        vote_a.option_name = Some("Nasi Padang".to_owned());
        vote_a.option_vote_count = Some(3);
        let mut vote_b = wa::message::poll_result_snapshot_message::PollVote::default();
        vote_b.option_name = Some("Soto".to_owned());
        vote_b.option_vote_count = Some(1);
        snapshot.poll_votes = vec![vote_a, vote_b];
        let mut msg = wa::Message::default();
        msg.poll_result_snapshot_message = MessageField::some(snapshot);
        let (kind, _) = classify_message(&msg);
        assert!(matches!(&kind, LiveMessageKind::Other(label) if label == "Poll results"));
        assert_eq!(structured_text(&msg).as_deref(), Some("Lunch?\nNasi Padang: 3\nSoto: 1"));

        let mut added = wa::message::PollAddOptionMessage::default();
        let mut option = wa::message::poll_creation_message::Option::default();
        option.option_name = Some("Bakso".to_owned());
        added.add_option = MessageField::some(option);
        let mut msg = wa::Message::default();
        msg.poll_add_option_message = MessageField::some(added);
        let (kind, _) = classify_message(&msg);
        assert!(matches!(&kind, LiveMessageKind::Other(label) if label == "Poll option added"));
        assert_eq!(structured_text(&msg).as_deref(), Some("Bakso"));
    }

    /// The newer poll creation versions are the same struct in
    /// later slots (and v4 inside a future-proof envelope):
    /// all of them classify as polls.
    #[test]
    fn poll_versions_classify_as_polls() {
        let build = || {
            let mut poll = wa::message::PollCreationMessage::default();
            poll.name = Some("Lunch?".to_owned());
            let mut option = wa::message::poll_creation_message::Option::default();
            option.option_name = Some("Pizza".to_owned());
            poll.options = vec![option];
            poll
        };
        let mut v2 = wa::Message::default();
        v2.poll_creation_message_v2 = MessageField::some(build());
        let mut v5 = wa::Message::default();
        v5.poll_creation_message_v5 = MessageField::some(build());
        let mut inner = wa::Message::default();
        inner.poll_creation_message = MessageField::some(build());
        let mut envelope = wa::message::FutureProofMessage::default();
        envelope.message = MessageField::some(inner);
        let mut v4 = wa::Message::default();
        v4.poll_creation_message_v4 = MessageField::some(envelope);
        for msg in [v2, v5, v4] {
            let (kind, _) = classify_message(&msg);
            assert!(
                matches!(kind, LiveMessageKind::Poll { ref question, .. } if question == "Lunch?"),
                "{kind:?}"
            );
        }
    }

    /// A future-proof envelope is classified by what it
    /// carries: a bot invocation wrapping a photo is a photo,
    /// a lottie envelope wrapping a sticker is a sticker.
    #[test]
    fn future_proof_envelopes_classify_their_contents() {
        let wrap = |field: fn(&mut wa::Message, wa::message::FutureProofMessage),
                    inner: wa::Message| {
            let mut envelope = wa::message::FutureProofMessage::default();
            envelope.message = MessageField::some(inner);
            let mut msg = wa::Message::default();
            field(&mut msg, envelope);
            msg
        };
        let photo = wrap(|m, e| m.bot_invoke_message = MessageField::some(e), image_proto(None));
        let (kind, _) = classify_message(&photo);
        assert!(matches!(kind, LiveMessageKind::Media { media: MediaKind::Photo, .. }));
        let mut sticker = wa::message::StickerMessage::default();
        sticker.file_length = Some(10);
        let mut inner = wa::Message::default();
        inner.sticker_message = MessageField::some(sticker);
        let lottie = wrap(|m, e| m.lottie_sticker_message = MessageField::some(e), inner);
        let (kind, _) = classify_message(&lottie);
        assert!(matches!(kind, LiveMessageKind::Media { media: MediaKind::Sticker, .. }));
    }

    /// Typed summaries: an album header, an event, a group
    /// invite, and a channel invite each name themselves
    /// instead of wearing the fake text "Message".
    #[test]
    fn unrepresented_kinds_get_typed_summaries() {
        let mut album = wa::Message::default();
        album.album_message = MessageField::some(wa::message::AlbumMessage::default());
        let (kind, _) = classify_message(&album);
        assert_eq!(body_for(&kind, false, None), "Album");

        let mut event_proto = wa::message::EventMessage::default();
        event_proto.name = Some("Launch dinner".to_owned());
        let mut event = wa::Message::default();
        event.event_message = MessageField::some(event_proto);
        let (kind, _) = classify_message(&event);
        assert_eq!(body_for(&kind, false, None), "Launch dinner");

        let mut invite_proto = wa::message::GroupInviteMessage::default();
        invite_proto.group_name = Some("Weekend Hikers".to_owned());
        let mut invite = wa::Message::default();
        invite.group_invite_message = MessageField::some(invite_proto);
        let (kind, _) = classify_message(&invite);
        assert_eq!(body_for(&kind, false, None), "Weekend Hikers");

        let mut channel_proto = wa::message::NewsletterAdminInviteMessage::default();
        channel_proto.newsletter_name = Some("City News".to_owned());
        let mut channel = wa::Message::default();
        channel.newsletter_admin_invite_message = MessageField::some(channel_proto);
        let (kind, _) = classify_message(&channel);
        assert_eq!(body_for(&kind, false, None), "City News");
    }

    /// A contacts array becomes one contact card: the array's
    /// name, and the count standing in the phone slot when the
    /// array carries several people.
    #[test]
    fn contacts_array_becomes_a_contact_card() {
        let contact = |name: &str| {
            let mut c = wa::message::ContactMessage::default();
            c.display_name = Some(name.to_owned());
            c.vcard = Some("BEGIN:VCARD\nTEL:+62001\nEND:VCARD".to_owned());
            c
        };
        let mut array = wa::message::ContactsArrayMessage::default();
        array.display_name = Some("Ana and Budi".to_owned());
        array.contacts = vec![contact("Ana"), contact("Budi")];
        let mut msg = wa::Message::default();
        msg.contacts_array_message = MessageField::some(array);
        let (kind, _) = classify_message(&msg);
        match kind {
            LiveMessageKind::Contact { name, phone } => {
                assert_eq!(name, "Ana and Budi");
                assert_eq!(phone, "2 contacts");
            }
            other => panic!("expected a contact card, got {other:?}"),
        }
    }

    /// A live location share is a location with its caption.
    #[test]
    fn live_location_becomes_a_location() {
        let mut location = wa::message::LiveLocationMessage::default();
        location.degrees_latitude = Some(-0.5);
        location.degrees_longitude = Some(117.1);
        location.caption = Some("on my way".to_owned());
        let mut msg = wa::Message::default();
        msg.live_location_message = MessageField::some(location);
        let (kind, _) = classify_message(&msg);
        assert!(matches!(
            kind,
            LiveMessageKind::Location { ref name, .. } if name == "on my way"
        ));
    }

    /// Structured kinds carry their words in their own fields:
    /// the recovery reads them, and the composed outcome (a
    /// Text classification plus the recovered body) is the
    /// sender's text, searchable and exportable.
    #[test]
    fn structured_text_recovers_the_senders_words() {
        let mut buttons = wa::message::ButtonsMessage::default();
        buttons.content_text = Some("Pick a slot".to_owned());
        let mut msg = wa::Message::default();
        msg.buttons_message = MessageField::some(buttons);
        assert_eq!(structured_text(&msg).as_deref(), Some("Pick a slot"));
        let (kind, _) = classify_message(&msg);
        assert!(matches!(kind, LiveMessageKind::Text));
        assert_eq!(body_for(&kind, false, structured_text(&msg).as_deref()), "Pick a slot");

        let mut template_inner = wa::message::template_message::HydratedFourRowTemplate::default();
        template_inner.hydrated_content_text = Some("Your code is 1234".to_owned());
        let mut template = wa::message::TemplateMessage::default();
        template.hydrated_template = MessageField::some(template_inner);
        let mut msg = wa::Message::default();
        msg.template_message = MessageField::some(template);
        assert_eq!(structured_text(&msg).as_deref(), Some("Your code is 1234"));

        let mut list = wa::message::ListMessage::default();
        list.title = Some("Menu".to_owned());
        let mut msg = wa::Message::default();
        msg.list_message = MessageField::some(list);
        assert_eq!(structured_text(&msg).as_deref(), Some("Menu"));

        let mut order = wa::message::OrderMessage::default();
        order.order_title = Some("Order #42".to_owned());
        let mut msg = wa::Message::default();
        msg.order_message = MessageField::some(order);
        assert_eq!(structured_text(&msg).as_deref(), Some("Order #42"));

        let mut body = wa::message::interactive_message::Body::default();
        body.text = Some("Track your package".to_owned());
        let mut interactive = wa::message::InteractiveMessage::default();
        interactive.body = MessageField::some(body);
        let mut msg = wa::Message::default();
        msg.interactive_message = MessageField::some(interactive);
        assert_eq!(structured_text(&msg).as_deref(), Some("Track your package"));
    }

    /// The fallthrough audit: a table of payloads no arm
    /// claims, each asserted to carry its own truthful label.
    /// Only a payload with nothing recognizable at all keeps
    /// the neutral word.
    #[test]
    fn unhandled_kinds_name_themselves() {
        let mut comment = wa::Message::default();
        comment.comment_message = MessageField::some(wa::message::CommentMessage::default());
        assert_eq!(unhandled_label(&comment), "Channel comment");
        let mut enc_comment = wa::Message::default();
        enc_comment.enc_comment_message =
            MessageField::some(wa::message::EncCommentMessage::default());
        assert_eq!(unhandled_label(&enc_comment), "Channel comment");
        let mut pack = wa::Message::default();
        pack.sticker_pack_message = MessageField::some(wa::message::StickerPackMessage::default());
        assert_eq!(unhandled_label(&pack), "Sticker pack");
        let mut secret = wa::Message::default();
        secret.secret_encrypted_message =
            MessageField::some(wa::message::SecretEncryptedMessage::default());
        assert_eq!(unhandled_label(&secret), "Encrypted message");
        let mut snapshot = wa::Message::default();
        snapshot.poll_result_snapshot_message =
            MessageField::some(wa::message::PollResultSnapshotMessage::default());
        assert_eq!(unhandled_label(&snapshot), "Poll results");
        let mut bare_template = wa::Message::default();
        bare_template.template_message =
            MessageField::some(wa::message::TemplateMessage::default());
        assert_eq!(structured_text(&bare_template), None);
        assert_eq!(unhandled_label(&bare_template), "Template message");
        assert_eq!(unhandled_label(&wa::Message::default()), "Message");
    }

    /// P0-09: a channel comment's body lives in its inner
    /// message. The classifier and the text recovery both
    /// descend into it, so a comment on a channel post
    /// renders as its own words instead of the plain
    /// placeholder every channel kind once collapsed to.
    #[test]
    fn channel_comment_descends_to_its_inner_message() {
        let mut comment = wa::message::CommentMessage::default();
        comment.message = MessageField::some(wa::Message::text("First comment on the post"));
        let mut msg = wa::Message::default();
        msg.comment_message = MessageField::some(comment);
        let (kind, view_once) = classify_message(&msg);
        assert!(!view_once);
        assert!(matches!(kind, LiveMessageKind::Text), "got {kind:?}");
        assert_eq!(structured_text(&msg).as_deref(), Some("First comment on the post"));
        assert_eq!(
            body_for(&kind, view_once, structured_text(&msg).as_deref()),
            "First comment on the post"
        );
    }

    /// A comment wrapping media classifies as the media it
    /// wraps, with the media's own caption.
    #[test]
    fn channel_comment_around_a_photo_classifies_as_media() {
        let mut comment = wa::message::CommentMessage::default();
        comment.message = MessageField::some(image_proto(Some("the photo comment")));
        let mut msg = wa::Message::default();
        msg.comment_message = MessageField::some(comment);
        let (kind, _) = classify_message(&msg);
        assert!(
            matches!(
                kind,
                LiveMessageKind::Media {
                    media: MediaKind::Photo,
                    ref caption,
                    ..
                } if caption == "the photo comment"
            ),
            "got {kind:?}"
        );
    }

    /// P0-09: an AI rich response carries its words in its
    /// submessages' text parts; recovered and joined, they
    /// are the message's body. A response with no text
    /// parts recovers nothing and keeps its typed label.
    #[test]
    fn rich_response_recovers_its_submessage_text() {
        let mut first = wa::AIRichResponseSubMessage::default();
        first.message_text = Some("The market opens".to_owned());
        let mut second = wa::AIRichResponseSubMessage::default();
        second.message_text = Some("at nine.".to_owned());
        let mut rich = wa::AIRichResponseMessage::default();
        rich.submessages = vec![first, second];
        let mut msg = wa::Message::default();
        msg.rich_response_message = MessageField::some(rich);
        assert_eq!(structured_text(&msg).as_deref(), Some("The market opens\nat nine."));
        let (kind, _) = classify_message(&msg);
        assert!(matches!(kind, LiveMessageKind::Text), "got {kind:?}");
        let mut bare = wa::Message::default();
        bare.rich_response_message = MessageField::some(wa::AIRichResponseMessage::default());
        assert_eq!(structured_text(&bare), None);
        assert_eq!(unhandled_label(&bare), "AI response");
    }

    /// An encrypted comment whose body this client cannot
    /// read keeps its truthful label: the library decrypts
    /// the comments it can before translation, so what
    /// reaches the classifier in this envelope has no
    /// readable body by construction.
    #[test]
    fn unreadable_encrypted_comment_keeps_its_label() {
        let mut msg = wa::Message::default();
        msg.enc_comment_message = MessageField::some(wa::message::EncCommentMessage::default());
        let (kind, _) = classify_message(&msg);
        assert!(
            matches!(kind, LiveMessageKind::Other(ref label) if label == "Channel comment"),
            "got {kind:?}"
        );
    }

    /// An instant image is an ImageMessage in its own slot:
    /// it classifies as a photo with its caption, and its
    /// thumbnail and download reference extract like a
    /// photo's.
    #[test]
    fn instant_image_is_a_photo() {
        let mut image = wa::message::ImageMessage::default();
        image.caption = Some("made for you".to_owned());
        image.file_length = Some(1234);
        image.jpeg_thumbnail = Some(vec![0xFF, 0xD8, 0xFF]);
        let mut msg = wa::Message::default();
        msg.instant_image_message = MessageField::some(image);
        let (kind, _) = classify_message(&msg);
        assert!(
            matches!(
                kind,
                LiveMessageKind::Media {
                    media: crate::MediaKind::Photo,
                    ref caption,
                    size_bytes: 1234,
                    ..
                } if caption == "made for you"
            ),
            "got {kind:?}"
        );
        assert_eq!(
            super::super::util::photo_thumbnail_of(&msg),
            Some(vec![0xFF, 0xD8, 0xFF]),
            "the thumbnail extracts from the instant slot too"
        );
    }

    /// A pin-in-chat notice names the act as a system line;
    /// an unpin names its own.
    #[test]
    fn pin_in_chat_names_the_act() {
        use wa::message::pin_in_chat_message::Type;
        let mut pin = wa::message::PinInChatMessage::default();
        pin.r#type = Some(Type::PinForAll);
        let mut key = wa::MessageKey::default();
        key.id = Some("target-1".to_owned());
        pin.key = MessageField::some(key);
        let mut msg = wa::Message::default();
        msg.pin_in_chat_message = MessageField::some(pin);
        let (kind, _) = classify_message(&msg);
        assert!(
            matches!(kind, LiveMessageKind::Other(ref label) if label == "Pinned a message"),
            "got {kind:?}"
        );
        let mut unpin = wa::message::PinInChatMessage::default();
        unpin.r#type = Some(Type::UnpinForAll);
        let mut msg = wa::Message::default();
        msg.pin_in_chat_message = MessageField::some(unpin);
        let (kind, _) = classify_message(&msg);
        assert!(
            matches!(kind, LiveMessageKind::Other(ref label) if label == "Unpinned a message"),
            "got {kind:?}"
        );
    }

    /// Replies to structured messages recover the words the
    /// sender picked, in each reply shape.
    #[test]
    fn reply_kinds_recover_the_picked_words() {
        let mut reply = wa::message::TemplateButtonReplyMessage::default();
        reply.selected_display_text = Some("Tomorrow at 9".to_owned());
        let mut msg = wa::Message::default();
        msg.template_button_reply_message = MessageField::some(reply);
        assert_eq!(structured_text(&msg).as_deref(), Some("Tomorrow at 9"));

        let mut reply = wa::message::ListResponseMessage::default();
        reply.title = Some("The sushi place".to_owned());
        let mut msg = wa::Message::default();
        msg.list_response_message = MessageField::some(reply);
        assert_eq!(structured_text(&msg).as_deref(), Some("The sushi place"));

        let mut reply = wa::message::QuestionResponseMessage::default();
        reply.text = Some("Blue, please".to_owned());
        let mut msg = wa::Message::default();
        msg.question_response_message = MessageField::some(reply);
        assert_eq!(structured_text(&msg).as_deref(), Some("Blue, please"));

        let mut body = wa::message::interactive_response_message::Body::default();
        body.text = Some("Confirmed".to_owned());
        let mut reply = wa::message::InteractiveResponseMessage::default();
        reply.body = MessageField::some(body);
        let mut msg = wa::Message::default();
        msg.interactive_response_message = MessageField::some(reply);
        assert_eq!(structured_text(&msg).as_deref(), Some("Confirmed"));
    }

    /// A voice note's waveform comes from its payload
    /// (AudioMessage field 19); a payload without one yields
    /// None, so the app renders the neutral form.
    #[test]
    fn voice_waveform_comes_from_the_payload() {
        let mut audio = wa::message::AudioMessage::default();
        audio.waveform = Some(vec![10, 200, 90]);
        let mut msg = wa::Message::default();
        msg.audio_message = MessageField::some(audio);
        assert_eq!(super::super::util::voice_waveform_of(&msg), Some(vec![10, 200, 90]));
        let mut bare = wa::Message::default();
        bare.audio_message = MessageField::some(wa::message::AudioMessage::default());
        assert_eq!(super::super::util::voice_waveform_of(&bare), None);
    }

    /// History sender resolution prefers the saved contact
    /// name over the payload's push name: a push name must
    /// never downgrade a saved name.
    #[test]
    fn history_sender_prefers_the_saved_name() {
        let mut book = super::IdentityBook {
            saved: std::collections::HashMap::new(),
            push: std::collections::HashMap::new(),
            lid_to_pn: std::collections::HashMap::new(),
            pn_to_lid: std::collections::HashMap::new(),
        };
        book.saved.insert("6281234567890@s.whatsapp.net".to_owned(), "Saved Sarah".to_owned());
        let mut info = wa::WebMessageInfo::default();
        let mut key = wa::MessageKey::default();
        key.id = Some("h1".to_owned());
        key.from_me = Some(false);
        key.participant = Some("6281234567890@s.whatsapp.net".to_owned());
        info.key = MessageField::some(key);
        info.push_name = Some("Push Sarah".to_owned());
        info.message_timestamp = Some(1_700_000_000);
        let mut inner = wa::Message::default();
        inner.conversation = Some("hello".to_owned());
        info.message = MessageField::some(inner);
        let mut resolved = Vec::new();
        let msg =
            super::history_message("6281234567890@s.whatsapp.net", &info, &book, &mut resolved)
                .expect("the message translates");
        assert_eq!(msg.sender.as_deref(), Some("Saved Sarah"));
    }

    /// The live path resolves through the session book in the
    /// documented order: a saved name beats the message's own
    /// push name, a push name recorded earlier beats the raw
    /// number, a LID sender resolves through the mapping to the
    /// formatted phone, and a quoted author resolves like a
    /// sender.
    #[test]
    fn live_labels_resolve_through_the_session_book() {
        let mut book = super::IdentityBook::default();
        let pn = "6281234567890@s.whatsapp.net";
        let lid = "123456789012345@lid";
        book.note_saved(pn, "Saved Sarah");
        book.note_mapping(lid, pn);
        // Saved name wins over this message's push name, under
        // either JID form.
        assert_eq!(book.sender_label(pn, "Push Sarah"), "Saved Sarah");
        assert_eq!(book.sender_label(lid, "Push Sarah"), "Saved Sarah");
        assert_eq!(book.author_label(lid), "Saved Sarah");
        // The DM title prefers the saved name for the chat, in
        // either form, over the sender label a push name made.
        assert_eq!(book.dm_title(lid, Some("Push Sarah")), "Saved Sarah");
        assert_eq!(book.dm_title(pn, Some("Push Sarah")), "Saved Sarah");

        let mut book = super::IdentityBook::default();
        book.note_push(pn, "Push Sarah");
        book.note_mapping(lid, pn);
        assert_eq!(book.sender_label(lid, ""), "Push Sarah");
        // No name anywhere: the formatted phone, never the raw
        // LID digits.
        let mut book = super::IdentityBook::default();
        book.note_mapping(lid, pn);
        assert_eq!(book.sender_label(lid, ""), "+6281234567890");
        assert_eq!(book.author_label(lid), "+6281234567890");
        assert_eq!(book.dm_title(lid, None), "+6281234567890");
        // Nothing known at all: the raw user part is the last
        // resort, exactly as documented.
        let book = super::IdentityBook::default();
        assert_eq!(book.sender_label(lid, ""), "123456789012345");
    }

    /// Absorbing a payload book keeps the resolution order: a
    /// push name absorbed later never shadows a saved name, and
    /// mappings arrive in both directions.
    #[test]
    fn absorbed_books_keep_the_resolution_order() {
        let mut session = super::IdentityBook::default();
        session.note_saved("6281234567890@s.whatsapp.net", "Saved Sarah");
        let mut payload = super::IdentityBook::default();
        payload.note_push("6281234567890@s.whatsapp.net", "Push Sarah");
        payload.note_mapping("123456789012345@lid", "6281234567890@s.whatsapp.net");
        session.absorb(payload);
        assert_eq!(session.sender_label("6281234567890@s.whatsapp.net", ""), "Saved Sarah");
        assert_eq!(session.author_label("123456789012345@lid"), "Saved Sarah");
    }
}

#[cfg(test)]
mod mute_until_tests {
    use super::mute_action_until;
    use whatsapp_rust::prelude::wa::sync_action_value::MuteAction;

    fn action(muted: Option<bool>, mute_end_timestamp: Option<i64>) -> MuteAction {
        let mut action = MuteAction::default();
        action.muted = muted;
        action.mute_end_timestamp = mute_end_timestamp;
        action
    }

    #[test]
    fn timed_mute_reports_its_end_in_seconds() {
        assert_eq!(mute_action_until(&action(Some(true), Some(1_700_028_800_000))), 1_700_028_800);
    }

    #[test]
    fn indefinite_and_unmuted_report_no_end() {
        assert_eq!(mute_action_until(&action(Some(true), Some(-1))), 0);
        assert_eq!(mute_action_until(&action(Some(true), None)), 0);
        assert_eq!(mute_action_until(&action(Some(false), Some(1_700_028_800_000))), 0);
    }
}
