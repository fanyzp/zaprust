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
            Some(Type::GROUP_MEMBER_LABEL_CHANGE) => {
                // A member set or cleared their label in this
                // group (row 65): state, never a bubble. The
                // label rides the protocol payload; an empty
                // one is a clear.
                let label = protocol
                    .member_label
                    .as_option()
                    .and_then(|member_label| member_label.label.clone())
                    .filter(|label| !label.trim().is_empty());
                sink.emit(BackendEvent::MemberLabelChanged {
                    chat_jid,
                    member_jid: source.sender.to_string(),
                    label,
                });
                return true;
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

/// LID-to-phone mappings one event taught the identity book
/// for the first time, as (LID JID, phone JID) pairs: the
/// session re-runs title healing for these and feeds them to
/// the library's own cache.
pub(super) type LearnedMappings = Vec<(String, String)>;

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
) -> (TitleHeals, RetryMarks, LearnedMappings) {
    let mut unresolved_titles: TitleHeals = Vec::new();
    let mut retry_marks: RetryMarks = Vec::new();
    let mut learned_mappings: LearnedMappings = Vec::new();
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
        Event::LockChatUpdate(update) => {
            // App-state sync reporting the account's chat-lock
            // state for one chat: a lock or unlock from another
            // device, or the state as a full sync delivers it
            // (the link-time import of the phone's locked set
            // rides this path). This is the only wire path that
            // knows lock; message arrivals do not carry it. The
            // library's dispatcher only fires this event when
            // the action's locked field is present (an omitted
            // field is malformed there, not an unlock), so the
            // state read is exact.
            sink.emit(BackendEvent::LockChanged {
                chat_jid: update.jid.to_string(),
                locked: update.action.locked.unwrap_or(false),
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
            // The action's JID windows (its own JID plus the
            // lid/pn fields) name the contact's two forms, and
            // the pairing is an identity fact the book resolves
            // through: it is learned whether or not the action
            // carries a name. Only the saved name below stays
            // gated on a name actually being present, so a
            // name-less action teaches the pair and nothing else.
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
            for pair in jids.windows(2) {
                if let Some(learned) = book.note_mapping(&pair[0], &pair[1]) {
                    learned_mappings.push(learned);
                }
            }
            if let Some(name) = name {
                // The name joins the session identity book as a
                // saved name under every JID form, so live traffic
                // from this contact resolves from here on.
                for jid in &jids {
                    book.note_saved(jid, &name);
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
        Event::ContactRemoved(removed) => {
            // A contact deleted on the phone or another device:
            // the book forgets the saved entries under both of
            // the contact's JID forms, and the report hook at
            // the end of this funnel re-reports the shrunken
            // set, so the app's copy (and with it the status
            // audience) stops including them in this session
            // instead of only at the next restart.
            book.forget_saved(&removed.jid.to_string());
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
        Event::LabelEditUpdate(update) => {
            // Label definitions sync through app state, the
            // pairing-time full sync included: the phone's
            // whole label set replays through this arm, one
            // label per mutation.
            let deleted = update.action.deleted.unwrap_or(false);
            sink.emit(BackendEvent::LabelEdited {
                label_id: update.label_id.clone(),
                name: if deleted { None } else { update.action.name.clone() },
                color: if deleted { None } else { update.action.color },
                predefined_id: update.action.predefined_id,
                deleted,
            });
        }
        Event::LabelAssociationUpdate(update) => {
            sink.emit(BackendEvent::ChatLabelChanged {
                label_id: update.label_id.clone(),
                chat_jid: update.chat_jid.to_string(),
                labeled: update.action.labeled.unwrap_or(false),
            });
        }
        Event::MessageLabelAssociationUpdate(update) => {
            sink.emit(BackendEvent::MessageLabelChanged {
                label_id: update.label_id.clone(),
                chat_jid: update.chat_jid.to_string(),
                message_id: update.message_id.clone(),
                labeled: update.action.labeled.unwrap_or(false),
            });
        }
        Event::QuickReplyUpdate(update) => {
            // Deletion is the same mutation with the deleted
            // flag set, never a separate removal: check the
            // flag rather than assuming a live reply.
            let deleted = update.action.deleted.unwrap_or(false);
            sink.emit(BackendEvent::QuickReplyEdited {
                id: update.id.clone(),
                shortcut: update.action.shortcut.clone().unwrap_or_default(),
                message: update.action.message.clone().unwrap_or_default(),
                keywords: update.action.keywords.clone(),
                count: update.action.count.unwrap_or(0),
                deleted,
            });
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
    // Every book mutation in this funnel that changes what
    // the saved-contacts report carries (a history payload's
    // absorb, a contact update's saved name, a mapping
    // learned for a saved contact) reports the saved contacts
    // whole, so the app's copy is replaced with the backend's
    // current address book.
    book.report_saved_contacts(sink);
    (unresolved_titles, retry_marks, learned_mappings)
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
    // The payload's inline contacts joined the book above:
    // when they changed the saved set, the new address book is
    // reported whole so the app's copy stays the backend's.
    book.report_saved_contacts(sink);
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
    /// Bumped whenever what the saved-contacts report carries
    /// actually changes: a saved pair added, changed, or
    /// forgotten, or a form mapping learned for a person the
    /// saved map holds (their reported entry gains the other
    /// form, or merges with it). The saved set can then be
    /// reported whole exactly when it moved.
    saved_revision: u64,
    /// The revision the last saved-contacts report carried.
    saved_reported: u64,
}

/// What one pair learning did to an [`IdentityBook`], so the
/// learning paths can keep the saved revision truthful: the
/// revision moves when what the saved-contacts report carries
/// changed, and only then.
#[derive(Default)]
struct PairLearning {
    /// The pairing maps or the saved map moved: the asserted
    /// pair was not already recorded exactly as asserted.
    changed: bool,
    /// A saved entry moved to the new number (or was dropped
    /// because the new number already held one) as part of a
    /// re-point, so the report's entries changed, not just an
    /// entry's alternate form.
    saved_moved: bool,
    /// The forms this learning unlinked: the LID's previous
    /// phone number and the number's previous LID.
    retired: Vec<String>,
}

impl IdentityBook {
    fn from_sync(sync: &whatsapp_rust::prelude::wa::HistorySync) -> Self {
        let mut book = Self::default();
        for contact in &sync.inline_contacts {
            // A contact record carrying both JID forms is
            // itself the evidence that the forms are one
            // person, the same evidence a contact update's
            // JID pair carries: record the pairing, or the
            // two forms stand as two people downstream (the
            // saved set reports both, the audience counts
            // both, an exception naming one form misses the
            // other). The pairing is an identity fact, so it
            // is learned whether or not the record carries a
            // name; only the saved name below is name-gated.
            if let (Some(pn), Some(lid)) = (&contact.pn_jid, &contact.lid_jid) {
                book.learn_pair(lid, pn);
            }
            let name = contact
                .full_name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .or_else(|| contact.first_name.clone().filter(|n| !n.trim().is_empty()))
                .or_else(|| contact.username.clone().filter(|n| !n.trim().is_empty()));
            if let Some(name) = name {
                if let Some(pn) = &contact.pn_jid {
                    book.saved.insert(pn.clone(), name.clone());
                }
                if let Some(lid) = &contact.lid_jid {
                    book.saved.insert(lid.clone(), name);
                }
            }
        }
        book.push = sync
            .pushnames
            .iter()
            .filter_map(|p| Some((p.id.clone()?, p.pushname.clone()?)))
            .filter(|(_, name)| !name.trim().is_empty())
            .collect();
        for mapping in &sync.phone_number_to_lid_mappings {
            if let (Some(pn), Some(lid)) = (&mapping.pn_jid, &mapping.lid_jid) {
                book.learn_pair(lid, pn);
            }
        }
        book
    }

    /// Merges another book (one history payload's sources) into
    /// this one. Later sources win within their own map; the
    /// resolution order keeps the maps apart, so a push name can
    /// never downgrade a saved name no matter the merge order.
    fn absorb(&mut self, other: IdentityBook) {
        // The saved revision moves only when an incoming pair
        // is new or changes a stored value: a replayed payload
        // must not re-report an unchanged address book.
        let saved_changed = other.saved.iter().any(|(jid, name)| self.saved.get(jid) != Some(name));
        // A mapping this payload teaches for a person the
        // address book holds (already, or in this same payload)
        // changes what the report carries for them the same
        // way: their entry gains the other form, or two
        // entries merge into one. The revision moves for those
        // too, so the set is re-reported with current forms.
        // A mapping for anyone outside the book changes
        // nothing the report carries and must not move it.
        // The comparison runs in both directions and over the
        // forms a re-point retires (the LID's previous number,
        // the number's previous LID): a payload that moves a
        // held contact to a new number changes their reported
        // entry even when neither asserted form is itself a
        // saved key, because the retired number's entry is
        // what moves.
        let mapping_touches_saved = other.lid_to_pn.iter().any(|(lid, pn)| {
            let differs = self.lid_to_pn.get(lid).map(String::as_str) != Some(pn.as_str())
                || self.pn_to_lid.get(pn).map(String::as_str) != Some(lid.as_str());
            if !differs {
                return false;
            }
            let mut involved: Vec<&str> = vec![lid.as_str(), pn.as_str()];
            if let Some(old_pn) = self.lid_to_pn.get(lid) {
                involved.push(old_pn.as_str());
            }
            if let Some(old_lid) = self.pn_to_lid.get(pn) {
                involved.push(old_lid.as_str());
            }
            involved
                .iter()
                .copied()
                .any(|form| self.saved.contains_key(form) || other.saved.contains_key(form))
        });
        self.saved.extend(other.saved);
        if saved_changed || mapping_touches_saved {
            self.saved_revision += 1;
        }
        self.push.extend(other.push);
        // The payload's pairings join through the one learning
        // path, so a pair this payload re-points retires the
        // stale edges it replaces here too, and a saved entry
        // held under a retired number follows the person.
        for (lid, pn) in &other.lid_to_pn {
            self.learn_pair(lid, pn);
        }
    }

    /// Records a saved contact name for a JID (a contact update,
    /// or a history payload's inline contact).
    pub(super) fn note_saved(&mut self, jid: &str, name: &str) {
        if !name.trim().is_empty() && self.saved.get(jid).map(String::as_str) != Some(name) {
            self.saved.insert(jid.to_owned(), name.to_owned());
            self.saved_revision += 1;
        }
    }

    /// Forgets a saved contact (a contact removal synced from
    /// another device): the saved entries under the JID and
    /// its counterpart form both go, and the saved revision
    /// moves only when an entry actually went, so an unchanged
    /// book reports nothing. Push names and the form mappings
    /// stay: they are identity facts, not address-book facts,
    /// and name resolution simply falls back to them the way
    /// it does for anyone who was never a contact.
    pub(super) fn forget_saved(&mut self, jid: &str) {
        let mut removed = self.saved.remove(jid).is_some();
        if let Some(other) = self.counterpart(jid).map(str::to_owned) {
            removed |= self.saved.remove(&other).is_some();
        }
        if removed {
            self.saved_revision += 1;
        }
    }

    /// The canonical key [`Self::saved_contacts`] reports for
    /// the person a JID names, in whichever form the JID
    /// arrives: a LID whose phone-number counterpart is saved
    /// stands under that phone-number JID; a phone-number JID
    /// that is not itself saved but whose LID counterpart is
    /// stands under the LID; anything else stands as itself.
    /// Lists recorded against the account (the status privacy
    /// list is the standing example) are rewritten through
    /// this, so an entry recorded under one form still names
    /// the person under the form the saved set uses.
    pub(super) fn canonical_key(&self, jid: &str) -> String {
        if let Some(pn) = self.lid_to_pn.get(jid) {
            if self.saved.contains_key(pn) {
                return pn.clone();
            }
        }
        if let Some(lid) = self.pn_to_lid.get(jid) {
            if !self.saved.contains_key(jid) && self.saved.contains_key(lid) {
                return lid.clone();
            }
        }
        jid.to_owned()
    }

    /// The saved contacts as one entry per person: a LID entry
    /// whose phone-number counterpart (through the learned
    /// mappings) is also saved is represented by that
    /// phone-number entry; phone-number entries and LIDs with
    /// no known counterpart stand for themselves. Each entry
    /// carries the person's other known JID form alongside, so
    /// a consumer holding a JID in either form can resolve it
    /// to the entry's canonical key. Sorted by JID, so the
    /// report is deterministic.
    pub(super) fn saved_contacts(&self) -> Vec<crate::SavedContact> {
        let mut out: Vec<crate::SavedContact> = Vec::new();
        for (jid, name) in &self.saved {
            if let Some(pn) = self.lid_to_pn.get(jid) {
                if self.saved.contains_key(pn) {
                    continue;
                }
            }
            let alt_jids = match self.counterpart(jid) {
                Some(other) => vec![other.to_owned()],
                None => Vec::new(),
            };
            out.push(crate::SavedContact { jid: jid.clone(), name: name.clone(), alt_jids });
        }
        out.sort_by(|a, b| a.jid.cmp(&b.jid));
        out
    }

    /// Reports the saved contacts whole on `sink` when they
    /// changed since the last report, and marks them reported.
    /// Every growth path (history payloads absorbed into the
    /// book, contact updates noted into it, mappings learned
    /// for people it holds) funnels through the callers of
    /// this helper, so one check covers them all.
    pub(super) fn report_saved_contacts(&mut self, sink: &EventSink) {
        if self.saved_revision != self.saved_reported {
            self.saved_reported = self.saved_revision;
            sink.emit(BackendEvent::SavedContacts { contacts: self.saved_contacts() });
        }
    }

    /// Records a push name for a JID (a live message carries its
    /// sender's current push name).
    pub(super) fn note_push(&mut self, jid: &str, name: &str) {
        if !name.trim().is_empty() {
            self.push.insert(jid.to_owned(), name.to_owned());
        }
    }

    /// Records one (LID, phone number) pairing, keeping the
    /// two pairing maps exact inverses of each other. Every
    /// pairing source (a history contact record, the payload
    /// mappings table, an absorbed payload book, a contact
    /// update, a live message's alternate address) learns
    /// through here, so the invariant lives in one place.
    ///
    /// A pairing is revised, not just learned: when the LID
    /// was paired with a different number (the contact changed
    /// numbers; the LID is the stable identity), the old
    /// number's reverse edge is retired, and symmetrically for
    /// a number whose LID changed. Without the retirement the
    /// maps stop being inverses, `counterpart` turns
    /// direction-dependent, and one person stands in the saved
    /// set as two entries (the old number's entry keeps the
    /// LID as its alternate alongside the new number's entry).
    ///
    /// A saved entry held under the retired number follows
    /// the person to the new number: the old number's entry is
    /// the ghost the re-point would otherwise leave behind,
    /// and a recycled number would inherit the person's name.
    /// (The symmetric case, a number whose LID changed, means
    /// the number changed hands; the old LID's saved entry is
    /// that person's own and stays, it only loses the edge.)
    ///
    /// A self-pair (a corrupt record naming one JID as both
    /// forms) teaches nothing. The saved revision is the
    /// callers' business; this reports what moved.
    fn learn_pair(&mut self, lid: &str, pn: &str) -> PairLearning {
        if lid == pn {
            return PairLearning::default();
        }
        let prev_pn = self.lid_to_pn.get(lid).cloned();
        let prev_lid = self.pn_to_lid.get(pn).cloned();
        if prev_pn.as_deref() == Some(pn) && prev_lid.as_deref() == Some(lid) {
            return PairLearning::default();
        }
        let mut retired = Vec::new();
        if let Some(old_pn) = &prev_pn {
            if old_pn != pn {
                if self.pn_to_lid.get(old_pn).map(String::as_str) == Some(lid) {
                    self.pn_to_lid.remove(old_pn);
                }
                retired.push(old_pn.clone());
            }
        }
        if let Some(old_lid) = &prev_lid {
            if old_lid != lid {
                if self.lid_to_pn.get(old_lid).map(String::as_str) == Some(pn) {
                    self.lid_to_pn.remove(old_lid);
                }
                retired.push(old_lid.clone());
            }
        }
        self.lid_to_pn.insert(lid.to_owned(), pn.to_owned());
        self.pn_to_lid.insert(pn.to_owned(), lid.to_owned());
        let mut saved_moved = false;
        if let Some(old_pn) = &prev_pn {
            if old_pn != pn {
                if let Some(name) = self.saved.remove(old_pn) {
                    self.saved.entry(pn.to_owned()).or_insert(name);
                    saved_moved = true;
                }
            }
        }
        PairLearning { changed: true, saved_moved, retired }
    }

    /// Records that two JIDs are the same person's two forms
    /// (phone number and LID), from whichever source learned it:
    /// a payload mapping, a contact update's JID pair, or a live
    /// message's alternate sender address. Returns the
    /// (LID, phone) JID pair when this call changed the pairing
    /// (a first learning, or a re-point to a new form), so the
    /// caller can re-run the healing the mapping unlocks; a pair
    /// the book already knew reports nothing.
    pub(super) fn note_mapping(&mut self, a: &str, b: &str) -> Option<(String, String)> {
        if a == b {
            return None;
        }
        let (lid, pn) = if a.ends_with("@lid") && b.ends_with("@s.whatsapp.net") {
            (a, b)
        } else if b.ends_with("@lid") && a.ends_with("@s.whatsapp.net") {
            (b, a)
        } else {
            return None;
        };
        let learned = self.learn_pair(lid, pn);
        if !learned.changed {
            return None;
        }
        // A mapping learned for a person the address book
        // holds changes what the saved-contacts report
        // carries for them (their entry gains the other form,
        // merges with it, or moves to their new number), so
        // the saved revision moves with it and the next report
        // hook re-reports the set with current forms. A
        // mapping for anyone else changes nothing the report
        // carries and leaves the revision alone, so report
        // volume stays bounded by actual changes to the book's
        // saved persons. A re-point counts the forms it
        // retired as held: the entry that stood under the old
        // number is the one that moved.
        if learned.saved_moved
            || self.saved.contains_key(lid)
            || self.saved.contains_key(pn)
            || learned.retired.iter().any(|form| self.saved.contains_key(form))
        {
            self.saved_revision += 1;
        }
        Some((lid.to_owned(), pn.to_owned()))
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
    /// number, and the masked identifier only when nothing
    /// better exists in any source: the raw digits are never a
    /// display name (qc1634 claim 3).
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
        masked_user_part(sender_jid)
    }

    /// The display label for an author named only by JID (a
    /// quoted message's author, a reaction's actor): the book's
    /// best name, then the formatted phone number, then the
    /// masked identifier.
    pub(super) fn author_label(&self, author_jid: &str) -> String {
        self.name_for(author_jid)
            .or_else(|| self.phone_for(author_jid))
            .unwrap_or_else(|| masked_user_part(author_jid))
    }

    /// The title a live arrival gives its direct chat: the book's
    /// name for the chat itself first (a name recorded under the
    /// counterpart JID form counts), then the sender's label,
    /// then the chat's formatted phone number, then the masked
    /// identifier.
    pub(super) fn dm_title(&self, chat_jid: &str, sender_label: Option<&str>) -> String {
        self.name_for(chat_jid)
            .or_else(|| sender_label.map(ToOwned::to_owned))
            .or_else(|| self.phone_for(chat_jid))
            .unwrap_or_else(|| masked_user_part(chat_jid))
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
        .unwrap_or_else(|| last_resort_title(&chat_jid, &kind));
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
        locked: false,
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
        from_history: true,
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
        kind = LiveMessageKind::Unsupported(unhandled_label(proto));
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
        resolved.or_else(|| sender_jid.as_deref().map(masked_user_part))
    };
    let mut msg = LiveMessage::text(id, chat_jid.to_owned(), sender_name, body, timestamp, from_me);
    // This message arrived inside the history payload: its
    // unread state is already counted by the seed its chat
    // upsert carried (see the chat emission in
    // translate_history_conversation).
    msg.from_history = true;
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
    // The payload states the sender's certified business name
    // on the message itself when the sender holds one. The
    // name is a display source only; the verification tier
    // the app badges from rides live envelopes, whose message
    // shape is the one that carries it.
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
        LiveMessageKind::Order { .. } => "Order".to_owned(),
        LiveMessageKind::Contact { name, .. } => name.clone(),
        LiveMessageKind::Carousel { body, .. } => body.clone(),
        LiveMessageKind::Buttons { .. } => "Buttons".to_owned(),
        LiveMessageKind::ListMsg { description, .. } => {
            if description.is_empty() {
                "List".to_owned()
            } else {
                description.clone()
            }
        }
        LiveMessageKind::Other(label) => label.clone(),
        LiveMessageKind::Unsupported(label) => label.clone(),
        LiveMessageKind::Text => "Message".to_owned(),
    }
}

/// The body text a structured payload carries in its own
/// fields: template, product, order, and non-carousel
/// interactive messages carry their words outside the two
/// slots `text_content` reads, and so do buttons and list
/// messages, whose typed kinds take their body from here.
/// Recovering them here means previews, bubbles, search, and
/// exports show the sender's words instead of a placeholder. A template whose hydrated text
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
        if let Some(hydrated) = template.hydrated_template.as_option() {
            if let Some(text) = clean(hydrated.hydrated_content_text.clone()) {
                if !text.contains("{{") {
                    return Some(text);
                }
            }
            // The hydrated title is final text resolved the
            // same way as the content: when the content is
            // empty or still a parameter skeleton, the title
            // is the body. The same placeholder rule applies.
            use whatsapp_rust::prelude::wa::__buffa::oneof::message::template_message::hydrated_four_row_template::Title;
            if let Some(Title::HydratedTitleText(title)) = &hydrated.title {
                if let Some(text) = clean(Some(title.clone())) {
                    if !text.contains("{{") {
                        return Some(text);
                    }
                }
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
        // not a text message and must not wear a text body. It
        // files as an unsupported message carrying the label
        // that names the kind it actually is, so the app can
        // present it as the message it is.
        msg_kind = LiveMessageKind::Unsupported(unhandled_label(&ctx.message));
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
        _ => last_resort_title(&chat_jid, &kind),
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
        locked: false,
        pinned: false,
        // A message arrival does not carry the chat's timer;
        // the app keeps the value it already has (see the field
        // docs on LiveChat).
        ephemeral_secs: 0,
        muted: false,
        mute_until: 0,
        parent_jid: None,
        is_parent: false,
        from_history: false,
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
    // The envelope's server-asserted verification tier for the
    // sender, when it asserts one: the app badges from this,
    // and only at its top level. The history payload's message
    // shape carries no tier, so this live path is the tier's
    // only source.
    msg.sender_verified_level = ctx.info.verified_level.clone().filter(|l| !l.trim().is_empty());
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
                kind = LiveMessageKind::Unsupported(unhandled_label(proto));
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
                .unwrap_or_else(|| masked_user_part(&author_jid)),
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

/// The carousel kind of an interactive message, when the
/// message is a carousel: the cards with their titles, bodies
/// and button labels, under the message's own body text.
/// Shared by the interactive arm and the template arm, because
/// a template can wrap an interactive message of exactly this
/// shape.
fn carousel_kind(
    interactive: &whatsapp_rust::prelude::wa::message::InteractiveMessage,
) -> Option<LiveMessageKind> {
    use whatsapp_rust::prelude::wa::__buffa::oneof::message::interactive_message::InteractiveMessage as InteractiveOneof;
    let Some(InteractiveOneof::CarouselMessage(carousel)) = &interactive.interactive_message else {
        return None;
    };
    let cards = carousel
        .cards
        .iter()
        .map(|card| {
            let body = card.body.as_option().and_then(|b| b.text.clone()).unwrap_or_default();
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
    let body = interactive.body.as_option().and_then(|b| b.text.clone()).unwrap_or_default();
    Some(LiveMessageKind::Carousel { body, cards })
}

/// The kind of a template message, when its words can be
/// recovered truthfully. The hydrated form (the dedicated
/// field, or the format oneof's hydrated variant) carries the
/// final text and the buttons with their actions, so it maps
/// to the Buttons kind; a wrapped interactive message maps to
/// its own kind. Everything else returns None and lets the
/// caller's fallthrough file the message as unsupported: the
/// legacy four-row form's text is a parameter skeleton (its
/// HighlyStructuredMessage parts carry a namespace, an element
/// name and parameter values, never the sentence), and a
/// hydrated text still holding "{{" placeholders has no
/// substitution values anywhere in its payload, so neither
/// can be painted without inventing words.
fn template_kind(
    template: &whatsapp_rust::prelude::wa::message::TemplateMessage,
) -> Option<LiveMessageKind> {
    use whatsapp_rust::prelude::wa::__buffa::oneof::message::template_message::Format;
    if let Some(hydrated) = template.hydrated_template.as_option() {
        return Some(hydrated_template_kind(hydrated));
    }
    match &template.format {
        Some(Format::HydratedFourRowTemplate(hydrated)) => Some(hydrated_template_kind(hydrated)),
        Some(Format::InteractiveMessageTemplate(interactive)) => carousel_kind(interactive),
        Some(Format::FourRowTemplate(_)) => {
            Some(LiveMessageKind::Unsupported("Template message".to_owned()))
        }
        // The oneof is non-exhaustive in the pinned library: a
        // format this build does not know cannot be recovered
        // truthfully, so it takes the caller's unsupported
        // fallthrough like the legacy form does.
        Some(_) => None,
        None => None,
    }
}

/// The Buttons kind of one hydrated template: content text as
/// the body (recovered by `structured_text`, which reads the
/// same field), footer as the footer, and each button with
/// the action its payload declares. A content text still
/// holding parameter placeholders cannot be completed from
/// anything in the payload; when the hydrated title cannot
/// serve as the body either, the whole message files as
/// unsupported rather than painting the skeleton.
fn hydrated_template_kind(
    hydrated: &whatsapp_rust::prelude::wa::message::template_message::HydratedFourRowTemplate,
) -> LiveMessageKind {
    let content = hydrated.hydrated_content_text.clone().unwrap_or_default();
    // The one text that can still serve as the body when the
    // content cannot: the hydrated title, under the same
    // recovery rule `structured_text` applies.
    use whatsapp_rust::prelude::wa::__buffa::oneof::message::template_message::hydrated_four_row_template::Title;
    let title_serves = matches!(&hydrated.title, Some(Title::HydratedTitleText(title))
        if !title.trim().is_empty() && !title.contains("{{"));
    if content.contains("{{") && !title_serves {
        return LiveMessageKind::Unsupported("Template message".to_owned());
    }
    let buttons = hydrated
        .hydrated_buttons
        .iter()
        .filter_map(|button| {
            use whatsapp_rust::prelude::wa::__buffa::oneof::hydrated_template_button::HydratedButton;
            let (id, text, action) = match &button.hydrated_button {
                Some(HydratedButton::QuickReplyButton(reply)) => (
                    reply.id.clone().unwrap_or_default(),
                    reply.display_text.clone()?,
                    None,
                ),
                Some(HydratedButton::UrlButton(url_button)) => (
                    String::new(),
                    url_button.display_text.clone()?,
                    Some(crate::LiveButtonAction::Url(url_button.url.clone().unwrap_or_default())),
                ),
                Some(HydratedButton::CallButton(call_button)) => (
                    String::new(),
                    call_button.display_text.clone()?,
                    Some(crate::LiveButtonAction::Call(
                        call_button.phone_number.clone().unwrap_or_default(),
                    )),
                ),
                // The oneof is non-exhaustive in the pinned
                // library: a button kind this build does not
                // know carries no action it can offer honestly,
                // so the button is skipped.
                Some(_) => return None,
                None => return None,
            };
            // A button without a label carries nothing to pick
            // or press, the same rule the buttons-message arm
            // applies.
            if text.trim().is_empty() {
                return None;
            }
            Some(crate::LiveButton { id, text, action })
        })
        .collect();
    LiveMessageKind::Buttons {
        footer: hydrated.hydrated_footer_text.clone().unwrap_or_default(),
        buttons,
    }
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
    if let Some(buttons_message) = msg.buttons_message.as_option() {
        // A buttons message is its own kind, not text wearing
        // structure (row 19): the bubble renders the buttons,
        // and an answer names the picked button's wire id.
        // Buttons without a label carry nothing to pick and
        // are dropped, like the carousel's nameless flow
        // buttons below.
        let buttons = buttons_message
            .buttons
            .iter()
            .filter_map(|button| {
                let text = button
                    .button_text
                    .as_option()
                    .and_then(|t| t.display_text.clone())
                    .filter(|t| !t.trim().is_empty())?;
                Some(crate::LiveButton {
                    id: button.button_id.clone().unwrap_or_default(),
                    text,
                    action: None,
                })
            })
            .collect();
        return (
            LiveMessageKind::Buttons {
                footer: buttons_message.footer_text.clone().unwrap_or_default(),
                buttons,
            },
            false,
        );
    }
    if let Some(list_message) = msg.list_message.as_option() {
        // A list message is its own kind (row 19): title in
        // the body, sections of pickable rows below it.
        let sections = list_message
            .sections
            .iter()
            .map(|section| crate::LiveListSection {
                title: section.title.clone().unwrap_or_default(),
                rows: section
                    .rows
                    .iter()
                    .map(|row| crate::LiveListRow {
                        id: row.row_id.clone().unwrap_or_default(),
                        title: row.title.clone().unwrap_or_default(),
                        description: row.description.clone().unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect();
        return (
            LiveMessageKind::ListMsg {
                description: list_message.description.clone().unwrap_or_default(),
                button_label: list_message.button_text.clone().unwrap_or_default(),
                footer: list_message.footer_text.clone().unwrap_or_default(),
                sections,
            },
            false,
        );
    }
    if let Some(interactive) = msg.interactive_message.as_option() {
        if let Some(kind) = carousel_kind(interactive) {
            return (kind, false);
        }
    }
    if let Some(template) = msg.template_message.as_option() {
        // A template message is a business message with its
        // text and buttons resolved at send time: the hydrated
        // form carries the final words, so it renders as the
        // Buttons kind with the buttons' own actions. Forms
        // whose words cannot be recovered (the legacy
        // four-row form carries parameter placeholders, never
        // the substituted text) file as unsupported instead of
        // painting placeholder skeletons.
        if let Some(kind) = template_kind(template) {
            return (kind, false);
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
    if let Some(order) = msg.order_message.as_option() {
        // An order keeps its capability (id + token) on the
        // kind: without them the order-details fetch has
        // nothing to authorize with, and the message would
        // be only its recovered words. The body still comes
        // from the text recovery (the order's own message or
        // title), so previews and search keep the words.
        use whatsapp_rust::prelude::wa::message::order_message::OrderStatus;
        let status = match order.status {
            Some(OrderStatus::Inquiry) => "INQUIRY",
            Some(OrderStatus::Accepted) => "ACCEPTED",
            Some(OrderStatus::Declined) => "DECLINED",
            _ => "",
        };
        let order_token_value = order.token.clone().unwrap_or_default();
        return (
            LiveMessageKind::Order {
                order_id: order.order_id.clone().unwrap_or_default(),
                token: order_token_value,
                item_count: u32::try_from(order.item_count.unwrap_or(0)).unwrap_or(0),
                total_amount_1000: order.total_amount1000.unwrap_or(0),
                currency: order.total_currency_code.clone().unwrap_or_default(),
                status: status.to_owned(),
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

/// The masked form of a JID's user part, mirroring the app
/// member-row rule: the first three and last four digits with
/// the bullet separator, a `+` only for phone-number JIDs. This
/// is the last-resort display for a person or chat whose name
/// and number are not knowable: enough to tell rows apart,
/// never the raw identifier itself (qc1634 claim 3, the
/// owner's masked-fallback decision). A user part that is not
/// a long digit string is not a phone-like identifier and is
/// returned unchanged.
pub(super) fn masked_user_part(jid: &str) -> String {
    let user = user_part(jid);
    if user.len() < 7 || !user.bytes().all(|b| b.is_ascii_digit()) {
        return user;
    }
    let masked = format!("{} ••• {}", &user[..3], &user[user.len() - 4..]);
    if jid.ends_with("@s.whatsapp.net") {
        format!("+{masked}")
    } else {
        masked
    }
}

/// Whether a chat title is still an unresolved placeholder for
/// the chat: the raw user part, or its masked form. The session
/// reports titles in either state for healing; a title that
/// advanced past both (a formatted phone, a name) is settled.
pub(super) fn title_is_unresolved(title: &str, chat_jid: &str) -> bool {
    title == user_part(chat_jid) || title == masked_user_part(chat_jid)
}

/// The last-resort title for a chat nothing could name. A
/// person's chat shows the masked identifier, never the raw
/// number or LID (the owner's masked-fallback decision). A
/// group or channel id is an opaque identifier, not a phone
/// number and not personal data: masking it would dress it up
/// as a number, so the raw id stays, and `title_is_unresolved`
/// still treats it as a placeholder a real subject heals over.
pub(super) fn last_resort_title(chat_jid: &str, kind: &LiveChatKind) -> String {
    match kind {
        LiveChatKind::Dm => masked_user_part(chat_jid),
        _ => user_part(chat_jid),
    }
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
mod saved_contacts_translate_tests {
    use super::{translate_event, translate_history_sync};
    use crate::{BackendEvent, EventSink, SavedContact};
    use whatsapp_rust::prelude::wa;
    use whatsapp_rust::wacore::types::events::{ContactRemoved, ContactUpdate, Event};
    use whatsapp_rust::wacore_binary::Jid;

    fn sink() -> (EventSink, std::sync::mpsc::Receiver<BackendEvent>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        (EventSink { tx, wake: std::sync::Arc::new(|| {}) }, rx)
    }

    fn saved_reports(events: &[BackendEvent]) -> Vec<Vec<SavedContact>> {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::SavedContacts { contacts } => Some(contacts.clone()),
                _ => None,
            })
            .collect()
    }

    /// H-2 (qc1644 review 2): a history payload's inline
    /// contacts are the account's address book: the saved set
    /// is reported whole, one entry per person under the
    /// phone-number JID (the same person also arrives under
    /// their LID and must not appear twice), and a contact
    /// known only under a LID stands under that LID.
    #[test]
    fn history_inline_contacts_are_reported_as_saved_contacts() {
        let mut sync = wa::HistorySync::default();
        sync.inline_contacts = vec![
            {
                let mut c = wa::InlineContact::default();
                c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
                c.lid_jid = Some("57243458396358@lid".to_owned());
                c.full_name = Some("Bima Prasetyo".to_owned());
                c
            },
            {
                let mut c = wa::InlineContact::default();
                c.lid_jid = Some("128815045062660@lid".to_owned());
                c.full_name = Some("Lid Only".to_owned());
                c
            },
        ];
        // The payload's mappings table also carries Bima's
        // pair; the contact record's own two fields carry it
        // too (qc1648), and either source ties the forms to
        // one person.
        sync.phone_number_to_lid_mappings = vec![{
            let mut m = wa::PhoneNumberToLIDMapping::default();
            m.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            m.lid_jid = Some("57243458396358@lid".to_owned());
            m
        }];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&sync, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![
                SavedContact {
                    jid: "128815045062660@lid".to_owned(),
                    name: "Lid Only".to_owned(),
                    alt_jids: Vec::new(),
                },
                SavedContact {
                    jid: "6281234567890@s.whatsapp.net".to_owned(),
                    name: "Bima Prasetyo".to_owned(),
                    alt_jids: vec!["57243458396358@lid".to_owned()],
                },
            ]],
            "one report, one entry per person, the phone-number form winning"
        );
        // A replay of the same payload changes nothing in the
        // saved set, so it reports nothing again.
        translate_history_sync(&sync, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(saved_reports(&events).is_empty(), "an unchanged set is not re-reported");
    }

    /// H-2 (qc1644 review 2): a contact update naming a new
    /// saved contact reports the new set through the event
    /// funnel, under the phone-number JID when the action
    /// carries both forms.
    #[test]
    fn contact_update_reports_the_new_saved_contact() {
        let mut action = wa::sync_action_value::ContactAction::default();
        action.full_name = Some("Sari Wulandari".to_owned());
        action.pn_jid = Some("628111222333@s.whatsapp.net".to_owned());
        action.lid_jid = Some("21750217740322@lid".to_owned());
        let at = whatsapp_rust::wacore::chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp");
        let event = Event::ContactUpdate(
            ContactUpdate::builder()
                .jid("628111222333@s.whatsapp.net".parse::<Jid>().expect("valid jid"))
                .timestamp(at)
                .action(Box::new(action))
                .from_full_sync(false)
                .build(),
        );
        let (sink, rx) = sink();
        translate_event(&event, &sink, &[], &mut super::IdentityBook::default());
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "628111222333@s.whatsapp.net".to_owned(),
                name: "Sari Wulandari".to_owned(),
                alt_jids: vec!["21750217740322@lid".to_owned()],
            }]],
            "the new contact is reported once, under the phone-number JID"
        );
    }

    /// M-1 (qc1646 review 4): a mapping learned AFTER the
    /// saved set was reported re-reports the set with the new
    /// form edge. The contact is saved under both JID forms
    /// by a history payload that carries no mapping, so the
    /// first report stands the person as two entries with no
    /// alternate forms. A contact update with the UNCHANGED
    /// name then teaches the pair: the saved notes no-op, the
    /// mapping is the only thing that moves, and it alone
    /// must produce a second report carrying one merged entry
    /// with the LID as the phone entry's alternate form.
    ///
    /// The unmapped twin state is built from two separate
    /// single-form records: since qc1648 (review 6 M-1) one
    /// record carrying both forms teaches the pair itself,
    /// so it can no longer stand as the pre-mapping state.
    #[test]
    fn mapping_learned_by_an_unchanged_name_update_re_reports_the_saved_set() {
        let mut sync = wa::HistorySync::default();
        sync.inline_contacts = vec![
            {
                let mut c = wa::InlineContact::default();
                c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
                c.full_name = Some("Bima Prasetyo".to_owned());
                c
            },
            {
                let mut c = wa::InlineContact::default();
                c.lid_jid = Some("57243458396358@lid".to_owned());
                c.full_name = Some("Bima Prasetyo".to_owned());
                c
            },
        ];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&sync, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![
                SavedContact {
                    jid: "57243458396358@lid".to_owned(),
                    name: "Bima Prasetyo".to_owned(),
                    alt_jids: Vec::new(),
                },
                SavedContact {
                    jid: "6281234567890@s.whatsapp.net".to_owned(),
                    name: "Bima Prasetyo".to_owned(),
                    alt_jids: Vec::new(),
                },
            ]],
            "without a mapping the two forms stand as two entries with no alternates"
        );

        let mut action = wa::sync_action_value::ContactAction::default();
        action.full_name = Some("Bima Prasetyo".to_owned());
        action.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
        action.lid_jid = Some("57243458396358@lid".to_owned());
        let at = whatsapp_rust::wacore::chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp");
        let update = Event::ContactUpdate(
            ContactUpdate::builder()
                .jid("6281234567890@s.whatsapp.net".parse::<Jid>().expect("valid jid"))
                .timestamp(at)
                .action(Box::new(action))
                .from_full_sync(false)
                .build(),
        );
        translate_event(&update, &sink, &[], &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "6281234567890@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: vec!["57243458396358@lid".to_owned()],
            }]],
            "the learned mapping re-reports the set: one entry, the LID as its alternate"
        );
    }

    /// M-1 (qc1647 review 6): one history inline contact
    /// carrying BOTH JID forms, with no mappings-table entry
    /// at all, is one saved person from the moment of
    /// absorption: the contact record's own two fields are
    /// the pairing evidence, so the report carries one merged
    /// entry with the LID as the phone entry's alternate, and
    /// the canonical key resolves both forms to the phone
    /// form. Before the fix the same payload reported two
    /// entries with no alternates, and an except-mode status
    /// exception naming one form missed the person under the
    /// other.
    #[test]
    fn history_inline_contact_with_both_forms_is_one_saved_person_without_a_mappings_entry() {
        let mut sync = wa::HistorySync::default();
        sync.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.lid_jid = Some("57243458396358@lid".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        // Deliberately no phone_number_to_lid_mappings: the
        // contact record's own fields are the only pairing
        // evidence in the payload.
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&sync, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "6281234567890@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: vec!["57243458396358@lid".to_owned()],
            }]],
            "one record with both forms reports one person, the LID as its alternate"
        );
        assert_eq!(
            book.canonical_key("57243458396358@lid"),
            "6281234567890@s.whatsapp.net",
            "the LID resolves to the phone form the saved set uses"
        );
        assert_eq!(
            book.canonical_key("6281234567890@s.whatsapp.net"),
            "6281234567890@s.whatsapp.net"
        );
        // A replay of the same payload changes nothing in the
        // book, so it reports nothing again.
        translate_history_sync(&sync, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(saved_reports(&events).is_empty(), "an unchanged set is not re-reported");
    }

    /// L-2 (qc1647 review 6): a contact update carrying both
    /// JID forms but NO name teaches the book the pairing
    /// anyway: the pair is an identity fact, only the saved
    /// name is name-gated. The contact below is saved under
    /// their LID alone; the name-less update teaches the
    /// pair, the funnel re-reports the set with the phone
    /// form as the LID entry's alternate, and the phone form
    /// now resolves to the saved name, while the update
    /// itself broadcasts no name anywhere.
    #[test]
    fn nameless_contact_update_teaches_the_form_pair() {
        let mut sync = wa::HistorySync::default();
        sync.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.lid_jid = Some("57243458396358@lid".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&sync, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "57243458396358@lid".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: Vec::new(),
            }]],
            "the contact is first reported under their LID alone"
        );

        let mut action = wa::sync_action_value::ContactAction::default();
        action.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
        action.lid_jid = Some("57243458396358@lid".to_owned());
        // No full_name, first_name or username: the action
        // carries the pair and no name.
        let at = whatsapp_rust::wacore::chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp");
        let update = Event::ContactUpdate(
            ContactUpdate::builder()
                .jid("6281234567890@s.whatsapp.net".parse::<Jid>().expect("valid jid"))
                .timestamp(at)
                .action(Box::new(action))
                .from_full_sync(false)
                .build(),
        );
        translate_event(&update, &sink, &[], &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "57243458396358@lid".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: vec!["6281234567890@s.whatsapp.net".to_owned()],
            }]],
            "the learned pair re-reports the set: the phone form is the LID entry's alternate"
        );
        assert!(
            events.iter().all(|e| !matches!(
                e,
                BackendEvent::SenderNamed { .. } | BackendEvent::ChatRenamed { .. }
            )),
            "a name-less update broadcasts no name"
        );
        assert_eq!(
            book.canonical_key("6281234567890@s.whatsapp.net"),
            "57243458396358@lid",
            "the phone form keys under the saved LID now that the pair is known"
        );
        assert_eq!(
            book.name_for("6281234567890@s.whatsapp.net").as_deref(),
            Some("Bima Prasetyo"),
            "the phone form resolves to the saved name through the learned pair"
        );
    }

    /// M-1 (qc1646 review 4): a mappings-only history payload
    /// (no inline contacts at all) that teaches the form pair
    /// of a contact the book already holds re-reports the set
    /// with the new alternate form, through the absorb path.
    #[test]
    fn mappings_only_payload_re_reports_a_saved_contact_with_its_new_form() {
        let mut first = wa::HistorySync::default();
        first.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&first, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "6281234567890@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: Vec::new(),
            }]],
            "the contact is first reported under its phone form alone"
        );

        let mut second = wa::HistorySync::default();
        second.phone_number_to_lid_mappings = vec![{
            let mut m = wa::PhoneNumberToLIDMapping::default();
            m.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            m.lid_jid = Some("57243458396358@lid".to_owned());
            m
        }];
        translate_history_sync(&second, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "6281234567890@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: vec!["57243458396358@lid".to_owned()],
            }]],
            "the mappings-only payload re-reports the contact with its alternate form"
        );
    }

    /// M-1 (qc1646 review 4): the report discipline stays
    /// bounded by the saved set. A mappings-only payload
    /// naming a person the book does NOT hold changes nothing
    /// the report carries, so it emits nothing, and neither
    /// does its replay.
    #[test]
    fn mappings_only_payload_for_an_unsaved_person_reports_nothing() {
        let mut first = wa::HistorySync::default();
        first.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&first, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(saved_reports(&events).len(), 1, "the saved contact is reported once");

        let mut second = wa::HistorySync::default();
        second.phone_number_to_lid_mappings = vec![{
            let mut m = wa::PhoneNumberToLIDMapping::default();
            m.pn_jid = Some("628999888777@s.whatsapp.net".to_owned());
            m.lid_jid = Some("999888777666555@lid".to_owned());
            m
        }];
        translate_history_sync(&second, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(
            saved_reports(&events).is_empty(),
            "a mapping for a person outside the saved set is not reported"
        );
        translate_history_sync(&second, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(saved_reports(&events).is_empty(), "its replay is not reported either");
    }

    /// M-1 (qc1648 review 7): a contact changes numbers: the
    /// LID is stable, the phone number changes, and a live
    /// mapping source teaches the new pair. The pairing maps
    /// must stay inverses: the old number's reverse edge is
    /// retired, the saved entry follows the person to the new
    /// number, and the re-report carries exactly ONE entry
    /// (the new number, the LID as its alternate). Before the
    /// fix the insert-only learning left the old number's
    /// edge standing, so the set reported the person twice,
    /// once per number, each entry claiming the same LID.
    #[test]
    fn repoint_through_note_mapping_keeps_one_saved_entry_under_the_new_number() {
        let mut sync = wa::HistorySync::default();
        sync.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.lid_jid = Some("57243458396358@lid".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&sync, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "6281234567890@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: vec!["57243458396358@lid".to_owned()],
            }]],
            "the contact is first reported under the old number"
        );

        let learned = book.note_mapping("57243458396358@lid", "628999000111@s.whatsapp.net");
        assert_eq!(
            learned,
            Some(("57243458396358@lid".to_owned(), "628999000111@s.whatsapp.net".to_owned())),
            "a re-point is a learning: the caller re-runs the healing it unlocks"
        );
        book.report_saved_contacts(&sink);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "628999000111@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: vec!["57243458396358@lid".to_owned()],
            }]],
            "after the re-point the set carries one entry, under the new number"
        );
        assert_eq!(book.canonical_key("57243458396358@lid"), "628999000111@s.whatsapp.net");
        assert_eq!(
            book.canonical_key("628999000111@s.whatsapp.net"),
            "628999000111@s.whatsapp.net"
        );
        assert_eq!(
            book.canonical_key("6281234567890@s.whatsapp.net"),
            "6281234567890@s.whatsapp.net",
            "the old number no longer resolves to the person"
        );
        assert_eq!(
            book.name_for("6281234567890@s.whatsapp.net"),
            None,
            "the saved name left the old number with the person: a recycled number inherits nothing"
        );
        assert_eq!(book.name_for("628999000111@s.whatsapp.net").as_deref(), Some("Bima Prasetyo"));

        // The same pair taught again changes nothing and
        // reports nothing: the pairing has settled.
        assert_eq!(book.note_mapping("57243458396358@lid", "628999000111@s.whatsapp.net"), None);
        book.report_saved_contacts(&sink);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(saved_reports(&events).is_empty(), "a settled pairing does not re-report");
    }

    /// M-1 (qc1648 review 7): the same number change arriving
    /// as a later history payload whose mappings table alone
    /// carries the new pair. Absorb learns through the same
    /// path, so the old number's edge is retired and the
    /// saved entry moves even though the payload carries no
    /// saved name at all: the retired number's saved key is
    /// what the relevance check must count as held.
    #[test]
    fn repoint_through_a_mappings_only_payload_moves_the_saved_entry() {
        let mut first = wa::HistorySync::default();
        first.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.lid_jid = Some("57243458396358@lid".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&first, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(saved_reports(&events).len(), 1, "the contact is reported under the old number");

        let mut second = wa::HistorySync::default();
        second.phone_number_to_lid_mappings = vec![{
            let mut m = wa::PhoneNumberToLIDMapping::default();
            m.pn_jid = Some("628999000111@s.whatsapp.net".to_owned());
            m.lid_jid = Some("57243458396358@lid".to_owned());
            m
        }];
        translate_history_sync(&second, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "628999000111@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: vec!["57243458396358@lid".to_owned()],
            }]],
            "the mappings-only re-point re-reports one entry, under the new number"
        );
        assert_eq!(
            book.name_for("6281234567890@s.whatsapp.net"),
            None,
            "the saved entry moved off the old number"
        );

        translate_history_sync(&second, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(saved_reports(&events).is_empty(), "a replay of the re-point reports nothing");
    }

    /// M-1 (qc1648 review 7): the number change arriving as a
    /// later payload's contact record, carrying both forms at
    /// the new number. The absorbed record's own pairing
    /// re-points the book, and the set still lands on one
    /// entry under the new number.
    #[test]
    fn repoint_through_a_contact_record_payload_keeps_one_saved_entry() {
        let mut first = wa::HistorySync::default();
        first.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.lid_jid = Some("57243458396358@lid".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&first, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(saved_reports(&events).len(), 1, "the contact is reported under the old number");

        let mut second = wa::HistorySync::default();
        second.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("628999000111@s.whatsapp.net".to_owned());
            c.lid_jid = Some("57243458396358@lid".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        translate_history_sync(&second, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "628999000111@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: vec!["57243458396358@lid".to_owned()],
            }]],
            "the record re-point re-reports one entry, under the new number"
        );
        assert_eq!(book.canonical_key("57243458396358@lid"), "628999000111@s.whatsapp.net");
        assert_eq!(book.name_for("6281234567890@s.whatsapp.net"), None);
    }

    /// M-1 companion (qc1648 review 7): forgetting a
    /// re-pointed contact removes the person completely. The
    /// removal resolves through the current counterpart, and
    /// because the re-point already moved the saved entry off
    /// the old number, no ghost entry under the old number
    /// survives the forget (before the fix, forgetting
    /// resolved through the stale counterpart and the old
    /// number's entry stayed behind).
    #[test]
    fn forgetting_a_repointed_contact_leaves_no_ghost_entry() {
        let mut sync = wa::HistorySync::default();
        sync.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.lid_jid = Some("57243458396358@lid".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&sync, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(saved_reports(&events).len(), 1, "the contact is reported under the old number");

        assert!(book.note_mapping("57243458396358@lid", "628999000111@s.whatsapp.net").is_some());
        book.report_saved_contacts(&sink);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(saved_reports(&events).len(), 1, "the re-point re-reported the moved entry");

        // Forgetting under the OLD number finds nothing: the
        // entry already moved off it, so the set is unchanged
        // and nothing re-reports.
        book.forget_saved("6281234567890@s.whatsapp.net");
        book.report_saved_contacts(&sink);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(saved_reports(&events).is_empty(), "the old number holds nothing to forget");
        assert_eq!(
            book.saved_contacts(),
            vec![SavedContact {
                jid: "628999000111@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: vec!["57243458396358@lid".to_owned()],
            }],
            "the one entry stands under the new number"
        );

        // Forgetting under the LID removes the person under
        // both remaining forms; no entry survives anywhere.
        book.forget_saved("57243458396358@lid");
        book.report_saved_contacts(&sink);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![Vec::<SavedContact>::new()],
            "the removal re-reports the set with the person gone"
        );
        assert!(book.saved_contacts().is_empty(), "no ghost entry remains under any form");
        assert_eq!(book.name_for("628999000111@s.whatsapp.net"), None);
        assert_eq!(book.name_for("57243458396358@lid"), None);
        assert_eq!(book.name_for("6281234567890@s.whatsapp.net"), None);
    }

    /// M-1 companion (qc1648 review 7): the revision
    /// discipline survives re-points. A re-point for a person
    /// the address book does not hold changes nothing the
    /// report carries, through either funnel (a live mapping
    /// source, an absorbed payload), so it emits nothing:
    /// report volume stays bounded by the saved set.
    #[test]
    fn repoint_for_an_unsaved_person_reports_nothing() {
        let (sink, rx) = sink();
        let mut book = mapped_book();
        // A first learning and then a re-point, both for a
        // person outside the saved set, through the live
        // mapping path.
        assert!(
            book.note_mapping("999888777666555@lid", "628999888777@s.whatsapp.net").is_some(),
            "the first learning is a learning"
        );
        assert!(
            book.note_mapping("999888777666555@lid", "628777666555@s.whatsapp.net").is_some(),
            "the re-point is a learning"
        );
        book.report_saved_contacts(&sink);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(
            saved_reports(&events).is_empty(),
            "neither the learning nor the re-point of an unsaved person is reported"
        );

        // The same through absorbed payloads: a mappings-only
        // payload teaches the pair, the next re-points it.
        let mut first = wa::HistorySync::default();
        first.phone_number_to_lid_mappings = vec![{
            let mut m = wa::PhoneNumberToLIDMapping::default();
            m.pn_jid = Some("628111000222@s.whatsapp.net".to_owned());
            m.lid_jid = Some("111222333444555@lid".to_owned());
            m
        }];
        translate_history_sync(&first, &sink, &mut book);
        let mut second = wa::HistorySync::default();
        second.phone_number_to_lid_mappings = vec![{
            let mut m = wa::PhoneNumberToLIDMapping::default();
            m.pn_jid = Some("628333000444@s.whatsapp.net".to_owned());
            m.lid_jid = Some("111222333444555@lid".to_owned());
            m
        }];
        translate_history_sync(&second, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(
            saved_reports(&events).is_empty(),
            "an absorbed re-point of an unsaved person is not reported either"
        );
    }

    /// L-1 (qc1648 review 7): a corrupt contact record (or
    /// mappings entry) naming one JID as both of a contact's
    /// forms teaches no pairing: the JID must not become its
    /// own counterpart and stand as its own alternate form.
    /// The live mapping path has always guarded the case; the
    /// history paths share the guard through the one learning
    /// path.
    #[test]
    fn self_pair_records_teach_no_pairing() {
        let mut sync = wa::HistorySync::default();
        sync.inline_contacts = vec![{
            let mut c = wa::InlineContact::default();
            c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.lid_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            c.full_name = Some("Bima Prasetyo".to_owned());
            c
        }];
        sync.phone_number_to_lid_mappings = vec![{
            let mut m = wa::PhoneNumberToLIDMapping::default();
            m.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            m.lid_jid = Some("6281234567890@s.whatsapp.net".to_owned());
            m
        }];
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&sync, &sink, &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![vec![SavedContact {
                jid: "6281234567890@s.whatsapp.net".to_owned(),
                name: "Bima Prasetyo".to_owned(),
                alt_jids: Vec::new(),
            }]],
            "the contact stands as one entry with no alternate form, not as its own alternate"
        );
        assert_eq!(
            book.canonical_key("6281234567890@s.whatsapp.net"),
            "6281234567890@s.whatsapp.net"
        );
    }

    /// The book a history payload with two mapped contacts
    /// leaves behind: Bima saved under both forms, and a
    /// contact saved only under their LID whose phone-number
    /// form the mappings still know.
    fn mapped_book() -> super::IdentityBook {
        let mut sync = wa::HistorySync::default();
        sync.inline_contacts = vec![
            {
                let mut c = wa::InlineContact::default();
                c.pn_jid = Some("6281234567890@s.whatsapp.net".to_owned());
                c.lid_jid = Some("57243458396358@lid".to_owned());
                c.full_name = Some("Bima Prasetyo".to_owned());
                c
            },
            {
                let mut c = wa::InlineContact::default();
                c.lid_jid = Some("128815045062660@lid".to_owned());
                c.full_name = Some("Lid Only".to_owned());
                c
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
                m.pn_jid = Some("628555000111@s.whatsapp.net".to_owned());
                m.lid_jid = Some("128815045062660@lid".to_owned());
                m
            },
        ];
        let (sink, _rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_history_sync(&sync, &sink, &mut book);
        book
    }

    /// H-1 (qc1645 review 3): the canonical key names the form
    /// the saved set uses, whichever form the JID arrives in:
    /// a person saved under both forms keys under the phone
    /// number from either side, a person saved only under
    /// their LID keys under the LID from either side, and an
    /// unsaved JID passes through unchanged.
    #[test]
    fn canonical_key_names_the_saved_form_in_both_directions() {
        let book = mapped_book();
        assert_eq!(
            book.canonical_key("57243458396358@lid"),
            "6281234567890@s.whatsapp.net",
            "a LID whose phone form is saved keys under the phone form"
        );
        assert_eq!(
            book.canonical_key("6281234567890@s.whatsapp.net"),
            "6281234567890@s.whatsapp.net"
        );
        assert_eq!(
            book.canonical_key("128815045062660@lid"),
            "128815045062660@lid",
            "a contact saved only under their LID keys under the LID"
        );
        assert_eq!(
            book.canonical_key("628555000111@s.whatsapp.net"),
            "128815045062660@lid",
            "the phone form of a LID-only contact keys under the saved LID"
        );
        assert_eq!(
            book.canonical_key("628999999999@s.whatsapp.net"),
            "628999999999@s.whatsapp.net",
            "an unsaved JID passes through unchanged"
        );
    }

    /// M-1 (qc1645 review 3): a contact removal synced from
    /// another device forgets the saved contact under BOTH
    /// JID forms, even though the removal event names only
    /// one: the shrunken set is re-reported (here, empty), and
    /// name resolution no longer returns the saved name for
    /// either form.
    #[test]
    fn contact_removal_forgets_the_saved_contact_under_both_forms() {
        let mut action = wa::sync_action_value::ContactAction::default();
        action.full_name = Some("Sari Wulandari".to_owned());
        action.pn_jid = Some("628111222333@s.whatsapp.net".to_owned());
        action.lid_jid = Some("21750217740322@lid".to_owned());
        let at = whatsapp_rust::wacore::chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp");
        let update = Event::ContactUpdate(
            ContactUpdate::builder()
                .jid("628111222333@s.whatsapp.net".parse::<Jid>().expect("valid jid"))
                .timestamp(at)
                .action(Box::new(action))
                .from_full_sync(false)
                .build(),
        );
        let (sink, rx) = sink();
        let mut book = super::IdentityBook::default();
        translate_event(&update, &sink, &[], &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(saved_reports(&events).len(), 1, "the update reported the new contact");
        assert_eq!(book.name_for("628111222333@s.whatsapp.net").as_deref(), Some("Sari Wulandari"));

        let removed = Event::ContactRemoved(
            ContactRemoved::builder()
                .jid("21750217740322@lid".parse::<Jid>().expect("valid jid"))
                .timestamp(at)
                .from_full_sync(false)
                .build(),
        );
        translate_event(&removed, &sink, &[], &mut book);
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(
            saved_reports(&events),
            vec![Vec::<SavedContact>::new()],
            "the removal re-reports the saved set with the contact gone"
        );
        assert_eq!(
            book.name_for("628111222333@s.whatsapp.net"),
            None,
            "the saved name is forgotten under the phone form too"
        );
        assert_eq!(book.name_for("21750217740322@lid"), None);
    }

    /// M-1 (qc1645 review 3): a removal naming a contact the
    /// book never saved changes nothing, so nothing is
    /// re-reported: the revision discipline matches the growth
    /// paths.
    #[test]
    fn contact_removal_of_an_unknown_contact_reports_nothing() {
        let at = whatsapp_rust::wacore::chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp");
        let removed = Event::ContactRemoved(
            ContactRemoved::builder()
                .jid("628111222333@s.whatsapp.net".parse::<Jid>().expect("valid jid"))
                .timestamp(at)
                .from_full_sync(false)
                .build(),
        );
        let (sink, rx) = sink();
        translate_event(&removed, &sink, &[], &mut super::IdentityBook::default());
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert!(saved_reports(&events).is_empty(), "an unknown removal is not re-reported");
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

    /// An order message classifies as the Order kind with
    /// its capability (id and token) retained: before this
    /// arm existed the payload fell through to Text and the
    /// order-details fetch had nothing to authorize with.
    #[test]
    fn order_message_classifies_with_its_capability() {
        let mut order = wa::message::OrderMessage::default();
        order.order_id = Some("ORDER-9".to_owned());
        order.token = Some("tok-9".to_owned());
        order.item_count = Some(3);
        order.total_amount1000 = Some(45_500);
        order.total_currency_code = Some("USD".to_owned());
        order.status = Some(wa::message::order_message::OrderStatus::Accepted);
        order.order_title = Some("Order #9".to_owned());
        let mut msg = wa::Message::default();
        msg.order_message = MessageField::some(order);
        let (kind, _) = classify_message(&msg);
        match kind {
            LiveMessageKind::Order {
                order_id,
                token,
                item_count,
                total_amount_1000,
                currency,
                status,
            } => {
                assert_eq!(order_id, "ORDER-9");
                assert_eq!(token, "tok-9");
                assert_eq!(item_count, 3);
                assert_eq!(total_amount_1000, 45_500);
                assert_eq!(currency, "USD");
                assert_eq!(status, "ACCEPTED");
            }
            other => panic!("expected the Order kind, got {other:?}"),
        }
        assert_eq!(structured_text(&msg).as_deref(), Some("Order #9"));
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

    /// P7: an event message classifies as the typed event kind
    /// with every display field carried, and its text summary
    /// names the event.
    #[test]
    fn event_message_classifies_with_fields() {
        let mut event_proto = wa::message::EventMessage::default();
        event_proto.name = Some("Launch dinner".to_owned());
        event_proto.description = Some("Bring a jacket".to_owned());
        event_proto.start_time = Some(1_700_086_400);
        event_proto.end_time = Some(1_700_097_200);
        event_proto.join_link = Some("https://meet.example.com/launch".to_owned());
        event_proto.extra_guests_allowed = Some(true);
        let mut location_proto = wa::message::LocationMessage::default();
        location_proto.name = Some("Rooftop".to_owned());
        event_proto.location = MessageField::some(location_proto);
        let mut event = wa::Message::default();
        event.event_message = MessageField::some(event_proto);
        let (kind, _) = classify_message(&event);
        let LiveMessageKind::Event {
            name,
            description,
            start_time,
            end_time,
            location,
            join_link,
            extra_guests,
            canceled,
            ..
        } = &kind
        else {
            panic!("the event classifies as an event kind: {kind:?}");
        };
        assert_eq!(name, "Launch dinner");
        assert_eq!(description, "Bring a jacket");
        assert_eq!((*start_time, *end_time), (1_700_086_400, 1_700_097_200));
        assert_eq!(location, "Rooftop");
        assert_eq!(join_link, "https://meet.example.com/launch");
        assert!(*extra_guests);
        assert!(!*canceled);
        assert_eq!(body_for(&kind, false, None), "Launch dinner");
        // A cancellation wears its marker in the summary.
        let mut canceled_proto = wa::message::EventMessage::default();
        canceled_proto.name = Some("Launch dinner".to_owned());
        canceled_proto.is_canceled = Some(true);
        let mut canceled_msg = wa::Message::default();
        canceled_msg.event_message = MessageField::some(canceled_proto);
        let (kind, _) = classify_message(&canceled_msg);
        assert_eq!(body_for(&kind, false, None), "Event canceled");
    }

    /// Typed summaries: an album header, a group invite, and a
    /// channel invite each name themselves instead of wearing
    /// the fake text "Message".
    #[test]
    fn unrepresented_kinds_get_typed_summaries() {
        let mut album = wa::Message::default();
        album.album_message = MessageField::some(wa::message::AlbumMessage::default());
        let (kind, _) = classify_message(&album);
        assert_eq!(body_for(&kind, false, None), "Album");

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
        assert!(matches!(kind, LiveMessageKind::Buttons { .. }), "got {kind:?}");
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

    /// A hydrated template renders as the Buttons kind: its
    /// content text is the body, its footer the footer, and
    /// each button carries the action its payload declares
    /// (a quick reply by id, a link, a call), because the
    /// hydrated form is the message with its words already
    /// resolved.
    #[test]
    fn hydrated_template_becomes_buttons_with_button_actions() {
        use whatsapp_rust::prelude::wa::__buffa::oneof::hydrated_template_button::HydratedButton;
        let mut reply = wa::hydrated_template_button::HydratedQuickReplyButton::default();
        reply.display_text = Some("Track again".to_owned());
        reply.id = Some("track".to_owned());
        let mut reply_button = wa::HydratedTemplateButton::default();
        reply_button.hydrated_button = Some(HydratedButton::QuickReplyButton(Box::new(reply)));
        let mut url = wa::hydrated_template_button::HydratedURLButton::default();
        url.display_text = Some("Open the store".to_owned());
        url.url = Some("https://example.com/order/7".to_owned());
        let mut url_button = wa::HydratedTemplateButton::default();
        url_button.hydrated_button = Some(HydratedButton::UrlButton(Box::new(url)));
        let mut call = wa::hydrated_template_button::HydratedCallButton::default();
        call.display_text = Some("Call the courier".to_owned());
        call.phone_number = Some("+6281234567890".to_owned());
        let mut call_button = wa::HydratedTemplateButton::default();
        call_button.hydrated_button = Some(HydratedButton::CallButton(Box::new(call)));

        let mut inner = wa::message::template_message::HydratedFourRowTemplate::default();
        inner.hydrated_content_text = Some("Your parcel is out for delivery".to_owned());
        inner.hydrated_footer_text = Some("Arrives today".to_owned());
        inner.hydrated_buttons = vec![reply_button, url_button, call_button];
        let mut template = wa::message::TemplateMessage::default();
        template.hydrated_template = MessageField::some(inner);
        let mut msg = wa::Message::default();
        msg.template_message = MessageField::some(template);

        let (kind, _) = classify_message(&msg);
        let LiveMessageKind::Buttons { footer, buttons } = &kind else {
            panic!("a hydrated template is a buttons message, got {kind:?}");
        };
        assert_eq!(footer, "Arrives today");
        assert_eq!(buttons.len(), 3);
        assert_eq!(buttons[0].id, "track");
        assert_eq!(buttons[0].text, "Track again");
        assert_eq!(buttons[0].action, None, "a quick reply is answered by id");
        assert_eq!(
            buttons[1].action,
            Some(crate::LiveButtonAction::Url("https://example.com/order/7".to_owned()))
        );
        assert_eq!(
            buttons[2].action,
            Some(crate::LiveButtonAction::Call("+6281234567890".to_owned()))
        );
        assert_eq!(
            body_for(&kind, false, structured_text(&msg).as_deref()),
            "Your parcel is out for delivery"
        );
    }

    /// A hydrated template whose content still holds
    /// parameter placeholders, with no title that can serve,
    /// files as unsupported: nothing in the payload carries
    /// the values the placeholders name, so painting the
    /// skeleton would put words in the sender's mouth.
    #[test]
    fn skeleton_template_files_as_unsupported() {
        let mut inner = wa::message::template_message::HydratedFourRowTemplate::default();
        inner.hydrated_content_text = Some("Hello {{1}}, your code is {{2}}".to_owned());
        let mut template = wa::message::TemplateMessage::default();
        template.hydrated_template = MessageField::some(inner);
        let mut msg = wa::Message::default();
        msg.template_message = MessageField::some(template);
        let (kind, _) = classify_message(&msg);
        assert!(
            matches!(&kind, LiveMessageKind::Unsupported(label) if label == "Template message"),
            "got {kind:?}"
        );
        assert_eq!(structured_text(&msg), None);
    }

    /// The legacy four-row template files as unsupported even
    /// though it carries text-shaped parts: its content is a
    /// HighlyStructuredMessage (a namespace, an element name
    /// and parameter values), which never contains the
    /// sentence those parameters belong to.
    #[test]
    fn legacy_four_row_template_files_as_unsupported() {
        use whatsapp_rust::prelude::wa::__buffa::oneof::message::template_message::Format;
        let mut template = wa::message::TemplateMessage::default();
        template.format = Some(Format::FourRowTemplate(Box::default()));
        let mut msg = wa::Message::default();
        msg.template_message = MessageField::some(template);
        let (kind, _) = classify_message(&msg);
        assert!(
            matches!(&kind, LiveMessageKind::Unsupported(label) if label == "Template message"),
            "got {kind:?}"
        );
    }

    /// A template wrapping a carousel interactive message
    /// renders as the carousel kind, through the same
    /// extraction the bare interactive arm uses.
    #[test]
    fn template_carousel_becomes_the_carousel_kind() {
        use whatsapp_rust::prelude::wa::__buffa::oneof::message::interactive_message::InteractiveMessage as InteractiveOneof;
        use whatsapp_rust::prelude::wa::__buffa::oneof::message::template_message::Format;
        let mut card_body = wa::message::interactive_message::Body::default();
        card_body.text = Some("Red, size 42".to_owned());
        let mut card = wa::message::interactive_message::CarouselMessage::default();
        let mut one_card = wa::message::InteractiveMessage::default();
        one_card.body = MessageField::some(card_body);
        card.cards = vec![one_card];
        let mut body = wa::message::interactive_message::Body::default();
        body.text = Some("Picks for you".to_owned());
        let mut interactive = wa::message::InteractiveMessage::default();
        interactive.body = MessageField::some(body);
        interactive.interactive_message = Some(InteractiveOneof::CarouselMessage(Box::new(card)));
        let mut template = wa::message::TemplateMessage::default();
        template.format = Some(Format::InteractiveMessageTemplate(Box::new(interactive)));
        let mut msg = wa::Message::default();
        msg.template_message = MessageField::some(template);
        let (kind, _) = classify_message(&msg);
        let LiveMessageKind::Carousel { body, cards } = &kind else {
            panic!("a template carousel is a carousel, got {kind:?}");
        };
        assert_eq!(body, "Picks for you");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].body, "Red, size 42");
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
            saved_revision: 0,
            saved_reported: 0,
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

    /// A payload no classifier arm claims and no text
    /// recovery reads files as an unsupported message carrying
    /// its truthful label, on the history path: the kind is
    /// Unsupported (never a text message wearing the label as
    /// its body by accident), and the body is the label, which
    /// is what previews and search read.
    #[test]
    fn unhandled_history_payload_files_as_unsupported() {
        let book = super::IdentityBook::default();
        let mut info = wa::WebMessageInfo::default();
        let mut key = wa::MessageKey::default();
        key.id = Some("h-unsupported".to_owned());
        key.from_me = Some(false);
        key.participant = Some("6281234567890@s.whatsapp.net".to_owned());
        info.key = MessageField::some(key);
        info.message_timestamp = Some(1_700_000_000);
        let mut inner = wa::Message::default();
        inner.template_message = MessageField::some(wa::message::TemplateMessage::default());
        info.message = MessageField::some(inner);
        let mut resolved = Vec::new();
        let msg =
            super::history_message("6281234567890@s.whatsapp.net", &info, &book, &mut resolved)
                .expect("the message translates");
        assert!(
            matches!(&msg.kind, LiveMessageKind::Unsupported(label) if label == "Template message"),
            "got {:?}",
            msg.kind
        );
        assert_eq!(msg.body, "Template message");
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
        // A first learning reports the pair; the same pair
        // again reports nothing.
        assert_eq!(
            book.note_mapping(lid, pn),
            Some((lid.to_owned(), pn.to_owned())),
            "a newly learned mapping reports its pair"
        );
        assert_eq!(book.note_mapping(lid, pn), None, "a known mapping reports nothing");
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
        let _ = book.note_mapping(lid, pn);
        assert_eq!(book.sender_label(lid, ""), "Push Sarah");
        // No name anywhere: the formatted phone, never the raw
        // LID digits.
        let mut book = super::IdentityBook::default();
        let _ = book.note_mapping(lid, pn);
        assert_eq!(book.sender_label(lid, ""), "+6281234567890");
        assert_eq!(book.author_label(lid), "+6281234567890");
        assert_eq!(book.dm_title(lid, None), "+6281234567890");
        // Nothing known at all: the masked identifier is the
        // last resort; the raw digits are never a display name
        // (qc1634 claim 3, the owner's masked-fallback rule).
        let book = super::IdentityBook::default();
        assert_eq!(book.sender_label(lid, ""), "123 ••• 2345");
        assert_eq!(book.author_label(lid), "123 ••• 2345");
        assert_eq!(book.dm_title(lid, None), "123 ••• 2345");
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
        let _ = payload.note_mapping("123456789012345@lid", "6281234567890@s.whatsapp.net");
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
