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
/// voting needs the creation secret, and so does decrypting
/// other people's votes. The backend caches it when this session
/// creates a poll AND when a poll creation message arrives,
/// because the secret travels inside the creation message's
/// context info (that is how every WhatsApp client learns it).
/// The app additionally archives the secret with the poll in its
/// encrypted archive and hands it back at the start of later
/// sessions ([`crate::BackendCommand::RegisterPollSecret`]),
/// which is what keeps polls from before the session votable
/// and talliable. A poll whose secret none of those paths
/// supplied answers votes with an honest error and its incoming
/// votes are dropped undecrypted. Secrets are never logged.
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

/// Extracts a poll vote's routing and ciphertext from a protocol
/// message: the poll's message ID (from the vote's creation
/// message key) and the encrypted payload + IV. Returns `None`
/// for anything that is not a poll vote carrying both parts.
/// Decryption itself needs the client (the creator/voter PN/LID
/// aliases live in its mapping cache), so it happens in the
/// session's message handler, not here.
pub(super) fn poll_vote_payload(
    msg: &whatsapp_rust::prelude::wa::Message,
) -> Option<(String, Vec<u8>, Vec<u8>)> {
    let update = msg.poll_update_message.as_option()?;
    let poll_id = update
        .poll_creation_message_key
        .as_option()
        .and_then(|key| key.id.clone())
        .filter(|id| !id.is_empty())?;
    let vote = update.vote.as_option()?;
    let payload = vote.enc_payload.clone().filter(|p| !p.is_empty())?;
    let iv = vote.enc_iv.clone().filter(|iv| !iv.is_empty())?;
    Some((poll_id, payload, iv))
}

