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
pub(super) fn translate_special(ctx: &MessageContext, sink: &EventSink) -> bool {
    let source = &ctx.info.source;
    let chat_jid = source.chat.to_string();
    let actor = {
        let push = ctx.info.push_name.trim();
        if source.is_from_me {
            "Me".to_owned()
        } else if !push.is_empty() {
            push.to_owned()
        } else {
            user_part(&source.sender.to_string())
        }
    };
    let msg = &ctx.message;
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
    false
}

/// Maps a library event the typed handlers do not cover.
/// `own_jids` are this account's own JIDs (phone number and
/// LID, non-AD spellings): the disappearing-mode notification
/// uses the same event for per-chat timers and for the
/// account-wide default, told apart only by whose JID the
/// change came from.
pub(super) fn translate_event(event: &Event, sink: &EventSink, own_jids: &[String]) {
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
            // device, or the state as a full sync delivers it.
            // This is the only wire path that knows star; message
            // arrivals do not carry it.
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
            sink.emit(BackendEvent::Error {
                message: format!("Pairing code error: {err:?}").chars().take(160).collect(),
            });
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
                translate_history_sync(sync, sink);
            }
        }
        Event::OfflineSyncPreview(_) => {
            sink.emit(BackendEvent::SyncProgress { percent: None });
        }
        Event::OfflineSyncCompleted(_) => {
            sink.emit(BackendEvent::SyncCompleted);
        }
        _ => {}
    }
}

/// Translates one decoded history sync payload into chat and
/// message upserts: the roster as the phone reported it, with each
/// conversation's carried messages translated by the same
/// classifier and body rules as live traffic, so history and live
/// messages are indistinguishable downstream.
pub(super) fn translate_history_sync(
    sync: &whatsapp_rust::prelude::wa::HistorySync,
    sink: &EventSink,
) {
    let push_names: std::collections::HashMap<&str, &str> = sync
        .pushnames
        .iter()
        .filter_map(|p| Some((p.id.as_deref()?, p.pushname.as_deref()?)))
        .collect();
    for conv in &sync.conversations {
        translate_history_conversation(conv, &push_names, sink);
    }
}

fn translate_history_conversation(
    conv: &whatsapp_rust::prelude::wa::Conversation,
    push_names: &std::collections::HashMap<&str, &str>,
    sink: &EventSink,
) {
    let chat_jid = conv.id.clone();
    if chat_jid.is_empty() {
        return;
    }
    let kind = kind_of(&chat_jid);
    let mut messages: Vec<LiveMessage> = Vec::new();
    for entry in &conv.messages {
        if let Some(info) = entry.message.as_option() {
            if let Some(msg) = history_message(&chat_jid, info) {
                messages.push(msg);
            }
        }
    }
    messages.sort_by_key(|m| m.timestamp);
    let last = messages.last();
    let title = conv
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| conv.display_name.clone().filter(|n| !n.trim().is_empty()))
        .or_else(|| match kind {
            LiveChatKind::Dm => last
                .and_then(|m| m.sender.clone())
                .or_else(|| push_names.get(chat_jid.as_str()).map(|s| (*s).to_owned())),
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
        preview: last.map(|m| m.body.clone()).unwrap_or_default(),
        preview_sender: if kind == LiveChatKind::Group {
            last.and_then(|m| m.sender.clone())
        } else {
            None
        },
        timestamp,
        unread: conv.unread_count.unwrap_or(0),
        archived: conv.archived.unwrap_or(false),
        pinned: conv.pinned.is_some_and(|p| p > 0),
        ephemeral_secs: conv.ephemeral_expiration.unwrap_or(0),
        muted: conv.mute_end_time.is_some_and(|end| i64::try_from(end).unwrap_or(0) > now_secs),
    };
    sink.emit(BackendEvent::ChatUpsert(chat));
    for msg in messages {
        sink.emit(BackendEvent::MessageUpsert(msg));
    }
}

/// Translates one history-carried message. Stub entries and
/// messages the phone marked to ignore carry no displayable
/// payload and are skipped.
fn history_message(
    chat_jid: &str,
    info: &whatsapp_rust::prelude::wa::WebMessageInfo,
) -> Option<LiveMessage> {
    if info.ignore == Some(true) {
        return None;
    }
    let proto = info.message.as_option()?;
    let key = info.key.as_option()?;
    let id = key.id.clone()?;
    let from_me = key.from_me.unwrap_or(false);
    let timestamp = i64::try_from(info.message_timestamp.unwrap_or(0)).unwrap_or(0);
    let text = proto.text_content().map(ToOwned::to_owned);
    let (mut kind, view_once) = classify_message(proto);
    if matches!(kind, LiveMessageKind::Text) && text.is_none() {
        kind = LiveMessageKind::Other("Message".to_owned());
    }
    let body = body_for(&kind, view_once, text.as_deref());
    let sender_jid = key.participant.clone().or_else(|| info.participant.clone());
    let sender_name = if from_me {
        None
    } else {
        info.push_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .or_else(|| sender_jid.as_deref().map(user_part))
    };
    let mut msg = LiveMessage::text(id, chat_jid.to_owned(), sender_name, body, timestamp, from_me);
    msg.kind = kind;
    msg.view_once = view_once;
    msg.photo_ref = super::util::photo_ref_of(proto);
    msg.voice_ref = super::util::voice_ref_of(proto);
    msg.video_ref = super::util::video_ref_of(proto);
    msg.video_thumbnail = super::util::video_thumbnail_of(proto);
    msg.sender_jid = sender_jid;
    if matches!(msg.kind, LiveMessageKind::Poll { .. }) {
        if let Some((secret, _names)) = super::util::received_poll_secret(proto) {
            msg.poll_secret = Some(secret);
            msg.poll_creator = msg.sender_jid.clone();
        }
    }
    Some(msg)
}

