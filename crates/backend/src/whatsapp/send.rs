//! Outgoing message construction and sending: text, media,
//! location and sticker builders over the library's message
//! protobufs, plus the ephemeral and view-once wrappers.

use whatsapp_rust::prelude::*;
use whatsapp_rust::usync::{
    UsyncContext, UsyncMode, UsyncOutcome, UsyncProtocol, UsyncProtocolKind, UsyncProtocolResult,
    UsyncQuery, UsyncUser,
};

use crate::QuotedRef;

/// One outgoing media payload, borrowed from the command.
pub(super) struct OutgoingMedia<'a> {
    pub(super) kind: crate::MediaKind,
    /// Caller-minted protocol ID for the send, when the command
    /// carried one (see [`send_with_id`]).
    pub(super) send_id: Option<&'a str>,
    pub(super) data: Vec<u8>,
    pub(super) mime: &'a str,
    pub(super) caption: &'a str,
    pub(super) file_name: Option<&'a str>,
    pub(super) gif_playback: bool,
    pub(super) seconds: u32,
    pub(super) view_once: bool,
    pub(super) ephemeral_secs: u32,
}

/// Sends one prepared message, under the caller-minted protocol
/// ID when the command carried one: the library's send options
/// accept a caller-chosen message ID (`SendOptions::with_message_id`,
/// documented by the library itself as the resend/idempotency
/// knob), so the stanza goes out under the very ID the app filed
/// its local echo with. Without a supplied ID the plain send
/// runs and the library mints as before. A supplied ID the
/// library refuses (only an empty ID is refused) falls back to
/// the plain send rather than failing the message over an ID
/// the app minted in the library's own format.
pub(super) async fn send_with_id(
    client: &whatsapp_rust::Client,
    jid: Jid,
    message: wa::Message,
    send_id: Option<&str>,
) -> Result<SendResult, whatsapp_rust::SendError> {
    let Some(raw) = send_id.filter(|id| !id.is_empty()) else {
        return client.send_message(jid, message).await;
    };
    let Ok(message_id) = whatsapp_rust::MessageId::new(raw) else {
        return client.send_message(jid, message).await;
    };
    let options = whatsapp_rust::SendOptions::default().with_message_id(message_id);
    let request = whatsapp_rust::SendRequest::new(&jid, message).with_options(options);
    client.send(request).await
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
/// assigned protocol ID. Failures are the structured
/// [`crate::SendFailure`] the caller forwards to the app: the
/// upload's error is untyped, so only its kind can be read (the
/// raw error is logged by the classifier), while the send step
/// classifies from the library's typed send error.
pub(super) async fn send_media(
    client: &whatsapp_rust::Client,
    jid: &Jid,
    outgoing: OutgoingMedia<'_>,
) -> Result<SendResult, crate::SendFailure> {
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
        .map_err(|e| {
            super::util::send_failure_from_text(&e.to_string(), crate::SendFailureStage::Other)
        })?;

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
    send_with_id(client, jid.clone(), message, outgoing.send_id)
        .await
        .map_err(|e| super::util::send_failure(&e, jid.to_string().ends_with("@g.us")))
}

/// Warms the caches one send to `jid` would otherwise pay for
/// inline: for a group, the routing metadata the group send
/// fetches first; for a direct chat, the recipient's LID
/// mapping and then the device list the DM send resolves.
/// Every step is best-effort and its failure is dropped: a
/// warm that fails leaves the next send exactly as cold as it
/// would have been without it. The mapping is warmed first
/// because device resolution addresses through it, and a
/// mapping that is already known is never re-queried, so
/// warming cannot change how a later send is addressed.
pub(super) async fn warm_chat_cache(client: &whatsapp_rust::Client, jid: &Jid) {
    if jid.to_string().ends_with("@g.us") {
        let _ = client.groups().routing_info(jid).await;
        return;
    }
    if jid.is_pn() || jid.is_lid() {
        warm_lid_mapping(client, jid).await;
        let _ = client.signal().get_user_devices(&[jid.to_non_ad()]).await;
    }
}

/// Starts [`warm_chat_cache`] in the background and returns at
/// once. The send path uses this because the LID usync a cold
/// DM send pays sits inside the library's `send_message` (its
/// DM branch awaits a best-effort LID query before falling
/// back to phone-number addressing, and the backend cannot
/// split that await out of the library call): racing the warm
/// ahead of the send lets the mapping land in the library's
/// cache first, in which case the library skips its inline
/// query entirely, and when the warm loses the race the send
/// behaves exactly as it did before. A warm that has already
/// landed (the app warms a chat when it is opened) makes this
/// a cache read, not a query.
pub(super) fn spawn_cache_warm(client: &std::sync::Arc<whatsapp_rust::Client>, jid: &Jid) {
    let client = client.clone();
    let jid = jid.clone();
    tokio::spawn(async move {
        warm_chat_cache(&client, &jid).await;
    });
}

/// Learns one direct chat's LID mapping ahead of a send, the
/// same query the library's DM branch runs inline: a usync
/// LID query whose answer is recorded in the library's own
/// mapping cache, so the send's later cache check hits it.
/// A mapping the cache or the persistent store already holds
/// ends the warm at once (the lookup is cache-aside over the
/// store and never goes to the wire); only a definitive miss
/// pays the query.
async fn warm_lid_mapping(client: &whatsapp_rust::Client, jid: &Jid) {
    if !jid.is_pn() {
        return;
    }
    if !matches!(client.get_lid_pn_entry(jid).await, Ok(None)) {
        return;
    }
    let query = match UsyncQuery::new(
        UsyncMode::Query,
        UsyncContext::Background,
        vec![UsyncProtocol::Lid],
        vec![UsyncUser::from_jid(jid.to_non_ad())],
    ) {
        Ok(query) => query,
        Err(_) => return,
    };
    let Ok(response) = client.query_usync(query).await else {
        return;
    };
    for user in &response.users {
        let Some(UsyncProtocolResult::Lid(UsyncOutcome::Value(Some(lid_jid)))) =
            user.protocol(UsyncProtocolKind::Lid)
        else {
            continue;
        };
        let _ = client
            .add_lid_pn_mapping(
                lid_jid.user.as_ref(),
                jid.user.as_ref(),
                whatsapp_rust::lid_pn_cache::LearningSource::Usync,
            )
            .await;
    }
}

/// The background a text status post carries, as an ARGB word:
/// the house green, the color a text status wears when the user
/// picks no color of their own.
pub(super) const STATUS_TEXT_BACKGROUND_ARGB: u32 = 0xFF00_A884;

/// Posts a text status through the library's status API. The
/// recipients are the post's audience (a status is a sender-key
/// fan-out, not a single-address send); `privacy` sets the
/// stanza's `status_setting` to match the audience the caller
/// computed. Returns the library's send result so the caller
/// can report success against the assigned protocol ID.
pub(super) async fn post_text_status(
    client: &whatsapp_rust::Client,
    text: &str,
    recipients: &[Jid],
    privacy: whatsapp_rust::StatusPrivacySetting,
) -> Result<SendResult, String> {
    client
        .status()
        .send_text(
            text,
            STATUS_TEXT_BACKGROUND_ARGB,
            wa::message::extended_text_message::FontType::SYSTEM,
            recipients,
            whatsapp_rust::StatusSendOptions::default().with_privacy(privacy),
        )
        .await
        .map_err(|e| e.to_string())
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
#[allow(clippy::too_many_arguments)]
pub(super) async fn post_media_status(
    client: &whatsapp_rust::Client,
    kind: crate::MediaKind,
    data: Vec<u8>,
    caption: &str,
    thumbnail: Vec<u8>,
    seconds: u32,
    recipients: &[Jid],
    privacy: whatsapp_rust::StatusPrivacySetting,
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
    let options = whatsapp_rust::StatusSendOptions::default().with_privacy(privacy);
    let caption = if caption.is_empty() { None } else { Some(caption) };
    match kind {
        crate::MediaKind::Photo => client
            .status()
            .send_image(upload, thumbnail, caption, recipients, options)
            .await
            .map_err(|e| e.to_string()),
        _ => client
            .status()
            .send_video(upload, thumbnail, seconds, caption, recipients, options)
            .await
            .map_err(|e| e.to_string()),
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
    let id = whatsapp_rust::MessageId::new(message_id).map_err(|e| e.to_string())?;
    client
        .status()
        .revoke(id, recipients, whatsapp_rust::StatusSendOptions::default())
        .await
        .map_err(|e| e.to_string())
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

/// Largest page body read for a link preview, in bytes. Open
/// Graph fields live in the document head; a page whose head
/// needs more than this is not carrying a usable preview.
const MAX_PREVIEW_PAGE_BYTES: u64 = 512 * 1024;

/// Largest preview image body read, in bytes, before it is
/// downscaled to a thumbnail. Bigger source images are refused
/// rather than decoded: the thumbnail never needs them.
const MAX_PREVIEW_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Redirect hops a preview fetch follows at most. Every hop is
/// re-validated through the public-HTTPS gate before it is
/// requested, so a redirect can never steer the fetch to a
/// private or plain-HTTP target.
const MAX_PREVIEW_REDIRECTS: usize = 3;

/// Per-request timeout for a preview fetch. A preview is a
/// nicety on the send path, never a reason to hold a send open:
/// a slow page simply yields no preview.
const PREVIEW_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// Total time budget for one send's whole preview work: the
/// page fetch, all its redirect hops, and the preview image
/// fetch together. The preview runs inside the chat's ordered
/// send queue, so without a total cap a slow page could hold
/// that chat's later sends for the per-request timeout times
/// the hop count times two fetches (worst case roughly 48
/// seconds). Eight seconds bounds that hold while leaving a
/// healthy page, which answers in well under a second per hop,
/// unaffected. Other chats are never affected either way: each
/// chat has its own queue. When the budget runs out mid-work,
/// the preview is abandoned and the send proceeds without one.
const PREVIEW_TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// Longest side of the JPEG thumbnail embedded in an outgoing
/// preview, matching the fidelity the app's own status
/// thumbnails use.
const PREVIEW_THUMBNAIL_PX: u32 = 320;

/// A link preview generated for an outgoing text message: the
/// fields the receiving side reads back (see
/// `translate::embedded_link_preview`), nothing more.
pub(super) struct OutgoingLinkPreview {
    /// The URL as it appears in the message body.
    pub(super) matched_text: String,
    /// Page title (`og:title`).
    pub(super) title: String,
    /// Page description (`og:description`).
    pub(super) description: String,
    /// JPEG thumbnail of the preview image, downscaled here
    /// because the wire carries the image itself, not a URL.
    pub(super) thumbnail: Option<PreviewThumbnail>,
}

/// A downscaled preview image plus its pixel size, which the
/// wire carries next to the bytes.
pub(super) struct PreviewThumbnail {
    pub(super) jpeg: Vec<u8>,
    pub(super) width: u32,
    pub(super) height: u32,
}

/// The first URL in a message body, or `None`. Only the first
/// URL ever gets a preview, matching what other clients do: a
/// message is about one link, the rest are mentions. Sentence
/// punctuation stuck to the end of a pasted URL (a closing
/// bracket, a full stop) is not part of the address and is
/// trimmed; anything inside the token is left alone.
pub(super) fn first_url(text: &str) -> Option<&str> {
    let lower = text.to_ascii_lowercase();
    // Both schemes are searched because the first link in the
    // text wins even when it is plain HTTP (which the fetch gate
    // then refuses): a later HTTPS link does not take its place.
    let https_at = lower.find("https://");
    let http_at = lower.find("http://");
    let start = match (https_at, http_at) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\''))
        .unwrap_or(rest.len());
    let token = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Resolves a possibly-relative URL reference against the page
/// URL that carried it (an `og:image` is often root-relative).
/// Only the shapes a page actually uses are handled: absolute,
/// scheme-relative, root-relative and path-relative references.
/// Anything else yields `None`, and the caller's gate check
/// would refuse a non-HTTPS result anyway.
fn resolve_url(base: &str, reference: &str) -> Option<String> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    if reference.starts_with("https://") || reference.starts_with("http://") {
        return Some(reference.to_owned());
    }
    let (scheme, rest) = base.split_once("://")?;
    if let Some(stripped) = reference.strip_prefix("//") {
        return Some(format!("{scheme}://{stripped}"));
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if let Some(stripped) = reference.strip_prefix('/') {
        return Some(format!("{scheme}://{authority}/{stripped}"));
    }
    // Path-relative: replace the base's last path segment.
    let path = &rest[authority_end..];
    let path = path.split(['?', '#']).next().unwrap_or("");
    let dir = path.rfind('/').map_or("/", |i| &path[..=i]);
    Some(format!("{scheme}://{authority}{dir}{reference}"))
}

/// Decodes the small set of HTML entities page metadata uses in
/// practice: the named XML five plus decimal and hex numeric
/// references. Unknown entities pass through untouched, because
/// a title with a literal `&` in it is more honest than one with
/// a hole.
fn decode_html_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(semi) = tail.find(';').filter(|i| *i <= 10) else {
            out.push('&');
            rest = &rest[at + 1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse::<u32>().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The Open Graph fields of one fetched page.
pub(super) struct OpenGraph {
    title: String,
    description: String,
    image: Option<String>,
}

/// Reads one attribute's value from a single HTML tag's text.
/// Attribute names match case-insensitively; values may be
/// double-quoted, single-quoted or bare.
fn tag_attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(at) = lower[search_from..].find(name).map(|i| i + search_from) {
        // The name must start an attribute, not sit inside a
        // longer word (data-property= is not property=).
        let before_ok = at == 0 || !matches!(lower.as_bytes()[at - 1], b'a'..=b'z' | b'-');
        let mut pos = at + name.len();
        while lower.as_bytes().get(pos).is_some_and(u8::is_ascii_whitespace) {
            pos += 1;
        }
        if before_ok && lower.as_bytes().get(pos) == Some(&b'=') {
            pos += 1;
            while lower.as_bytes().get(pos).is_some_and(u8::is_ascii_whitespace) {
                pos += 1;
            }
            let bytes = tag.as_bytes();
            return match bytes.get(pos) {
                Some(b'"') => {
                    let start = pos + 1;
                    let end = tag[start..].find('"').map(|i| i + start)?;
                    Some(tag[start..end].to_owned())
                }
                Some(b'\'') => {
                    let start = pos + 1;
                    let end = tag[start..].find('\'').map(|i| i + start)?;
                    Some(tag[start..end].to_owned())
                }
                Some(_) => {
                    let end = tag[pos..]
                        .find(|c: char| c.is_whitespace() || c == '>')
                        .map_or(tag.len(), |i| i + pos);
                    Some(tag[pos..end].to_owned())
                }
                None => None,
            };
        }
        search_from = at + name.len();
    }
    None
}

/// Parses the Open Graph fields out of a fetched page's HTML.
/// Deliberately a small scanner, not an HTML parser: it reads
/// `<meta>` tags in the head and the `<title>` element, which is
/// all preview metadata ever uses. `og:` fields win, the
/// `twitter:` spellings and the plain description meta are
/// fallbacks, and the document title is the last resort, the
/// same precedence other clients apply. Yields `None` when the
/// page offers neither a title nor a description, because the
/// receiving side treats that as no preview at all.
pub(super) fn parse_open_graph(html: &str, page_url: &str) -> Option<OpenGraph> {
    let lower = html.to_ascii_lowercase();
    let mut og_title = String::new();
    let mut og_description = String::new();
    let mut og_image: Option<String> = None;
    let mut fallback_title = String::new();
    let mut fallback_description = String::new();
    let mut pos = 0;
    while let Some(at) = lower[pos..].find("<meta").map(|i| i + pos) {
        let tag_end = lower[at..].find('>').map_or(lower.len(), |i| i + at);
        let tag = &html[at..tag_end];
        let key = tag_attribute(tag, "property")
            .or_else(|| tag_attribute(tag, "name"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if let Some(content) = tag_attribute(tag, "content") {
            let content = decode_html_entities(content.trim());
            match key.as_str() {
                "og:title" if og_title.is_empty() => og_title = content,
                "og:description" if og_description.is_empty() => og_description = content,
                "og:image" | "og:image:url" if og_image.is_none() => {
                    og_image = resolve_url(page_url, &content);
                }
                "twitter:title" if fallback_title.is_empty() => fallback_title = content,
                "twitter:description" if fallback_description.is_empty() => {
                    fallback_description = content;
                }
                "twitter:image" if og_image.is_none() => {
                    og_image = resolve_url(page_url, &content);
                }
                "description" if fallback_description.is_empty() => {
                    fallback_description = content;
                }
                _ => {}
            }
        }
        pos = tag_end + 1;
    }
    let title = if !og_title.is_empty() {
        og_title
    } else if !fallback_title.is_empty() {
        fallback_title
    } else if let Some(at) = lower.find("<title") {
        // The document title: text between the tag's close and
        // the end tag, with any inner markup spacing collapsed.
        let open_end = lower[at..].find('>').map(|i| i + at + 1)?;
        let close = lower[open_end..].find("</title").map_or(lower.len(), |i| i + open_end);
        decode_html_entities(html[open_end..close].trim())
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        String::new()
    };
    let description =
        if !og_description.is_empty() { og_description } else { fallback_description };
    if title.is_empty() && description.is_empty() {
        return None;
    }
    Some(OpenGraph { title, description, image: og_image })
}

/// A ureq resolver that answers every lookup with exactly the
/// addresses `core::net::validated_target` validated for one
/// hop, so the connection goes to the addresses that were
/// checked instead of whatever a second DNS resolution at
/// connect time would answer (the DNS-rebinding window
/// `core::net` documents, SEC-F05). The agent carrying this
/// resolver is built for a single request and discarded, so
/// answering regardless of the queried name cannot steer a
/// different host's connection: there is no other request. The
/// TLS handshake and the Host header still use the URL's own
/// host name, because ureq derives both from the request URI,
/// not from the resolved address; only the socket destination
/// is pinned. This mirrors the Giphy media client's
/// `resolve_to_addrs` pin, expressed through ureq's resolver
/// seam because ureq has no equivalent builder option.
#[derive(Debug)]
struct PinnedResolver {
    addrs: Vec<std::net::SocketAddr>,
}

impl ureq::unversioned::resolver::Resolver for PinnedResolver {
    fn resolve(
        &self,
        _uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        // The resolver contract caps the answer set (16 in the
        // bundled ureq); validated targets beyond that are
        // truncated, never padded with unvalidated addresses.
        let mut result = self.empty();
        for addr in self.addrs.iter().take(16) {
            result.push(*addr);
        }
        if result.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(result)
    }
}

/// Builds the single-request agent for one preview hop,
/// pinned to the hop's validated target (see
/// [`PinnedResolver`]), with the request timeout the caller
/// computed from the send's remaining preview budget.
fn pinned_preview_agent(
    target: &core::net::ValidatedTarget,
    timeout: std::time::Duration,
) -> ureq::Agent {
    let config =
        ureq::Agent::config_builder().timeout_global(Some(timeout)).max_redirects(0).build();
    ureq::Agent::with_parts(
        config,
        ureq::unversioned::transport::DefaultConnector::new(),
        PinnedResolver { addrs: target.addrs.clone() },
    )
}

/// One gated fetch: the final URL after redirects, the body
/// bytes and the response's content type. Every hop, the page
/// fetch and each redirect target alike, is validated and
/// resolved once through `core::net::validated_target` and
/// then connected to exactly those validated addresses via a
/// pinned agent (see [`PinnedResolver`]), because a check-only
/// gate would let the HTTP stack resolve the name again at
/// connect time and a rebinding answer could slip a private
/// address past it. Every request runs inside the caller's
/// total preview budget (`deadline`): a hop whose turn comes
/// after the budget is spent is never requested, and each
/// request's own timeout is the smaller of the per-request
/// cap and the time left, so the whole fetch, hops included,
/// returns by the deadline. Blocking HTTP; call on a blocking
/// thread.
fn fetch_gated(
    url: &str,
    max_bytes: u64,
    deadline: std::time::Instant,
) -> Option<(String, Vec<u8>, Option<String>)> {
    let mut current = url.to_owned();
    for hop in 0..=MAX_PREVIEW_REDIRECTS {
        if deadline.saturating_duration_since(std::time::Instant::now()).is_zero() {
            return None;
        }
        let target = core::net::validated_target(&current)?;
        // Validation itself resolves the name, which the budget
        // cannot interrupt; what it can do is refuse to spend a
        // request on time that is already gone, so the remaining
        // budget is measured again after resolution.
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let agent = pinned_preview_agent(&target, PREVIEW_FETCH_TIMEOUT.min(remaining));
        let mut response = agent.get(&current).call().ok()?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            if hop == MAX_PREVIEW_REDIRECTS {
                return None;
            }
            let location = response.headers().get("location")?.to_str().ok()?.to_owned();
            current = resolve_url(&current, &location)?;
            continue;
        }
        if status != 200 {
            return None;
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        let body = response.body_mut().with_config().limit(max_bytes).read_to_vec().ok()?;
        return Some((current, body, content_type));
    }
    None
}

/// Downscales fetched image bytes to the JPEG thumbnail an
/// outgoing preview embeds: longest side capped, quality in
/// line with the app's own status thumbnails. Bytes that do not
/// decode as an image yield `None`; the preview then goes out
/// without a picture instead of with a fabricated one.
pub(super) fn preview_jpeg_thumbnail(bytes: &[u8]) -> Option<PreviewThumbnail> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > PREVIEW_THUMBNAIL_PX || img.height() > PREVIEW_THUMBNAIL_PX {
        img.resize(
            PREVIEW_THUMBNAIL_PX,
            PREVIEW_THUMBNAIL_PX,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let (width, height) = (img.width(), img.height());
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 70);
    image::ImageEncoder::write_image(
        encoder,
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )
    .ok()?;
    Some(PreviewThumbnail { jpeg: out, width, height })
}

/// Generates the preview for a message body's first URL, or
/// `None` when there is nothing to attach: no URL, the gate
/// refused the target, the fetch failed, the page is not HTML,
/// or it carries no preview fields. Every failure is silent by
/// design: the caller sends the plain text either way. All of
/// the fetching shares one total budget
/// ([`PREVIEW_TOTAL_BUDGET`]) counted from here, so the image
/// fetch gets only what the page fetch left over. Blocking
/// (network and image decode); call on a blocking thread.
pub(super) fn fetch_link_preview(body: &str) -> Option<OutgoingLinkPreview> {
    let deadline = std::time::Instant::now() + PREVIEW_TOTAL_BUDGET;
    let url = first_url(body)?;
    let (final_url, page, content_type) = fetch_gated(url, MAX_PREVIEW_PAGE_BYTES, deadline)?;
    // Only an HTML page can carry Open Graph fields; a PDF or
    // an image behind the link gets no preview rather than a
    // parse attempt on bytes that are not markup.
    let is_html = content_type.as_deref().is_some_and(|ct| {
        let ct = ct.to_ascii_lowercase();
        ct.contains("text/html") || ct.contains("application/xhtml+xml")
    });
    if !is_html {
        return None;
    }
    let html = String::from_utf8_lossy(&page);
    let graph = parse_open_graph(&html, &final_url)?;
    let thumbnail = graph.image.and_then(|image_url| {
        let (_, bytes, _) = fetch_gated(&image_url, MAX_PREVIEW_IMAGE_BYTES, deadline)?;
        preview_jpeg_thumbnail(&bytes)
    });
    Some(OutgoingLinkPreview {
        matched_text: url.to_owned(),
        title: graph.title,
        description: graph.description,
        thumbnail,
    })
}

/// Attaches a generated preview to an outgoing text message,
/// promoting a bare conversation to extended text when needed
/// and preserving any quote context an extended text already
/// carries. The fields are exactly the ones the receiving side
/// reads (`translate::embedded_link_preview`): `matched_text`,
/// `title`, `description` and the embedded `jpeg_thumbnail`.
pub(super) fn attach_link_preview(
    message: wa::Message,
    preview: &OutgoingLinkPreview,
) -> wa::Message {
    let mut message = message;
    if message.extended_text_message.as_option().is_none() {
        let text = message.conversation.take().unwrap_or_default();
        let mut extended = wa::message::ExtendedTextMessage::default();
        extended.text = Some(text);
        message.extended_text_message = MessageField::some(extended);
    }
    if let Some(extended) = message.extended_text_message.as_option_mut() {
        extended.matched_text = Some(preview.matched_text.clone());
        if !preview.title.is_empty() {
            extended.title = Some(preview.title.clone());
        }
        if !preview.description.is_empty() {
            extended.description = Some(preview.description.clone());
        }
        if let Some(thumb) = &preview.thumbnail {
            extended.jpeg_thumbnail = Some(thumb.jpeg.clone());
            extended.thumbnail_width = Some(thumb.width);
            extended.thumbnail_height = Some(thumb.height);
            extended.preview_type = Some(wa::message::extended_text_message::PreviewType::Image);
        }
    }
    message
}

/// The send path's preview step: when previews are enabled and
/// the body carries a URL, generate the preview off the async
/// worker (the fetch is blocking) and attach it. Any failure
/// along the way returns the message unchanged, so a preview
/// can never fail the send itself; it can delay the send (and,
/// through the per-chat queue, that chat's later sends) by at
/// most the total preview budget, [`PREVIEW_TOTAL_BUDGET`].
pub(super) async fn with_link_preview(
    message: wa::Message,
    body: &str,
    enabled: bool,
) -> wa::Message {
    if !enabled || first_url(body).is_none() {
        return message;
    }
    let body = body.to_owned();
    let preview =
        tokio::task::spawn_blocking(move || fetch_link_preview(&body)).await.ok().flatten();
    match preview {
        Some(preview) => attach_link_preview(message, &preview),
        None => message,
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

#[cfg(test)]
mod link_preview_tests {
    use super::{
        attach_link_preview, fetch_gated, fetch_link_preview, first_url, parse_open_graph,
        pinned_preview_agent, OutgoingLinkPreview, PreviewThumbnail,
    };
    use whatsapp_rust::prelude::*;

    #[test]
    fn pinned_agent_builds_only_for_validated_targets() {
        // SEC-F05: agent construction itself needs no network;
        // a public literal validates and builds a pinned agent,
        // while non-public targets never reach construction
        // because validated_target refuses them first.
        let target = core::net::validated_target("https://8.8.8.8/page").expect("public literal");
        let _agent = pinned_preview_agent(&target, std::time::Duration::from_secs(1));
        for bad in ["http://8.8.8.8/page", "https://127.0.0.1/page", "https://10.0.0.5/page"] {
            assert!(core::net::validated_target(bad).is_none(), "{bad} must be rejected");
        }
    }

    #[test]
    fn gated_fetch_with_a_spent_budget_requests_nothing() {
        // The total preview budget is the queue-hold bound: once
        // it is spent, the fetch yields no preview without
        // touching the network, even for a valid public target.
        let spent = std::time::Instant::now() - std::time::Duration::from_secs(1);
        assert!(fetch_gated("https://8.8.8.8/page", 1024, spent).is_none());
    }

    #[test]
    fn first_url_finds_none_without_a_link() {
        assert_eq!(first_url("no links here"), None);
        assert_eq!(first_url("visit example.com/page sometime"), None);
        assert_eq!(first_url(""), None);
    }

    #[test]
    fn first_url_returns_only_the_first_link() {
        let body = "see https://one.example/a and https://two.example/b later";
        assert_eq!(first_url(body), Some("https://one.example/a"));
    }

    #[test]
    fn first_url_stops_at_whitespace_and_trims_sentence_punctuation() {
        assert_eq!(first_url("look (https://example.com/page)."), Some("https://example.com/page"));
        assert_eq!(
            first_url("https://example.com/a?x=1, then more"),
            Some("https://example.com/a?x=1")
        );
    }

    #[test]
    fn first_url_prefers_an_earlier_http_link() {
        // The first link wins even when it is plain HTTP: the
        // fetch gate refuses it later, and a subsequent HTTPS
        // link does not take its place.
        let body = "http://first.example/a https://second.example/b";
        assert_eq!(first_url(body), Some("http://first.example/a"));
    }

    const FIXTURE_HTML: &str = r#"<!doctype html>
<html><head>
<title>Fixture Fallback Title</title>
<meta name="description" content="The plain description">
<meta property="og:title" content="Tom &amp; Jerry &#8212; The Page">
<meta property="og:description" content="A page about examples">
<meta property="og:image" content="/img/cover.png">
</head><body>body text</body></html>"#;

    #[test]
    fn open_graph_fields_are_parsed_from_a_fixture() {
        let graph = parse_open_graph(FIXTURE_HTML, "https://example.com/articles/one")
            .expect("the fixture carries a preview");
        assert_eq!(graph.title, "Tom & Jerry \u{2014} The Page");
        assert_eq!(graph.description, "A page about examples");
        // A root-relative og:image resolves against the page.
        assert_eq!(graph.image.as_deref(), Some("https://example.com/img/cover.png"));
    }

    #[test]
    fn open_graph_falls_back_to_the_document_title_and_meta_description() {
        let html = r#"<html><head><title>Just a Title</title>
<meta name="description" content="Only a description"></head></html>"#;
        let graph = parse_open_graph(html, "https://example.com/")
            .expect("title plus description still preview");
        assert_eq!(graph.title, "Just a Title");
        assert_eq!(graph.description, "Only a description");
        assert!(graph.image.is_none());
    }

    #[test]
    fn open_graph_without_title_or_description_is_no_preview() {
        let html = r#"<html><head><meta property="og:image" content="https://example.com/i.png"></head></html>"#;
        assert!(parse_open_graph(html, "https://example.com/").is_none());
        assert!(parse_open_graph("<html></html>", "https://example.com/").is_none());
    }

    #[test]
    fn the_gate_rejects_http_and_private_targets_before_any_fetch() {
        // Literal hosts only: the gate refuses these without a
        // DNS lookup or a connection, so the test stays offline.
        // fetch_link_preview must yield no preview for each.
        for body in [
            "go http://8.8.8.8/page",
            "go https://127.0.0.1/page",
            "go https://10.0.0.5/page",
            "go https://192.168.1.10/page",
            "go https://[::1]/page",
            "go https://169.254.169.254/latest",
        ] {
            assert!(fetch_link_preview(body).is_none(), "{body} must not be fetched");
        }
    }

    fn sample_preview() -> OutgoingLinkPreview {
        OutgoingLinkPreview {
            matched_text: "https://example.com/page".to_owned(),
            title: "Example Page".to_owned(),
            description: "A page about examples".to_owned(),
            thumbnail: Some(PreviewThumbnail {
                jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9],
                width: 4,
                height: 3,
            }),
        }
    }

    #[test]
    fn attach_promotes_plain_text_and_matches_the_receive_mapping() {
        let msg = wa::Message::text("look https://example.com/page");
        let msg = attach_link_preview(msg, &sample_preview());
        // The receive side must read back exactly what was
        // attached: the same field mapping, verified through
        // the translate layer's own extractor.
        let preview = super::super::translate::embedded_link_preview(&msg)
            .expect("the receive side reads the attached preview");
        assert_eq!(preview.title, "Example Page");
        assert_eq!(preview.description, "A page about examples");
        assert_eq!(preview.url, "https://example.com/page");
        assert_eq!(preview.thumbnail.as_deref(), Some(&[0xFF, 0xD8, 0xFF, 0xD9][..]));
    }

    #[test]
    fn attach_keeps_an_existing_quote_context() {
        let jid: Jid = "12025550112@s.whatsapp.net".parse().expect("jid");
        let quoted = crate::QuotedRef {
            message_id: "wamid.9".to_owned(),
            sender_jid: None,
            excerpt: "earlier".to_owned(),
            remote_jid: None,
        };
        let msg =
            super::outgoing_text_message("reply https://example.com/page", Some(&quoted), &jid);
        let msg = attach_link_preview(msg, &sample_preview());
        let extended = msg.extended_text_message.as_option().expect("extended text");
        assert_eq!(extended.title.as_deref(), Some("Example Page"));
        let ctx = extended.context_info.as_option().expect("quote context kept");
        assert_eq!(ctx.stanza_id.as_deref(), Some("wamid.9"));
    }
}