/// Maps decrypted vote option hashes back to option indices.
/// Votes reference options by the SHA-256 of the option name
/// (see `wacore::poll::compute_option_hash`); a hash that names
/// no option of this poll is dropped. The result is sorted and
/// deduplicated: it is the voter's current selection set.
pub(super) fn selected_indices(option_names: &[String], hashes: &[Vec<u8>]) -> Vec<usize> {
    let known: Vec<([u8; 32], usize)> = option_names
        .iter()
        .enumerate()
        .map(|(idx, name)| (whatsapp_rust::wacore::poll::compute_option_hash(name), idx))
        .collect();
    let mut out: Vec<usize> = hashes
        .iter()
        .filter_map(|hash| {
            let hash: &[u8; 32] = hash.as_slice().try_into().ok()?;
            known.iter().find(|(known, _)| known == hash).map(|(_, idx)| *idx)
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Locks a session cache, recovering from poisoning instead of
/// panicking: a poisoned cache must not take down message handling.
pub(super) fn lock_cache<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Slot for the session's current background job of one kind: a new
/// job replaces the running one instead of racing it, so results
/// from an older job can never land after (and overwrite) a newer
/// job's. Holds only the abort handle; dropping the join handle
/// detaches the job, and the abort handle is what stops it.
pub(super) type LatestTask = std::sync::Arc<std::sync::Mutex<Option<tokio::task::AbortHandle>>>;

/// Spawns `task` as the slot's current job, aborting the job it
/// replaces. The replaced job is cancelled wherever it is suspended,
/// so whatever it would have emitted or stored never happens.
pub(super) fn spawn_latest(
    slot: &LatestTask,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let handle = tokio::spawn(task);
    let replaced = lock_cache(slot).replace(handle.abort_handle());
    if let Some(replaced) = replaced {
        replaced.abort();
    }
}

/// Aborts the slot's current job, if one is running (session
/// teardown: the job belongs to the dying session and must not
/// deliver its result after the session has ended).
pub(super) fn abort_latest(slot: &LatestTask) {
    if let Some(handle) = lock_cache(slot).take() {
        handle.abort();
    }
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

/// Whether a protocol message carries view-once media, in any of
/// its wrapper forms (v1, v2, v2 extension). The walk cannot ride
/// on `get_base_message`: one library pass peels device-sent,
/// ephemeral, and view-once in a single fixed order, so it can
/// step clean over a view-once layer without ever exposing it as
/// the current message. Instead the wrappers that can enclose a
/// view-once message (device-sent, ephemeral) are descended one
/// layer at a time and every layer is checked. An ephemeral
/// wrapper alone does not make a message view-once: disappearing
/// media that is not view-once stays downloadable and
/// forwardable while it exists.
pub(super) fn proto_is_view_once(proto: &whatsapp_rust::prelude::wa::Message) -> bool {
    let mut current = proto;
    for _ in 0..8 {
        if current.view_once_message.as_option().is_some()
            || current.view_once_message_v2.as_option().is_some()
            || current.view_once_message_v2_extension.as_option().is_some()
        {
            return true;
        }
        let inner = current
            .device_sent_message
            .as_option()
            .and_then(|dsm| dsm.message.as_option())
            .or_else(|| current.ephemeral_message.as_option().and_then(|w| w.message.as_option()));
        let Some(inner) = inner else { return false };
        current = inner;
    }
    false
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

/// Removes the file at `path` on drop unless disarmed. A
/// download can end three ways: success (disarm, keep the file),
/// failure (the error path returns and the drop removes the
/// partial file), and abandonment, where the download task is
/// dropped mid-transfer when the session runtime goes down and
/// no error path ever runs. The guard is what makes the third
/// way end like the second: no partial file that looks complete.
struct PartialFileGuard {
    path: std::path::PathBuf,
    armed: bool,
}

impl Drop for PartialFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Downloads the media inside a protocol message into a new file
/// named `<stem>.<ext>` inside `dir`, returning the saved path.
/// The library streams the decrypted plaintext straight into the
/// file, so no whole-file buffer ever sits in memory, and the byte
/// budget bounds the transfer even when the declared length lies.
/// On failure (or abandonment, see [`PartialFileGuard`]) the
/// partial file is removed, so a broken download never leaves a
/// file behind that looks complete.
pub(super) async fn download_proto(
    client: &whatsapp_rust::Client,
    proto: &whatsapp_rust::prelude::wa::Message,
    dir: &std::path::Path,
    stem: &str,
) -> Result<std::path::PathBuf, String> {
    // View-once media is never written to disk: the sender marked
    // it to be viewed once inside the app, so the save path
    // refuses before any byte moves.
    if proto_is_view_once(proto) {
        return Err("View-once media cannot be saved.".to_owned());
    }
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
    // The file will hold decrypted media in plaintext: it is
    // owner-only from creation (0600 on Unix), not merely
    // tightened by the next startup's hardening pass.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&path).map_err(|e| format!("Could not create the download file: {e}"))?;
    #[cfg(unix)]
    {
        // create+truncate keeps an existing file's mode, so
        // assert 0600 explicitly before any media bytes land.
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut guard = PartialFileGuard { path: path.clone(), armed: true };
    match client.download_to_writer(media, file, download_limits()).await {
        Ok(_file) => {
            guard.armed = false;
            Ok(path)
        }
        Err(e) => Err(download_error_message(e)),
    }
}

/// Extracts the persistable re-download reference of a photo
/// message (see [`crate::PhotoRef`]), or `None` when the message
/// is not a photo or its payload does not carry a complete
/// reference. A reference is complete only when every field the
/// CDN download validates is present and well-formed; a partial
/// one is worthless after the session ends, so it is reported as
/// absent rather than archived in a shape that can only fail.
pub(super) fn photo_ref_of(proto: &whatsapp_rust::prelude::wa::Message) -> Option<crate::PhotoRef> {
    let image = unwrap_media_proto(proto).image_message.as_option()?;
    let direct_path = image.direct_path.clone().filter(|p| !p.is_empty())?;
    let media_key = image.media_key.clone().filter(|k| k.len() == 32)?;
    let file_sha256 = image.file_sha256.clone().filter(|h| h.len() == 32)?;
    let file_enc_sha256 = image.file_enc_sha256.clone().filter(|h| h.len() == 32)?;
    let file_length = image.file_length?;
    Some(crate::PhotoRef { direct_path, media_key, file_sha256, file_enc_sha256, file_length })
}

/// Rebuilds the library's validated download parameters from an
/// archived photo reference. The library re-validates every field
/// (key and digest lengths, encrypted media type), so a reference
/// that was archived complete cannot silently degrade: a failure
/// here means the stored value itself is malformed.
pub(super) fn photo_download_params(
    reference: &crate::PhotoRef,
) -> Result<whatsapp_rust::download::DownloadParams, String> {
    whatsapp_rust::download::DownloadParams::encrypted(
        reference.direct_path.clone(),
        &reference.media_key,
        &reference.file_sha256,
        &reference.file_enc_sha256,
        reference.file_length,
        whatsapp_rust::download::MediaType::Image,
    )
    .map_err(|_| "That photo's stored download reference is not usable.".to_owned())
}

/// Downloads a photo message's bytes into memory for display.
/// Unlike [`download_proto`] nothing is written to disk, and
/// view-once photos are allowed: viewing inside the app is
/// exactly what view-once permits, while saving to disk is what
/// it forbids. The byte budget still applies, so one hostile or
/// accidental giant photo cannot fill the app's memory either.
pub(super) async fn fetch_photo_bytes(
    client: &whatsapp_rust::Client,
    proto: &whatsapp_rust::prelude::wa::Message,
) -> Result<Vec<u8>, String> {
    let proto = unwrap_media_proto(proto);
    let Some(image) = proto.image_message.as_option() else {
        return Err("That message is not a photo.".to_owned());
    };
    check_declared_length(image.file_length)?;
    client.download(image, download_limits()).await.map_err(download_error_message)
}

/// Downloads a photo's bytes into memory from its archived
/// reference, for messages the session's protocol cache no
/// longer holds. Same in-memory discipline as
/// [`fetch_photo_bytes`].
pub(super) async fn fetch_photo_by_ref(
    client: &whatsapp_rust::Client,
    reference: &crate::PhotoRef,
) -> Result<Vec<u8>, String> {
    check_declared_length(Some(reference.file_length))?;
    let params = photo_download_params(reference)?;
    client.download(&params, download_limits()).await.map_err(download_error_message)
}

/// Extracts the persistable re-download reference of a voice
/// message (see [`crate::VoiceRef`]), or `None` when the message
/// is not a voice message or its payload does not carry a
/// complete reference. The completeness rule is the photo one:
/// every field the CDN download validates must be present and
/// well-formed, because a partial reference can only fail later.
pub(super) fn voice_ref_of(proto: &whatsapp_rust::prelude::wa::Message) -> Option<crate::VoiceRef> {
    let audio = unwrap_media_proto(proto).audio_message.as_option()?;
    let direct_path = audio.direct_path.clone().filter(|p| !p.is_empty())?;
    let media_key = audio.media_key.clone().filter(|k| k.len() == 32)?;
    let file_sha256 = audio.file_sha256.clone().filter(|h| h.len() == 32)?;
    let file_enc_sha256 = audio.file_enc_sha256.clone().filter(|h| h.len() == 32)?;
    let file_length = audio.file_length?;
    Some(crate::VoiceRef { direct_path, media_key, file_sha256, file_enc_sha256, file_length })
}

/// Rebuilds the library's validated download parameters from an
/// archived voice reference (see [`photo_download_params`] for
/// why a rebuild failure means the stored value is malformed).
pub(super) fn voice_download_params(
    reference: &crate::VoiceRef,
) -> Result<whatsapp_rust::download::DownloadParams, String> {
    whatsapp_rust::download::DownloadParams::encrypted(
        reference.direct_path.clone(),
        &reference.media_key,
        &reference.file_sha256,
        &reference.file_enc_sha256,
        reference.file_length,
        whatsapp_rust::download::MediaType::Audio,
    )
    .map_err(|_| "That voice note's stored download reference is not usable.".to_owned())
}

/// Downloads a voice message's bytes into memory for playback.
/// Like [`fetch_photo_bytes`] nothing is written to disk: the
/// note is played from memory and no copy of it is left behind.
/// The byte budget still applies.
pub(super) async fn fetch_voice_bytes(
    client: &whatsapp_rust::Client,
    proto: &whatsapp_rust::prelude::wa::Message,
) -> Result<Vec<u8>, String> {
    let proto = unwrap_media_proto(proto);
    let Some(audio) = proto.audio_message.as_option() else {
        return Err("That message is not a voice note.".to_owned());
    };
    check_declared_length(audio.file_length)?;
    client.download(audio, download_limits()).await.map_err(download_error_message)
}

/// Downloads a voice message's bytes into memory from its
/// archived reference, for messages the session's protocol cache
/// no longer holds. Same in-memory discipline as
/// [`fetch_voice_bytes`].
pub(super) async fn fetch_voice_by_ref(
    client: &whatsapp_rust::Client,
    reference: &crate::VoiceRef,
) -> Result<Vec<u8>, String> {
    check_declared_length(Some(reference.file_length))?;
    let params = voice_download_params(reference)?;
    client.download(&params, download_limits()).await.map_err(download_error_message)
}

/// Extracts the persistable re-download reference of a video
/// message (see [`crate::VideoRef`]), or `None` when the message
/// is not a video or its payload does not carry a complete
/// reference. The completeness rule is the photo one: every
/// field the CDN download validates must be present and
/// well-formed, because a partial reference can only fail later.
pub(super) fn video_ref_of(proto: &whatsapp_rust::prelude::wa::Message) -> Option<crate::VideoRef> {
    let video = unwrap_media_proto(proto).video_message.as_option()?;
    let direct_path = video.direct_path.clone().filter(|p| !p.is_empty())?;
    let media_key = video.media_key.clone().filter(|k| k.len() == 32)?;
    let file_sha256 = video.file_sha256.clone().filter(|h| h.len() == 32)?;
    let file_enc_sha256 = video.file_enc_sha256.clone().filter(|h| h.len() == 32)?;
    let file_length = video.file_length?;
    Some(crate::VideoRef { direct_path, media_key, file_sha256, file_enc_sha256, file_length })
}

/// The largest wire thumbnail a video reference will archive.
/// A video thumbnail is a small poster frame (WhatsApp's own
/// are a few kilobytes); anything larger is not a thumbnail
/// worth carrying in the message archive.
const MAX_VIDEO_THUMBNAIL_BYTES: usize = 256 * 1024;

/// Extracts the JPEG thumbnail a video message's payload
/// carries: a real frame of the video, produced by the sender's
/// client. `None` when the message is not a video, carries no
/// thumbnail, or carries one too large to be a thumbnail.
pub(super) fn video_thumbnail_of(proto: &whatsapp_rust::prelude::wa::Message) -> Option<Vec<u8>> {
    let video = unwrap_media_proto(proto).video_message.as_option()?;
    video.jpeg_thumbnail.clone().filter(|t| !t.is_empty() && t.len() <= MAX_VIDEO_THUMBNAIL_BYTES)
}

/// Rebuilds the library's validated download parameters from an
/// archived video reference (see [`photo_download_params`] for
/// why a rebuild failure means the stored value is malformed).
pub(super) fn video_download_params(
    reference: &crate::VideoRef,
) -> Result<whatsapp_rust::download::DownloadParams, String> {
    whatsapp_rust::download::DownloadParams::encrypted(
        reference.direct_path.clone(),
        &reference.media_key,
        &reference.file_sha256,
        &reference.file_enc_sha256,
        reference.file_length,
        whatsapp_rust::download::MediaType::Video,
    )
    .map_err(|_| "That video's stored download reference is not usable.".to_owned())
}

/// Downloads a video message's bytes into memory for playback.
/// Only view-once videos take this path: viewing inside the app
/// is what view-once permits, while writing the bytes to a file
/// is what it forbids (regular videos stream to the cache file
/// instead, see [`fetch_video_to_file`]). The byte budget still
/// applies.
pub(super) async fn fetch_video_bytes(
    client: &whatsapp_rust::Client,
    proto: &whatsapp_rust::prelude::wa::Message,
) -> Result<Vec<u8>, String> {
    let proto = unwrap_media_proto(proto);
    let Some(video) = proto.video_message.as_option() else {
        return Err("That message is not a video.".to_owned());
    };
    check_declared_length(video.file_length)?;
    client.download(video, download_limits()).await.map_err(download_error_message)
}

/// Downloads a view-once video's bytes into memory from its
/// archived reference. Same in-memory discipline as
/// [`fetch_video_bytes`].
pub(super) async fn fetch_video_by_ref(
    client: &whatsapp_rust::Client,
    reference: &crate::VideoRef,
) -> Result<Vec<u8>, String> {
    check_declared_length(Some(reference.file_length))?;
    let params = video_download_params(reference)?;
    client.download(&params, download_limits()).await.map_err(download_error_message)
}

/// Creates the cache file a video download streams into, with
/// the same owner-only discipline as [`download_proto`]'s file:
/// the contents are decrypted media in plaintext.
fn create_cache_file(dir: &std::path::Path, stem: &str) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("Could not create the video cache folder: {e}"))?;
    let path = dir.join(format!("{stem}.mp4"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&path).map_err(|e| format!("Could not create the video file: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

/// Streams a video message's bytes into a new file named
/// `<stem>.mp4` inside `dir`, returning the saved path. The
/// library streams the decrypted plaintext straight into the
/// file, so no whole-file buffer ever sits in memory. On failure
/// (or abandonment, see [`PartialFileGuard`]) the partial file
/// is removed, so a broken download never leaves a file behind
/// that looks complete. Only regular videos take this path;
/// view-once videos are fetched into memory instead.
pub(super) async fn fetch_video_to_file(
    client: &whatsapp_rust::Client,
    proto: &whatsapp_rust::prelude::wa::Message,
    dir: &std::path::Path,
    stem: &str,
) -> Result<std::path::PathBuf, String> {
    let proto = unwrap_media_proto(proto);
    let Some(video) = proto.video_message.as_option() else {
        return Err("That message is not a video.".to_owned());
    };
    check_declared_length(video.file_length)?;
    let path = dir.join(format!("{stem}.mp4"));
    let file = create_cache_file(dir, stem)?;
    let mut guard = PartialFileGuard { path: path.clone(), armed: true };
    match client.download_to_writer(video, file, download_limits()).await {
        Ok(_file) => {
            guard.armed = false;
            Ok(path)
        }
        Err(e) => Err(download_error_message(e)),
    }
}

/// Streams a video's bytes into the cache file from its
/// archived reference, for messages the session's protocol
/// cache no longer holds. Same streaming discipline as
/// [`fetch_video_to_file`].
pub(super) async fn fetch_video_file_by_ref(
    client: &whatsapp_rust::Client,
    reference: &crate::VideoRef,
    dir: &std::path::Path,
    stem: &str,
) -> Result<std::path::PathBuf, String> {
    check_declared_length(Some(reference.file_length))?;
    let params = video_download_params(reference)?;
    let path = dir.join(format!("{stem}.mp4"));
    let file = create_cache_file(dir, stem)?;
    let mut guard = PartialFileGuard { path: path.clone(), armed: true };
    match client.download_to_writer(&params, file, download_limits()).await {
        Ok(_file) => {
            guard.armed = false;
            Ok(path)
        }
        Err(e) => Err(download_error_message(e)),
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

    fn wrap_view_once_v2(inner: wa::Message) -> wa::Message {
        let mut wrapper = wa::message::FutureProofMessage::default();
        wrapper.message = MessageField::some(inner);
        let mut msg = wa::Message::default();
        msg.view_once_message_v2 = MessageField::some(wrapper);
        msg
    }

    /// View-once detection must see the wrapper in every nesting
    /// order the sender can produce, and must not mistake a plain
    /// ephemeral message for view-once (disappearing media stays
    /// saveable and forwardable while it exists).
    #[test]
    fn view_once_detection_sees_through_wrappers() {
        assert!(!proto_is_view_once(&image_proto()));
        assert!(!proto_is_view_once(&wrap_ephemeral(image_proto())));
        assert!(!proto_is_view_once(&wrap_ephemeral(wrap_ephemeral(image_proto()))));
        for wrapped in [
            wrap_view_once(image_proto()),
            wrap_view_once_v2(image_proto()),
            wrap_ephemeral(wrap_view_once(image_proto())),
            wrap_view_once(wrap_ephemeral(image_proto())),
            wrap_ephemeral(wrap_view_once_v2(image_proto())),
            wrap_ephemeral(wrap_ephemeral(wrap_view_once(image_proto()))),
        ] {
            assert!(proto_is_view_once(&wrapped), "a view-once layer was missed");
        }
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

    /// A job replaced via `spawn_latest` must never deliver its
    /// result: this is what stops a slow privacy fetch from an
    /// earlier connect overwriting a newer connect's state.
    #[test]
    fn spawn_latest_discards_the_replaced_jobs_result() {
        let slot: LatestTask = std::sync::Arc::new(std::sync::Mutex::new(None));
        let landed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let landed_in = landed.clone();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime builds")
            .block_on(async move {
                let stale = landed_in.clone();
                spawn_latest(&slot, async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    lock_cache(&stale).push("stale");
                });
                let fresh = landed_in.clone();
                spawn_latest(&slot, async move {
                    lock_cache(&fresh).push("fresh");
                });
                // Long enough for the stale job to have landed, had
                // it not been replaced.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            });
        assert_eq!(*lock_cache(&landed), vec!["fresh"]);
    }

    /// `abort_latest` stops the current job outright (the session
    /// teardown path): its result never lands.
    #[test]
    fn abort_latest_stops_the_current_job() {
        let slot: LatestTask = std::sync::Arc::new(std::sync::Mutex::new(None));
        let landed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let landed_in = landed.clone();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime builds")
            .block_on(async move {
                spawn_latest(&slot, async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    lock_cache(&landed_in).push("late");
                });
                abort_latest(&slot);
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            });
        assert!(lock_cache(&landed).is_empty());
    }

    /// A photo proto carrying a complete set of CDN fields, the
    /// shape a real received photo arrives in.
    fn photo_proto_complete() -> wa::Message {
        let mut image = wa::message::ImageMessage::default();
        image.direct_path = Some("/v/t62.7118-24/photo.enc".to_owned());
        image.media_key = Some(vec![7u8; 32]);
        image.file_sha256 = Some(vec![8u8; 32]);
        image.file_enc_sha256 = Some(vec![9u8; 32]);
        image.file_length = Some(123_456);
        let mut msg = wa::Message::default();
        msg.image_message = MessageField::some(image);
        msg
    }

    /// The photo reference is extracted whole from a complete
    /// payload, including through the view-once and ephemeral
    /// wrappers a photo can arrive inside.
    #[test]
    fn photo_ref_comes_from_a_complete_payload() {
        for proto in [
            photo_proto_complete(),
            wrap_view_once(photo_proto_complete()),
            wrap_ephemeral(photo_proto_complete()),
        ] {
            let reference = photo_ref_of(&proto).expect("a complete photo yields a reference");
            assert_eq!(reference.direct_path, "/v/t62.7118-24/photo.enc");
            assert_eq!(reference.media_key, vec![7u8; 32]);
            assert_eq!(reference.file_sha256, vec![8u8; 32]);
            assert_eq!(reference.file_enc_sha256, vec![9u8; 32]);
            assert_eq!(reference.file_length, 123_456);
        }
    }

    /// Any missing or malformed field makes the reference absent:
    /// a partial reference can never be downloaded from later, so
    /// archiving one would only defer the failure.
    #[test]
    fn photo_ref_rejects_incomplete_payloads() {
        // The bare default image (no CDN fields at all).
        assert!(photo_ref_of(&image_proto()).is_none());
        // One field short: no direct path.
        let mut image = wa::message::ImageMessage::default();
        image.media_key = Some(vec![7u8; 32]);
        image.file_sha256 = Some(vec![8u8; 32]);
        image.file_enc_sha256 = Some(vec![9u8; 32]);
        image.file_length = Some(10);
        let mut msg = wa::Message::default();
        msg.image_message = MessageField::some(image);
        assert!(photo_ref_of(&msg).is_none());
        // A key of the wrong length is malformed, not a reference.
        let mut image = wa::message::ImageMessage::default();
        image.direct_path = Some("/v/t62.7118-24/photo.enc".to_owned());
        image.media_key = Some(vec![1u8; 16]);
        image.file_sha256 = Some(vec![8u8; 32]);
        image.file_enc_sha256 = Some(vec![9u8; 32]);
        image.file_length = Some(10);
        let mut msg = wa::Message::default();
        msg.image_message = MessageField::some(image);
        assert!(photo_ref_of(&msg).is_none());
        // Not a photo at all.
        assert!(photo_ref_of(&wa::Message::default()).is_none());
    }

    /// An archived reference rebuilds validated download
    /// parameters; a malformed one is refused with the user-facing
    /// message instead of reaching the library half-built.
    #[test]
    fn photo_download_params_validate_the_reference() {
        let reference = crate::PhotoRef {
            direct_path: "/v/t62.7118-24/photo.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 123_456,
        };
        assert!(photo_download_params(&reference).is_ok());
        let malformed = crate::PhotoRef { media_key: vec![1u8; 4], ..reference.clone() };
        assert!(photo_download_params(&malformed).is_err());
    }

    fn voice_proto_complete() -> wa::Message {
        let mut audio = wa::message::AudioMessage::default();
        audio.direct_path = Some("/v/t62.7118-24/voice.enc".to_owned());
        audio.media_key = Some(vec![7u8; 32]);
        audio.file_sha256 = Some(vec![8u8; 32]);
        audio.file_enc_sha256 = Some(vec![9u8; 32]);
        audio.file_length = Some(45_678);
        let mut msg = wa::Message::default();
        msg.audio_message = MessageField::some(audio);
        msg
    }

    /// The voice reference is extracted whole from a complete
    /// payload, including through the wrappers a voice note can
    /// arrive inside (an ephemeral chat carrying view-once audio
    /// is the extreme shape).
    #[test]
    fn voice_ref_comes_from_a_complete_payload() {
        for proto in [
            voice_proto_complete(),
            wrap_view_once(voice_proto_complete()),
            wrap_ephemeral(voice_proto_complete()),
        ] {
            let reference = voice_ref_of(&proto).expect("a complete voice note yields a reference");
            assert_eq!(reference.direct_path, "/v/t62.7118-24/voice.enc");
            assert_eq!(reference.media_key, vec![7u8; 32]);
            assert_eq!(reference.file_sha256, vec![8u8; 32]);
            assert_eq!(reference.file_enc_sha256, vec![9u8; 32]);
            assert_eq!(reference.file_length, 45_678);
        }
    }

    /// Any missing or malformed field makes the voice reference
    /// absent, exactly like the photo rule: a partial reference
    /// can never be downloaded from later.
    #[test]
    fn voice_ref_rejects_incomplete_payloads() {
        // The bare default audio message (no CDN fields at all).
        let mut msg = wa::Message::default();
        msg.audio_message = MessageField::some(wa::message::AudioMessage::default());
        assert!(voice_ref_of(&msg).is_none());
        // One field short: no direct path.
        let mut audio = wa::message::AudioMessage::default();
        audio.media_key = Some(vec![7u8; 32]);
        audio.file_sha256 = Some(vec![8u8; 32]);
        audio.file_enc_sha256 = Some(vec![9u8; 32]);
        audio.file_length = Some(10);
        let mut msg = wa::Message::default();
        msg.audio_message = MessageField::some(audio);
        assert!(voice_ref_of(&msg).is_none());
        // A key of the wrong length is malformed, not a reference.
        let mut audio = wa::message::AudioMessage::default();
        audio.direct_path = Some("/v/t62.7118-24/voice.enc".to_owned());
        audio.media_key = Some(vec![1u8; 16]);
        audio.file_sha256 = Some(vec![8u8; 32]);
        audio.file_enc_sha256 = Some(vec![9u8; 32]);
        audio.file_length = Some(10);
        let mut msg = wa::Message::default();
        msg.audio_message = MessageField::some(audio);
        assert!(voice_ref_of(&msg).is_none());
        // A photo is not a voice note, and vice versa.
        assert!(voice_ref_of(&photo_proto_complete()).is_none());
        assert!(photo_ref_of(&voice_proto_complete()).is_none());
        assert!(voice_ref_of(&wa::Message::default()).is_none());
    }

    /// An archived voice reference rebuilds validated download
    /// parameters; a malformed one is refused up front.
    #[test]
    fn voice_download_params_validate_the_reference() {
        let reference = crate::VoiceRef {
            direct_path: "/v/t62.7118-24/voice.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 45_678,
        };
        assert!(voice_download_params(&reference).is_ok());
        let malformed = crate::VoiceRef { media_key: vec![1u8; 4], ..reference.clone() };
        assert!(voice_download_params(&malformed).is_err());
    }

    fn video_proto_complete() -> wa::Message {
        let mut video = wa::message::VideoMessage::default();
        video.direct_path = Some("/v/t62.7118-24/video.enc".to_owned());
        video.media_key = Some(vec![7u8; 32]);
        video.file_sha256 = Some(vec![8u8; 32]);
        video.file_enc_sha256 = Some(vec![9u8; 32]);
        video.file_length = Some(1_234_567);
        video.jpeg_thumbnail = Some(vec![0xFF, 0xD8, 0xFF, 0xE0]);
        let mut msg = wa::Message::default();
        msg.video_message = MessageField::some(video);
        msg
    }

    /// The video reference is extracted whole from a complete
    /// payload, including through the wrappers a video can
    /// arrive inside.
    #[test]
    fn video_ref_comes_from_a_complete_payload() {
        for proto in [
            video_proto_complete(),
            wrap_view_once(video_proto_complete()),
            wrap_ephemeral(video_proto_complete()),
        ] {
            let reference = video_ref_of(&proto).expect("a complete video yields a reference");
            assert_eq!(reference.direct_path, "/v/t62.7118-24/video.enc");
            assert_eq!(reference.media_key, vec![7u8; 32]);
            assert_eq!(reference.file_sha256, vec![8u8; 32]);
            assert_eq!(reference.file_enc_sha256, vec![9u8; 32]);
            assert_eq!(reference.file_length, 1_234_567);
        }
    }

    /// Any missing or malformed field makes the video reference
    /// absent, exactly like the photo rule.
    #[test]
    fn video_ref_rejects_incomplete_payloads() {
        // The bare default video message (no CDN fields at all).
        let mut msg = wa::Message::default();
        msg.video_message = MessageField::some(wa::message::VideoMessage::default());
        assert!(video_ref_of(&msg).is_none());
        // One field short: no file length.
        let mut video = wa::message::VideoMessage::default();
        video.direct_path = Some("/v/t62.7118-24/video.enc".to_owned());
        video.media_key = Some(vec![7u8; 32]);
        video.file_sha256 = Some(vec![8u8; 32]);
        video.file_enc_sha256 = Some(vec![9u8; 32]);
        let mut msg = wa::Message::default();
        msg.video_message = MessageField::some(video);
        assert!(video_ref_of(&msg).is_none());
        // A digest of the wrong length is malformed, not a reference.
        let mut video = wa::message::VideoMessage::default();
        video.direct_path = Some("/v/t62.7118-24/video.enc".to_owned());
        video.media_key = Some(vec![7u8; 32]);
        video.file_sha256 = Some(vec![8u8; 16]);
        video.file_enc_sha256 = Some(vec![9u8; 32]);
        video.file_length = Some(10);
        let mut msg = wa::Message::default();
        msg.video_message = MessageField::some(video);
        assert!(video_ref_of(&msg).is_none());
        // A photo is not a video, and vice versa.
        assert!(video_ref_of(&photo_proto_complete()).is_none());
        assert!(photo_ref_of(&video_proto_complete()).is_none());
        assert!(video_ref_of(&wa::Message::default()).is_none());
    }

    /// The thumbnail is the payload's own JPEG bytes, taken
    /// verbatim; absent, empty and oversized thumbnails yield
    /// none (the video then presents its neutral tile, never a
    /// generated picture).
    #[test]
    fn video_thumbnail_comes_from_the_payload() {
        let thumb = video_thumbnail_of(&video_proto_complete()).expect("a thumbnail is carried");
        assert_eq!(thumb, vec![0xFF, 0xD8, 0xFF, 0xE0]);
        // Through a wrapper, too.
        assert!(video_thumbnail_of(&wrap_view_once(video_proto_complete())).is_some());
        // No thumbnail on the payload.
        let mut video = wa::message::VideoMessage::default();
        video.jpeg_thumbnail = None;
        let mut msg = wa::Message::default();
        msg.video_message = MessageField::some(video);
        assert!(video_thumbnail_of(&msg).is_none());
        // An empty thumbnail is no thumbnail.
        let mut video = wa::message::VideoMessage::default();
        video.jpeg_thumbnail = Some(Vec::new());
        let mut msg = wa::Message::default();
        msg.video_message = MessageField::some(video);
        assert!(video_thumbnail_of(&msg).is_none());
        // Oversized is not a poster frame; it is refused.
        let mut video = wa::message::VideoMessage::default();
        video.jpeg_thumbnail = Some(vec![0u8; MAX_VIDEO_THUMBNAIL_BYTES + 1]);
        let mut msg = wa::Message::default();
        msg.video_message = MessageField::some(video);
        assert!(video_thumbnail_of(&msg).is_none());
        // Not a video at all.
        assert!(video_thumbnail_of(&voice_proto_complete()).is_none());
        assert!(video_thumbnail_of(&wa::Message::default()).is_none());
    }

    /// Builds a wire poll-vote message the way a voter's client
    /// does: the selection's option hashes encrypted under the
    /// poll's creation secret with the library's own crypto.
    fn vote_message(
        poll_id: &str,
        secret: &[u8],
        creator: &str,
        voter: &str,
        selected_names: &[&str],
    ) -> wa::Message {
        use whatsapp_rust::wacore::poll::{compute_option_hash, encrypt_poll_vote_with_secret};
        let hashes: Vec<Vec<u8>> =
            selected_names.iter().map(|name| compute_option_hash(name).to_vec()).collect();
        let (payload, iv) =
            encrypt_poll_vote_with_secret(&hashes, secret, poll_id, creator, voter).unwrap();
        let mut key = wa::MessageKey::default();
        key.id = Some(poll_id.to_owned());
        let mut vote = wa::message::PollEncValue::default();
        vote.enc_payload = Some(payload);
        vote.enc_iv = Some(iv.to_vec());
        let mut update = wa::message::PollUpdateMessage::default();
        update.poll_creation_message_key = MessageField::some(key);
        update.vote = MessageField::some(vote);
        let mut msg = wa::Message::default();
        msg.poll_update_message = MessageField::some(update);
        msg
    }

    /// A real encrypted vote payload extracts, decrypts and maps
    /// back to the selected option indices; a changed vote is a
    /// new payload whose selection replaces the old one wholesale.
    #[test]
    fn poll_vote_payload_round_trips_through_the_library_crypto() {
        use whatsapp_rust::wacore::poll::decrypt_poll_vote_with_secret;
        let secret = [7u8; 32];
        let creator = "10000000001@s.whatsapp.net";
        let voter = "10000000007@s.whatsapp.net";
        let names: Vec<String> = vec!["Alpha".into(), "Beta".into(), "Gamma".into()];

        let msg = vote_message("poll-1", &secret, creator, voter, &["Gamma", "Alpha"]);
        let (poll_id, payload, iv) = poll_vote_payload(&msg).expect("a vote extracts");
        assert_eq!(poll_id, "poll-1");
        let hashes = decrypt_poll_vote_with_secret(
            whatsapp_rust::PollVoteCiphertext { enc_payload: &payload, enc_iv: &iv },
            &secret,
            &poll_id,
            creator,
            voter,
        )
        .expect("the vote decrypts under the creation secret");
        assert_eq!(selected_indices(&names, &hashes), vec![0, 2]);

        // The voter's changed vote: a fresh payload selecting only
        // Beta. Decrypted the same way, it is the replacement set.
        let msg = vote_message("poll-1", &secret, creator, voter, &["Beta"]);
        let (poll_id, payload, iv) = poll_vote_payload(&msg).expect("a vote extracts");
        let hashes = decrypt_poll_vote_with_secret(
            whatsapp_rust::PollVoteCiphertext { enc_payload: &payload, enc_iv: &iv },
            &secret,
            &poll_id,
            creator,
            voter,
        )
        .unwrap();
        assert_eq!(selected_indices(&names, &hashes), vec![1]);

        // Under a different secret the same payload does not open.
        let wrong = [9u8; 32];
        assert!(decrypt_poll_vote_with_secret(
            whatsapp_rust::PollVoteCiphertext { enc_payload: &payload, enc_iv: &iv },
            &wrong,
            &poll_id,
            creator,
            voter,
        )
        .is_err());
    }

    /// Non-votes and incomplete votes extract as nothing.
    #[test]
    fn poll_vote_payload_rejects_non_votes() {
        assert!(poll_vote_payload(&wa::Message::default()).is_none());
        let mut update = wa::message::PollUpdateMessage::default();
        let mut key = wa::MessageKey::default();
        key.id = Some("poll-1".to_owned());
        update.poll_creation_message_key = MessageField::some(key);
        let mut msg = wa::Message::default();
        msg.poll_update_message = MessageField::some(update);
        assert!(poll_vote_payload(&msg).is_none(), "a vote without ciphertext is nothing");
    }

    /// An archived video reference rebuilds validated download
    /// parameters; a malformed one is refused up front.
    #[test]
    fn video_download_params_validate_the_reference() {
        let reference = crate::VideoRef {
            direct_path: "/v/t62.7118-24/video.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 1_234_567,
        };
        assert!(video_download_params(&reference).is_ok());
        let malformed = crate::VideoRef { media_key: vec![1u8; 4], ..reference.clone() };
        assert!(video_download_params(&malformed).is_err());
    }
}
