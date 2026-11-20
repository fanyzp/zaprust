//! Backend, the WhatsApp protocol boundary of Zaprust.
//!
//! The app never talks to a protocol library directly. It talks to
//! this crate through two channels and a small vocabulary of
//! [`BackendCommand`]s and [`BackendEvent`]s expressed in Zaprust's
//! own terms (JIDs as strings, unix timestamps, plain text). No
//! protocol-library type appears in this crate's public API, so the
//! library can be upgraded, or replaced, behind the [`Backend`]
//! trait without the app changing.
//!
//! Two implementations exist:
//!
//! - [`WhatsappBackend`] (feature `whatsapp`, on by default), the
//!   real protocol client, built on the whatsapp-rust library. The
//!   library's `SqliteStore` holds **protocol/session state only**;
//!   the chat archive is the app's own store (the `store` crate).
//! - [`FakeBackend`], a deterministic, in-process script used by
//!   tests, screenshots, and the live-mode demo. First-class, not a
//!   stub: it implements the same contract, including pair codes and
//!   scripted incoming messages.
//!
//! Threading model: a backend runs on its own thread (see
//! [`spawn_backend`]). Commands go in over an mpsc channel; events
//! come out over another. Every emitted event also fires the `wake`
//! callback the app supplies (the egui repaint hook), so the UI never
//! polls.
//!
//! Crash containment: [`spawn_backend`] supervises the worker thread.
//! If the backend panics, or its run returns an error (the startup
//! failure path, see [`Backend::run`]), the supervisor emits
//! [`BackendEvent::Crashed`] and restarts the backend **once** from
//! its factory. A second failure produces a final `Crashed` +
//! [`BackendEvent::Error`] and the worker stops, the UI stays alive
//! and shows a persistent error state.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::Arc;

#[cfg(feature = "calls")]
pub(crate) mod audio;
pub mod fake;
pub mod qr;
#[cfg(feature = "whatsapp")]
mod whatsapp;

pub use fake::{FakeBackend, FAKE_GROUP_JID};
pub use qr::{qr_matrix, QrMatrix};
#[cfg(feature = "whatsapp")]
pub use whatsapp::WhatsappBackend;

/// A wake callback: called after every event is queued, so the UI can
/// request a repaint. Must be cheap and non-blocking.
pub type WakeFn = Arc<dyn Fn() + Send + Sync>;

/// Where a backend keeps its files. All paths derive from the
/// platform data directory (see [`data_dir`]), no OS-specific path
/// is hardcoded anywhere in the workspace.
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// Root data directory for this app (platform data dir).
    pub data_dir: PathBuf,
    /// Account slot, e.g. `"default"`. The session store lives under
    /// `accounts/<account_id>/` inside `data_dir`.
    pub account_id: String,
    /// SQLCipher key for the session store (SEC-F01), the same key
    /// that encrypts the chat archive. Held by the app, which loads
    /// it from the OS keyring; the fake backend ignores it.
    pub db_key: [u8; 32],
}

impl BackendConfig {
    /// The directory holding this account's session database.
    #[must_use]
    pub fn account_dir(&self) -> PathBuf {
        self.data_dir.join("accounts").join(&self.account_id)
    }

    /// The protocol session database path (the library's store).
    #[must_use]
    pub fn session_db_path(&self) -> PathBuf {
        self.account_dir().join("session.db")
    }
}

/// The platform data directory for Zaprust, via the `directories`
/// crate (XDG on Linux, AppData on Windows, Application Support on
/// macOS). Falls back to a `.zaprust` folder under the current
/// directory if the platform reports no data dir.
#[must_use]
pub fn data_dir() -> PathBuf {
    let dir = directories::ProjectDirs::from("", "", "Zaprust")
        .map_or_else(|| PathBuf::from(".zaprust"), |d| d.data_dir().to_path_buf());
    adopt_legacy_data_dir(&dir);
    dir
}

/// The platform data directory from before the rename (the app
/// shipped as Zaplite until 1.0.0). The fallback mirrors
/// [`data_dir`]'s: a `.zaplite` folder under the current directory
/// when the platform reports no data dir.
fn legacy_data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "Zaplite")
        .map_or_else(|| PathBuf::from(".zaplite"), |d| d.data_dir().to_path_buf())
}

/// One-time adoption of the data directory from before the rename.
/// On the first run after the rename the Zaprust directory does
/// not exist yet; if the legacy one does, its contents are copied
/// over, so the session, archive, and settings carry across. The
/// copy is staged in a sibling directory and renamed into place,
/// so an interrupted copy never leaves a half-populated directory
/// that a later run would mistake for a completed adoption. The
/// legacy directory itself stays in place for now: it is removed
/// by [`remove_legacy_data_dir`] once the app has proven it runs
/// from the new directory. Later runs see the Zaprust directory
/// in place and do nothing.
fn adopt_legacy_data_dir(new_dir: &std::path::Path) {
    adopt_legacy_dir(new_dir, &legacy_data_dir());
}

/// The adoption itself, split out from the platform path lookup
/// so tests can drive it with temporary directories.
fn adopt_legacy_dir(new_dir: &std::path::Path, legacy: &std::path::Path) {
    if new_dir.exists() || !legacy.is_dir() {
        return;
    }
    // Stage the copy next to its destination and rename it into
    // place: the rename is atomic on the destination filesystem,
    // so the new directory either appears complete or not at all.
    let Some(parent) = new_dir.parent() else {
        return;
    };
    let Some(name) = new_dir.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let staging = parent.join(format!(".{name}.adopting"));
    let _ = std::fs::remove_dir_all(&staging);
    if copy_dir_contents(legacy, &staging).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        return;
    }
    if std::fs::rename(&staging, new_dir).is_err() && new_dir.exists() {
        // Another starter won the race and adopted first; the
        // staged copy is a duplicate and must not linger either.
        let _ = std::fs::remove_dir_all(&staging);
    }
}

/// Removes the legacy (pre-rename) data directory, once the app
/// has proven it runs from the current one.
///
/// The adoption in [`data_dir`] copies the legacy directory and
/// deliberately leaves the original in place, so a first run that
/// goes wrong loses nothing. But the leftover is a complete second
/// copy of the user's data, and any database in it predates the
/// SQLCipher conversion (SEC-F01): never adopted copies stay in
/// plaintext forever, outside the current directory's lifecycle,
/// permission hardening, and pruning. Callers invoke this only
/// after the current directory's archive has opened successfully,
/// which is the proof that the app runs from the new location.
/// The removal is best-effort and never fails startup; a later
/// start retries whatever it could not do.
///
/// The rails, all checked against canonical paths: nothing happens
/// when there is no legacy directory, when the current directory
/// does not exist (adoption never completed), or when the two
/// resolve to the same place or one inside the other, so the
/// current directory is never deleted, in whole or in part.
pub fn remove_legacy_data_dir() {
    let _ = remove_legacy_dir(&data_dir(), &legacy_data_dir());
}

/// The cleanup itself, split out from the platform path lookup
/// so tests can drive it with temporary directories. Returns
/// whether the legacy directory was removed.
fn remove_legacy_dir(current: &std::path::Path, legacy: &std::path::Path) -> bool {
    if !legacy.is_dir() || !current.is_dir() {
        return false;
    }
    let (Ok(current), Ok(legacy)) = (current.canonicalize(), legacy.canonicalize()) else {
        return false;
    };
    if current.starts_with(&legacy) || legacy.starts_with(&current) {
        return false;
    }
    std::fs::remove_dir_all(&legacy).is_ok()
}

/// The file stem every download of a message's media is saved
/// under in the downloads folder: `<chat user part>-<message
/// id>`, with the id kept filename-safe. This is the one place
/// the naming rule lives: the download path in the WhatsApp
/// backend builds the saved file's name from it, and the
/// removal helpers below match files by it, so a rename here
/// renames both sides of the contract.
#[must_use]
pub fn downloaded_media_stem(chat_jid: &str, message_id: &str) -> String {
    let user = chat_jid.split('@').next().unwrap_or(chat_jid);
    let safe: String = message_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("{user}-{safe}")
}

/// Removes the downloaded media file of one message, if the
/// user ever saved it.
///
/// A saved download is the decrypted media in plaintext, in a
/// file of its own: deleting, clearing or expiring the message
/// in the archive must take the file with it, or the deletion
/// is only cosmetic (privacy audit A3). The file is identified
/// by the naming rule in [`downloaded_media_stem`]; the
/// extension is whatever the media carried, so any file whose
/// name is the stem plus an extension goes. Best-effort:
/// returns whether a file was removed, never errors.
pub fn remove_download_for_message(
    data_dir: &std::path::Path,
    chat_jid: &str,
    message_id: &str,
) -> bool {
    let stem = downloaded_media_stem(chat_jid, message_id);
    remove_downloads_matching(&data_dir.join("downloads"), &format!("{stem}."), true) > 0
}

/// Removes every downloaded media file belonging to a chat,
/// for when the whole chat is deleted (privacy audit A3). The
/// chat prefix is the JID's user part plus the separator dash,
/// so a different chat whose user part merely starts with the
/// same digits is not matched. Best-effort: returns how many
/// files were removed.
pub fn remove_downloads_for_chat(data_dir: &std::path::Path, chat_jid: &str) -> usize {
    let user = chat_jid.split('@').next().unwrap_or(chat_jid);
    remove_downloads_matching(&data_dir.join("downloads"), &format!("{user}-"), false)
}

/// Removes files in `dir` whose names start with `prefix`.
/// With `one`, stops after the first removal (a message has at
/// most one saved file; its extension is the only unknown).
fn remove_downloads_matching(dir: &std::path::Path, prefix: &str, one: bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(prefix)
            && entry.file_type().is_ok_and(|t| t.is_file())
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
            if one {
                break;
            }
        }
    }
    removed
}