/// The display body for one translated message: its own text when
/// it has any, otherwise a typed summary from its kind (a photo
/// reads "Photo", a document its file name from the caption slot,
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
        LiveMessageKind::Location { name, .. } => name.clone(),
        LiveMessageKind::Contact { name, .. } => name.clone(),
        LiveMessageKind::Carousel { body, .. } => body.clone(),
        LiveMessageKind::Other(label) => label.clone(),
        LiveMessageKind::Text => "Message".to_owned(),
    }
}

/// Translates one incoming message context into a chat + message
/// upsert, in Zaprust terms.
pub(super) fn translate_message(ctx: &MessageContext) -> Option<(LiveChat, LiveMessage)> {
    let source = &ctx.info.source;
    let chat_jid = source.chat.to_string();
    let kind = kind_of(&chat_jid);
    let text = ctx.message.text_content().map(ToOwned::to_owned);
    let (mut msg_kind, view_once) = classify_kind(ctx);
    if matches!(msg_kind, LiveMessageKind::Text) && text.is_none() {
        // A payload with no text and no classifier arm (an
        // interactive reply, a template, an album): it is not a
        // text message and must not wear a text body.
        msg_kind = LiveMessageKind::Other("Message".to_owned());
    }
    let body = body_for(&msg_kind, view_once, text.as_deref());
    let push_name = ctx.info.push_name.trim();
    let sender_name = if source.is_from_me {
        None
    } else if !push_name.is_empty() {
        Some(push_name.to_owned())
    } else {
        Some(user_part(&source.sender.to_string()))
    };
    let title = match kind {
        LiveChatKind::Dm => sender_name.clone().unwrap_or_else(|| user_part(&chat_jid)),
        _ => user_part(&chat_jid),
    };
    let timestamp = ctx.info.timestamp.timestamp();
    let chat = LiveChat {
        jid: chat_jid.clone(),
        title,
        kind,
        preview: body.clone(),
        preview_sender: if kind == LiveChatKind::Group { sender_name.clone() } else { None },
        timestamp,
        unread: 0,
        archived: false,
        pinned: false,
        // A message arrival does not carry the chat's timer;
        // the app keeps the value it already has (see the field
        // docs on LiveChat).
        ephemeral_secs: 0,
        muted: false,
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
    // Quote context, when the message carries one.
    if let Some(quoted) = quoted_context(ctx) {
        msg.quoted_sender = quoted.0;
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
    if matches!(msg.kind, LiveMessageKind::Poll { .. }) {
        if let Some((secret, _names)) = super::util::received_poll_secret(&ctx.message) {
            msg.poll_secret = Some(secret);
            msg.poll_creator = Some(source.sender.to_string());
        }
    }
    Some((chat, msg))
}

/// Classifies a message's rich kind from its protocol payload.
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
    if let Some(poll) =
        msg.poll_creation_message.as_option().or_else(|| msg.poll_creation_message_v3.as_option())
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
            .and_then(|card| {
                card.lines().find_map(|line| {
                    let line = line.trim();
                    let rest = line.strip_prefix("TEL")?;
                    rest.split(':').nth(1).map(str::to_owned)
                })
            })
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "Contact card".to_owned());
        return (
            LiveMessageKind::Contact {
                name: contact.display_name.clone().unwrap_or_default(),
                phone,
            },
            false,
        );
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
    if let Some(image) = msg.image_message.as_option() {
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
    if let Some(video) = msg.video_message.as_option() {
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
    if let Some(audio) = msg.audio_message.as_option() {
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
    if let Some(document) = msg.document_message.as_option() {
        return (
            LiveMessageKind::Media {
                media: crate::MediaKind::Document,
                caption: document.file_name.clone().unwrap_or_default(),
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
    Some(LivePreview { title, description, url: extended.matched_text.clone().unwrap_or_default() })
}

/// Extracts (sender, excerpt) when a message quotes another one:
/// the context info of an extended-text message carries the stanza
/// id + participant + quoted payload.
pub(super) fn quoted_context(ctx: &MessageContext) -> Option<(Option<String>, Option<String>)> {
    let extended = ctx.message.get_base_message().extended_text_message.as_option()?;
    let context = extended.context_info.as_option()?;
    context.stanza_id.as_ref()?;
    let excerpt =
        context.quoted_message.as_option().and_then(|q| q.text_content().map(ToOwned::to_owned));
    let sender = context.participant.as_ref().map(|p| user_part(&p.clone()));
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

pub(super) fn kind_of(jid: &str) -> LiveChatKind {
    if jid.ends_with("@g.us") {
        LiveChatKind::Group
    } else if jid.ends_with("@newsletter") {
        LiveChatKind::Channel
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
        translate_event(&event, &sink, &[]);
        assert!(matches!(rx.try_recv(), Ok(BackendEvent::QrExpired)));
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
        translate_event(&event, &sink, &[]);
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
        translate_event(&event, &sink, &[]);
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
    use super::{body_for, classify_message, translate_history_sync};
    use crate::{BackendEvent, EventSink, LiveMessageKind, MediaKind};
    use whatsapp_rust::prelude::wa;
    use whatsapp_rust::prelude::MessageField;

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
        translate_history_sync(&sync, &sink);
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
}
