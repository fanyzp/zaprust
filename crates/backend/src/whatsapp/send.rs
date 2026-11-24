//! Outgoing message construction and sending: text, media,
//! location and sticker builders over the library's message
//! protobufs, plus the ephemeral and view-once wrappers.

use whatsapp_rust::prelude::*;

use crate::QuotedRef;

/// One outgoing media payload, borrowed from the command.
pub(super) struct OutgoingMedia<'a> {
    pub(super) kind: crate::MediaKind,
    pub(super) data: Vec<u8>,
    pub(super) mime: &'a str,
    pub(super) caption: &'a str,
    pub(super) file_name: Option<&'a str>,
    pub(super) gif_playback: bool,
    pub(super) seconds: u32,
    pub(super) view_once: bool,
    pub(super) ephemeral_secs: u32,
}

/// Stamps a chat's disappearing-messages timer onto an outgoing
/// message (the wire carries it as the context-info expiration,
/// the same field the library's own send options set). A zero
/// timer leaves the message untouched.
pub(super) fn stamp_ephemeral(message: &mut wa::Message, ephemeral_secs: u32) {
    if ephemeral_secs > 0 {
        message.set_ephemeral_expiration(ephemeral_secs);
    }
}

/// Uploads the payload through the library and sends the typed
/// media message. Voice notes go out as push-to-talk audio; videos
/// flagged `gif_playback` loop like GIFs. Stickers have no library
/// builder, so their proto is assembled from the upload response
/// exactly the way the library's own image builder assembles its
/// fields. Photos and videos marked view-once go out wrapped in the
/// same `viewOnceMessageV2` wrapper the incoming side unwraps.
/// Returns the library's send result so the caller can report the
/// assigned protocol ID.
pub(super) async fn send_media(
    client: &whatsapp_rust::Client,
    jid: &Jid,
    outgoing: OutgoingMedia<'_>,
) -> Result<SendResult, String> {
    use whatsapp_rust::media;
    use whatsapp_rust::wacore::download::MediaType;

    let media_type = match outgoing.kind {
        crate::MediaKind::Photo => MediaType::Image,
        crate::MediaKind::Video => MediaType::Video,
        crate::MediaKind::Voice => MediaType::Audio,
        crate::MediaKind::Audio => MediaType::Audio,
        crate::MediaKind::Document => MediaType::Document,
        crate::MediaKind::Sticker => MediaType::Sticker,
    };
    let upload = client
        .upload(outgoing.data, media_type, whatsapp_rust::upload::UploadOptions::new())
        .await
        .map_err(|e| format!("upload: {e}"))?;

    let message = match outgoing.kind {
        crate::MediaKind::Photo => {
            let mut opts = media::ImageOptions::default().with_mimetype(outgoing.mime);
            if !outgoing.caption.is_empty() {
                opts = opts.with_caption(outgoing.caption);
            }
            media::image_message(upload, opts)
        }
        crate::MediaKind::Video => {
            let mut opts = media::VideoOptions::default()
                .with_mimetype(outgoing.mime)
                .with_gif_playback(outgoing.gif_playback);
            if !outgoing.caption.is_empty() {
                opts = opts.with_caption(outgoing.caption);
            }
            if outgoing.seconds > 0 {
                opts = opts.with_duration_seconds(outgoing.seconds);
            }
            media::video_message(upload, opts)
        }
        crate::MediaKind::Voice => {
            let mut opts =
                media::AudioOptions::default().with_mimetype(outgoing.mime).with_ptt(true);
            if outgoing.seconds > 0 {
                opts = opts.with_duration_seconds(outgoing.seconds);
            }
            media::audio_message(upload, opts)
        }
        crate::MediaKind::Audio => {
            // An audio FILE: the same wire shape with the ptt
            // flag off, which is what makes the receiver render
            // a file card instead of a voice note.
            let mut opts =
                media::AudioOptions::default().with_mimetype(outgoing.mime).with_ptt(false);
            if outgoing.seconds > 0 {
                opts = opts.with_duration_seconds(outgoing.seconds);
            }
            media::audio_message(upload, opts)
        }
        crate::MediaKind::Document => {
            let mut opts = media::DocumentOptions::default().with_mimetype(outgoing.mime);
            if let Some(name) = outgoing.file_name {
                opts = opts.with_file_name(name);
            }
            if !outgoing.caption.is_empty() {
                opts = opts.with_caption(outgoing.caption);
            }
            media::document_message(upload, opts)
        }
        crate::MediaKind::Sticker => sticker_message(upload, outgoing.mime),
    };
    let mut message = message;
    stamp_ephemeral(&mut message, outgoing.ephemeral_secs);
    let message = apply_view_once(message, outgoing.kind, outgoing.view_once);
    client.send_message(jid.clone(), message).await.map_err(|e| format!("send: {e}"))
}