/// Recursive file copy used by the legacy data-dir adoption above.
fn copy_dir_contents(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_contents(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// The quoted message a reply points at, carried on outgoing
/// `SendText` commands so the reply context reaches the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotedRef {
    /// Protocol ID of the quoted message.
    pub message_id: String,
    /// JID of the quoted message's author (in groups the original
    /// participant; in direct chats the other party).
    pub sender_jid: Option<String>,
    /// Short text excerpt of the quoted body for the preview.
    pub excerpt: String,
}

/// A command from the UI to the backend. `PartialEq` only: the
/// location command carries degree floats, which have no `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendCommand {
    /// Start (or continue) the QR linking flow. When the account is
    /// already linked this simply reconnects.
    ConnectQr,
    /// Request an 8-character pairing code for a phone number in
    /// international format, digits only (no `+`, no spaces).
    ConnectPairCode {
        /// Phone number, digits only.
        phone: String,
    },
    /// Log the linked device out and stop.
    Disconnect,
    /// Send a plain-text message to a chat.
    SendText {
        /// Chat JID, e.g. `"12345@s.whatsapp.net"` or `"…@g.us"`.
        chat_jid: String,
        /// Message body.
        body: String,
        /// Quote context when this message replies to another.
        quoted: Option<QuotedRef>,
        /// Disappearing-messages timer of the chat, in seconds:
        /// the message carries this expiration on the wire. `0`
        /// sends a normal, non-expiring message.
        ephemeral_secs: u32,
    },
    /// Upload and send a media message to a chat. The backend
    /// encrypts and uploads `data`, builds the typed media message,
    /// and sends it; failures surface as an `Error` event (an empty
    /// payload is rejected up front and never uploaded).
    SendMedia {
        /// Chat JID, e.g. `"12345@s.whatsapp.net"` or `"…@g.us"`.
        chat_jid: String,
        /// Which media kind to send.
        kind: MediaKind,
        /// Raw file bytes (the plaintext; the backend encrypts).
        data: Vec<u8>,
        /// MIME type of `data`, e.g. `"image/webp"`, `"video/mp4"`.
        mime: String,
        /// Caption for photo/video/document (may be empty; ignored
        /// for stickers and voice notes).
        caption: String,
        /// File name for documents.
        file_name: Option<String>,
        /// Send a video as a looping GIF-style clip.
        gif_playback: bool,
        /// Duration in seconds for video/voice (0 when unknown).
        seconds: u32,
        /// Wrap the media in a view-once wrapper (photos and
        /// videos only; ignored for other kinds).
        view_once: bool,
        /// Disappearing-messages timer of the chat, in seconds:
        /// the message carries this expiration on the wire. `0`
        /// sends a normal, non-expiring message.
        ephemeral_secs: u32,
    },
    /// Set a chat's disappearing-messages timer (`0` turns it
    /// off). Direct chats and groups take different protocol
    /// paths; the backend picks the right one from the JID. The
    /// result comes back as a [`BackendEvent::DisappearingChanged`]
    /// echo, and remote changes arrive through the same event.
    SetDisappearingTimer {
        /// Chat JID.
        chat_jid: String,
        /// New timer in seconds (`0`, `86400`, `604800` or
        /// `7776000`).
        duration_secs: u32,
    },
    /// Send a location message to a chat.
    SendLocation {
        /// Chat JID.
        chat_jid: String,
        /// Display name of the place (may be empty).
        name: String,
        /// Latitude in degrees.
        latitude: f64,
        /// Longitude in degrees.
        longitude: f64,
        /// Disappearing-messages timer of the chat, in seconds
        /// (`0` sends a normal, non-expiring message).
        ephemeral_secs: u32,
    },
    /// Start an outgoing 1:1 call to a chat. `video` asks for a
    /// video call; backends without video wiring report that
    /// honestly instead of silently placing a voice call.
    StartCall {
        /// Chat JID of the person to call.
        chat_jid: String,
        /// Whether this is a video call.
        video: bool,
    },
    /// Accept the currently ringing incoming call.
    AcceptCall,
    /// Reject the currently ringing incoming call.
    RejectCall,
    /// End the current call (ringing or active).
    EndCall,
    /// Mute or unmute the microphone in the current call.
    SetCallMuted {
        /// True to mute the microphone.
        muted: bool,
    },
    /// Route call audio to the speaker (alternate output device)
    /// or back to the default output.
    SetCallSpeaker {
        /// True for the alternate (speaker) output device.
        speaker: bool,
    },
    /// Create a poll in a chat.
    CreatePoll {
        /// Chat JID.
        chat_jid: String,
        /// Poll question.
        question: String,
        /// Option labels (2–12).
        options: Vec<String>,
        /// Whether several options may be selected.
        multiple: bool,
    },
    /// Vote in a poll (replaces my previous vote set).
    VotePoll {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the poll message.
        message_id: String,
        /// Selected option indices.
        options: Vec<usize>,
    },
    /// Set (or clear, with an empty string) my reaction to a message.
    React {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the target message.
        message_id: String,
        /// Sender JID of the target message (group participant), if any.
        sender_jid: Option<String>,
        /// Whether the target message is my own.
        from_me: bool,
        /// The emoji, or empty to remove my reaction.
        emoji: String,
    },
    /// Mark incoming messages as read (blue ticks for the sender).
    /// The app sends this only when the read-receipts privacy
    /// toggle is ON.
    MarkAsRead {
        /// Chat JID.
        chat_jid: String,
        /// Sender JID (group participant), if any.
        sender_jid: Option<String>,
        /// Protocol IDs of the messages to mark.
        message_ids: Vec<String>,
    },
    /// Send (or stop) a "typing…" chat-state for a chat. The app
    /// sends this only when the typing-indicators toggle is ON.
    SetTyping {
        /// Chat JID.
        chat_jid: String,
        /// True = composing, false = paused.
        typing: bool,
    },
    /// Edit one of my own messages.
    EditMessage {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message to edit.
        message_id: String,
        /// Replacement body.
        new_body: String,
    },
    /// Delete one of my own messages for everyone.
    RevokeMessage {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message to revoke.
        message_id: String,
    },
    /// Set a WhatsApp privacy option (live mode; the values are the
    /// protocol spellings: `all`, `contacts`, `none`,
    /// `contact_blacklist`, `match_last_seen`, `known`). The
    /// backend validates the value against the category and
    /// refuses pairs the category does not accept, and refuses
    /// `contact_blacklist` outright: it names an exception list
    /// this command cannot carry.
    SetPrivacy {
        /// One of: `last`, `online`, `profile`, `status`,
        /// `groupadd`, `readreceipts`.
        category: String,
        /// The privacy value spelling.
        value: String,
    },
    /// Forward a message to another chat. The real backend can
    /// only forward messages it saw during this session (it needs
    /// the original protocol payload); the fake backend forwards
    /// from its scripted set. View-once media is refused by both
    /// backends with an `Error` event: media the sender marked
    /// view-once is never relayed.
    ForwardMessage {
        /// Source chat JID.
        from_chat_jid: String,
        /// Protocol ID of the message to forward.
        message_id: String,
        /// Target chat JID (a single chat; there is no multi-target
        /// forward by design).
        to_chat_jid: String,
    },
    /// Star or unstar a message (chat-actions app-state sync).
    StarMessage {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// Sender JID of the message (group participant), if any.
        sender_jid: Option<String>,
        /// Whether the message is my own.
        from_me: bool,
        /// True to star, false to unstar.
        star: bool,
    },
    /// Pin or unpin a message in its chat (7-day pin when pinning).
    PinMessage {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// Sender JID of the message (group participant), if any.
        sender_jid: Option<String>,
        /// Whether the message is my own.
        from_me: bool,
        /// True to pin, false to unpin.
        pin: bool,
    },
    /// Apply a chat-level action (archive, pin, mute...).
    ChatAction {
        /// Chat JID.
        chat_jid: String,
        /// Which action.
        action: ChatAction,
    },
    /// Block or unblock a contact (DM chats).
    BlockContact {
        /// Contact JID.
        chat_jid: String,
        /// True to block, false to unblock.
        block: bool,
    },
    /// Create a group with the given name and participant JIDs (or
    /// bare phone numbers, which the backend normalises).
    CreateGroup {
        /// Group name (subject).
        name: String,
        /// Participant JIDs or phone numbers.
        participants: Vec<String>,
    },
    /// Add, remove, promote, or demote group participants.
    GroupParticipants {
        /// Group JID.
        chat_jid: String,
        /// Participant JIDs or phone numbers.
        participants: Vec<String>,
        /// Which operation.
        op: ParticipantOp,
    },
    /// Set a group's subject and/or description.
    SetGroupInfo {
        /// Group JID.
        chat_jid: String,
        /// New subject, when changing it.
        subject: Option<String>,
        /// New description, when changing it.
        description: Option<String>,
    },
    /// Fetch a group's member list (JIDs + admin flags) for the
    /// profile panel. Answered with a `GroupMembers` event.
    FetchGroupMembers {
        /// Group JID.
        chat_jid: String,
    },
    /// Fetch a group's invite link, optionally resetting it first
    /// (a reset invalidates the previous link). Answered with a
    /// `GroupInviteLink` event.
    FetchGroupInviteLink {
        /// Group JID.
        chat_jid: String,
        /// True to mint a fresh link, invalidating the old one.
        reset: bool,
    },
    /// Turn admin approval for new members on or off for a group.
    /// Answered with a `GroupApprovalState` event.
    SetMembershipApproval {
        /// Group JID.
        chat_jid: String,
        /// True when joining needs an admin's approval.
        enabled: bool,
    },
    /// Fetch a group's pending membership requests. Answered
    /// with a `JoinRequests` event.
    FetchJoinRequests {
        /// Group JID.
        chat_jid: String,
    },
    /// Approve or reject one pending membership request. The
    /// refreshed list is answered with a `JoinRequests` event.
    ReviewJoinRequest {
        /// Group JID.
        chat_jid: String,
        /// The requester's JID or phone number.
        participant: String,
        /// True to approve, false to reject.
        approve: bool,
    },
    /// Set the account-wide default disappearing-messages timer
    /// applied to new chats (0 disables). Fire-and-forget like
    /// the privacy settings; failures surface as `Error` events.
    SetDefaultDisappearingTimer {
        /// Timer in seconds; 0 is off.
        duration_secs: u32,
    },
    /// Set a group's announcement mode (only admins can send).
    /// Answered with a `GroupModesState` event.
    SetGroupAnnounce {
        /// Group JID.
        chat_jid: String,
        /// True for announcement mode.
        announce: bool,
    },
    /// Set a group's locked mode (only admins can edit the
    /// group info). Answered with a `GroupModesState` event.
    SetGroupLocked {
        /// Group JID.
        chat_jid: String,
        /// True for locked mode.
        locked: bool,
    },
    /// Join a group from an invite link or bare invite code.
    /// Answered with a `ChatUpsert` for the joined group.
    JoinGroupByLink {
        /// The full invite link or just its code.
        link: String,
    },
    /// Leave a group.
    LeaveGroup {
        /// Group JID.
        chat_jid: String,
    },
    /// Download a media message's bytes to the downloads folder
    /// under the app data dir. Answered with `MediaSaved` or an
    /// `Error` event. View-once media is refused with an `Error`
    /// event and nothing is written: it is viewed inside the app
    /// only, never saved to disk.
    DownloadMedia {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the media message.
        message_id: String,
    },
    /// Stop the backend worker.
    Shutdown,
}

