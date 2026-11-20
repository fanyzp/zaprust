//! Small shared helpers for the whatsapp backend: poll-secret
//! bookkeeping, cache locking, JID parsing and message-target
//! construction, and media download into a message protobuf.

use whatsapp_rust::prelude::*;

/// Session-scoped cache of protocol payloads, keyed by
/// (chat JID, message ID). Forward and media download need the
/// original payload (media keys live only there); the archive
/// deliberately does not store protocol payloads, so both actions
/// cover messages seen during the current session.
pub(super) type ProtoCache = std::sync::Arc<
    std::sync::Mutex<(
        std::collections::HashMap<
            (String, String),
            std::sync::Arc<whatsapp_rust::prelude::wa::Message>,
        >,
        std::collections::VecDeque<(String, String)>,
    )>,
>;

/// Session-scoped poll secrets, keyed by (chat JID, message ID):
/// voting needs the creation secret. The backend caches it when
/// this session creates a poll AND when a poll creation message
/// arrives, because the secret travels inside the creation
/// message's context info (that is how every WhatsApp client
/// learns it). Polls from before the session have no cached
/// secret and answer with an honest error. Secrets are never
/// logged or persisted.
pub(super) type PollSecrets = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<(String, String), (String, Vec<u8>, Vec<String>)>>,
>;

/// Cap for the poll-secret cache, mirroring the proto cache's
/// bound. Secrets are a session convenience: evicting one only
/// means that poll answers with the honest "cannot vote" error, so
/// arbitrary eviction past the cap is safe (v0.25 perf review:
/// this map used to grow without bound).
pub(super) const POLL_SECRETS_CAP: usize = 1000;

pub(super) fn cap_poll_secrets(secrets: &PollSecrets) {
    let mut map = lock_cache(secrets);
    while map.len() > POLL_SECRETS_CAP {
        match map.keys().next().cloned() {
            Some(key) => {
                map.remove(&key);
            }
            None => break,
        }
    }
}

/// Extracts (creation secret, option names) from a received poll
/// creation message, or None for anything else. The secret is the
/// `message_secret` of the message context info; the option names
/// come from the poll body itself (the vote command refers to
/// options by index into this list).
pub(super) fn received_poll_secret(
    msg: &whatsapp_rust::prelude::wa::Message,
) -> Option<(Vec<u8>, Vec<String>)> {
    let poll = msg
        .poll_creation_message
        .as_option()
        .or_else(|| msg.poll_creation_message_v3.as_option())?;
    let secret = msg
        .message_context_info
        .as_option()
        .and_then(|ctx| ctx.message_secret.clone())
        .filter(|s| !s.is_empty())?;
    let names = poll.options.iter().map(|o| o.option_name.clone().unwrap_or_default()).collect();
    Some((secret, names))
}

/// Locks a session cache, recovering from poisoning instead of
/// panicking: a poisoned cache must not take down message handling.
pub(super) fn lock_cache<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Parses the owned parts of a message reference; the caller builds
/// the borrowing `MessageRef` in its own scope (the reference
/// borrows these parts, so it cannot outlive them).
pub(super) fn build_message_target(
    chat_jid: &str,
    message_id: &str,
    sender_jid: Option<&String>,
    _from_me: bool,
) -> Result<(Jid, Option<Jid>, whatsapp_rust::MessageId), String> {
    let chat = chat_jid.parse::<Jid>().map_err(|e| format!("Invalid chat address: {e}"))?;
    let sender = sender_jid.and_then(|s| s.parse::<Jid>().ok());
    let mid = whatsapp_rust::MessageId::new(message_id)
        .map_err(|e| format!("Invalid message id: {e}"))?;
    Ok((chat, sender, mid))
}

/// Parses a participant address: a full JID passes through, a bare
/// phone number (any formatting) becomes a user JID.
pub(super) fn parse_address(text: &str) -> Result<Jid, String> {
    let text = text.trim();
    if text.contains('@') {
        text.parse::<Jid>().map_err(|e| format!("Invalid address: {e}"))
    } else {
        let digits: String = text.chars().filter(char::is_ascii_digit).collect();
        if digits.len() < 7 {
            return Err(format!("\"{text}\" does not look like a phone number or address."));
        }
        Ok(Jid::pn(&digits))
    }
}

