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
        .map_err(|e| format!("upload: {}", super::util::error_words(&e)))?;

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
    client
        .send_message(jid.clone(), message)
        .await
        .map_err(|e| format!("send: {}", super::util::error_words(&e)))
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
        .map_err(|e| super::util::error_words(&e).to_string())
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
        .map_err(|e| format!("upload: {}", super::util::error_words(&e)))?;
    let options = whatsapp_rust::StatusSendOptions::default();
    let caption = if caption.is_empty() { None } else { Some(caption) };
    match kind {
        crate::MediaKind::Photo => client
            .status()
            .send_image(upload, thumbnail, caption, recipients, options)
            .await
            .map_err(|e| super::util::error_words(&e).to_string()),
        _ => client
            .status()
            .send_video(upload, thumbnail, seconds, caption, recipients, options)
            .await
            .map_err(|e| super::util::error_words(&e).to_string()),
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
    let id = whatsapp_rust::MessageId::new(message_id)
        .map_err(|e| super::util::error_words(&e).to_string())?;
    client
        .status()
        .revoke(id, recipients, whatsapp_rust::StatusSendOptions::default())
        .await
        .map_err(|e| super::util::error_words(&e).to_string())
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

/// Builds an interactive message proto from the neutral spec
/// (row 19). The library ships no typed builders for these
/// shapes at the pinned revision, so the protos are assembled
/// field by field, mirroring what the receive classifier
/// reads back: a sent message and a received one classify to
/// the same kind. Button and row ids are positional, because
/// an answer only needs to name the pick within its message.
pub(super) fn outgoing_interactive_message(spec: &crate::InteractiveSpec) -> wa::Message {
    use wa::message::interactive_message::InteractiveMessage as InteractiveOneof;
    let mut message = wa::Message::default();
    match spec {
        crate::InteractiveSpec::Buttons { text, footer, buttons } => {
            let mut buttons_message = wa::message::ButtonsMessage::default();
            buttons_message.content_text = Some(text.clone());
            if !footer.is_empty() {
                buttons_message.footer_text = Some(footer.clone());
            }
            buttons_message.header_type = Some(wa::message::buttons_message::HeaderType::Empty);
            buttons_message.buttons = buttons
                .iter()
                .enumerate()
                .map(|(i, label)| {
                    let mut button = wa::message::buttons_message::Button::default();
                    button.button_id = Some((i + 1).to_string());
                    let mut button_text =
                        wa::message::buttons_message::button::ButtonText::default();
                    button_text.display_text = Some(label.clone());
                    button.button_text = MessageField::some(button_text);
                    button.r#type = Some(wa::message::buttons_message::button::Type::Response);
                    button
                })
                .collect();
            message.buttons_message = MessageField::some(buttons_message);
        }
        crate::InteractiveSpec::List { title, description, button_label, sections } => {
            let mut list = wa::message::ListMessage::default();
            list.title = Some(title.clone());
            if !description.is_empty() {
                list.description = Some(description.clone());
            }
            list.button_text = Some(button_label.clone());
            list.list_type = Some(wa::message::list_message::ListType::SingleSelect);
            let mut row_number = 0usize;
            list.sections = sections
                .iter()
                .map(|(section_title, rows)| {
                    let mut section = wa::message::list_message::Section::default();
                    if !section_title.is_empty() {
                        section.title = Some(section_title.clone());
                    }
                    section.rows = rows
                        .iter()
                        .map(|(row_title, row_description)| {
                            row_number += 1;
                            let mut row = wa::message::list_message::Row::default();
                            row.title = Some(row_title.clone());
                            if !row_description.is_empty() {
                                row.description = Some(row_description.clone());
                            }
                            row.row_id = Some(row_number.to_string());
                            row
                        })
                        .collect();
                    section
                })
                .collect();
            message.list_message = MessageField::some(list);
        }
        crate::InteractiveSpec::Carousel { body, cards } => {
            let mut interactive = wa::message::InteractiveMessage::default();
            if !body.is_empty() {
                let mut body_proto = wa::message::interactive_message::Body::default();
                body_proto.text = Some(body.clone());
                interactive.body = MessageField::some(body_proto);
            }
            let mut carousel = wa::message::interactive_message::CarouselMessage::default();
            carousel.message_version = Some(1);
            carousel.cards = cards
                .iter()
                .map(|card| {
                    let mut card_message = wa::message::InteractiveMessage::default();
                    if !card.title.is_empty() {
                        let mut header = wa::message::interactive_message::Header::default();
                        header.title = Some(card.title.clone());
                        header.has_media_attachment = Some(false);
                        card_message.header = MessageField::some(header);
                    }
                    if !card.body.is_empty() {
                        let mut card_body = wa::message::interactive_message::Body::default();
                        card_body.text = Some(card.body.clone());
                        card_message.body = MessageField::some(card_body);
                    }
                    if !card.buttons.is_empty() {
                        let mut flow = wa::message::interactive_message::NativeFlowMessage::default();
                        flow.message_version = Some(1);
                        flow.buttons = card
                            .buttons
                            .iter()
                            .enumerate()
                            .map(|(i, label)| {
                                let mut flow_button =
                                    wa::message::interactive_message::native_flow_message::NativeFlowButton::default();
                                flow_button.name = Some("quick_reply".to_owned());
                                flow_button.button_params_json = Some(
                                    serde_json::json!({
                                        "display_text": label,
                                        "id": (i + 1).to_string(),
                                    })
                                    .to_string(),
                                );
                                flow_button
                            })
                            .collect();
                        card_message.interactive_message =
                            Some(InteractiveOneof::NativeFlowMessage(Box::new(flow)));
                    }
                    card_message
                })
                .collect();
            interactive.interactive_message =
                Some(InteractiveOneof::CarouselMessage(Box::new(carousel)));
            message.interactive_message = MessageField::some(interactive);
        }
    }
    message
}

/// Builds the answer to a received buttons or list message
/// (row 19): the wire's response proto quoting the original
/// through its context info, the same shape the receive
/// classifier reads back as a text message carrying the
/// chosen label.
pub(super) fn outgoing_interactive_response(
    chat_jid: &Jid,
    message_id: &str,
    sender_jid: Option<&str>,
    is_list: bool,
    choice_id: &str,
    choice_text: &str,
) -> wa::Message {
    let mut context = wa::ContextInfo::default();
    context.stanza_id = Some(message_id.to_owned());
    context.participant =
        Some(sender_jid.map(ToOwned::to_owned).unwrap_or_else(|| chat_jid.to_string()));
    let mut message = wa::Message::default();
    if is_list {
        let mut response = wa::message::ListResponseMessage::default();
        response.title = Some(choice_text.to_owned());
        response.list_type = Some(wa::message::list_response_message::ListType::SingleSelect);
        let mut reply = wa::message::list_response_message::SingleSelectReply::default();
        reply.selected_row_id = Some(choice_id.to_owned());
        response.single_select_reply = MessageField::some(reply);
        response.context_info = MessageField::some(context);
        message.list_response_message = MessageField::some(response);
    } else {
        let mut response = wa::message::ButtonsResponseMessage::default();
        response.selected_button_id = Some(choice_id.to_owned());
        response.response =
            Some(wa::message::buttons_response_message::Response::SelectedDisplayText(
                choice_text.to_owned(),
            ));
        response.r#type = Some(wa::message::buttons_response_message::Type::DisplayText);
        response.context_info = MessageField::some(context);
        message.buttons_response_message = MessageField::some(response);
    }
    message
}