/// The kind of rich content a live message carries.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LiveMessageKind {
    /// Plain text (body in [`LiveMessage::body`]).
    Text,
    /// A poll.
    Poll {
        /// Poll question.
        question: String,
        /// (label, votes) pairs.
        options: Vec<(String, u32)>,
        /// Whether several options may be selected.
        multiple: bool,
        /// Indices I voted for.
        my_votes: Vec<usize>,
        /// Quiz: the correct option index, when known.
        correct: Option<usize>,
    },
    /// A shared location.
    Location {
        /// Place name / address label.
        name: String,
        /// Latitude.
        latitude: f64,
        /// Longitude.
        longitude: f64,
    },
    /// A shared contact card.
    Contact {
        /// Contact display name.
        name: String,
        /// Phone/JID label.
        phone: String,
    },
    /// A received carousel template (render-only).
    Carousel {
        /// Body text above the cards.
        body: String,
        /// The cards.
        cards: Vec<LiveCard>,
    },
    /// A media message (photo, video, voice, document, sticker).
    /// The bytes are not carried; they can be fetched on demand
    /// through the download command while the session lasts.
    Media {
        /// Which media kind.
        media: MediaKind,
        /// Caption or file name (may be empty).
        caption: String,
        /// Duration in seconds for video/voice (0 otherwise).
        seconds: u32,
        /// Announced size in bytes (0 when unknown).
        size_bytes: u64,
    },
    /// Another media type, summarized by its label (e.g. `"[photo]"`).
    Other(String),
}

/// Media kinds carried by [`LiveMessageKind::Media`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MediaKind {
    /// Still image.
    Photo,
    /// Video clip.
    Video,
    /// Voice note / audio.
    Voice,
    /// File/document.
    Document,
    /// Sticker.
    Sticker,
}

/// Lifecycle of a voice/video call, in the terms the app renders.
/// The real backend maps the protocol library's richer phase and
/// event set onto these four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPhase {
    /// Outgoing: ringing at the peer.
    Ringing,
    /// Accepted, media path still being set up.
    Connecting,
    /// Media flowing; the duration counter runs.
    Active,
    /// Over. The event's reason says how it ended.
    Ended,
}

/// One card of a live carousel message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveCard {
    /// Card title.
    pub title: String,
    /// Card body.
    pub body: String,
    /// Image description, when present.
    pub image: Option<String>,
    /// Button labels (static).
    pub buttons: Vec<String>,
}

/// An OpenGraph preview attached to a live message. For real backends
/// it is read from the preview data embedded in the wire message
/// itself; the app never fetches preview data from the network. The
/// fake backend scripts it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LivePreview {
    /// Page title.
    pub title: String,
    /// Page description.
    pub description: String,
    /// The URL.
    pub url: String,
}

/// Receipt kinds surfaced to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptKind {
    /// Server accepted the message (single tick).
    Sent,
    /// Delivered to the recipient's device (double grey tick).
    Delivered,
    /// Read by the recipient (double blue tick).
    Read,
    /// Voice/video message played.
    Played,
}

/// Which kind of conversation a live chat is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveChatKind {
    /// One-to-one conversation.
    Dm,
    /// A group.
    Group,
    /// A channel / newsletter.
    Channel,
}

/// A chat as the backend reports it (protocol-neutral).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveChat {
    /// Chat JID (stable identity).
    pub jid: String,
    /// Display title.
    pub title: String,
    /// Conversation kind.
    pub kind: LiveChatKind,
    /// Preview of the latest message.
    pub preview: String,
    /// Who sent the preview message (groups), if known.
    pub preview_sender: Option<String>,
    /// Unix timestamp (seconds) of the latest message.
    pub timestamp: i64,
    /// Unread count as reported by the protocol. Like the
    /// timer, a message-arrival upsert does not learn it from
    /// the wire and reports 0, so the app keeps a known count
    /// across upserts that report 0; the app's message upserts
    /// own the increments, and read state changes made on the
    /// server arrive as [`BackendEvent::ChatReadChanged`]
    /// instead.
    pub unread: u32,
    /// Archived (chat-actions state; the app mirrors it
    /// locally). Like mute, a message-arrival upsert does not
    /// learn it from the wire, so the app keeps a known archive
    /// state across upserts that report `false`; archive changes
    /// made on the server arrive as
    /// [`BackendEvent::ArchiveChanged`] instead.
    pub archived: bool,
    /// Pinned to the top of the chat list. Like archive, a
    /// message-arrival upsert does not learn it from the wire,
    /// so the app keeps a known pin across upserts that report
    /// `false`; pin changes made on the server arrive as
    /// [`BackendEvent::PinChanged`] instead.
    pub pinned: bool,
    /// Disappearing-messages timer in seconds (`0` is off), as
    /// persisted in the archive. Message-arrival upserts do not
    /// learn the timer from the wire, so the app treats this
    /// field as authoritative only from the archive and from
    /// [`BackendEvent::DisappearingChanged`].
    pub ephemeral_secs: u32,
    /// Muted (notifications silenced), as persisted in the
    /// archive. Like the timer, a message-arrival upsert does
    /// not learn it from the wire, so the app keeps a known
    /// mute across upserts that report `false`; mute changes
    /// made on the server arrive as
    /// [`BackendEvent::MuteChanged`] instead.
    pub muted: bool,
}

/// A chat-level action from the chat-actions set (app-state sync).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatAction {
    /// Move the chat to the archive.
    Archive,
    /// Move the chat back to the main list.
    Unarchive,
    /// Pin the chat to the top of the list.
    Pin,
    /// Unpin the chat.
    Unpin,
    /// Mute notifications indefinitely.
    Mute,
    /// Unmute notifications.
    Unmute,
}

/// A group participant operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipantOp {
    /// Add participants.
    Add,
    /// Remove participants.
    Remove,
    /// Promote participants to admin.
    Promote,
    /// Demote participants from admin.
    Demote,
}

/// A message as the backend reports it (protocol-neutral).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveMessage {
    /// Protocol message ID (stable within its chat).
    pub id: String,
    /// Chat JID the message belongs to.
    pub chat_jid: String,
    /// Sender display name; `None` means "me".
    pub sender: Option<String>,
    /// Plain-text body. Non-text messages are summarized by the
    /// backend (e.g. `"[photo]"`) in this phase.
    pub body: String,
    /// Unix timestamp (seconds).
    pub timestamp: i64,
    /// True when this device/account sent the message.
    pub from_me: bool,
    /// Rich content kind (text by default).
    pub kind: LiveMessageKind,
    /// (emoji, count) reaction summary.
    pub reactions: Vec<(String, u32)>,
    /// My own reaction, if any.
    pub my_reaction: Option<String>,
    /// Quoted message sender name, when this message quotes another.
    pub quoted_sender: Option<String>,
    /// Quoted message excerpt.
    pub quoted_body: Option<String>,
    /// Link preview the sender's client attached to the message,
    /// when the wire carried one. Never fetched by this client.
    pub link_preview: Option<LivePreview>,
    /// Whether the message was edited.
    pub edited: bool,
    /// The message arrived as a forward.
    pub forwarded: bool,
    /// Starred by me.
    pub starred: bool,
    /// Pinned in the chat by someone.
    pub pinned: bool,
    /// View-once media: render once, then it is gone.
    pub view_once: bool,
    /// JID of the message author (for reply context and group
    /// reactions; a display name is in `sender`).
    pub sender_jid: Option<String>,
    /// Unix second at which this message disappears, computed by
    /// the backend from the message timestamp plus the ephemeral
    /// expiration the wire carried. `0` means it never expires.
    pub expires_at: i64,
}