/// Seconds since the Unix epoch (for locally created chat rows).
pub(super) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Keeps a protocol ID safe for use in a file name.
pub(super) fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Peels every wrapper layer (ephemeral, view-once and its v2
/// forms, device-sent, edited, captioned documents) off a protocol
/// message, down to the message that carries the payload fields.
/// The library's `get_base_message` makes one pass in a fixed
/// wrapper order, so a view-once message inside an ephemeral one
/// (or any repeated nesting) survives a single call; repeat until
/// a pass changes nothing.
fn unwrap_media_proto(
    proto: &whatsapp_rust::prelude::wa::Message,
) -> &whatsapp_rust::prelude::wa::Message {
    let mut current = proto.get_base_message();
    for _ in 0..8 {
        let next = current.get_base_message();
        if std::ptr::eq(next, current) {
            break;
        }
        current = next;
    }
    current
}

/// Hard cap on one media download, in plaintext bytes. WhatsApp
/// allows documents up to about 2 GB and the declared length is
/// sender-controlled, so without a cap one hostile or accidental
/// giant file could fill the disk (and, in the old buffered design,
/// the RAM) of the machine running Zaprust. 512 MiB sits far above
/// anything WhatsApp itself produces in practice (its videos are
/// recompressed to tens of megabytes) while keeping a single save
/// bounded.
pub(super) const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Wire allowance on top of the plaintext cap: the ciphertext
/// carries CBC padding and a MAC beyond the plaintext, so the wire
/// budget needs a little slack or a file exactly at the cap would
/// fail on its overhead.
const DOWNLOAD_WIRE_SLACK_BYTES: u64 = 1024 * 1024;

fn download_limits() -> whatsapp_rust::download::DownloadLimits {
    whatsapp_rust::download::DownloadLimits::new(
        MAX_DOWNLOAD_BYTES + DOWNLOAD_WIRE_SLACK_BYTES,
        MAX_DOWNLOAD_BYTES,
    )
}

fn too_large_message() -> String {
    format!(
        "That file is too large to download (the limit is {} MB).",
        MAX_DOWNLOAD_BYTES / 1024 / 1024
    )
}

/// Rejects a download whose declared plaintext length already
/// exceeds the cap, before any network traffic. The declared length
/// is sender-controlled, so this is only a fast path: the library
/// budget from [`download_limits`] enforces the same cap
/// mid-transfer when the declaration understates the real size.
fn check_declared_length(declared: Option<u64>) -> Result<(), String> {
    match declared {
        Some(len) if len > MAX_DOWNLOAD_BYTES => Err(too_large_message()),
        _ => Ok(()),
    }
}

/// The downloadable media inside an unwrapped protocol message,
/// with the file extension for its kind.
fn media_part<'p>(
    proto: &'p whatsapp_rust::prelude::wa::Message,
) -> Option<(&'p (dyn whatsapp_rust::download::Downloadable + 'p), &'static str)> {
    if let Some(m) = proto.image_message.as_option() {
        return Some((m, "jpg"));
    }
    if let Some(m) = proto.video_message.as_option() {
        return Some((m, "mp4"));
    }
    if let Some(m) = proto.audio_message.as_option() {
        return Some((m, "ogg"));
    }
    if let Some(m) = proto.document_message.as_option() {
        return Some((m, "bin"));
    }
    if let Some(m) = proto.sticker_message.as_option() {
        return Some((m, "webp"));
    }
    None
}

/// Maps a library download error to the user-facing message: a
/// budget refusal is the size cap speaking, everything else is a
/// plain failure. A writer-cleanup failure wraps the real cause, so
/// look one level in.
fn download_error_message(error: whatsapp_rust::download::ClientDownloadError) -> String {
    use whatsapp_rust::download::{ClientDownloadError, DownloadPolicyError};
    match error {
        ClientDownloadError::ResourcePolicy(DownloadPolicyError::LimitExceeded { .. }) => {
            too_large_message()
        }
        ClientDownloadError::WriterCleanup { failure, .. } => download_error_message(*failure),
        other => format!("Download failed: {other}"),
    }
}