/// The background a text status post carries, as an ARGB word:
/// the house green, the color a text status wears when the user
/// picks no color of their own.
pub(super) const STATUS_TEXT_BACKGROUND_ARGB: u32 = 0xFF00_A884;

/// Posts a text status through the library's status API. The
/// recipients are the post's audience (a status is a sender-key
/// fan-out, not a single-address send); the options keep the
/// library default, the `contacts` privacy setting. Returns the
/// library's send result so the caller can report success
/// against the assigned protocol ID.
pub(super) async fn post_text_status(
    client: &whatsapp_rust::Client,
    text: &str,
    recipients: &[Jid],
) -> Result<SendResult, String> {
    client
        .status()
        .send_text(
            text,
            STATUS_TEXT_BACKGROUND_ARGB,
            wa::message::extended_text_message::FontType::SYSTEM,
            recipients,
            whatsapp_rust::StatusSendOptions::default(),
        )
        .await
        .map_err(|e| format!("{e}"))
}

/// Posts a photo or video status through the library's status
/// API. The media uploads through the same path a chat send
/// uses; the caller supplies the thumbnail the receiving
/// clients present (a status wears the preview its author's
/// client made for it: the photo at preview fidelity, or a
/// real frame of the video). An empty thumbnail is accepted by
/// the library and simply leaves the post without a preview.
/// Returns the send result so the caller can adopt the post
/// under its assigned protocol ID.
pub(super) async fn post_media_status(
    client: &whatsapp_rust::Client,
    kind: crate::MediaKind,
    data: Vec<u8>,
    caption: &str,
    thumbnail: Vec<u8>,
    seconds: u32,
    recipients: &[Jid],
) -> Result<SendResult, String> {
    use whatsapp_rust::wacore::download::MediaType;

    let media_type = match kind {
        crate::MediaKind::Photo => MediaType::Image,
        crate::MediaKind::Video => MediaType::Video,
        _ => return Err("only a photo or a video can be a status".to_owned()),
    };
    let upload = client
        .upload(data, media_type, whatsapp_rust::upload::UploadOptions::new())
        .await
        .map_err(|e| format!("upload: {e}"))?;
    let options = whatsapp_rust::StatusSendOptions::default();
    let caption = if caption.is_empty() { None } else { Some(caption) };
    match kind {
        crate::MediaKind::Photo => client
            .status()
            .send_image(upload, thumbnail, caption, recipients, options)
            .await
            .map_err(|e| format!("{e}")),
        _ => client
            .status()
            .send_video(upload, thumbnail, Some(seconds), caption, recipients, options)
            .await
            .map_err(|e| format!("{e}")),
    }
}

/// Revokes one of my own statuses through the library's status
/// API. A revoke is itself a status fan-out, encrypted for the
/// same audience the post went to, so the caller supplies that
/// recipient list. Returns the send result for symmetry with
/// the post paths.
pub(super) async fn delete_status(
    client: &whatsapp_rust::Client,
    message_id: &str,
    recipients: &[Jid],
) -> Result<SendResult, String> {
    let id = whatsapp_rust::MessageId::new(message_id).map_err(|e| format!("{e}"))?;
    client
        .status()
        .revoke(id, recipients, whatsapp_rust::StatusSendOptions::default())
        .await
        .map_err(|e| format!("{e}"))
}