impl LiveMessage {
    /// A plain-text live message with no rich payload.
    #[must_use]
    pub fn text(
        id: impl Into<String>,
        chat_jid: impl Into<String>,
        sender: Option<String>,
        body: impl Into<String>,
        timestamp: i64,
        from_me: bool,
    ) -> Self {
        Self {
            id: id.into(),
            chat_jid: chat_jid.into(),
            sender,
            body: body.into(),
            timestamp,
            from_me,
            kind: LiveMessageKind::Text,
            reactions: Vec::new(),
            my_reaction: None,
            quoted_sender: None,
            quoted_body: None,
            link_preview: None,
            edited: false,
            forwarded: false,
            starred: false,
            pinned: false,
            view_once: false,
            sender_jid: None,
            expires_at: 0,
        }
    }
}

/// An event from the backend to the UI.
#[derive(Debug, Clone, PartialEq)]
// The PrivacyState payload (a settings map) makes this enum
// large, but events are infrequent and channel-passed by value
// deliberately, boxing the hot variants (messages) would tax
// every message for a rare settings event.
#[allow(clippy::large_enum_variant)]
pub enum BackendEvent {
    /// A QR payload to render (replaces the previous one, payloads
    /// rotate while waiting for a scan).
    QrCode {
        /// The raw QR payload string.
        payload: String,
    },
    /// A pairing code to show the user (8 characters).
    PairCode {
        /// The pairing code.
        code: String,
    },
    /// The account is linked and connected.
    Connected {
        /// Human-readable account label (never a phone number in UI
        /// copy beyond what the protocol itself reports).
        account: String,
    },
    /// The device was logged out (by the phone or the server).
    LoggedOut,
    /// The account is temporarily banned by the server.
    TemporaryBan {
        /// Server ban reason code.
        code: u32,
    },
    /// A chat appeared or changed.
    ChatUpsert(LiveChat),
    /// A message appeared or changed.
    MessageUpsert(LiveMessage),
    /// A message this client sent was accepted by the server, with
    /// the protocol ID the library assigned it. The UI binds its
    /// pending local echo to this ID, so the backend's own upsert
    /// and later receipts find the message directly instead of
    /// guessing by content.
    MessageSent {
        /// Chat JID the message went to.
        chat_jid: String,
        /// Protocol ID assigned to the sent message.
        message_id: String,
    },
    /// A delivery/read receipt for outgoing messages.
    Receipt {
        /// Chat JID.
        chat_jid: String,
        /// Protocol IDs the receipt covers.
        message_ids: Vec<String>,
        /// Receipt kind.
        kind: ReceiptKind,
        /// Unix timestamp (seconds) of the receipt.
        at: i64,
    },
    /// Someone reacted to a message (full per-sender mapping is the
    /// app's job; this carries one actor's current emoji).
    ReactionUpdate {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the target message.
        message_id: String,
        /// Who reacted (display name or JID user part).
        actor: String,
        /// Their current emoji; empty means removed.
        emoji: String,
    },
    /// A message was edited by its sender.
    MessageEdited {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the edited message.
        message_id: String,
        /// Replacement body.
        new_body: String,
    },
    /// A message was deleted for everyone by its sender.
    MessageRevoked {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the revoked message.
        message_id: String,
    },
    /// A chat's disappearing-messages timer changed: set from
    /// this client (the echo of
    /// [`BackendCommand::SetDisappearingTimer`]) or by another
    /// participant / device. `duration_secs` is `0` for off.
    DisappearingChanged {
        /// Chat JID.
        chat_jid: String,
        /// New timer in seconds (`0` is off).
        duration_secs: u32,
    },
    /// The account-wide default disappearing-messages timer
    /// changed on another device (the phone included): the
    /// server pushes it as an account sync notification, and the
    /// app adopts it the way it adopts a fetched privacy setting.
    /// There is no fetch for this value; this event is the only
    /// reconciliation channel, so a default changed while this
    /// client was offline is adopted when the sync delivers it.
    DefaultDisappearingTimerChanged {
        /// New default timer in seconds (`0` is off).
        duration_secs: u32,
    },
    /// A chat's mute state changed on the server: muted or
    /// unmuted from another device, or the account state as a
    /// full sync reports it. This is the authoritative path for
    /// mute changes made elsewhere; a mute set from this client
    /// goes out as [`BackendCommand::ChatAction`] and applies
    /// locally at once, without waiting for this event.
    MuteChanged {
        /// Chat JID.
        chat_jid: String,
        /// Whether the chat is muted now. A timed mute whose
        /// end has passed reports `false`.
        muted: bool,
    },
    /// A chat's pin state changed on the server: pinned or
    /// unpinned from another device, or the account state as a
    /// full sync reports it. This is the authoritative path for
    /// pin changes made elsewhere; a pin set from this client
    /// goes out as [`BackendCommand::ChatAction`] and applies
    /// locally at once, without waiting for this event.
    PinChanged {
        /// Chat JID.
        chat_jid: String,
        /// Whether the chat is pinned now.
        pinned: bool,
    },
    /// A chat's archive state changed on the server: archived
    /// or unarchived from another device, or the account state
    /// as a full sync reports it. This is the authoritative
    /// path for archive changes made elsewhere; an archive set
    /// from this client goes out as
    /// [`BackendCommand::ChatAction`] and applies locally at
    /// once, without waiting for this event.
    ArchiveChanged {
        /// Chat JID.
        chat_jid: String,
        /// Whether the chat is archived now.
        archived: bool,
    },
    /// A message's star state changed on the server: starred or
    /// unstarred from another device, or the account state as a
    /// full sync reports it. This is the authoritative path for
    /// star changes made elsewhere; a star set from this client
    /// goes out as [`BackendCommand::StarMessage`] and applies
    /// locally at once, without waiting for this event.
    StarChanged {
        /// Chat JID the message belongs to.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// Whether the message is starred now.
        starred: bool,
    },
    /// A chat's read state changed on the server: marked read or
    /// marked unread from another device, or the account state
    /// as a full sync reports it. This is state adoption only:
    /// the device that made the change already sent any read
    /// receipts, so the app must never answer this event with
    /// [`BackendCommand::MarkAsRead`]. A read set from this
    /// client goes out as that command (the receipt path) and
    /// clears the badge locally at once.
    ChatReadChanged {
        /// Chat JID.
        chat_jid: String,
        /// Whether the chat is read now.
        read: bool,
        /// Unread count to adopt when `read` is false: the
        /// number of messages the sync action's message range
        /// lists as unread (0 when it lists none). Ignored when
        /// `read` is true; the count is 0 then.
        unread_count: u32,
    },
    /// A chat was deleted on the server (deleted on the phone).
    /// The app removes the chat from its list, its session
    /// mirror and its archive. There is no local counterpart:
    /// this client has no delete-chat action of its own, so
    /// nothing here echoes back to the backend.
    ChatDeleted {
        /// Chat JID.
        chat_jid: String,
    },
    /// A chat's messages were cleared on the server (cleared
    /// on the phone; the chat itself stays in the list). The
    /// app removes the cleared messages from its session
    /// mirror and its archive; the chat, its list position
    /// and its settings are untouched. There is no local
    /// counterpart: this client has no clear-chat action of
    /// its own, so nothing here echoes back to the backend.
    ChatCleared {
        /// Chat JID.
        chat_jid: String,
        /// Whether the clear deleted starred messages too
        /// (the clearing device's choice). When this is
        /// false, starred messages survive the clear.
        delete_starred: bool,
        /// The clear's bound, from the sync action's message
        /// range: messages stamped at or before this Unix
        /// timestamp (seconds) are cleared; later messages
        /// are not the clear's to remove. `None` when the
        /// action carries no bound; the clear then covers
        /// the whole chat.
        last_message_timestamp: Option<i64>,
    },
    /// One message was deleted for the account on the server
    /// (deleted with "delete for me" on the phone): it is
    /// gone from the account's own view but was not revoked
    /// for anyone else. The app removes it from its session
    /// mirror and its archive. There is no local counterpart:
    /// this client has no delete-for-me action of its own, so
    /// nothing here echoes back to the backend.
    MessageDeletedForMe {
        /// Chat JID the message belongs to.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
    },
    /// Current privacy settings as fetched from the account
    /// (category → value spellings; see [`BackendCommand::SetPrivacy`]).
    PrivacyState(std::collections::BTreeMap<String, String>),
    /// The account's blocklist as fetched at connect: the JIDs
    /// blocked on the server, in the server's own spelling. The
    /// app adopts the list as its blocked set, replacing any
    /// locally persisted copy: the server is the source of truth
    /// for blocks, and a client that only remembers its own
    /// toggles shows the wrong state after a restart.
    Blocklist(Vec<String>),
    /// An incoming 1:1 call is ringing. Accept or reject it with
    /// the matching commands; only one call exists at a time.
    IncomingCall {
        /// Call identifier (protocol call id).
        call_id: String,
        /// Chat JID of the caller (the 1:1 chat with them).
        chat_jid: String,
        /// Whether the offer asks for video.
        video: bool,
    },
    /// The current call changed phase. While `Active`, the
    /// backend re-emits this roughly once a second with the
    /// running duration in `seconds`.
    CallChanged {
        /// Call identifier.
        call_id: String,
        /// Chat JID the call belongs to.
        chat_jid: String,
        /// New phase.
        phase: CallPhase,
        /// Talk duration in seconds (0 until Active).
        seconds: u32,
        /// For `Ended`: how it ended, in user words. None while
        /// the call is live.
        reason: Option<String>,
    },
    /// A media download finished; the file is on disk.
    MediaSaved {
        /// Chat JID the media belongs to.
        chat_jid: String,
        /// Protocol ID of the media message.
        message_id: String,
        /// Absolute path of the saved file.
        path: String,
    },
    /// A chat partner started or stopped typing (or recording).
    TypingReceived {
        /// Chat JID.
        chat_jid: String,
        /// True while typing/recording, false when idle.
        typing: bool,
    },
    /// A group's member list, answering `FetchGroupMembers`.
    GroupMembers {
        /// Group JID.
        chat_jid: String,
        /// (jid, display name, is admin) rows. The display name
        /// is the name the server rendered for the participant,
        /// or empty when it rendered none; the host then resolves
        /// a label from its own data or masks the identifier. It
        /// is never the raw phone number or LID.
        members: Vec<(String, String, bool)>,
    },
    /// A group's invite link, answering `FetchGroupInviteLink`.
    GroupInviteLink {
        /// Group JID.
        chat_jid: String,
        /// The full invite link.
        link: String,
    },
    /// Whether a group requires admin approval for new members,
    /// answering `FetchGroupMembers` and `SetMembershipApproval`.
    GroupApprovalState {
        /// Group JID.
        chat_jid: String,
        /// True when joining needs an admin's approval.
        enabled: bool,
    },
    /// A group's announcement and locked modes, answering
    /// `FetchGroupMembers`, `SetGroupAnnounce` and
    /// `SetGroupLocked`.
    GroupModesState {
        /// Group JID.
        chat_jid: String,
        /// True when only admins can send messages.
        announce: bool,
        /// True when only admins can edit the group info.
        locked: bool,
    },
    /// A group's pending membership requests, answering
    /// `FetchJoinRequests` and `ReviewJoinRequest`.
    JoinRequests {
        /// Group JID.
        chat_jid: String,
        /// (jid, display name) rows. The protocol carries no
        /// name for a requester, so the real backend leaves the
        /// display name empty and the host resolves a label from
        /// its own data or masks the identifier; it is never the
        /// raw phone number or LID.
        requests: Vec<(String, String)>,
    },
    /// The connection dropped (the backend will try to reconnect).
    ConnectionLost {
        /// Short human-readable reason.
        reason: String,
    },
    /// The backend worker panicked and was contained by the
    /// supervisor. `restarted` says whether the single automatic
    /// restart was attempted after this crash.
    Crashed {
        /// Short failure summary: the panic message, or the
        /// reason a run returned when it died before or outside a
        /// session. Scrubbed by the supervisor before emission
        /// with the crash reports' display scrubber (secret
        /// shapes and phone-like numbers masked, the user's home
        /// path replaced with `~`), because the app renders it
        /// verbatim in the connection status.
        summary: String,
        /// Whether the supervisor restarted the backend afterwards.
        restarted: bool,
    },
    /// A non-fatal error the user should see.
    Error {
        /// Short human-readable message.
        message: String,
    },
}