/// Downloads the media inside a protocol message into a new file
/// named `<stem>.<ext>` inside `dir`, returning the saved path.
/// The library streams the decrypted plaintext straight into the
/// file, so no whole-file buffer ever sits in memory, and the byte
/// budget bounds the transfer even when the declared length lies.
/// On failure the partial file is removed, so a broken download
/// never leaves a file behind that looks complete.
pub(super) async fn download_proto(
    client: &whatsapp_rust::Client,
    proto: &whatsapp_rust::prelude::wa::Message,
    dir: &std::path::Path,
    stem: &str,
) -> Result<std::path::PathBuf, String> {
    // Media inside view-once and ephemeral messages travels wrapped:
    // the payload fields sit on the inner message, so unwrap first or
    // those messages report no downloadable media even though the
    // conversation view shows them fine.
    let proto = unwrap_media_proto(proto);
    let Some((media, ext)) = media_part(proto) else {
        return Err("That message has no downloadable media.".to_owned());
    };
    check_declared_length(media.file_length())?;
    let path = dir.join(format!("{stem}.{ext}"));
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("Could not create the downloads folder: {e}"))?;
    let file = std::fs::File::create(&path)
        .map_err(|e| format!("Could not create the download file: {e}"))?;
    match client.download_to_writer(media, file, download_limits()).await {
        Ok(_file) => Ok(path),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(download_error_message(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_proto() -> wa::Message {
        let mut msg = wa::Message::default();
        msg.image_message = MessageField::some(wa::message::ImageMessage::default());
        msg
    }

    fn wrap_view_once(inner: wa::Message) -> wa::Message {
        let mut wrapper = wa::message::FutureProofMessage::default();
        wrapper.message = MessageField::some(inner);
        let mut msg = wa::Message::default();
        msg.view_once_message = MessageField::some(wrapper);
        msg
    }

    fn wrap_ephemeral(inner: wa::Message) -> wa::Message {
        let mut wrapper = wa::message::FutureProofMessage::default();
        wrapper.message = MessageField::some(inner);
        let mut msg = wa::Message::default();
        msg.ephemeral_message = MessageField::some(wrapper);
        msg
    }

    /// `download_proto` matches the media fields of the base message;
    /// these are the wrapped shapes it must see through, including an
    /// ephemeral chat carrying view-once media.
    #[test]
    fn wrapped_media_unwraps_to_the_payload_message() {
        for wrapped in [
            image_proto(),
            wrap_view_once(image_proto()),
            wrap_ephemeral(image_proto()),
            wrap_ephemeral(wrap_view_once(image_proto())),
        ] {
            assert!(wrapped.get_base_message().image_message.as_option().is_some());
        }
        // Orders and repetitions a single library pass cannot see
        // through: view-once outside ephemeral (the library checks
        // ephemeral first), and the same wrapper twice. The download
        // unwrap loop must still reach the payload.
        for wrapped in [
            wrap_view_once(wrap_ephemeral(image_proto())),
            wrap_ephemeral(wrap_ephemeral(image_proto())),
            wrap_view_once(wrap_view_once(image_proto())),
            wrap_ephemeral(wrap_view_once(wrap_ephemeral(image_proto()))),
        ] {
            assert!(unwrap_media_proto(&wrapped).image_message.as_option().is_some());
        }
        // The wrapped forms hide the payload from a direct field
        // match, which is the failure this guards against.
        assert!(wrap_view_once(image_proto()).image_message.as_option().is_none());
        assert!(wrap_ephemeral(image_proto()).image_message.as_option().is_none());
    }

    /// The size cap's fast path: a declared length over the cap is
    /// refused before any network traffic, with the reason stated;
    /// unknown and at-cap lengths pass (the library budget still
    /// enforces the cap mid-transfer).
    #[test]
    fn declared_length_over_the_cap_is_rejected() {
        assert!(check_declared_length(None).is_ok());
        assert!(check_declared_length(Some(1024)).is_ok());
        assert!(check_declared_length(Some(MAX_DOWNLOAD_BYTES)).is_ok());
        let err = check_declared_length(Some(MAX_DOWNLOAD_BYTES + 1))
            .expect_err("one byte over the cap must be refused");
        assert!(err.contains("too large"), "the refusal says why: {err}");
    }

    /// The declared length the pre-check reads comes from the media
    /// part itself: a document proto declaring an over-cap length
    /// yields a part whose length the check rejects.
    #[test]
    fn media_part_exposes_the_declared_length() {
        let mut doc = wa::message::DocumentMessage::default();
        doc.file_length = Some(MAX_DOWNLOAD_BYTES + 1);
        let mut msg = wa::Message::default();
        msg.document_message = MessageField::some(doc);
        let (media, ext) = media_part(&msg).expect("a document is downloadable media");
        assert_eq!(ext, "bin");
        assert!(check_declared_length(media.file_length()).is_err());
    }
}
