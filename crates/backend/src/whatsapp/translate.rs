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
/// category → value spelling map.
pub(super) fn privacy_map(
    resp: &whatsapp_rust::privacy_settings::PrivacySettingsResponse,
) -> std::collections::BTreeMap<String, String> {
    use whatsapp_rust::privacy_settings::{PrivacyCategory, PrivacyValue};
    let mut out = std::collections::BTreeMap::new();
    for setting in &resp.settings {
        let category = match setting.category {
            PrivacyCategory::Last => "last",
            PrivacyCategory::Online => "online",
            PrivacyCategory::Profile => "profile",
            PrivacyCategory::Status => "status",
            PrivacyCategory::GroupAdd => "groupadd",
            PrivacyCategory::ReadReceipts => "readreceipts",
            _ => continue,
        };
        let value = match setting.value {
            PrivacyValue::All => "all",
            PrivacyValue::Contacts => "contacts",
            PrivacyValue::None => "none",
            PrivacyValue::ContactBlacklist => "contact_blacklist",
            PrivacyValue::MatchLastSeen => "match_last_seen",
            PrivacyValue::Known => "known",
            _ => continue,
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
            _ => {}
        }
    }
    false
}

/// Maps a library event the typed handlers do not cover.
pub(super) fn translate_event(event: &Event, sink: &EventSink) {
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
            // translate_special instead).
            sink.emit(BackendEvent::DisappearingChanged {
                chat_jid: change.from.to_string(),
                duration_secs: change.duration,
            });
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
        _ => {}
    }
}

/// Translates one incoming message context into a chat + message
/// upsert, in Zaprust terms. Non-text messages are summarized with a
/// bracketed placeholder in this phase (media download/display is a
/// later milestone).
pub(super) fn translate_message(ctx: &MessageContext) -> Option<(LiveChat, LiveMessage)> {
    let source = &ctx.info.source;
    let chat_jid = source.chat.to_string();
    let kind = kind_of(&chat_jid);
    let body = ctx
        .message
        .text_content()
        .map_or_else(|| "[non-text message]".to_owned(), ToOwned::to_owned);
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
    let (kind, view_once) = classify_kind(ctx);
    msg.kind = kind;
    msg.view_once = view_once;
    if let LiveMessageKind::Other(label) = &msg.kind {
        if msg.body == "[non-text message]" {
            msg.body = label.clone();
        }
    }
    if let LiveMessageKind::Media { caption, .. } = &msg.kind {
        // A view-once caption is part of the sealed payload: it
        // stays on the kind (the viewer shows it with the media)
        // but is never promoted to the body, where previews,
        // notifications and the archive would pick it up as
        // ordinary text. The body keeps the generic non-text
        // marker those consumers already handle.
        if !view_once && !caption.is_empty() && msg.body == "[non-text message]" {
            msg.body = caption.clone();
        }
    }
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
    Some((chat, msg))
}

/// Classifies a message's rich kind from its protocol payload.
/// Poll *votes* arrive encrypted as separate protocol messages and
/// are not aggregated here (documented limitation); poll creations
/// map fully.
pub(super) fn classify_kind(ctx: &MessageContext) -> (LiveMessageKind, bool) {
    classify_message(&ctx.message)
}

/// Classifies one protocol message; the second return value is the
/// view-once flag (set when the message arrived view-once wrapped).
pub(super) fn classify_message(
    msg: &whatsapp_rust::prelude::wa::Message,
) -> (LiveMessageKind, bool) {
    // View-once wrappers: classify the wrapped message and flag it.
    let wrapped = msg
        .view_once_message
        .as_option()
        .and_then(|vo| vo.message.as_option())
        .or_else(|| msg.view_once_message_v2.as_option().and_then(|vo| vo.message.as_option()));
    if let Some(inner) = wrapped {
        let (kind, _) = classify_message(inner);
        return (kind, true);
    }
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
    let extended = msg.extended_text_message.as_option()?;
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
    let extended = ctx.message.extended_text_message.as_option()?;
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
    use super::embedded_link_preview;
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
}