/// Capacity of the event channel between a backend and the UI.
///
/// The channel is bounded so a UI that stops draining (a hung
/// frame, a stalled modal) cannot accumulate events, each carrying
/// full message bodies, in memory for the life of the process.
/// The capacity is sized against the two rates it sits between:
/// the library's ordered delivery queue holds 4096 events and one
/// incoming message becomes two or more backend events, so 8192
/// absorbs a full reconnect backlog flush without the producer
/// ever waiting in practice; the UI drains up to 256 events per
/// frame into its own queue, emptying a full channel in well
/// under a second of frames. Past the capacity, `emit` blocks:
/// backpressure paces the backend (and, behind it, the library's
/// delivery worker) instead of dropping anything. No event kind
/// is droppable (message upserts carry chat content, the *Changed
/// events carry state assertions, receipts carry read state), so
/// blocking is the only honest overflow behavior.
const EVENT_CHANNEL_CAPACITY: usize = 8192;

/// The sending half of the event channel, with the wake hook attached.
///
/// Backends emit through this so every event wakes the UI exactly
/// once. Cloning is cheap (channel sender + `Arc`).
#[derive(Clone)]
pub struct EventSink {
    tx: SyncSender<BackendEvent>,
    wake: WakeFn,
}

impl EventSink {
    /// Queues an event and fires the wake callback. Blocks while
    /// the bounded channel is full (see [`EVENT_CHANNEL_CAPACITY`]),
    /// which in practice means the UI has stalled; a closed
    /// receiver (UI gone) silently drops the event and returns at
    /// once.
    pub fn emit(&self, event: BackendEvent) {
        if self.tx.send(event).is_ok() {
            (self.wake)();
        }
    }
}

/// The protocol boundary. Implementations run on a dedicated thread
/// owned by [`spawn_backend`]; `run` blocks until
/// [`BackendCommand::Shutdown`] or until every command sender drops.
///
/// Implementations must not panic for recoverable failures. A failure
/// that ends the run is returned as `Err` from `run`; a failure the
/// session survives is reported as [`BackendEvent::Error`] and the
/// run continues. (If an implementation panics anyway, the
/// supervisor contains it; see the crate docs.)
pub trait Backend: Send {
    /// Runs the backend until shutdown. Called on the worker thread.
    ///
    /// The return value tells the supervisor how the run ended.
    /// `Ok(())` means the run ended on purpose: Shutdown was
    /// requested, the command channel closed, or the session
    /// completed on its own (a logout, a stopped connection). The
    /// supervisor does not restart after a clean exit. `Err(reason)`
    /// means the run died, typically before the session was ever
    /// established (the store would not open, the client would not
    /// build): the supervisor treats it exactly like a crash,
    /// restarting once and surfacing `reason` as the
    /// [`BackendEvent::Crashed`] summary, so a startup failure can
    /// never look like a clean exit and leave the command channel
    /// queueing into a dead backend.
    fn run(
        self: Box<Self>,
        config: &BackendConfig,
        commands: Receiver<BackendCommand>,
        events: EventSink,
    ) -> Result<(), String>;
}

/// Channels the UI holds after spawning a backend.
pub struct BackendChannels {
    /// Send commands to the backend.
    pub commands: Sender<BackendCommand>,
    /// Receive events from the backend.
    pub events: Receiver<BackendEvent>,
}

/// Spawns a supervised backend worker thread.
///
/// `factory` builds a fresh backend instance, it is called once at
/// start and, if the worker panics or its run returns an error, once
/// more for the single restart.
/// The returned channels stay valid across the restart.
///
/// # Panics
/// Panics only if the operating system refuses to spawn the worker
/// thread; there is no useful recovery at this call site.
pub fn spawn_backend<F>(factory: F, config: BackendConfig, wake: WakeFn) -> BackendChannels
where
    F: Fn() -> Box<dyn Backend> + Send + 'static,
{
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<BackendCommand>();
    let (evt_tx, evt_rx) = std::sync::mpsc::sync_channel::<BackendEvent>(EVENT_CHANNEL_CAPACITY);
    let sink = EventSink { tx: evt_tx, wake };
    std::thread::Builder::new()
        .name("zaprust-backend".to_owned())
        .spawn(move || supervise(factory, &config, cmd_rx, sink))
        .expect("spawning the backend thread cannot fail in practice");
    BackendChannels { commands: cmd_tx, events: evt_rx }
}