/// Builds the outgoing text message: bare text, or extendedText
/// carrying the reply context (the quoted message's protocol id, its
/// author as the participant, and a text preview of its body). A
/// missing author JID falls back to the chat address, which is the
/// right participant for direct chats.
pub(super) fn outgoing_text_message(
    body: &str,
    quoted: Option<&QuotedRef>,
    chat_jid: &Jid,
) -> wa::Message {
    use whatsapp_rust::prelude::MessageBuilderExt;
    match quoted {
        Some(q) => {
            let mut context = wa::ContextInfo::default();
            context.stanza_id = Some(q.message_id.clone());
            context.participant =
                Some(q.sender_jid.clone().unwrap_or_else(|| chat_jid.to_string()));
            // A cross-chat quote (a status reply: the quoted
            // post lives in `status@broadcast`, the reply in the
            // author's direct chat) carries the quoted message's
            // home as remote_jid; the protocol emits that field
            // for cross-chat quotes only, so a same-chat quote
            // leaves it unset.
            context.remote_jid = q.remote_jid.clone();
            context.quoted_message = MessageField::some(wa::Message::text(q.excerpt.clone()));
            wa::Message::text_with_context(body, context)
        }
        None => wa::Message::text(body),
    }
}

/// Wraps a photo or video in the `viewOnceMessageV2` wrapper when
/// `view_once` is set; other kinds ignore the flag, matching the
/// receive side which only unwraps media.
pub(super) fn apply_view_once(
    message: wa::Message,
    kind: crate::MediaKind,
    view_once: bool,
) -> wa::Message {
    if !view_once || !matches!(kind, crate::MediaKind::Photo | crate::MediaKind::Video) {
        return message;
    }
    let mut inner = wa::message::FutureProofMessage::default();
    inner.message = MessageField::some(message);
    let mut wrapper = wa::Message::default();
    wrapper.view_once_message_v2 = MessageField::some(inner);
    wrapper
}

/// Builds a location message proto: degrees plus an optional place
/// name.
pub(super) fn outgoing_location_message(name: &str, latitude: f64, longitude: f64) -> wa::Message {
    let mut message = wa::Message::default();
    message.location_message = MessageField::some({
        let mut location = wa::message::LocationMessage::default();
        location.degrees_latitude = Some(latitude);
        location.degrees_longitude = Some(longitude);
        if !name.is_empty() {
            location.name = Some(name.to_owned());
        }
        location
    });
    message
}

/// Builds a contact-card message: the display name plus a
/// vCard 3.0 carrying the number. The vCard's TEL line is the
/// shape the receiving side parses back (`waid` names the
/// account's id for the number, the value is the full
/// international form), so a card this client sends renders on
/// other clients and round-trips through this one's own receive
/// translation.
pub(super) fn outgoing_contact_message(name: &str, phone: &str) -> wa::Message {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    let vcard = format!(
        "BEGIN:VCARD\nVERSION:3.0\nFN:{name}\nTEL;type=CELL;waid={digits}:+{digits}\nEND:VCARD"
    );
    let mut message = wa::Message::default();
    message.contact_message = MessageField::some({
        let mut contact = wa::message::ContactMessage::default();
        contact.display_name = Some(name.to_owned());
        contact.vcard = Some(vcard);
        contact
    });
    message
}

/// Builds a sticker message from an upload response. Mirrors the
/// field population of the library's media builders (the library
/// ships no sticker builder at the pinned revision).
pub(super) fn sticker_message(
    upload: whatsapp_rust::upload::UploadResponse,
    mime: &str,
) -> whatsapp_rust::prelude::wa::Message {
    use whatsapp_rust::prelude::wa;
    let mut message = wa::Message::default();
    message.sticker_message = whatsapp_rust::prelude::MessageField::some({
        let mut sticker = wa::message::StickerMessage::default();
        sticker.url = Some(upload.url);
        sticker.direct_path = Some(upload.direct_path);
        sticker.media_key = Some(upload.media_key.to_vec());
        sticker.file_sha256 = Some(upload.file_sha256.to_vec());
        sticker.file_enc_sha256 = Some(upload.file_enc_sha256.to_vec());
        sticker.file_length = Some(upload.file_length);
        sticker.media_key_timestamp = Some(upload.media_key_timestamp);
        sticker.mimetype = Some(mime.to_owned());
        sticker
    });
    message
}