/// The supervisor: run the backend, contain a panic or an error
/// return, restart once, then give up loudly (via events) rather
/// than silently. A run that returns `Ok(())` ended on purpose and
/// is not restarted.
///
/// Commands flow UI → outer channel → pump thread → per-run channel →
/// backend. The pump exists so the outer receiver survives a crash:
/// after a restart, the pump targets the new run's channel. Commands
/// already forwarded into a crashed run's channel are lost; the
/// documented contract is that the UI re-issues its Connect command
/// when it sees [`BackendEvent::Crashed`].
fn supervise<F>(
    factory: F,
    config: &BackendConfig,
    commands: Receiver<BackendCommand>,
    sink: EventSink,
) where
    F: Fn() -> Box<dyn Backend>,
{
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex};
    // Commands are queued, never dropped: the UI may send Connect
    // before the first run registers, and a command lost in that
    // window would leave live mode stuck in "starting" forever.
    // The queue is capped; beyond the cap the oldest commands are
    // dropped (they are stale by definition at that point).
    const QUEUE_CAP: usize = 512;
    // (queue, current run target with its generation)
    type Shared =
        (Mutex<(VecDeque<BackendCommand>, Option<(u64, Sender<BackendCommand>)>)>, Condvar);
    let shared: Arc<Shared> = Arc::new((Mutex::new((VecDeque::new(), None)), Condvar::new()));
    // Set when the supervisor gives up after the final crash: the
    // pump and the drainer watch it and shut the shared queue down,
    // so the outer command channel closes and later sends fail
    // visibly instead of queueing into a queue nobody drains.
    let queue_dead: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let pump_shared = shared.clone();
    let pump_dead = queue_dead.clone();
    let pump =
        std::thread::Builder::new().name("zaprust-backend-pump".to_owned()).spawn(move || {
            // recv_timeout, not recv: the pump must notice the dead
            // flag even while no command is arriving, so it can drop
            // the outer receiver and close the channel.
            loop {
                match commands.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(cmd) => {
                        if pump_dead.load(Ordering::SeqCst) {
                            // Supervisor gave up: drop this command
                            // and the receiver with it. Every later
                            // send on the outer channel now errors.
                            break;
                        }
                        let (m, cv) = &*pump_shared;
                        let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if state.0.len() >= QUEUE_CAP {
                            state.0.pop_front();
                        }
                        state.0.push_back(cmd);
                        cv.notify_all();
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if pump_dead.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
    if pump.is_err() {
        sink.emit(BackendEvent::Error {
            message: "The connection worker could not start. Restart Zaprust.".to_owned(),
        });
        return;
    }

    // ONE drainer for the supervisor's whole lifetime. It pops a
    // command only while a live run target is registered, and a
    // failed send pushes the command back: a crash between attempts
    // can therefore never lose a command to a dead channel (the
    // failure mode of the earlier per-attempt drainers).
    let drain_shared = shared.clone();
    let drain_dead = queue_dead.clone();
    let _drainer =
        std::thread::Builder::new().name("zaprust-backend-drain".to_owned()).spawn(move || loop {
            if drain_dead.load(Ordering::SeqCst) {
                // The supervisor gave up; nothing will register a
                // new run target, so stop instead of waiting on a
                // queue that is being shut down.
                return;
            }
            let (cmd, gen, tx) = {
                let (m, cv) = &*drain_shared;
                let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                loop {
                    if drain_dead.load(Ordering::SeqCst) {
                        return;
                    }
                    if let (true, Some((gen, tx))) = (!state.0.is_empty(), state.1.clone()) {
                        let cmd = state.0.pop_front().expect("non-empty checked");
                        break (cmd, gen, tx);
                    }
                    // wait_timeout, not wait, so the dead flag is
                    // re-checked even when nobody notifies.
                    let waited = cv
                        .wait_timeout(state, std::time::Duration::from_millis(50))
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state = waited.0;
                }
            };
            if let Err(std::sync::mpsc::SendError(cmd)) = tx.send(cmd) {
                // The run died between the check and the send:
                // requeue at the front and clear the dead target.
                let (m, cv) = &*drain_shared;
                let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                state.0.push_front(cmd);
                if state.1.as_ref().is_some_and(|(g, _)| *g == gen) {
                    state.1 = None;
                }
                cv.notify_all();
            }
        });

    for attempt in 1..=2u64 {
        let (run_tx, run_rx) = std::sync::mpsc::channel::<BackendCommand>();
        {
            let (m, cv) = &*shared;
            let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.1 = Some((attempt, run_tx));
            cv.notify_all();
        }
        let run_sink = sink.clone();
        let cfg = config.clone();
        // Construction is inside the guard too: a panicking factory
        // must surface as Crashed, not kill the supervisor silently.
        let factory_ref = &factory;
        let result = catch_unwind(AssertUnwindSafe(move || {
            let backend = factory_ref();
            backend.run(&cfg, run_rx, run_sink)
        }));
        {
            let (m, _cv) = &*shared;
            let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.1.as_ref().is_some_and(|(g, _)| *g == attempt) {
                state.1 = None;
            }
        }
        // A clean Ok(()) return ends the supervision: the run ended
        // on purpose and must not be restarted. Every other outcome
        // is a failure with a summary: a panic payload, or the Err a
        // run returns when it died before or outside a session (the
        // startup failure path). Both get the same restart-once,
        // then-give-up treatment below.
        let summary = match result {
            Ok(Ok(())) => return,
            Ok(Err(reason)) => reason,
            Err(payload) => panic_summary(&payload),
        };
        // Both summary sources are free text: a panic payload can
        // interpolate identifiers, and a startup reason is built
        // from store and filesystem errors that can carry the
        // data-dir path (the user's home path and username). The
        // app renders the summary verbatim in the connection
        // status, so scrub it here, at construction, with the
        // same display scrubber the crash report banner uses:
        // every consumer of the event receives the redacted form.
        let summary = core::crash::scrub_for_display(&summary);
        if attempt == 1 {
            sink.emit(BackendEvent::Crashed { summary, restarted: true });
        } else {
            // Giving up: shut the shared queue down first.
            // The flag makes the pump drop the outer
            // receiver (closing the command channel, so
            // later sends fail visibly instead of queueing
            // to nowhere) and the drainer stop, and any
            // commands still queued are discarded with it.
            queue_dead.store(true, Ordering::SeqCst);
            {
                let (m, cv) = &*shared;
                let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                state.0.clear();
                state.1 = None;
                cv.notify_all();
            }
            sink.emit(BackendEvent::Crashed { summary, restarted: false });
            sink.emit(BackendEvent::Error {
                message: "The connection worker failed twice and stopped. \
                          Restart Zaprust to reconnect."
                    .to_owned(),
            });
            return;
        }
    }
}

/// Extracts the panic message from a panic payload, verbatim.
/// The text is not redaction-safe on its own: `supervise` runs
/// every summary through the crash display scrubber before it
/// leaves in a [`BackendEvent::Crashed`].
fn panic_summary(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn test_config() -> BackendConfig {
        BackendConfig {
            data_dir: PathBuf::from("test-data"),
            account_id: "default".to_owned(),
            db_key: [0; 32],
        }
    }

    fn no_wake() -> WakeFn {
        Arc::new(|| {})
    }

    /// The event channel is bounded, and a full channel makes
    /// `emit` wait rather than drop: events carry chat content
    /// and state assertions that no later event re-sends, so
    /// losing one to make room would corrupt the UI's picture
    /// silently. Once the UI drains, the blocked emit lands and
    /// ordering is preserved.
    #[test]
    fn event_sink_blocks_at_capacity_instead_of_dropping() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<BackendEvent>(2);
        let sink = EventSink { tx, wake: no_wake() };
        sink.emit(BackendEvent::Connected { account: "a".to_owned() });
        sink.emit(BackendEvent::Connected { account: "b".to_owned() });

        let blocked_sink = sink.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let emitter = std::thread::spawn(move || {
            blocked_sink.emit(BackendEvent::Connected { account: "c".to_owned() });
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "emit must block while the channel is full, not drop the event"
        );

        let first = rx.recv().expect("first event queued");
        assert!(matches!(first, BackendEvent::Connected { ref account } if account == "a"));
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the blocked emit completes once the UI drains");
        emitter.join().expect("emitter thread finishes");

        let second = rx.recv().expect("second event queued");
        let third = rx.recv().expect("the blocked emit was queued, not dropped");
        assert!(matches!(second, BackendEvent::Connected { ref account } if account == "b"));
        assert!(matches!(third, BackendEvent::Connected { ref account } if account == "c"));
    }

    /// With the UI gone (receiver dropped), `emit` returns at
    /// once instead of blocking on a channel nobody will drain:
    /// a closed bounded channel fails its sends immediately.
    #[test]
    fn event_sink_emit_returns_at_once_when_the_ui_is_gone() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<BackendEvent>(1);
        let sink = EventSink { tx, wake: no_wake() };
        drop(rx);
        // Would hang the test if a closed channel blocked.
        sink.emit(BackendEvent::Connected { account: "a".to_owned() });
    }

    #[test]
    fn session_paths_live_under_the_account_dir() {
        let cfg = test_config();
        assert_eq!(
            cfg.session_db_path(),
            PathBuf::from("test-data").join("accounts").join("default").join("session.db")
        );
    }

    struct PanicOnceBackend {
        runs: Arc<AtomicUsize>,
    }

    impl Backend for PanicOnceBackend {
        fn run(
            self: Box<Self>,
            _config: &BackendConfig,
            commands: Receiver<BackendCommand>,
            events: EventSink,
        ) -> Result<(), String> {
            let n = self.runs.fetch_add(1, Ordering::SeqCst);
            assert!(n != 0, "first run explodes");
            events.emit(BackendEvent::Connected { account: "restarted".to_owned() });
            while let Ok(cmd) = commands.recv() {
                if matches!(cmd, BackendCommand::Shutdown) {
                    return Ok(());
                }
            }
            Ok(())
        }
    }

    #[test]
    fn worker_panic_is_contained_and_restarted_once() {
        let runs = Arc::new(AtomicUsize::new(0));
        let runs2 = runs.clone();
        let channels = spawn_backend(
            move || Box::new(PanicOnceBackend { runs: runs2.clone() }) as Box<dyn Backend>,
            test_config(),
            no_wake(),
        );
        let first = channels
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("a Crashed event must arrive");
        assert!(
            matches!(first, BackendEvent::Crashed { restarted: true, .. }),
            "first crash must announce a restart: {first:?}"
        );
        // Just verify the restarted run announced itself.
        let second = channels.events.recv_timeout(Duration::from_secs(5));
        assert!(
            matches!(second, Ok(BackendEvent::Connected { .. })),
            "restarted backend must emit events: {second:?}"
        );
        drop(channels.commands);
    }

    /// A panic message stuffed with everything a payload can
    /// interpolate: a bearer token, an API key, a phone number,
    /// and the user's home path. The Crashed summary is rendered
    /// verbatim by the app, so none of it may leave in the event.
    struct LeakyPanicBackend;

    impl Backend for LeakyPanicBackend {
        fn run(
            self: Box<Self>,
            _config: &BackendConfig,
            _commands: Receiver<BackendCommand>,
            _events: EventSink,
        ) -> Result<(), String> {
            let home = std::env::var("HOME").unwrap_or_default();
            panic!(
                "auth failed: Bearer <redacted> key sk-proj-0123456789abcdef \
                 for +62 812-3456-7890 at {home}/zaprust/session.db"
            );
        }
    }

    fn assert_summary_is_scrubbed(summary: &str) {
        assert!(
            !summary.contains("tok_live_0123456789abcdef"),
            "bearer token leaked into the summary: {summary}"
        );
        assert!(
            !summary.contains("sk-proj-0123456789abcdef"),
            "API key leaked into the summary: {summary}"
        );
        assert!(!summary.contains("812-3456"), "phone number leaked into the summary: {summary}");
        assert!(
            summary.contains("***90"),
            "the phone number should be masked to its last digits: {summary}"
        );
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() && home != "/" {
                assert!(!summary.contains(&home), "home path leaked into the summary: {summary}");
                assert!(
                    summary.contains("~/zaprust/session.db"),
                    "the home prefix should be replaced with ~: {summary}"
                );
            }
        }
        // Scrubbing masks shapes; it does not blank the summary.
        assert!(summary.contains("auth failed"), "summary must stay informative: {summary}");
    }

    #[test]
    fn crashed_summary_is_scrubbed_before_it_is_emitted() {
        let channels = spawn_backend(
            || Box::new(LeakyPanicBackend) as Box<dyn Backend>,
            test_config(),
            no_wake(),
        );
        let first = channels
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("a Crashed event must arrive");
        match &first {
            BackendEvent::Crashed { summary, restarted } => {
                assert!(*restarted, "first crash must announce a restart");
                assert_summary_is_scrubbed(summary);
            }
            other => panic!("expected Crashed, got {other:?}"),
        }
        // The restarted run fails the same way: the supervisor
        // gives up (restart once, then stop), and the final
        // summary is scrubbed too.
        let mut saw_final_crash = false;
        let mut saw_error = false;
        for _ in 0..4 {
            match channels.events.recv_timeout(Duration::from_secs(5)) {
                Ok(BackendEvent::Crashed { summary, restarted: false }) => {
                    assert_summary_is_scrubbed(&summary);
                    saw_final_crash = true;
                }
                Ok(BackendEvent::Error { .. }) => saw_error = true,
                Ok(_) => {}
                Err(_) => break,
            }
            if saw_final_crash && saw_error {
                break;
            }
        }
        assert!(saw_final_crash, "second crash must be reported as not restarted");
        assert!(saw_error, "a persistent Error event must follow the give-up");
        drop(channels.commands);
    }

    #[test]
    fn command_sent_after_crash_reaches_the_restarted_run() {
        // Regression (Round 3): per-attempt drainers raced after a
        // crash; the stale one popped the re-issued Connect command
        // and dropped it on a dead channel, hanging live start.
        let runs = Arc::new(AtomicUsize::new(0));
        let runs2 = runs.clone();
        let channels = spawn_backend(
            move || Box::new(PanicOnceBackend { runs: runs2.clone() }) as Box<dyn Backend>,
            test_config(),
            no_wake(),
        );
        let first = channels
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("a Crashed event must arrive");
        assert!(matches!(first, BackendEvent::Crashed { restarted: true, .. }));
        let _ = channels.events.recv_timeout(Duration::from_secs(5));
        channels.commands.send(BackendCommand::ConnectQr).expect("channel open");
        // PanicOnceBackend's restarted run loops on recv without
        // emitting; observe delivery through the runs counter path:
        // send Shutdown, which makes it return, then confirm the run
        // consumed commands by checking no event error appears and
        // the supervisor thread ends quietly (implicit in drop).
        channels.commands.send(BackendCommand::Shutdown).expect("channel open");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(runs.load(Ordering::SeqCst), 2, "exactly one restart");
        drop(channels.commands);
    }

    struct SlowStartBackend;

    impl Backend for SlowStartBackend {
        fn run(
            self: Box<Self>,
            _config: &BackendConfig,
            commands: Receiver<BackendCommand>,
            events: EventSink,
        ) -> Result<(), String> {
            // Simulate a slow-starting worker: commands sent during
            // this window used to be dropped by the supervisor pump.
            std::thread::sleep(Duration::from_millis(300));
            match commands.recv_timeout(Duration::from_secs(5)) {
                Ok(BackendCommand::ConnectQr) => {
                    events.emit(BackendEvent::Connected { account: "slow".to_owned() });
                }
                other => panic!("expected the queued ConnectQr, got {other:?}"),
            }
            Ok(())
        }
    }

    #[test]
    fn commands_sent_before_the_run_starts_are_delivered() {
        let channels = spawn_backend(|| Box::new(SlowStartBackend), test_config(), no_wake());
        // Sent immediately, while the worker is still in its slow
        // start: the supervisor must queue it, not drop it.
        channels.commands.send(BackendCommand::ConnectQr).expect("queue accepts the command");
        let event = channels
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("the queued command must reach the worker");
        assert!(matches!(event, BackendEvent::Connected { .. }), "got {event:?}");
    }

    #[test]
    fn factory_panic_is_contained_like_a_run_panic() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let channels = spawn_backend(
            move || {
                assert!(calls2.fetch_add(1, Ordering::SeqCst) != 0, "factory explodes");
                Box::new(SlowStartBackend) as Box<dyn Backend>
            },
            test_config(),
            no_wake(),
        );
        let first = channels
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("a Crashed event must arrive for a factory panic");
        assert!(
            matches!(first, BackendEvent::Crashed { restarted: true, .. }),
            "factory panic must announce a restart: {first:?}"
        );
        drop(channels.commands);
    }

    struct AlwaysPanicBackend;

    impl Backend for AlwaysPanicBackend {
        fn run(
            self: Box<Self>,
            _config: &BackendConfig,
            _commands: Receiver<BackendCommand>,
            _events: EventSink,
        ) -> Result<(), String> {
            panic!("always explodes");
        }
    }

    #[test]
    fn second_panic_surfaces_a_persistent_error() {
        let channels = spawn_backend(
            || Box::new(AlwaysPanicBackend) as Box<dyn Backend>,
            test_config(),
            no_wake(),
        );
        let mut saw_final_crash = false;
        let mut saw_error = false;
        for _ in 0..4 {
            match channels.events.recv_timeout(Duration::from_secs(5)) {
                Ok(BackendEvent::Crashed { restarted: false, .. }) => saw_final_crash = true,
                Ok(BackendEvent::Error { .. }) => saw_error = true,
                Ok(_) => {}
                Err(_) => break,
            }
            if saw_final_crash && saw_error {
                break;
            }
        }
        assert!(saw_final_crash, "second crash must be reported as not restarted");
        assert!(saw_error, "a persistent Error event must follow");
    }

    #[test]
    fn commands_fail_visibly_after_the_supervisor_gives_up() {
        // Regression (F5): after the final crash the detached pump
        // kept queueing commands into the capped queue, where they
        // were silently dropped. The supervisor must shut the queue
        // down on give-up so the outer channel closes and sends
        // start failing instead.
        let channels = spawn_backend(
            || Box::new(AlwaysPanicBackend) as Box<dyn Backend>,
            test_config(),
            no_wake(),
        );
        let mut saw_final_crash = false;
        let mut saw_error = false;
        for _ in 0..4 {
            match channels.events.recv_timeout(Duration::from_secs(5)) {
                Ok(BackendEvent::Crashed { restarted: false, .. }) => saw_final_crash = true,
                Ok(BackendEvent::Error { .. }) => saw_error = true,
                Ok(_) => {}
                Err(_) => break,
            }
            if saw_final_crash && saw_error {
                break;
            }
        }
        assert!(saw_final_crash && saw_error, "the give-up must be announced first");
        // The pump notices the dead flag on its poll tick and drops
        // the outer receiver; from then on sends must error. Probe
        // briefly: sends that still succeed land in the closing
        // window and are discarded with the queue.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if channels.commands.send(BackendCommand::ConnectQr).is_err() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sends must fail once the supervisor has given up"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    struct StartupFailOnceBackend {
        runs: Arc<AtomicUsize>,
    }

    impl Backend for StartupFailOnceBackend {
        fn run(
            self: Box<Self>,
            _config: &BackendConfig,
            commands: Receiver<BackendCommand>,
            events: EventSink,
        ) -> Result<(), String> {
            let n = self.runs.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // The startup failure path: the run dies before the
                // session exists, the way run_async returning Err for
                // a store that will not open does. No panic, no
                // events, just the error return.
                return Err("Could not open the session store: file is locked".to_owned());
            }
            events.emit(BackendEvent::Connected { account: "restarted".to_owned() });
            while let Ok(cmd) = commands.recv() {
                if matches!(cmd, BackendCommand::Shutdown) {
                    return Ok(());
                }
            }
            Ok(())
        }
    }

    #[test]
    fn startup_failure_is_restarted_once_with_its_reason() {
        let runs = Arc::new(AtomicUsize::new(0));
        let runs2 = runs.clone();
        let channels = spawn_backend(
            move || Box::new(StartupFailOnceBackend { runs: runs2.clone() }) as Box<dyn Backend>,
            test_config(),
            no_wake(),
        );
        let first = channels
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("a Crashed event must arrive for a startup failure");
        match &first {
            BackendEvent::Crashed { summary, restarted } => {
                assert!(*restarted, "first startup failure must announce a restart");
                assert!(
                    summary.contains("session store"),
                    "the Crashed summary must carry the failure's real reason: {summary}"
                );
            }
            other => panic!("expected Crashed, got {other:?}"),
        }
        // The restart is a fresh run from the factory, so a transient
        // failure (here: only the first attempt fails) recovers.
        let second = channels.events.recv_timeout(Duration::from_secs(5));
        assert!(
            matches!(second, Ok(BackendEvent::Connected { .. })),
            "restarted backend must emit events: {second:?}"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2, "exactly one restart");
        drop(channels.commands);
    }

    struct AlwaysStartupFailBackend;

    impl Backend for AlwaysStartupFailBackend {
        fn run(
            self: Box<Self>,
            _config: &BackendConfig,
            _commands: Receiver<BackendCommand>,
            _events: EventSink,
        ) -> Result<(), String> {
            Err("Could not open the session store: file is locked".to_owned())
        }
    }

    #[test]
    fn second_startup_failure_surfaces_a_persistent_error() {
        let channels = spawn_backend(
            || Box::new(AlwaysStartupFailBackend) as Box<dyn Backend>,
            test_config(),
            no_wake(),
        );
        let mut saw_final_crash = false;
        let mut saw_error = false;
        for _ in 0..4 {
            match channels.events.recv_timeout(Duration::from_secs(5)) {
                Ok(BackendEvent::Crashed { summary, restarted: false }) => {
                    assert!(
                        summary.contains("session store"),
                        "the final Crashed summary must carry the reason: {summary}"
                    );
                    saw_final_crash = true;
                }
                Ok(BackendEvent::Error { .. }) => saw_error = true,
                Ok(_) => {}
                Err(_) => break,
            }
            if saw_final_crash && saw_error {
                break;
            }
        }
        assert!(saw_final_crash, "second startup failure must be reported as not restarted");
        assert!(saw_error, "a persistent Error event must follow");
    }

    #[test]
    fn commands_fail_visibly_after_startup_failures_give_up() {
        // The silent-death regression this fix exists for: before it,
        // a startup failure returned cleanly, the supervisor exited,
        // and UI commands queued forever into a channel nobody
        // drained. After the give-up the sends must fail instead.
        let channels = spawn_backend(
            || Box::new(AlwaysStartupFailBackend) as Box<dyn Backend>,
            test_config(),
            no_wake(),
        );
        let mut saw_final_crash = false;
        let mut saw_error = false;
        for _ in 0..4 {
            match channels.events.recv_timeout(Duration::from_secs(5)) {
                Ok(BackendEvent::Crashed { restarted: false, .. }) => saw_final_crash = true,
                Ok(BackendEvent::Error { .. }) => saw_error = true,
                Ok(_) => {}
                Err(_) => break,
            }
            if saw_final_crash && saw_error {
                break;
            }
        }
        assert!(saw_final_crash && saw_error, "the give-up must be announced first");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if channels.commands.send(BackendCommand::ConnectQr).is_err() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sends must fail once the supervisor has given up"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    struct CleanExitBackend {
        runs: Arc<AtomicUsize>,
    }

    impl Backend for CleanExitBackend {
        fn run(
            self: Box<Self>,
            _config: &BackendConfig,
            _commands: Receiver<BackendCommand>,
            _events: EventSink,
        ) -> Result<(), String> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            // Ends at once and on purpose, the way a run that was
            // asked to shut down before anything else happened does.
            Ok(())
        }
    }

    #[test]
    fn clean_exit_is_not_restarted_and_emits_no_crash() {
        let runs = Arc::new(AtomicUsize::new(0));
        let runs2 = runs.clone();
        let channels = spawn_backend(
            move || Box::new(CleanExitBackend { runs: runs2.clone() }) as Box<dyn Backend>,
            test_config(),
            no_wake(),
        );
        // No Crashed, no Error, no event of any kind: a clean exit
        // is silent (the receiver either times out or disconnects
        // when the supervisor thread drops the event sink on its way
        // out), and the factory is never called a second time.
        let event = channels.events.recv_timeout(Duration::from_millis(500));
        assert!(event.is_err(), "a clean exit must emit nothing: {event:?}");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "a clean exit must not be restarted");
        drop(channels.commands);
    }

    /// A unique empty directory under the system temp dir for the
    /// data-dir adoption and cleanup tests; removed on drop.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "zaprust-backend-test-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp tree");
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A legacy tree shaped like a pre-rename install: an archive,
    /// a per-account session store, and settings.
    fn seed_legacy_tree(legacy: &std::path::Path) {
        std::fs::create_dir_all(legacy.join("accounts").join("default")).expect("seed dirs");
        std::fs::write(legacy.join("archive.db"), b"plaintext archive").expect("seed archive");
        std::fs::write(
            legacy.join("accounts").join("default").join("session.db"),
            b"plaintext session",
        )
        .expect("seed session");
        std::fs::write(legacy.join("settings.json"), b"{}").expect("seed settings");
    }

    /// Seeds a downloads folder the way the download path names
    /// files: two media files for one chat (one of them for a
    /// message id that is a prefix of the other's), one for a
    /// chat whose user part starts with the same digits, and an
    /// unrelated file.
    fn seed_downloads(data: &std::path::Path) {
        let dir = data.join("downloads");
        std::fs::create_dir_all(&dir).expect("seed downloads dir");
        for name in [
            "6281234567890-ABC123.jpg",
            "6281234567890-ABC1234.mp4",
            "62812345678901-XYZ.png",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), b"media").expect("seed download");
        }
    }

    #[test]
    fn download_removal_for_a_message_takes_only_its_file() {
        let root = TempTree::new("dl-message");
        seed_downloads(root.path());
        let dir = root.path().join("downloads");
        // The stem rule is the naming contract with the download
        // path: protocol punctuation is sanitized the same way.
        assert_eq!(
            downloaded_media_stem("6281234567890@s.whatsapp.net", "ABC:123"),
            "6281234567890-ABC_123"
        );
        assert!(remove_download_for_message(root.path(), "6281234567890@s.whatsapp.net", "ABC123"));
        assert!(!dir.join("6281234567890-ABC123.jpg").exists(), "the message's file is gone");
        assert!(
            dir.join("6281234567890-ABC1234.mp4").exists(),
            "a message whose id extends this one keeps its file"
        );
        assert!(
            !remove_download_for_message(root.path(), "6281234567890@s.whatsapp.net", "ABC123"),
            "a second removal reports nothing removed"
        );
    }

    #[test]
    fn download_removal_for_a_chat_takes_only_its_files() {
        let root = TempTree::new("dl-chat");
        seed_downloads(root.path());
        let dir = root.path().join("downloads");
        let removed = remove_downloads_for_chat(root.path(), "6281234567890@s.whatsapp.net");
        assert_eq!(removed, 2, "both of the chat's downloads go");
        assert!(!dir.join("6281234567890-ABC123.jpg").exists());
        assert!(!dir.join("6281234567890-ABC1234.mp4").exists());
        assert!(
            dir.join("62812345678901-XYZ.png").exists(),
            "a chat whose number starts with the same digits is a different chat"
        );
        assert!(dir.join("notes.txt").exists(), "unrelated files are untouched");
        assert_eq!(
            remove_downloads_for_chat(root.path(), "12025550123@s.whatsapp.net"),
            0,
            "an unknown chat removes nothing"
        );
    }

    #[test]
    fn adoption_then_cleanup_removes_the_legacy_tree() {
        let root = TempTree::new("adopt-clean");
        let legacy = root.path().join("zaplite");
        let current = root.path().join("zaprust");
        seed_legacy_tree(&legacy);

        adopt_legacy_dir(&current, &legacy);
        // The adoption copies and leaves the original in place.
        assert_eq!(
            std::fs::read(current.join("archive.db")).expect("copied archive"),
            b"plaintext archive"
        );
        assert!(legacy.join("archive.db").exists(), "adoption alone must not remove the legacy");

        // Once the app runs from the new directory, the cleanup
        // removes the whole legacy tree and nothing else.
        assert!(remove_legacy_dir(&current, &legacy));
        assert!(!legacy.exists(), "the legacy copy must be gone");
        assert_eq!(
            std::fs::read(current.join("archive.db")).expect("current archive survives"),
            b"plaintext archive"
        );
        assert!(current.join("accounts").join("default").join("session.db").exists());
        // A later start finds nothing to do and does not error.
        assert!(!remove_legacy_dir(&current, &legacy));
    }

    #[test]
    fn cleanup_refuses_when_current_is_the_legacy_dir() {
        let root = TempTree::new("clean-self");
        let legacy = root.path().join("zaplite");
        seed_legacy_tree(&legacy);
        // The app still runs from the legacy directory itself:
        // removing it would delete the live data.
        assert!(!remove_legacy_dir(&legacy, &legacy));
        assert_eq!(
            std::fs::read(legacy.join("archive.db")).expect("legacy archive survives"),
            b"plaintext archive"
        );
    }

    #[test]
    fn cleanup_refuses_when_current_is_inside_legacy() {
        let root = TempTree::new("clean-nested");
        let legacy = root.path().join("zaplite");
        seed_legacy_tree(&legacy);
        let nested = legacy.join("zaprust");
        std::fs::create_dir_all(&nested).expect("nested current");
        // Removing the legacy tree would remove the current
        // directory with it; the nesting rail refuses.
        assert!(!remove_legacy_dir(&nested, &legacy));
        assert!(legacy.join("archive.db").exists());
        assert!(nested.exists());
    }

    #[test]
    fn cleanup_waits_until_adoption_has_completed() {
        let root = TempTree::new("clean-early");
        let legacy = root.path().join("zaplite");
        seed_legacy_tree(&legacy);
        let current = root.path().join("zaprust");
        // The new directory does not exist: adoption never
        // completed, so the legacy copy is the only one and stays.
        assert!(!remove_legacy_dir(&current, &legacy));
        assert!(legacy.join("archive.db").exists());
    }

    #[test]
    fn cleanup_without_a_legacy_dir_is_a_no_op() {
        let root = TempTree::new("clean-none");
        let current = root.path().join("zaprust");
        std::fs::create_dir_all(&current).expect("current dir");
        assert!(!remove_legacy_dir(&current, &root.path().join("zaplite")));
        assert!(current.exists());
    }

    #[test]
    fn cleanup_failure_is_reported_not_raised() {
        let root = TempTree::new("clean-fail");
        let current = root.path().join("zaprust");
        std::fs::create_dir_all(&current).expect("current dir");
        // A legacy path that is a file, not a directory: the
        // cleanup skips it and reports false instead of failing,
        // which is what keeps a stuck legacy copy from ever
        // blocking startup.
        let legacy_file = root.path().join("zaplite");
        std::fs::write(&legacy_file, b"not a directory").expect("legacy file");
        assert!(!remove_legacy_dir(&current, &legacy_file));
        assert_eq!(std::fs::read(&legacy_file).expect("file survives"), b"not a directory");
    }
}
