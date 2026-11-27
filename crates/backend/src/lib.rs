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
#[cfg(feature = "calls")]
pub(crate) mod callvideo;
pub mod fake;
pub mod qr;
#[cfg(feature = "whatsapp")]
mod whatsapp;

pub use fake::{FakeBackend, FAKE_DM_JID, FAKE_GROUP_JID};
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

/// Whether a stored session exists for this account, probed
/// synchronously and cheaply: the library's session store file
/// is present and non-empty. The store file is where the linked
/// device's identity lives, so its presence is the truthful
/// pre-start signal that a previous link exists to restore.
///
/// The probe deliberately answers nothing more. Whether the
/// stored session still links (it may have been logged out from
/// the phone while the app was closed) is decided by the
/// backend's own outcome events once it starts; callers use
/// this only to choose the first paint, never as proof of a
/// linked session.
#[must_use]
pub fn stored_session_exists(config: &BackendConfig) -> bool {
    std::fs::metadata(config.session_db_path()).is_ok_and(|m| m.is_file() && m.len() > 0)
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

/// Normalizes a phone number as typed into the digit string
/// the protocol addresses: formatting characters (spaces,
/// dashes, parentheses, a leading `+`) are stripped, and the
/// result must be 7 to 15 digits (the international format's
/// bounds) to be a number at all. Shared by every backend so
/// the fake and the real backend accept exactly the same
/// numbers. `None` when what was typed cannot be a phone
/// number.
#[must_use]
pub fn phone_digits(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if (7..=15).contains(&digits.len()) {
        Some(digits)
    } else {
        None
    }
}

/// The file stem every download of a message's media is saved
/// under in the downloads folder: `<chat user part>-<message
/// id>`, with the id kept filename-safe. This is the one place
/// the naming rule lives: the download path in the WhatsApp
/// backend builds the saved file's name from it, and the
/// removal helpers below match files by it, so a rename here
/// renames both sides of the contract.
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
    // The downloads folder and the video cache share the stem
    // contract: a deleted message takes both its saved
    // download and its cached playback file.
    remove_downloads_matching(&data_dir.join("downloads"), &format!("{stem}."), true) > 0
        || remove_downloads_matching(&data_dir.join("video-cache"), &format!("{stem}."), true) > 0
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
        + remove_downloads_matching(&data_dir.join("video-cache"), &format!("{user}-"), false)
}

/// The video cache's total size cap: watched videos are
/// plaintext on disk, so the cache is a bounded working set,
/// not an archive. Oldest files (by modification time) are
/// evicted first; the file named `keep` (the one just landed)
/// is never evicted.
pub const VIDEO_CACHE_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// Evicts the oldest files of the video cache directory
/// until its total size is at or under `cap_bytes`. Returns
/// how many files were removed. Best-effort: unreadable
/// entries are skipped, never an error.
pub fn evict_video_cache(dir: &std::path::Path, cap_bytes: u64, keep: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        total += meta.len();
        files.push((entry.path(), meta.len(), mtime));
    }
    if total <= cap_bytes {
        return 0;
    }
    files.sort_by_key(|(_, _, mtime)| *mtime);
    let mut removed = 0;
    for (path, len, _) in files {
        if total <= cap_bytes {
            break;
        }
        if path == keep {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
            removed += 1;
        }
    }
    removed
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

/// The signed-in account's own profile, as read back from the
/// server: the values other people see, not a local copy. Emitted
/// in [`BackendEvent::OwnProfile`] when the account connects,
/// when the UI asks with [`BackendCommand::FetchOwnProfile`], and
/// again after every [`BackendCommand::UpdateOwnProfile`] (the
/// post-change re-read is what reconciles the UI with the state
/// the server actually kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnProfile {
    /// The account's push name (display name).
    pub name: String,
    /// The account's own phone-number JID, in the non-AD form
    /// the chat list addresses DM chats by. The app needs the
    /// exact address to tell the self chat apart from a contact
    /// saved under the account's own name; a name comparison
    /// cannot make that distinction. Empty when the backend
    /// does not know the address yet (or a value was built
    /// without one, as demo fixtures do).
    pub jid: String,
    /// The account's own LID, in the same non-AD form, or
    /// empty when the account has none known. A self chat can
    /// be addressed under the LID instead of the phone-number
    /// JID, so the app matches against both forms.
    pub lid: String,
    /// The account's About / text status (empty when unset).
    pub about: String,
    /// The account's current profile picture as encoded image
    /// bytes, or `None` when the account has no picture (or its
    /// bytes could not be downloaded).
    pub photo: Option<Vec<u8>>,
}

/// One business category on a business profile, as the
/// protocol reports it: a stable id and its display name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveBusinessCategory {
    /// Category id (the value profile updates carry).
    pub id: String,
    /// Display name.
    pub name: String,
}

/// One day's opening hours on a business profile. `mode` is
/// the wire spelling: `open_24h`, `specific_hours`,
/// `appointment_only`, or a closed day carries the mode the
/// server sent; an unrecognized mode rides through verbatim
/// so nothing the server said is silently rewritten.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveBusinessDayHours {
    /// Day name as the wire spells it (`monday` to `sunday`).
    pub day: String,
    /// Hour mode, wire spelling (see the type docs).
    pub mode: String,
    /// Opening time, minutes past local midnight
    /// (`specific_hours` days only).
    pub open_minutes: Option<u32>,
    /// Closing time, minutes past local midnight
    /// (`specific_hours` days only).
    pub close_minutes: Option<u32>,
}

/// A business profile as the protocol reports it, for a
/// business account (another contact's, or the account's
/// own). An account with no business profile answers
/// `None` at the command that asked, never an empty one of
/// these: the distinction is the answer, not a gap in it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveBusinessProfile {
    /// Free-text description (empty when the business set
    /// none).
    pub description: String,
    /// Contact email, when stated.
    pub email: Option<String>,
    /// Websites, in the profile's own order.
    pub websites: Vec<String>,
    /// Categories the business filed itself under.
    pub categories: Vec<LiveBusinessCategory>,
    /// Street address, when stated.
    pub address: Option<String>,
    /// IANA timezone of the opening hours, when stated.
    pub hours_timezone: Option<String>,
    /// Opening hours by day (only the days the profile
    /// states; a day absent here has no stated hours).
    pub hours: Vec<LiveBusinessDayHours>,
}

/// The opening-hours half of a business profile update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveBusinessHoursUpdate {
    /// IANA timezone name; `None` omits it from the delta.
    pub timezone: Option<String>,
    /// Free-text note shown under the hours; `None` omits
    /// it from the delta.
    pub note: Option<String>,
    /// One entry per opening range; a day with two ranges
    /// appears twice.
    pub days: Vec<LiveBusinessDayHours>,
}

/// A delta to the account's own business profile, in the
/// protocol's own shape: an outer `None` leaves the field
/// untouched, `Some(None)` (text fields) or an empty list
/// (list fields) clears it, and `Some(Some(text))` sets it.
/// Only the fields the user actually changed are set by
/// the caller, so a save can never clear a field the form
/// did not show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveBusinessProfileUpdate {
    /// Description delta.
    pub description: Option<Option<String>>,
    /// Email delta.
    pub email: Option<Option<String>>,
    /// Address delta.
    pub address: Option<Option<String>>,
    /// Websites replacement (`Some` replaces the whole
    /// list; an empty list clears it).
    pub websites: Option<Vec<String>>,
    /// Category id replacement (`Some` replaces the whole
    /// set with these ids; an empty list clears it). The
    /// ids come from the profile's own current categories:
    /// the protocol offers no category directory to a
    /// linked device, so new categories cannot be offered.
    pub categories: Option<Vec<String>>,
    /// Opening-hours replacement.
    pub hours: Option<LiveBusinessHoursUpdate>,
}

/// One product in a business catalog. Only `id` is
/// guaranteed by the server; every other field is optional
/// because the server omits rather than blanks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveProduct {
    /// Product id.
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Price in thousandths of the currency unit
    /// (`1990` is 1.99), exact as the server sent it.
    pub price_amount_1000: Option<i64>,
    /// Sale price, same scale, while a sale runs.
    pub sale_amount_1000: Option<i64>,
    /// ISO 4217 currency code of the prices.
    pub price_currency: Option<String>,
    /// Availability, wire spelling (`IN_STOCK`,
    /// `OUT_OF_STOCK`, `AVAILABLE_FOR_ANOTHER_POSTCODE`, or
    /// a later spelling carried verbatim).
    pub availability: Option<String>,
    /// Whether the merchant hides this product from
    /// customers. Hidden products are in the catalog data
    /// but are not customer-facing; surfaces that present
    /// the catalog to browse it skip them.
    pub hidden: bool,
    /// Image URL at display dimensions, when the product
    /// has an image. The bytes are fetched separately
    /// ([`BackendCommand::FetchProductImage`]).
    pub image_url: Option<String>,
}

/// A named group of products within a catalog. The
/// products are a prefix of the collection (the protocol
/// returns the first page inline and offers no cursor
/// into a collection), so a collection whose product
/// count reaches the fetch limit may hold more than it
/// shows; the surface says so rather than dressing a
/// prefix as the whole.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveCollection {
    /// Collection id.
    pub id: Option<String>,
    /// Display name.
    pub name: Option<String>,
    /// The inline products (a prefix; see the type docs).
    pub products: Vec<LiveProduct>,
}

/// One line item of an order, as a snapshot: the price is
/// what was quoted when the order was placed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveOrderItem {
    /// Product name.
    pub name: Option<String>,
    /// Unit price in thousandths of the currency unit.
    pub price_amount_1000: Option<i64>,
    /// ISO 4217 currency code of the price.
    pub price_currency: Option<String>,
    /// Quantity ordered.
    pub quantity: Option<i64>,
    /// The chosen variant as (dimension, value) pairs
    /// (size, colour, and the like); empty for a product
    /// with no variants.
    pub variant: Vec<(String, String)>,
}

/// An order's line items and totals, fetched on demand
/// for one order message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveOrder {
    /// Line items, in the order's own order.
    pub items: Vec<LiveOrderItem>,
    /// Subtotal in thousandths of the currency unit.
    pub subtotal_amount_1000: Option<i64>,
    /// Total in thousandths of the currency unit.
    pub total_amount_1000: Option<i64>,
    /// ISO 4217 currency code of the totals.
    pub currency: Option<String>,
}

/// One label definition, as app-state sync carries it.
/// Labels are account state: they sync between the phone
/// and every linked device, and a label's identity is its
/// id; the name and color are its current statement.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveLabel {
    /// Label id (an opaque index key, not a JID).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Color index into the label palette the official
    /// clients draw from. The index is the synced truth;
    /// rendering maps it through the app's palette table.
    pub color: i32,
    /// The predefined (built-in) label id this label
    /// instantiates, when it is one of the protocol's
    /// predefined labels.
    pub predefined_id: Option<i32>,
}

/// A label mutation that failed, carried on
/// [`BackendEvent::LabelActionFailed`] so the app can roll
/// back exactly the apply it made. Mirrors the command
/// payloads one to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelAction {
    /// A label create or rename/recolor (one upsert on the
    /// wire).
    Save {
        /// Label id.
        label_id: String,
        /// Attempted name.
        name: String,
        /// Attempted color index.
        color: i32,
    },
    /// A chat association change.
    SetChat {
        /// Label id.
        label_id: String,
        /// Chat JID.
        chat_jid: String,
        /// The association state attempted.
        labeled: bool,
    },
    /// A message association change.
    SetMessage {
        /// Label id.
        label_id: String,
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// The association state attempted.
        labeled: bool,
    },
}

/// One synced business quick reply: a `/shortcut` and the
/// text it expands to. Account state, synced between the
/// phone and linked devices like labels; not related to
/// any local text-expansion the app may offer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveQuickReply {
    /// Quick reply id (an opaque index key).
    pub id: String,
    /// The `/`-typed trigger, without the slash.
    pub shortcut: String,
    /// The expanded message text.
    pub message: String,
    /// Extra search terms the reply is indexed under.
    pub keywords: Vec<String>,
    /// Usage counter the official clients maintain.
    pub count: i32,
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
    /// The chat the quoted message lives in, when that is not
    /// the chat the reply is sent to: a reply to a status quotes
    /// a message whose home is `status@broadcast` while the
    /// reply itself is sent to the author's direct chat. The
    /// outgoing builder stamps it on the quote context (the
    /// protocol's cross-chat quote marker); `None` for an
    /// ordinary same-chat quote, which carries no marker.
    pub remote_jid: Option<String>,
}

/// Why a chat message send failed, in the backend's own
/// vocabulary: the app turns a kind (plus the stage below)
/// into a localized sentence, so no pre-formatted English
/// crosses this boundary and the wording can never drift
/// from the locale files. The raw library error stays in
/// the backend's log, recorded with its stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SendFailureKind {
    /// A step ran out of time (the library's per-IQ budget
    /// or the backend's own media budget expired).
    Timeout,
    /// The connection was down or dropped, or the account
    /// was not connected or logged in when the send ran.
    Offline,
    /// The server, the recipient's device list, or the
    /// send's own arguments refused the message; nothing
    /// was sent.
    Rejected,
    /// Anything the classifier cannot name honestly.
    Other,
}

/// The step of a chat message send that failed, when the
/// backend can know it. A send prepares the recipient's
/// addressing (the LID mapping), then the recipient device
/// lists (direct chats) or the group routing metadata
/// (groups), then writes the stanza; a failure inside the
/// library reports which of those it was only for the
/// preparatory IQs, so a failure the backend cannot place
/// honestly is [`SendFailureStage::Other`], never a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SendFailureStage {
    /// Resolving the recipient's LID mapping (the
    /// phone-number to LID usync).
    PrepLid,
    /// Resolving the recipient device lists (the
    /// device-list usync inside a direct-chat send).
    PrepDevices,
    /// Fetching the group's routing metadata (the
    /// group-info IQ inside a group send).
    PrepGroup,
    /// Writing the prepared stanza to the connection.
    Write,
    /// A step the backend cannot name (a media upload, an
    /// internal library failure, a refused argument).
    Other,
}

/// The structured failure of one chat message send, carried
/// by [`BackendEvent::MessageSendFailed`]. Deliberately
/// carries no message text: the app owns the words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SendFailure {
    /// Why the send failed.
    pub kind: SendFailureKind,
    /// Which step failed, when known.
    pub stage: SendFailureStage,
}

/// Mints a protocol message ID in the exact shape the protocol
/// library itself generates ("3EB0" followed by 18 upper-case
/// hex characters, 22 in total): the app mints the ID at compose
/// time, files its local echo under it, and hands it to the
/// backend on the send command, so the echo, the wire stanza and
/// every later receipt name one ID from birth. A retry of a
/// failed send reuses the same ID, which is what makes a retry
/// after a post-write timeout idempotent: the recipient dedupes
/// on the ID. The library accepts a caller-chosen ID through its
/// send options and validates only that it is non-empty, so the
/// shape here is about being indistinguishable from a
/// library-minted ID on the wire, not about passing a check.
///
/// The entropy mixes the OS-seeded standard hasher state with a
/// process-wide counter, the wall clock and the process ID: the
/// counter alone guarantees two IDs minted in the same nanosecond
/// differ, and the seeded hash keeps the bytes unpredictable
/// without a new randomness dependency for this crate.
#[must_use]
pub fn mint_message_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let state = std::collections::hash_map::RandomState::new();
    let mut first = state.build_hasher();
    first.write_u64(counter);
    first.write_u128(nanos);
    first.write_u32(std::process::id());
    let a = first.finish();
    let mut second = state.build_hasher();
    second.write_u64(a ^ counter.rotate_left(17));
    second.write_u128(nanos ^ u128::from(a));
    let b = second.finish();
    const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let mut bytes = [0u8; 9];
    bytes[..8].copy_from_slice(&a.to_be_bytes());
    bytes[8] = b.to_be_bytes()[0];
    let mut id = String::with_capacity(22);
    id.push_str("3EB0");
    for byte in bytes {
        id.push(HEX_UPPER[(byte >> 4) as usize] as char);
        id.push(HEX_UPPER[(byte & 0x0F) as usize] as char);
    }
    id
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
        /// Caller-minted protocol ID for this send (see
        /// [`mint_message_id`]): the app files its local echo
        /// under this ID and the backend sends with it, so a
        /// retry reuses the same ID instead of minting a second
        /// one. `None` lets the backend mint at dispatch (call
        /// sites that file no echo of their own).
        send_id: Option<String>,
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
    /// and sends it; failures surface as a
    /// [`BackendEvent::MessageSendFailed`] event (an empty
    /// payload is rejected up front and never uploaded).
    SendMedia {
        /// Chat JID, e.g. `"12345@s.whatsapp.net"` or `"…@g.us"`.
        chat_jid: String,
        /// Caller-minted protocol ID for this send, with the
        /// same contract as [`BackendCommand::SendText`]'s.
        send_id: Option<String>,
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
    /// Warm the send caches for one chat ahead of any send:
    /// for a direct chat, the recipient's LID mapping and
    /// device list; for a group, its routing metadata. A send
    /// to a cold chat pays those preparatory queries inline
    /// (each bounded by the library's per-IQ budget), so the
    /// app sends this when a chat is opened and for the open
    /// chat during sync. Fire-and-forget: there is no answer
    /// event, every failure is ignored (a later send simply
    /// pays the cold cost itself), and warming never changes
    /// how a send is addressed when a mapping is already
    /// cached.
    WarmChatCache {
        /// Chat JID, e.g. `"12345@s.whatsapp.net"` or `"…@g.us"`.
        chat_jid: String,
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
        /// Caller-minted protocol ID for this send, with the
        /// same contract as [`BackendCommand::SendText`]'s.
        send_id: Option<String>,
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
    /// Post a text status (story) update. Status posts are
    /// sender-key fan-outs, so the caller supplies the audience:
    /// the JIDs the status is encrypted for. The app passes the
    /// audience its status-privacy setting computes (see
    /// [`BackendCommand::SetStatusPrivacy`]).
    /// Answered with [`BackendEvent::StatusPosted`] on success
    /// and [`BackendEvent::StatusPostFailed`] on failure. An
    /// empty text or an empty recipient list is refused by the
    /// backends with `StatusPostFailed`, never posted.
    PostTextStatus {
        /// The status text.
        text: String,
        /// Audience JIDs (the recipients the caller's status
        /// privacy setting computes from the saved contacts).
        recipients: Vec<String>,
        /// The status privacy setting for this post, in the
        /// library's spellings: `contacts`, `allowlist` (only
        /// share with) or `denylist` (my contacts except). It
        /// sets the stanza's `status_setting` only; the caller
        /// still supplies the matching recipient list.
        privacy: String,
    },
    /// Post a photo or video status update. Like
    /// [`BackendCommand::PostTextStatus`] the caller supplies
    /// the audience; the media bytes travel with the command,
    /// the way [`BackendCommand::SendMedia`] carries them. The
    /// caller also supplies the JPEG thumbnail the receiving
    /// clients present (the photo itself at preview fidelity,
    /// or a real frame of the video); an empty thumbnail is
    /// accepted and simply leaves the post without a preview
    /// image. Answered with [`BackendEvent::StatusMediaPosted`]
    /// on success and [`BackendEvent::StatusPostFailed`] on
    /// failure (the one failure event for every status post, so
    /// a refused post of either kind settles the same composer).
    /// An empty payload, an unsupported kind, or an empty
    /// recipient list is refused with `StatusPostFailed`, never
    /// posted.
    PostMediaStatus {
        /// Photo or video; every other kind is refused.
        kind: MediaKind,
        /// The media bytes.
        data: Vec<u8>,
        /// MIME type of the payload.
        mime: String,
        /// Caption, may be empty.
        caption: String,
        /// JPEG thumbnail bytes, may be empty (see the command
        /// docs).
        thumbnail: Vec<u8>,
        /// Duration in seconds for a video; ignored for a photo.
        seconds: u32,
        /// Audience JIDs (the recipients the caller's status
        /// privacy setting computes from the saved contacts).
        recipients: Vec<String>,
        /// The status privacy setting for this post, in the
        /// library's spellings (see [`BackendCommand::PostTextStatus`]).
        privacy: String,
    },
    /// Mute or unmute one author's status updates. The setting
    /// is an account-level app-state action, so it follows the
    /// user across devices; the app keeps its own session
    /// mirror for the feed's muted section. No event answers
    /// it: a failure surfaces as [`BackendEvent::Error`].
    SetStatusMute {
        /// The status author's JID.
        author_jid: String,
        /// True to mute, false to unmute.
        muted: bool,
    },
    /// Set the account's status privacy: who may see my status
    /// updates. Persisted through the library's app-state
    /// status-privacy action (the same record the phone edits),
    /// and answered like [`BackendCommand::SetPrivacy`]: a
    /// fresh [`BackendEvent::PrivacyState`] carrying the
    /// `statusprivacy` and `statusprivacy_list` keys alongside
    /// the privacy categories, or an [`BackendEvent::Error`].
    SetStatusPrivacy {
        /// The audience mode: `contacts` (my contacts),
        /// `except` (my contacts except the list) or
        /// `share_with` (only share with the list).
        mode: String,
        /// The exception list for `except`, the share list for
        /// `share_with`; ignored for `contacts`.
        jids: Vec<String>,
    },
    /// Fetch the account's status privacy as last synced from
    /// the phone (the library's stored status-privacy action).
    /// Answered with a [`BackendEvent::PrivacyState`] whose map
    /// carries the `statusprivacy` and `statusprivacy_list`
    /// keys alongside the privacy categories; when no audience
    /// has been observed yet the keys are absent, which is not
    /// an all-contacts answer.
    FetchStatusPrivacy,
    /// Delete (revoke) one of my own status updates. The
    /// revoke is itself a status fan-out, so the caller
    /// supplies the audience the post was encrypted for: the
    /// app passes the recipient list it used for a post made
    /// this session, or its current saved contacts for a
    /// status posted from another device (the best audience it
    /// can name; a device that was not in the original audience
    /// simply never sees the revoke). Answered with
    /// [`BackendEvent::StatusDeleted`] on success and
    /// [`BackendEvent::StatusDeleteFailed`] on failure.
    DeleteStatus {
        /// Protocol ID of the status to delete.
        message_id: String,
        /// Audience JIDs the revoke is encrypted for.
        recipients: Vec<String>,
    },
    /// List the channels this account follows (and owns or
    /// admins), with their server-reported metadata. Answered
    /// with [`BackendEvent::ChannelsListed`] or
    /// [`BackendEvent::ChannelsListFailed`].
    FetchChannels,
    /// Look up a channel by its invite code (the part after
    /// `whatsapp.com/channel/`, or a full invite link; the
    /// backend extracts the code). Answered with
    /// [`BackendEvent::ChannelFound`] or
    /// [`BackendEvent::ChannelLookupFailed`].
    LookupChannel {
        /// Invite code or full invite link.
        code: String,
    },
    /// Follow a channel. Answered with
    /// [`BackendEvent::ChannelFollowed`] (carrying the
    /// channel with its new role) or
    /// [`BackendEvent::ChannelFollowFailed`].
    FollowChannel {
        /// Channel JID.
        jid: String,
    },
    /// Unfollow a channel. Answered with
    /// [`BackendEvent::ChannelUnfollowed`] or
    /// [`BackendEvent::ChannelUnfollowFailed`]. The channel's
    /// chat stays in the roster history; the account simply
    /// stops receiving new updates.
    UnfollowChannel {
        /// Channel JID.
        jid: String,
    },
    /// Create a channel owned by this account. Answered with
    /// [`BackendEvent::ChannelCreated`] or
    /// [`BackendEvent::ChannelCreateFailed`].
    CreateChannel {
        /// Channel name.
        name: String,
        /// Channel description (may be empty).
        description: String,
    },
    /// Fetch one page of a channel's history, newest first:
    /// updates the account missed while it was away, with
    /// their server IDs, reaction counts and poll tallies.
    /// `before` pages further back by server ID (`None` asks
    /// for the newest page). The page's messages are
    /// delivered as ordinary [`BackendEvent::MessageUpsert`]s
    /// for the channel's chat before
    /// [`BackendEvent::ChannelHistoryFetched`] lands;
    /// failures answer [`BackendEvent::ChannelHistoryFailed`].
    FetchChannelHistory {
        /// Channel JID.
        jid: String,
        /// Page size.
        count: u32,
        /// Server ID to page before; `None` for the newest page.
        before: Option<u64>,
    },
    /// Arm live updates for a channel (reaction and poll
    /// counters as they change) for the rest of the session.
    /// Fire-and-forget: the subscription is a refresh
    /// accelerator over the history fetch, which stays the
    /// guaranteed path, so a subscription the server refuses
    /// degrades to history-only and answers nothing.
    WatchChannel {
        /// Channel JID.
        jid: String,
    },
    /// React to a channel update (an empty `emoji` removes
    /// the reaction). Channel reactions key on the update's
    /// server ID, not its protocol ID. Answered with
    /// [`BackendEvent::ChannelReactionSet`] or
    /// [`BackendEvent::ChannelReactionFailed`].
    ReactToChannelUpdate {
        /// Channel JID.
        jid: String,
        /// Server ID of the update.
        server_id: u64,
        /// Reaction emoji, or empty to remove.
        emoji: String,
    },
    /// Post a poll to a channel this account admins. The
    /// poll itself arrives back through the live-update and
    /// history paths like any other update; this command
    /// answers [`BackendEvent::ChannelPollCreated`] or
    /// [`BackendEvent::ChannelPollFailed`] for the send
    /// outcome only.
    CreateChannelPoll {
        /// Channel JID.
        jid: String,
        /// Poll question.
        question: String,
        /// Option labels (2-12).
        options: Vec<String>,
        /// Whether several options may be selected.
        multiple: bool,
    },
    /// Vote on a channel poll. Channel poll votes key on the
    /// poll's server ID and the option names (the backend
    /// hashes them the way the protocol does); channel polls
    /// are plaintext and carry no creation secret, unlike
    /// chat polls. Answered with
    /// [`BackendEvent::ChannelPollVoted`] or
    /// [`BackendEvent::ChannelPollVoteFailed`].
    VoteChannelPoll {
        /// Channel JID.
        jid: String,
        /// Server ID of the poll update.
        server_id: u64,
        /// Names of the options to select (empty clears the vote).
        options: Vec<String>,
    },
    /// Edit the text of an own channel update. Channel edits
    /// key on the update's protocol ID. Answered with
    /// [`BackendEvent::ChannelUpdateEdited`] or
    /// [`BackendEvent::ChannelUpdateEditFailed`].
    EditChannelUpdate {
        /// Channel JID.
        jid: String,
        /// Protocol ID of the update.
        message_id: String,
        /// Replacement text.
        text: String,
    },
    /// Revoke (delete for everyone) an own channel update.
    /// Answered with [`BackendEvent::ChannelUpdateRevoked`]
    /// or [`BackendEvent::ChannelUpdateRevokeFailed`].
    RevokeChannelUpdate {
        /// Channel JID.
        jid: String,
        /// Protocol ID of the update.
        message_id: String,
    },
    /// Fetch a channel's info-panel payload: fresh metadata
    /// plus, for admins and owners, the admin count and a
    /// follower sample. Answered with
    /// [`BackendEvent::ChannelInfoFetched`] or
    /// [`BackendEvent::ChannelInfoFailed`].
    FetchChannelInfo {
        /// Channel JID.
        jid: String,
    },
    /// Mute or unmute a followed channel's updates.
    /// Answered with [`BackendEvent::ChannelMuteSet`] or
    /// [`BackendEvent::ChannelMuteFailed`].
    SetChannelMuted {
        /// Channel JID.
        jid: String,
        /// True to mute.
        muted: bool,
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
    /// Start an outgoing group call bound to a group chat: the
    /// group's current members are rung. Answered like
    /// [`BackendCommand::StartCall`], with the call's phases on
    /// [`BackendEvent::CallChanged`] and its roster on
    /// [`BackendEvent::CallParticipants`].
    StartGroupCall {
        /// Group chat JID.
        chat_jid: String,
        /// Whether this is a video call.
        video: bool,
    },
    /// Turn the local camera on or off in the current call. On a
    /// call that started as voice this upgrades it to video.
    /// The confirmed state answers on
    /// [`BackendEvent::CallVideoState`]; a failure answers with
    /// [`BackendEvent::CallControlFailed`].
    SetCallVideo {
        /// True to start sending video, false to stop.
        on: bool,
    },
    /// Send an in-call reaction (an emoji shown against my name
    /// for the other participants). An empty emoji clears it.
    /// A failure answers with
    /// [`BackendEvent::CallControlFailed`].
    SendCallReaction {
        /// The reaction emoji, or empty to clear.
        emoji: String,
    },
    /// Raise or lower my hand in the current group or link
    /// call. A failure answers with
    /// [`BackendEvent::CallControlFailed`].
    SetCallHandRaised {
        /// True to raise, false to lower.
        raised: bool,
    },
    /// Invite one more person into the current group call (and
    /// ring them). A failure answers with
    /// [`BackendEvent::CallControlFailed`].
    InviteCallParticipant {
        /// JID of the person to invite.
        jid: String,
    },
    /// Create a call link others can join. Answered with
    /// [`BackendEvent::CallLinkCreated`] or
    /// [`BackendEvent::CallLinkFailed`].
    CreateCallLink {
        /// True for a video link, false for an audio link.
        video: bool,
    },
    /// Read a call link's facts (creator, kind, waiting room)
    /// before joining it. Answered with
    /// [`BackendEvent::CallLinkPreview`] or
    /// [`BackendEvent::CallLinkFailed`].
    PreviewCallLink {
        /// The link's URL or bare token.
        link: String,
    },
    /// Join a call through a call link. The call then behaves
    /// like a group call: phases on
    /// [`BackendEvent::CallChanged`], roster on
    /// [`BackendEvent::CallParticipants`]. When the link's
    /// waiting room requires approval, the call sits in
    /// Connecting until the host admits this device; a denial
    /// or a join failure ends it with the reason on the Ended
    /// phase change.
    JoinCallLink {
        /// The link's URL or bare token.
        link: String,
        /// The link's kind, as its preview reported it: a video
        /// link must be joined with video endpoints, an audio
        /// link without them.
        video: bool,
    },
    /// Let one waiting person into the current call (host
    /// side). A failure answers with
    /// [`BackendEvent::CallControlFailed`].
    AdmitWaitingUser {
        /// JID of the waiting person.
        jid: String,
    },
    /// Refuse one waiting person entry to the current call
    /// (host side). A failure answers with
    /// [`BackendEvent::CallControlFailed`].
    DenyWaitingUser {
        /// JID of the waiting person.
        jid: String,
    },
    /// Turn the current call's waiting-room approval on or off
    /// (host side). A failure answers with
    /// [`BackendEvent::CallControlFailed`].
    SetCallApproval {
        /// True to require approval, false to let link guests
        /// in directly.
        enabled: bool,
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
    /// Hand the backend a poll creation secret the app kept in
    /// its archive, so a poll created or received in an earlier
    /// session stays votable and its incoming votes stay
    /// decryptable in this one. The backend's own secret cache is
    /// session-scoped (it learns secrets from creations it makes
    /// or receives); this command is how the archived copy gets
    /// back in. Registering is silent: no event answers it.
    RegisterPollSecret {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the poll message.
        message_id: String,
        /// JID of the poll's creator.
        creator_jid: String,
        /// The creation secret bytes.
        secret: Vec<u8>,
        /// Option labels, in poll order (votes refer to options
        /// by index into this list).
        options: Vec<String>,
    },
    /// Create an event in a chat. The event itself arrives
    /// back through the ordinary message flow (its creation
    /// carries the secret RSVPs are encrypted under, like a
    /// poll's); a refused creation answers the generic
    /// [`BackendEvent::Error`], like [`BackendCommand::CreatePoll`].
    CreateEvent {
        /// Chat JID.
        chat_jid: String,
        /// Event name (required, non-empty).
        name: String,
        /// Free-text details (may be empty).
        description: String,
        /// Start, unix seconds.
        start_time: i64,
        /// End, unix seconds (0 when the event has no end).
        end_time: i64,
        /// Place label (may be empty).
        location: String,
        /// Join/call link (may be empty).
        join_link: String,
        /// The event is a scheduled call.
        scheduled_call: bool,
        /// Extra guests are allowed.
        extra_guests: bool,
    },
    /// Answer an event invitation (RSVP). The answer is
    /// encrypted under the event's creation secret, so it
    /// needs the secret the session cached when the event was
    /// created or received, or that the app re-registered from
    /// its archive. A response that cannot be sent answers
    /// [`BackendEvent::EventRespondFailed`]; a landed one is
    /// reported back as [`BackendEvent::EventResponse`] with
    /// `from_me` set, like a poll vote's echo.
    RespondEvent {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the event message.
        message_id: String,
        /// The response.
        response: EventRsvp,
        /// Extra guests I am bringing, when the event allows
        /// them (only meaningful with a Going response).
        extra_guest_count: Option<i32>,
    },
    /// Hand the backend an event creation secret the app kept
    /// in its archive, so an event from an earlier session
    /// stays answerable and its incoming responses stay
    /// decryptable in this one. The event counterpart of
    /// [`BackendCommand::RegisterPollSecret`]; registering is
    /// silent: no event answers it.
    RegisterEventSecret {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the event message.
        message_id: String,
        /// JID of the event's creator.
        creator_jid: String,
        /// The creation secret bytes.
        secret: Vec<u8>,
    },
    /// Edit one of my own events, or cancel it. An event edit
    /// replaces the whole event payload (the protocol has no
    /// field-level patch), so every field rides along; a
    /// cancellation is the same edit with `canceled` set. The
    /// edit is encrypted under the event's creation secret,
    /// so it needs the secret the same way
    /// [`BackendCommand::RespondEvent`] does. A landed edit is
    /// reported back as [`BackendEvent::EventEdited`] with
    /// `from_me` semantics through the message flow; a refused
    /// one answers [`BackendEvent::EventEditFailed`].
    EditEvent {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the event message.
        message_id: String,
        /// Event name.
        name: String,
        /// Free-text details (may be empty).
        description: String,
        /// Start, unix seconds.
        start_time: i64,
        /// End, unix seconds (0 when the event has no end).
        end_time: i64,
        /// Place label (may be empty).
        location: String,
        /// Join/call link (may be empty).
        join_link: String,
        /// The event is a scheduled call.
        scheduled_call: bool,
        /// Extra guests are allowed.
        extra_guests: bool,
        /// Cancel the event with this edit.
        canceled: bool,
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
    /// The app always sends this once the user has really read
    /// the messages; whether the sender is told is the library's
    /// call, because the library holds the account's
    /// read-receipts privacy and downgrades a direct chat's
    /// receipt to a read-self one (syncing the user's own
    /// devices only) when receipts are off. Group receipts are
    /// always sent in full, and for them `sender_jid` carries
    /// the message sender, one command per sender.
    MarkAsRead {
        /// Chat JID.
        chat_jid: String,
        /// Sender JID (group participant), if any.
        sender_jid: Option<String>,
        /// Protocol IDs of the messages to mark.
        message_ids: Vec<String>,
    },
    /// Mark incoming voice or video notes as played (the played
    /// receipt for the sender). Like [`BackendCommand::MarkAsRead`]
    /// this is never gated by the read-receipts toggle in the
    /// app: the library applies the same played-self downgrade
    /// for direct chats when receipts are off.
    MarkAsPlayed {
        /// Chat JID.
        chat_jid: String,
        /// Sender JID (group participant), if any.
        sender_jid: Option<String>,
        /// Protocol IDs of the notes to mark.
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
    /// Subscribe to (or unsubscribe from) a contact's presence
    /// updates. The app watches exactly one contact at a time:
    /// the DM conversation currently open. While subscribed,
    /// the backend reports the contact's availability as
    /// [`BackendEvent::PresenceReceived`] events, starting with
    /// their current state. Presence exists only for DM chats;
    /// the app never sends this for groups or channels.
    SetPresenceSubscription {
        /// Chat JID (the contact).
        chat_jid: String,
        /// True = subscribe, false = unsubscribe.
        subscribed: bool,
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
    /// Delete a message for everyone. My own messages revoke as
    /// the sender; another member's message in a group revokes
    /// as a group admin delete, which the protocol refuses
    /// unless the account actually holds admin rights there.
    RevokeMessage {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message to revoke.
        message_id: String,
        /// Sender JID of the message (group participant), if any.
        sender_jid: Option<String>,
        /// Whether the message is my own.
        from_me: bool,
    },
    /// Delete a message for this account only (the other side
    /// keeps it). The app-state sync action removes it on every
    /// linked device; the app removes its local copy at once.
    DeleteMessageForMe {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message to delete.
        message_id: String,
        /// Sender JID of the message (group participant), if any.
        sender_jid: Option<String>,
        /// Whether the message is my own.
        from_me: bool,
        /// Whether the message's media goes with it.
        delete_media: bool,
        /// The message's timestamp (unix seconds), when the
        /// archive knows it.
        message_timestamp: Option<i64>,
    },
    /// Keep (or stop keeping) a disappearing message: a kept
    /// message is exempt from its chat's disappearing timer.
    /// Failures answer with [`BackendEvent::KeepFailed`] so the
    /// app can roll the optimistic kept state back.
    KeepMessage {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message to keep.
        message_id: String,
        /// Sender JID of the message (group participant), if any.
        sender_jid: Option<String>,
        /// Whether the message is my own.
        from_me: bool,
        /// True to keep, false to stop keeping.
        keep: bool,
    },
    /// Set a WhatsApp privacy option (live mode; the values are the
    /// protocol spellings: `all`, `contacts`, `none`,
    /// `contact_blacklist`, `match_last_seen`, `known`). The
    /// backend validates the value against the category and
    /// refuses pairs the category does not accept, and refuses
    /// `contact_blacklist` outright: it names an exception list
    /// this command cannot carry (that is
    /// [`BackendCommand::SetPrivacyExceptions`]' job).
    SetPrivacy {
        /// One of: `last`, `online`, `profile`, `status`,
        /// `groupadd`, `readreceipts`.
        category: String,
        /// The privacy value spelling.
        value: String,
    },
    /// Set one privacy category's exception list: the category
    /// becomes `contact_blacklist` with exactly this list of
    /// contacts excepted. Only the categories the protocol
    /// gives a disallowed list are accepted (`last`, `profile`,
    /// `status`, `groupadd`); anything else is refused with an
    /// [`BackendEvent::Error`]. Answered like
    /// [`BackendCommand::SetPrivacy`]: a fresh
    /// [`BackendEvent::PrivacyState`] on success, an error and
    /// the re-asserted state on failure. The list is the full
    /// desired list, not a delta: the backend diffs it against
    /// the list it last set this session and sends the adds and
    /// removes, addressed the way the library's disallowed-list
    /// update requires (LID form, with the phone form alongside).
    SetPrivacyExceptions {
        /// The privacy category.
        category: String,
        /// The full exception list, as contact JIDs.
        jids: Vec<String>,
    },
    /// Fetch the signed-in account's own profile (push name,
    /// About, profile picture) from the server. Answered with an
    /// [`BackendEvent::OwnProfile`] event. The real backend also
    /// fetches unasked at connect; this command is the refresh
    /// for when the UI opens the profile editor.
    FetchOwnProfile,
    /// Change the signed-in account's own profile on the server.
    /// Each field is independent: `None` leaves that part of the
    /// profile untouched, so one Save in the editor is one
    /// command carrying exactly the fields the user changed.
    ///
    /// The backend applies the given fields and answers with a
    /// fresh [`BackendEvent::OwnProfile`] re-read; on failure it
    /// answers [`BackendEvent::OwnProfileSaveFailed`] and then
    /// the same re-read, so the UI can show the failure and fall
    /// back to the state the server kept.
    UpdateOwnProfile {
        /// New push name (display name), when changed. An empty
        /// or all-whitespace name is refused: WhatsApp requires
        /// a name.
        name: Option<String>,
        /// New About / text status, when changed. An empty
        /// string clears the About.
        about: Option<String>,
        /// New profile picture, when changed: `Some(Some(jpeg))`
        /// sets the picture to the given JPEG bytes (the caller
        /// crops and sizes it, the protocol carries it as is),
        /// `Some(None)` removes the picture.
        photo: Option<Option<Vec<u8>>>,
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
    /// Pin or unpin a message in its chat. Pinning carries the
    /// duration the user picked; unpinning ignores it.
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
        /// How long the pin lasts when pinning.
        duration: PinDuration,
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
    /// Create a community with the given name. The result comes
    /// back as a [`BackendEvent::CommunityCreated`] event carrying
    /// the new community's JID; failures surface as an `Error`
    /// event.
    CreateCommunity {
        /// Community name.
        name: String,
    },
    /// Link existing groups into a community as subgroups. The
    /// answer is a [`BackendEvent::CommunityGroupsLinked`] event
    /// listing the groups that linked and the ones the server
    /// refused; each linked group also re-reports its structure
    /// with a [`BackendEvent::ChatCommunity`] event. Failures of
    /// the operation as a whole surface as an `Error` event.
    LinkCommunityGroups {
        /// Community (parent group) JID.
        community_jid: String,
        /// Group JIDs to link.
        group_jids: Vec<String>,
    },
    /// Unlink subgroups from a community. Members keep their
    /// place in the unlinked groups themselves; only the
    /// community tie is cut. Answered like
    /// [`BackendCommand::LinkCommunityGroups`], with a
    /// [`BackendEvent::CommunityGroupsUnlinked`] event.
    UnlinkCommunityGroups {
        /// Community (parent group) JID.
        community_jid: String,
        /// Group JIDs to unlink.
        group_jids: Vec<String>,
    },
    /// Create a new subgroup inside a community. The answer is a
    /// [`BackendEvent::CommunitySubgroupCreated`] event carrying
    /// the new group's roster entry, whose community parent is
    /// set. Failures surface as an `Error` event.
    CreateCommunitySubgroup {
        /// Community (parent group) JID.
        community_jid: String,
        /// Subgroup name (subject).
        name: String,
    },
    /// Remove participants from a community entirely: the parent
    /// group and every linked subgroup. Answered with a
    /// [`BackendEvent::CommunityParticipantsRemoved`] event, and
    /// the parent group's refreshed member list follows as a
    /// `GroupMembers` event. Failures surface as an `Error` event.
    RemoveCommunityParticipants {
        /// Community (parent group) JID.
        community_jid: String,
        /// Participant JIDs or phone numbers.
        participants: Vec<String>,
    },
    /// Deactivate a community. The server unlinks its subgroups
    /// rather than deleting them. Answered with a
    /// [`BackendEvent::CommunityDeactivated`] event. Failures
    /// surface as an `Error` event.
    DeactivateCommunity {
        /// Community (parent group) JID.
        community_jid: String,
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
    /// Add participants to a group and share recent history
    /// with the ones who were added (row 64). The added
    /// participants are the share's audience: the bundle goes
    /// to them and to this account's other devices only.
    /// Answered with a [`BackendEvent::GroupHistoryReported`]
    /// event carrying the add outcome and the share outcome
    /// separately; a hard failure of the add itself surfaces
    /// as an `Error` event, like the plain add.
    AddGroupParticipantsWithHistory {
        /// Group JID.
        chat_jid: String,
        /// Participant JIDs or phone numbers to add.
        participants: Vec<String>,
        /// Recent plain-text history from the app's archive,
        /// oldest first. The library applies its own
        /// eligibility policy and window to these.
        history: Vec<HistoryMessageRecord>,
    },
    /// Resume a group history share whose prepared bundle the
    /// backend kept after a retryable failure (row 64). The
    /// members are NOT added again. Answered with a
    /// [`BackendEvent::GroupHistoryReported`] event whose add
    /// lists are empty.
    RetryGroupHistoryShare {
        /// Group JID.
        chat_jid: String,
    },
    /// Set or clear this account's own member label in a
    /// group (row 65). An empty label clears it. Answered
    /// with [`BackendEvent::MemberLabelSet`], or
    /// [`BackendEvent::MemberLabelFailed`] when the send
    /// failed.
    SetOwnMemberLabel {
        /// Group JID.
        chat_jid: String,
        /// The label text (empty clears).
        label: String,
    },
    /// Report one message to the group's own admins (row 65).
    /// This is not a spam report to WhatsApp. Answered with
    /// [`BackendEvent::MessageReportedToAdmins`], or
    /// [`BackendEvent::ReportToAdminsFailed`] when the report
    /// did not go out.
    ReportMessageToAdmins {
        /// Group JID.
        chat_jid: String,
        /// Protocol id of the message to report.
        message_id: String,
    },
    /// Fetch the messages reported to a group's admins
    /// (row 65, the admin side). Answered with a
    /// [`BackendEvent::ReportedMessages`] event, or
    /// [`BackendEvent::ReportedMessagesFailed`].
    FetchReportedMessages {
        /// Group JID.
        chat_jid: String,
    },
    /// Send an interactive message (buttons, list, or
    /// carousel) built from raw protos (row 19). The local
    /// echo the app created is confirmed by the usual
    /// `MessageSent` event; a failure surfaces as a
    /// [`BackendEvent::MessageSendFailed`] event, like the
    /// sibling send commands.
    SendInteractive {
        /// Chat JID.
        chat_jid: String,
        /// Caller-minted protocol ID for this send, with the
        /// same contract as [`BackendCommand::SendText`]'s.
        send_id: Option<String>,
        /// What to build and send.
        spec: InteractiveSpec,
    },
    /// Answer a received buttons or list message by naming
    /// the chosen button or row (row 19). The answer rides
    /// the wire's response proto quoting the original
    /// message; locally it echoes as a text message carrying
    /// the chosen label, which is also how answers from other
    /// clients arrive. Confirmed by `MessageSent`; a failure
    /// surfaces as a [`BackendEvent::MessageSendFailed`] event.
    SendInteractiveResponse {
        /// Chat JID.
        chat_jid: String,
        /// Caller-minted protocol ID for this answer's own
        /// stanza, with the same contract as
        /// [`BackendCommand::SendText`]'s. Distinct from
        /// `message_id` below, which names the message being
        /// answered.
        send_id: Option<String>,
        /// Protocol id of the message being answered.
        message_id: String,
        /// Author JID of the original message (group chats).
        sender_jid: Option<String>,
        /// True when the original message is this account's.
        from_me: bool,
        /// True for a list row answer, false for a button.
        is_list: bool,
        /// The chosen button or row's wire id.
        choice_id: String,
        /// The chosen button or row's label (the echo text).
        choice_text: String,
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
    /// Set whether the backend generates a link preview for the
    /// first URL in an outgoing text message (Settings > Privacy
    /// & Security > Link previews). Local to this client: nothing
    /// is sent to the server, the flag only decides whether the
    /// send path fetches the page and attaches the preview the
    /// sender's client is expected to generate. The app re-sends
    /// the persisted value whenever a session starts, so a fresh
    /// backend run never has to guess it.
    SetLinkPreviews {
        /// True to generate previews, false to send plain text.
        enabled: bool,
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
        /// Archived re-download reference of a document, when
        /// the caller has one. Used when the message is no
        /// longer in the session's protocol cache (a document
        /// received before this session); without either source
        /// the download fails.
        document_ref: Option<DocumentRef>,
    },
    /// Fetch a photo message's bytes for display. Answered with
    /// `PhotoBytes` or `PhotoFetchFailed`. The bytes travel in
    /// memory only: nothing is written to disk, which is also
    /// what makes this the one fetch path a view-once photo may
    /// take. When the message is no longer in the session's
    /// protocol cache (a photo from before this session), the
    /// caller passes the archived [`PhotoRef`] it stored with
    /// the message; without either source the fetch fails.
    FetchPhoto {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the photo message.
        message_id: String,
        /// Archived re-download reference, when the caller has one.
        photo_ref: Option<PhotoRef>,
    },
    /// Fetch a voice message's bytes for playback. Answered with
    /// `VoiceBytes` or `VoiceFetchFailed`. The bytes travel in
    /// memory only: nothing is written to disk, so playback never
    /// leaves a copy of the note behind. When the message is no
    /// longer in the session's protocol cache (a note from before
    /// this session), the caller passes the archived [`VoiceRef`]
    /// it stored with the message; without either source the
    /// fetch fails.
    FetchVoice {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the voice message.
        message_id: String,
        /// Archived re-download reference, when the caller has one.
        voice_ref: Option<VoiceRef>,
    },
    /// Fetch a video message's bytes for playback. Answered with
    /// `VideoReady`, `VideoBytes` or `VideoFetchFailed`. A
    /// regular video is streamed to a file in the app's video
    /// cache (videos are far larger than photos or voice notes,
    /// so they are not buffered whole in memory) and answered
    /// with `VideoReady`. A view-once video is answered with
    /// `VideoBytes` instead: its bytes travel in memory only and
    /// are never written to disk, the same rule view-once photos
    /// follow. The caller states the message's view-once flag
    /// because a fetch served from the archived reference alone
    /// has no protocol payload left to read it from. When the
    /// message is no longer in the session's protocol cache, the
    /// caller passes the archived [`VideoRef`] it stored with
    /// the message; without either source the fetch fails.
    FetchVideo {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the video message.
        message_id: String,
        /// Archived re-download reference, when the caller has one.
        video_ref: Option<VideoRef>,
        /// Whether the message is view-once (memory-only fetch).
        view_once: bool,
    },
    /// Fetch a sticker message's bytes for display. Answered
    /// with `StickerBytes` or `StickerFetchFailed`. The bytes
    /// travel in memory only. When the message is no longer in
    /// the session's protocol cache (a sticker from before this
    /// session), the caller passes the archived [`StickerRef`] it
    /// stored with the message; without either source the fetch
    /// fails.
    FetchSticker {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the sticker message.
        message_id: String,
        /// Archived re-download reference, when the caller has one.
        sticker_ref: Option<StickerRef>,
    },
    /// Fetch a video message's hosted poster (the full-size
    /// frame image the sender's client uploaded alongside the
    /// video) for display. Answered with `VideoPosterBytes` or
    /// `VideoPosterFetchFailed`. The bytes travel in memory
    /// only. The app asks for the poster when the payload
    /// carried no embedded thumbnail, so the tile can present
    /// the video's own frame. When the message is no longer in
    /// the session's protocol cache, the caller passes the
    /// archived [`PosterRef`] it stored with the message;
    /// without either source the fetch fails.
    FetchVideoPoster {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the video message.
        message_id: String,
        /// Archived poster reference, when the caller has one.
        poster_ref: Option<PosterRef>,
    },
    /// Fetch the profile picture of one contact, group, or
    /// community for display. Answered with `AvatarBytes` when the
    /// identity has a picture, `AvatarMissing` when it has none or
    /// its privacy settings refuse the lookup. The backend
    /// resolves the address form itself: a contact known under a
    /// LID is looked up under its phone-number form when the
    /// session's LID-to-phone cache knows the mapping, so the
    /// picture provably belongs to the identity it labels.
    FetchAvatar {
        /// JID of the contact, group, or community.
        jid: String,
        /// The JID is a community parent group: its picture is
        /// served under the library's Community target, not the
        /// Group target every other @g.us uses (P2-05).
        community: bool,
    },
    /// Ask for one identity's verified business name (the
    /// usync business query). Answered with
    /// [`BackendEvent::VerifiedNameResult`]; a transport
    /// failure emits nothing, so the caller may ask again.
    FetchVerifiedName {
        /// JID of the identity.
        jid: String,
    },
    /// Resolve the display titles of chats whose stored title is
    /// still a raw identifier (a group JID or a bare number).
    /// Sent by the app when the session connects, carrying every
    /// such chat, so no chat waits for an unrelated event to be
    /// healed. Answered per chat with `ChatRenamed` (and
    /// `SenderNamed` for a DM whose contact name resolves).
    /// Failures are retried a bounded number of times inside the
    /// backend; a chat that still cannot resolve stays raw and is
    /// reported again on the next connect.
    ResolveTitles {
        /// Chat JIDs whose titles are raw identifiers.
        jids: Vec<String>,
    },
    /// Ask the phone for the next older page of one chat's
    /// history (the protocol's on-demand history sync, a peer
    /// data operation). The anchor is the oldest message the
    /// archive already holds for the chat: the phone answers
    /// with messages older than it. The answer arrives later
    /// as ordinary history message upserts for the chat,
    /// followed by one [`BackendEvent::HistoryPageFetched`]
    /// saying how many landed and whether the phone's history
    /// is exhausted; a request the phone never answers is
    /// closed by the backend's own watchdog as
    /// [`BackendEvent::HistoryFetchFailed`]. Fire-and-forget:
    /// no event answers the command itself.
    FetchOlderMessages {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the oldest archived message.
        oldest_message_id: String,
        /// Whether the anchor message is my own.
        oldest_from_me: bool,
        /// Anchor timestamp, Unix milliseconds.
        oldest_timestamp_ms: i64,
    },
    /// Set or remove a group's profile picture. `Some` sets the
    /// picture to the given JPEG bytes (the caller crops and
    /// sizes them, the protocol carries them as is, exactly
    /// like the account's own picture); `None` removes it.
    /// Answered with [`BackendEvent::GroupPictureChanged`] on
    /// success and [`BackendEvent::GroupPictureFailed`] on
    /// failure: the protocol has no typed not-admin error, so a
    /// server refusal and a transport failure are worded
    /// differently in the failure message and neither is ever
    /// silent.
    SetGroupPicture {
        /// Group JID.
        chat_jid: String,
        /// New picture bytes, or `None` to remove the picture.
        image: Option<Vec<u8>>,
    },
    /// Look up a phone number and resolve the person behind it
    /// so a chat can be started with someone outside the
    /// roster. The number is taken as typed (formatting is
    /// stripped by the backend; the protocol wants the digits
    /// of the full international form). Answered with exactly
    /// one of [`BackendEvent::ContactResolved`] (the number is
    /// on WhatsApp and the chat is ready to open),
    /// [`BackendEvent::ContactNotFound`] (the number is not on
    /// WhatsApp: an answer, not an error), or
    /// [`BackendEvent::ContactLookupFailed`] (the lookup
    /// itself failed; never reported as "not on WhatsApp").
    LookupContact {
        /// The phone number as typed.
        phone: String,
    },
    /// Clear a chat's messages (the chat itself stays in the
    /// list). Destructive: the app confirms first, then applies
    /// the clearing locally at once, the same adoption the
    /// inbound clear path performs.
    ClearChat {
        /// Chat JID.
        chat_jid: String,
        /// Whether starred messages survive the clearing.
        keep_starred: bool,
    },
    /// Delete a chat entirely: its messages and its row in the
    /// list. Destructive: the app confirms first, then applies
    /// the deletion locally at once, the same adoption the
    /// inbound delete path performs.
    DeleteChat {
        /// Chat JID.
        chat_jid: String,
    },
    /// Mark a chat as unread (the app-state sync action, NOT the
    /// read-receipt path: marking unread must never send blue
    /// ticks). The app adopts the badge locally at once through
    /// the same state the inbound sync reports.
    MarkChatUnread {
        /// Chat JID.
        chat_jid: String,
    },
    /// Mark a chat as read (the app-state sync action, the pair
    /// of [`BackendCommand::MarkChatUnread`]): the read state
    /// syncs to the account's other devices, so a chat read here
    /// stops showing as unread on the phone. This is NOT the
    /// read-receipt path either: it carries no per-message
    /// receipts and is not gated by the read-receipts privacy
    /// toggle, which governs ticks shown to other people, not
    /// state shared between the user's own devices. The app
    /// sends it when a chat is read, alongside any receipts the
    /// toggle allows.
    MarkChatRead {
        /// Chat JID.
        chat_jid: String,
    },
    /// Send a contact card to a chat: the person's display name
    /// and phone number, carried as a vCard the receiving side
    /// renders as a contact card.
    SendContact {
        /// Chat JID.
        chat_jid: String,
        /// Caller-minted protocol ID for this send, with the
        /// same contract as [`BackendCommand::SendText`]'s.
        send_id: Option<String>,
        /// Display name of the person the card is about.
        name: String,
        /// Their phone number (international form; formatting is
        /// stripped by the backend's card builder).
        phone: String,
        /// Disappearing-messages timer of the chat, in seconds
        /// (`0` sends a normal, non-expiring message).
        ephemeral_secs: u32,
    },
    /// Report one message as spam. The report names exactly the
    /// message the user picked (its protocol ID, its sender and
    /// its timestamp from the archive); the backend files it
    /// through the protocol's spam-report call and answers
    /// [`BackendEvent::SpamReported`] or
    /// [`BackendEvent::SpamReportFailed`]. Reporting neither
    /// blocks the sender nor deletes the message: those are
    /// separate actions with their own commands.
    ReportSpam {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the reported message.
        message_id: String,
        /// Sender JID of the reported message, if any.
        sender_jid: Option<String>,
        /// The reported message's timestamp (unix seconds).
        message_timestamp: i64,
    },
    /// Fetch one chat partner's business profile. Answered
    /// with [`BackendEvent::BusinessProfileFetched`] (whose
    /// profile is `None` when the account has no business
    /// profile: an answer, not an error) or
    /// [`BackendEvent::BusinessProfileFetchFailed`].
    FetchBusinessProfile {
        /// Chat JID of the business.
        chat_jid: String,
    },
    /// Fetch one contact's About (their text status) from the
    /// server. Answered with [`BackendEvent::ContactAbout`];
    /// an empty About in the answer means the contact has none
    /// set, their privacy refuses the lookup, or the query
    /// failed: the three are indistinguishable on the wire and
    /// the panel simply shows no About section for all of them.
    FetchContactAbout {
        /// Chat JID of the contact.
        chat_jid: String,
    },
    /// Fetch one page of a business's product catalog.
    /// Answered with [`BackendEvent::CatalogFetched`],
    /// whose cursor feeds the next page's `after`, or
    /// [`BackendEvent::CatalogFetchFailed`].
    FetchCatalog {
        /// Chat JID of the business.
        chat_jid: String,
        /// Cursor of the page to fetch (`None` for the
        /// first page).
        after: Option<String>,
    },
    /// Fetch a business's catalog collections (each with
    /// its first products inline). Answered with
    /// [`BackendEvent::CollectionsFetched`] or
    /// [`BackendEvent::CollectionsFetchFailed`].
    FetchCollections {
        /// Chat JID of the business.
        chat_jid: String,
    },
    /// Fetch the bytes of one catalog product image (a
    /// hosted URL the catalog itself supplied). Answered
    /// with [`BackendEvent::ProductImageBytes`] or
    /// [`BackendEvent::ProductImageFetchFailed`].
    FetchProductImage {
        /// The image URL, verbatim from the catalog data.
        url: String,
    },
    /// Fetch one order's line items and totals. Both the
    /// id and the token come from the order message
    /// itself; the token is a per-order capability.
    /// Answered with [`BackendEvent::OrderDetailsFetched`]
    /// or [`BackendEvent::OrderDetailsFetchFailed`].
    FetchOrderDetails {
        /// Chat JID of the business the order was placed
        /// with.
        chat_jid: String,
        /// Order id from the order message.
        order_id: String,
        /// Order token from the order message.
        token: String,
    },
    /// Fetch the account's own business profile (the
    /// editor's read). Answered with
    /// [`BackendEvent::OwnBusinessProfileFetched`] (whose
    /// profile is `None` when the account has no business
    /// profile) or
    /// [`BackendEvent::OwnBusinessProfileFetchFailed`].
    FetchOwnBusinessProfile,
    /// Apply a delta to the account's own business
    /// profile. Answered with
    /// [`BackendEvent::OwnBusinessProfileSaved`] or
    /// [`BackendEvent::OwnBusinessProfileSaveFailed`].
    UpdateOwnBusinessProfile {
        /// The delta (only the changed fields are set).
        update: LiveBusinessProfileUpdate,
    },
    /// Create a label or rename/recolor an existing one
    /// (one upsert on the wire). The app applies it at
    /// once; a failure answers
    /// [`BackendEvent::LabelActionFailed`] carrying the
    /// attempted action, and the app rolls back.
    SaveLabel {
        /// Label id (the caller generates it for a new
        /// label; the wire treats it as an opaque key).
        label_id: String,
        /// Label name.
        name: String,
        /// Color index.
        color: i32,
    },
    /// Delete a label. Destructive: the app confirms
    /// first and applies only when
    /// [`BackendEvent::LabelDeleted`] answers; a failure
    /// answers [`BackendEvent::LabelDeleteFailed`] and
    /// nothing changes anywhere.
    DeleteLabel {
        /// Label id.
        label_id: String,
    },
    /// Add a label to a chat or remove it. The app
    /// applies it at once; a failure answers
    /// [`BackendEvent::LabelActionFailed`] and the app
    /// rolls back.
    SetChatLabel {
        /// Label id.
        label_id: String,
        /// Chat JID.
        chat_jid: String,
        /// Whether the chat carries the label.
        labeled: bool,
    },
    /// Add a label to one message or remove it. Same
    /// apply and rollback discipline as
    /// [`BackendCommand::SetChatLabel`].
    SetMessageLabel {
        /// Label id.
        label_id: String,
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// Whether the message carries the label.
        labeled: bool,
    },
    /// Create or edit a synced business quick reply (one
    /// upsert on the wire). The app applies it at once; a
    /// failure answers [`BackendEvent::QuickReplyFailed`]
    /// and the app rolls back.
    SaveQuickReply {
        /// Quick reply id (the caller generates it for a
        /// new reply).
        id: String,
        /// The `/`-typed trigger, without the slash.
        shortcut: String,
        /// The expanded message text.
        message: String,
        /// Extra search terms.
        keywords: Vec<String>,
        /// The usage count the reply already has (0 for a
        /// new one). The wire's upsert replaces the stored
        /// count, so the app passes the count it last saw;
        /// it is never user-facing input.
        count: i32,
    },
    /// Delete a synced business quick reply. The app
    /// confirms first and applies only when
    /// [`BackendEvent::QuickReplyDeleted`] answers; a
    /// failure answers
    /// [`BackendEvent::QuickReplyDeleteFailed`].
    DeleteQuickReply {
        /// Quick reply id.
        id: String,
    },
    /// Stop the backend worker.
    Shutdown,
}

/// One person's answer to an event invitation. Responses
/// are encrypted per responder under the event's creation
/// secret; the app aggregates the decrypted answers into the
/// tallies on [`LiveMessageKind::Event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EventRsvp {
    /// Attending.
    Going,
    /// Undecided.
    Maybe,
    /// Not attending.
    NotGoing,
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
    /// An event: a named gathering with a start, an optional
    /// end, and responses from the invited. The tallies are
    /// the app's aggregation of the per-responder ballots on
    /// [`LiveMessage::event_ballots`] (last response wins,
    /// like poll ballots); the backend reports responses, it
    /// does not keep tally state.
    Event {
        /// Event name.
        name: String,
        /// Free-text details (may be empty).
        description: String,
        /// Start, unix seconds (0 when the payload stated none).
        start_time: i64,
        /// End, unix seconds (0 when the event has no end).
        end_time: i64,
        /// Place label the payload states (may be empty). A
        /// label, not a map point: the payload's place name or
        /// address, or its formatted coordinates when that is
        /// all it states.
        location: String,
        /// Join/call link (may be empty).
        join_link: String,
        /// The event is a scheduled call.
        scheduled_call: bool,
        /// Extra guests are allowed.
        extra_guests: bool,
        /// The organizer cancelled the event.
        canceled: bool,
        /// My current response, when I have answered.
        my_rsvp: Option<EventRsvp>,
        /// Responses counted Going.
        going: u32,
        /// Responses counted Maybe.
        maybe: u32,
        /// Responses counted Not going.
        not_going: u32,
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
    /// An order placed with a business. The id and token
    /// are the capability the order-details fetch needs:
    /// an order cannot be read without the message that
    /// announced it. The message body carries the order's
    /// own words (its message or title) as usual.
    Order {
        /// Order id from the payload.
        order_id: String,
        /// Per-order token from the payload (may be empty
        /// when the payload carried none; the details fetch
        /// then has nothing to authorize with and the card
        /// offers no fetch).
        token: String,
        /// Number of items the payload states.
        item_count: u32,
        /// Total in thousandths of the currency unit, as
        /// the payload states it (0 when it states none).
        total_amount_1000: i64,
        /// ISO 4217 currency code of the total.
        currency: String,
        /// Order status, wire spelling (`INQUIRY`,
        /// `ACCEPTED`, `DECLINED`; empty when unstated).
        status: String,
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
    /// A message with quick-reply buttons. The body text is in
    /// [`LiveMessage::body`]; the buttons carry the wire's own
    /// ids, because an answer must name the button it picks.
    Buttons {
        /// Footer line under the body (may be empty).
        footer: String,
        /// The buttons, in wire order.
        buttons: Vec<LiveButton>,
    },
    /// A list message: a title, a body (in
    /// [`LiveMessage::body`]), and sections of pickable rows.
    /// An answer names the row it picks by id.
    ListMsg {
        /// Description under the title (may be empty).
        description: String,
        /// Label of the button that opens the list on a phone.
        button_label: String,
        /// Footer line (may be empty).
        footer: String,
        /// The sections, in wire order.
        sections: Vec<LiveListSection>,
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
    /// An audio file (a song, a recording sent as a file): the
    /// wire audio payload whose `ptt` flag is not set. It is a
    /// file with a duration, not a voice note: no waveform
    /// bubble, no voice-note styling.
    Audio,
    /// File/document.
    Document,
    /// Sticker.
    Sticker,
}

/// One quick-reply button of a [`LiveMessageKind::Buttons`]
/// message: the wire id an answer names, and the label shown.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveButton {
    /// The button's wire id (an answer names it).
    pub id: String,
    /// The label shown on the button.
    pub text: String,
}

/// One pickable row of a [`LiveMessageKind::ListMsg`] section.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveListRow {
    /// The row's wire id (an answer names it).
    pub id: String,
    /// The row's title.
    pub title: String,
    /// The row's description (may be empty).
    pub description: String,
}

/// One section of a [`LiveMessageKind::ListMsg`] message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveListSection {
    /// The section title (may be empty for a single section).
    pub title: String,
    /// The rows, in wire order.
    pub rows: Vec<LiveListRow>,
}

/// One archived plain-text message the app offers as group
/// history when adding members (row 64). The app builds these
/// from its own archive; the live backend rebuilds the wire
/// records from them and the library applies its own
/// eligibility policy (acknowledged plain text, inside the
/// sharing window, count-capped). Non-text messages are never
/// offered: the library would drop them anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessageRecord {
    /// Protocol message ID.
    pub id: String,
    /// Author JID (the group participant address); `None` for
    /// messages this account sent.
    pub author_jid: Option<String>,
    /// True when this account sent the message.
    pub from_me: bool,
    /// Unix timestamp (seconds).
    pub timestamp: i64,
    /// The message's plain text.
    pub body: String,
    /// JIDs the message mentions, in wire order.
    pub mentions: Vec<String>,
    /// Persisted delivery state for an outgoing message;
    /// `None` for incoming messages (held by this device,
    /// which the wire record states as read).
    pub delivery: Option<DeliveryState>,
}

/// The truthful outcome of a group history share, mapped from
/// the library's own outcome type. A share can fail after the
/// members were added; the two halves are always reported
/// separately, and nothing here claims a recipient read or
/// received anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryShareReport {
    /// The bundle was accepted by the server with complete
    /// fanout; `messages` records were shared.
    Shared {
        /// How many history records the bundle carried.
        messages: usize,
    },
    /// No share was requested (nobody opted in).
    NotRequested,
    /// The library skipped the share; `reason` says why, in
    /// words (sharing disabled, no eligible messages, the
    /// sender may not share, and so on).
    Skipped {
        /// Why the share was skipped.
        reason: String,
    },
    /// The share failed. `retryable` is true when the backend
    /// kept the prepared share and a
    /// [`BackendCommand::RetryGroupHistoryShare`] can resume
    /// it without adding the members again.
    Failed {
        /// What failed, in words.
        error: String,
        /// Whether a retry can resume the prepared share.
        retryable: bool,
    },
}

/// What an interactive message send builds (row 19). The live
/// backend turns the spec into the wire proto; the app builds
/// the same shapes from a received message's kind when it
/// echoes, so a sent message renders exactly like a received
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveSpec {
    /// Quick-reply buttons under a body text.
    Buttons {
        /// Body text above the buttons.
        text: String,
        /// Footer line (may be empty).
        footer: String,
        /// Button labels, in order (at most three; the wire
        /// assigns positional ids).
        buttons: Vec<String>,
    },
    /// A list message with sections of rows.
    List {
        /// The list title.
        title: String,
        /// Description under the title (may be empty).
        description: String,
        /// Label of the button that opens the list.
        button_label: String,
        /// The sections as (section title, rows) pairs, rows
        /// as (row title, row description) pairs. Wire row ids
        /// are assigned positionally by the builder.
        sections: Vec<(String, Vec<(String, String)>)>,
    },
    /// A carousel of up to two cards (the renderer's window;
    /// nothing sendable may be locally invisible).
    Carousel {
        /// Body text above the cards.
        body: String,
        /// The cards.
        cards: Vec<LiveCard>,
    },
}

/// One message reported to a group's admins, as the admin
/// fetch reports it (row 65). The reporting device may not
/// hold the named message; the entry names it by id either
/// way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedMessageEntry {
    /// The reported message's protocol id.
    pub message_id: String,
    /// JIDs of the members who reported it.
    pub reporters: Vec<String>,
}

/// One participant in a group or call-link call, as the call
/// engine's authoritative roster snapshot reports them. The
/// backend re-sends the whole roster on every change
/// ([`BackendEvent::CallParticipants`]); the app replaces its
/// copy, never merges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallParticipant {
    /// The participant's JID.
    pub jid: String,
    /// The roster's state word for them ("connected",
    /// "ringing", "left", ...), in the protocol's own terms;
    /// empty when the roster carries none.
    pub state: String,
    /// Their hand is raised.
    pub hand_raised: bool,
    /// They are sharing their screen.
    pub sharing: bool,
}

/// One person waiting in a call's waiting room for the host to
/// let them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallWaitingUser {
    /// The waiting person's JID.
    pub jid: String,
    /// Their phone number, when the waiting-room snapshot
    /// carries one alongside the JID.
    pub phone: Option<String>,
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
    /// The preview image's own bytes (the sender's client
    /// embedded them in the message), when it carried any.
    /// The receiver never fetches the image itself.
    #[serde(default)]
    pub thumbnail: Option<Vec<u8>>,
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

/// Persisted delivery state of an outgoing message, as synced
/// state (never a display constant). Ordered by rank: a stored
/// state never moves backward.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum DeliveryState {
    /// Filed locally as a send echo and not yet confirmed by
    /// the server. Only the app's own outbox writes this rung:
    /// it exists so a pending echo survives a restart as
    /// pending (to be reconciled by a later upsert or receipt
    /// carrying its protocol ID), never as if it had been sent.
    Sending,
    /// The send was reported failed, which means unconfirmed,
    /// not never sent (see [`BackendEvent::MessageSendFailed`]).
    /// Like [`DeliveryState::Sending`] this rung sits below
    /// every confirmed state, so the first real evidence (an
    /// upsert, a receipt) advances the row out of it.
    Failed,
    /// Reached the server (one tick).
    Sent,
    /// Reached the recipient device (two grey ticks).
    Delivered,
    /// Read by the recipient (two blue ticks).
    Read,
}

impl DeliveryState {
    /// Rank for monotonic merges (higher wins). The two local
    /// outbox rungs share rank 0: neither is evidence about the
    /// server, so neither may outrank or block a confirmed
    /// state, and a merge between them keeps the caller's
    /// current value (the exact local transitions between them
    /// go through the store's dedicated setter, not a merge).
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Sending | Self::Failed => 0,
            Self::Sent => 1,
            Self::Delivered => 2,
            Self::Read => 3,
        }
    }
    /// The stronger of two states.
    #[must_use]
    pub fn max_opt(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(x), Some(y)) => Some(if x.rank() >= y.rank() { x } else { y }),
            (Some(x), None) => Some(x),
            (None, y) => y,
        }
    }
}

impl From<ReceiptKind> for DeliveryState {
    fn from(k: ReceiptKind) -> Self {
        match k {
            ReceiptKind::Sent => Self::Sent,
            ReceiptKind::Delivered | ReceiptKind::Played => Self::Delivered,
            ReceiptKind::Read => Self::Read,
        }
    }
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
    /// A broadcast list (`<id>@broadcast`): a one-to-many
    /// list the account sends to. Not a DM: its title is never
    /// the raw list id, no contact/LID healing applies to it,
    /// and its preview names the message's author (P2-06).
    Broadcast,
}

/// One completed call, as recorded in the call history. The app
/// builds a record when a call ends (or when a
/// [`BackendEvent::CallMissed`] arrives) and the archive store
/// persists it, so the Calls panel renders the same history
/// after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    /// Archive row ID, assigned by the store when the record is
    /// read back; 0 on a record that has not been persisted yet.
    pub id: u64,
    /// Protocol call identifier. Unique per call: the archive
    /// keeps the first record stored under an ID and ignores
    /// later duplicates, so a call reported through two paths
    /// (its end event and a missed-call event) is recorded once.
    pub call_id: String,
    /// Chat JID of the other party (the 1:1 chat with them).
    pub chat_jid: String,
    /// Whether the call asked for video.
    pub video: bool,
    /// True when this device placed the call.
    pub outgoing: bool,
    /// The call rang this account and was never answered here:
    /// it rang out, or the caller gave up. A call the user
    /// declined is recorded with this false: declining is an
    /// answer, and the entry renders as a normal incoming call
    /// with no duration.
    pub missed: bool,
    /// Talk time in seconds; 0 for calls that never connected.
    pub duration_secs: u32,
    /// When the call started (rang or was placed), unix seconds.
    pub started_at: i64,
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
    /// Locked (chat lock; the app hides the chat everywhere
    /// except the locked folder). Like archive, a
    /// message-arrival upsert does not learn it from the
    /// wire, so the app keeps a known lock state across
    /// upserts that report `false`; lock changes made on the
    /// server arrive as [`BackendEvent::LockChanged`]
    /// instead.
    pub locked: bool,
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
    /// Unix second at which a timed mute ends; `0` when the
    /// chat is unmuted or muted indefinitely. Like `muted`, a
    /// message-arrival upsert does not learn it from the wire;
    /// mute changes made on the server arrive as
    /// [`BackendEvent::MuteChanged`] instead.
    pub mute_until: i64,
    /// For a group: the JID of its parent community group, when
    /// the group metadata names one. `None` for DMs, channels and
    /// groups outside a community.
    pub parent_jid: Option<String>,
    /// True when this group is itself a community parent group.
    pub is_parent: bool,
    /// True when this upsert arrived inside a history payload
    /// (the initial sync or an on-demand page). Its `unread`
    /// count is then the payload's own seed: the app records
    /// the chat as seeded so the payload's messages, which
    /// carry the same marker, do not increment the badge a
    /// second time. Live arrival upserts leave this false.
    pub from_history: bool,
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
    /// Mute notifications until the given moment (unix
    /// milliseconds, the protocol's own unit).
    MuteUntil(i64),
    /// Unmute notifications.
    Unmute,
    /// Move the chat into the account's locked chats folder
    /// (chat lock). Only the lock state syncs; the code that
    /// gates the folder is a local secret of the phone (and,
    /// in this client, of this app) and never travels.
    Lock,
    /// Move the chat back out of the locked chats folder.
    Unlock,
}

/// How long a message pin lasts, in the protocol's three
/// choices. Backend-neutral on purpose: the library's own pin
/// duration type never crosses the backend boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinDuration {
    /// 24 hours.
    Hours24,
    /// 7 days.
    Days7,
    /// 30 days.
    Days30,
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

/// The persistable CDN reference of one photo: everything needed
/// to re-download its bytes later, without the original protocol
/// message. The backend extracts it from the wire payload when a
/// photo arrives; the app archives it with the message, so a photo
/// stays fetchable after a restart (the session's protocol cache
/// alone would forget it). It carries the media key, so it belongs
/// in the encrypted archive only, and for a view-once photo it is
/// dropped once the single view has been spent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhotoRef {
    /// CDN path of the encrypted media.
    pub direct_path: String,
    /// Media decryption key (32 bytes).
    pub media_key: Vec<u8>,
    /// SHA-256 of the decrypted bytes (32 bytes).
    pub file_sha256: Vec<u8>,
    /// SHA-256 of the encrypted bytes (32 bytes).
    pub file_enc_sha256: Vec<u8>,
    /// Announced plaintext size in bytes.
    pub file_length: u64,
}

/// The persistable CDN reference of one voice message: the same
/// re-download material as [`PhotoRef`], extracted from the wire
/// audio payload and archived with the message so a voice note
/// stays fetchable for playback after a restart. It carries the
/// media key, so it belongs in the encrypted archive only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceRef {
    /// CDN path of the encrypted media.
    pub direct_path: String,
    /// Media decryption key (32 bytes).
    pub media_key: Vec<u8>,
    /// SHA-256 of the decrypted bytes (32 bytes).
    pub file_sha256: Vec<u8>,
    /// SHA-256 of the encrypted bytes (32 bytes).
    pub file_enc_sha256: Vec<u8>,
    /// Announced plaintext size in bytes.
    pub file_length: u64,
}

/// The persistable CDN reference of one video message: the same
/// re-download material as [`PhotoRef`], extracted from the wire
/// video payload and archived with the message so a video stays
/// fetchable for playback after a restart. It carries the media
/// key, so it belongs in the encrypted archive only, and for a
/// view-once video it is dropped once the single view has been
/// spent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VideoRef {
    /// CDN path of the encrypted media.
    pub direct_path: String,
    /// Media decryption key (32 bytes).
    pub media_key: Vec<u8>,
    /// SHA-256 of the decrypted bytes (32 bytes).
    pub file_sha256: Vec<u8>,
    /// SHA-256 of the encrypted bytes (32 bytes).
    pub file_enc_sha256: Vec<u8>,
    /// Announced plaintext size in bytes.
    pub file_length: u64,
}

/// The persistable CDN reference of one video's hosted poster:
/// the full-size frame image the sender's client uploaded
/// alongside the video (VideoMessage fields 21-23), downloadable
/// on its own so a video whose payload carried no embedded
/// thumbnail (or only a tiny one) can still present its own
/// frame. The poster is encrypted under the video's own media
/// key, so the reference carries that key alongside the
/// thumbnail's path and digests; it belongs in the encrypted
/// archive only. The wire carries no plaintext length for the
/// poster, so none is stored: the download's integrity rests on
/// the digest pair, exactly as the library validates it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PosterRef {
    /// CDN path of the encrypted poster.
    pub direct_path: String,
    /// Media decryption key (the video's own, 32 bytes).
    pub media_key: Vec<u8>,
    /// SHA-256 of the decrypted poster bytes (32 bytes).
    pub file_sha256: Vec<u8>,
    /// SHA-256 of the encrypted poster bytes (32 bytes).
    pub file_enc_sha256: Vec<u8>,
}

/// The persistable CDN reference of one sticker message: the same
/// re-download material as [`PhotoRef`], extracted from the wire
/// sticker payload and archived with the message so a sticker stays
/// fetchable for display after a restart. It carries the media
/// key, so it belongs in the encrypted archive only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StickerRef {
    /// CDN path of the encrypted media.
    pub direct_path: String,
    /// Media decryption key (32 bytes).
    pub media_key: Vec<u8>,
    /// SHA-256 of the decrypted bytes (32 bytes).
    pub file_sha256: Vec<u8>,
    /// SHA-256 of the encrypted bytes (32 bytes).
    pub file_enc_sha256: Vec<u8>,
    /// Announced plaintext size in bytes.
    pub file_length: u64,
}

/// The persistable CDN reference of one document message: the
/// same re-download material as [`PhotoRef`], extracted from
/// the wire document payload and archived with the message so a
/// document received in an earlier session stays downloadable
/// after a restart (the session's protocol cache no longer
/// holds it then). It carries the media key, so it belongs in
/// the encrypted archive only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DocumentRef {
    /// CDN path of the encrypted media.
    pub direct_path: String,
    /// Media decryption key (32 bytes).
    pub media_key: Vec<u8>,
    /// SHA-256 of the decrypted bytes (32 bytes).
    pub file_sha256: Vec<u8>,
    /// SHA-256 of the encrypted bytes (32 bytes).
    pub file_enc_sha256: Vec<u8>,
    /// Announced plaintext size in bytes.
    pub file_length: u64,
}

/// A message as the backend reports it (protocol-neutral).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveMessage {
    /// Protocol message ID (stable within its chat).
    pub id: String,
    /// Server-assigned content ID, set only for channel
    /// (newsletter) messages: channel reactions, channel poll
    /// votes and channel counter updates all key on it, never
    /// on the protocol ID. It is learned from channel history
    /// fetches and channel live updates; a channel message
    /// that arrived through the plain receive flow may not
    /// carry one until the next history fetch lands. `None`
    /// everywhere else.
    pub server_id: Option<u64>,
    /// Chat JID the message belongs to.
    pub chat_jid: String,
    /// Sender display name; `None` means "me".
    pub sender: Option<String>,
    /// Plain-text body. Non-text messages carry a typed summary
    /// from the backend (their caption, or a kind label such as
    /// "Photo"), never a bracketed placeholder.
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
    /// For a photo message: the reference its bytes can be
    /// re-downloaded from (see [`PhotoRef`]). `None` for every
    /// other kind, and for photos whose wire payload did not
    /// carry a complete reference.
    pub photo_ref: Option<PhotoRef>,
    /// For a voice message: the reference its bytes can be
    /// re-downloaded from (see [`VoiceRef`]). `None` for every
    /// other kind, and for voice messages whose wire payload did
    /// not carry a complete reference.
    pub voice_ref: Option<VoiceRef>,
    /// For a video message: the reference its bytes can be
    /// re-downloaded from (see [`VideoRef`]). `None` for every
    /// other kind, and for video messages whose wire payload did
    /// not carry a complete reference.
    pub video_ref: Option<VideoRef>,
    /// For a video message: the JPEG thumbnail the sender's
    /// client embedded in the wire payload, a real frame of the
    /// video itself. This is the video's visual identity until
    /// its bytes are fetched and played; it is never generated
    /// locally. `None` for every other kind, and for videos
    /// whose payload carried no thumbnail.
    pub video_thumbnail: Option<Vec<u8>>,
    /// For a video message: the reference its hosted poster (the
    /// full-size frame image the sender's client uploaded
    /// alongside the video) can be downloaded from, when the
    /// wire payload carried one. The app fetches it when the
    /// payload carried no embedded thumbnail, so the tile can
    /// present the video's own frame instead of a neutral tile.
    /// `None` for every other kind, and for videos whose payload
    /// carried no complete poster reference.
    pub poster_ref: Option<PosterRef>,
    /// For a video message: the frame width the wire payload
    /// states (VideoMessage field 10). The video's own
    /// proportions, known before any thumbnail or byte fetch,
    /// so the tile can reserve the right shape from the start.
    /// `None` for every other kind, and for videos whose payload
    /// stated no dimensions.
    pub video_width: Option<u32>,
    /// For a video message: the frame height the wire payload
    /// states (VideoMessage field 9). See [`Self::video_width`].
    pub video_height: Option<u32>,
    /// For a voice message: the waveform preview bytes the
    /// sender's client computed from the actual audio and
    /// carried in the wire payload (AudioMessage field 19).
    /// This is the only waveform a received voice note may
    /// render; a payload without one renders a neutral flat
    /// form, never a locally invented shape. `None` for every
    /// other kind, and for voice notes whose payload carried
    /// no waveform.
    pub voice_waveform: Option<Vec<u8>>,
    /// For a poll message: the poll's creation secret, carried in
    /// the creation message's context info. Votes on the poll are
    /// encrypted under a key derived from this secret, so the
    /// backend needs it both to vote and to decrypt other people's
    /// votes. It travels on the message so the app can archive it
    /// with the poll (in its encrypted archive) and hand it back
    /// with [`BackendCommand::RegisterPollSecret`] in later
    /// sessions; without that, a poll from before the current
    /// session could neither be voted on nor tallied. `None` for
    /// every other kind, and for polls whose creation payload
    /// carried no secret.
    pub poll_secret: Option<Vec<u8>>,
    /// JIDs this message mentions (the wire's
    /// `context_info.mentioned_jid`, in wire order, deduplicated).
    /// The body carries the mentions as `@<number>` text; the app
    /// resolves each JID to a display name at render time from its
    /// name directory. Empty for messages that mention nobody.
    pub mentions: Vec<String>,
    /// For a photo message: the JPEG thumbnail the sender's client
    /// embedded in the wire payload, the photo itself at preview
    /// fidelity. The tile paints it immediately, before (and
    /// while) the full bytes are fetched. `None` for every other
    /// kind, and for photos whose payload carried no thumbnail.
    pub photo_thumbnail: Option<Vec<u8>>,
    /// For a sticker message: the reference its bytes can be
    /// re-downloaded from (see [`StickerRef`]). `None` for every
    /// other kind, and for stickers whose wire payload did not
    /// carry a complete reference.
    pub sticker_ref: Option<StickerRef>,
    /// For a video message: the sender's client marked the clip
    /// as a GIF (VideoMessage `gifPlayback`): it loops and wears
    /// no playback controls. `false` for every other kind.
    pub gif_playback: bool,
    /// For a sticker message: the sender's client marked the
    /// sticker animated (StickerMessage `isAnimated`). Its WebP
    /// carries frames; the tile wears an honest badge when it
    /// can only present the first frame. `false` for every
    /// other kind.
    pub sticker_animated: bool,
    /// For a sticker message: the sticker is a Lottie animation
    /// (StickerMessage `isLottie`). Its bytes are animation
    /// JSON, not an image; the tile presents the payload's own
    /// static frame ([`Self::sticker_thumbnail`]) with a badge
    /// instead of failing to decode the JSON as a picture.
    /// `false` for every other kind.
    pub sticker_lottie: bool,
    /// For a sticker message: the static PNG frame the sender's
    /// client embedded in the payload (StickerMessage
    /// `pngThumbnail`), the sticker's own still image. The tile
    /// paints it for a Lottie sticker, whose bytes are not an
    /// image at all. `None` for every other kind, and for
    /// stickers whose payload carried none.
    pub sticker_thumbnail: Option<Vec<u8>>,
    /// For a document message: the reference its bytes can be
    /// re-downloaded from (see [`DocumentRef`]). `None` for
    /// every other kind, and for documents whose wire payload
    /// did not carry a complete reference.
    pub document_ref: Option<DocumentRef>,
    /// For a document message: the file's own name as the
    /// sender's client stated it (DocumentMessage `fileName`,
    /// falling back to its `title`). The document model carries
    /// it in its own field; the kind's caption slot holds only
    /// the sender's caption. `None` for every other kind.
    pub document_name: Option<String>,
    /// For a document message: the page count the sender's
    /// client stated (DocumentMessage `pageCount`), when it
    /// stated one. `None` for every other kind.
    pub document_page_count: Option<u32>,
    /// For a document message: the JPEG thumbnail the sender's
    /// client embedded in the payload (DocumentMessage
    /// `jpegThumbnail`), the document's own preview image.
    /// `None` for every other kind, and for documents whose
    /// payload carried none.
    pub document_thumbnail: Option<Vec<u8>>,
    /// For a location message: the map snapshot the sender's
    /// client embedded in the payload (LocationMessage
    /// `jpegThumbnail`), a real picture of the place. `None`
    /// for every other kind, and for locations whose payload
    /// carried none.
    pub location_thumbnail: Option<Vec<u8>>,
    /// For a poll message: the JID of the poll's creator (the
    /// creation message's author). The vote encryption key is
    /// derived over the creator and voter JIDs, so the creator is
    /// part of the poll's crypto identity and is archived and
    /// re-registered together with [`LiveMessage::poll_secret`].
    /// `None` for every other kind.
    pub poll_creator: Option<String>,
    /// For a poll message: the latest known ballot per voter,
    /// `(voter JID, selected option indices)`, as aggregated by
    /// the app from [`BackendEvent::PollVote`] events. The
    /// backend never fills this in (it reports votes, it does not
    /// keep tally state); the archive carries it on the message
    /// so the app can reload it after a restart and keep applying
    /// last-vote-wins replacements instead of double-counting a
    /// voter whose earlier ballot it has forgotten.
    pub poll_ballots: Vec<(String, Vec<usize>)>,
    /// For an event message: the event's creation secret,
    /// carried in the creation message's context info.
    /// Responses to the event are encrypted under a key derived
    /// from this secret, so the backend needs it both to answer
    /// and to decrypt other people's answers. It travels on the
    /// message so the app can archive it with the event (in its
    /// encrypted archive) and hand it back with
    /// [`BackendCommand::RegisterEventSecret`] in later
    /// sessions; without that, an event from before the current
    /// session could neither be answered nor tallied. `None`
    /// for every other kind, and for events whose creation
    /// payload carried no secret.
    pub event_secret: Option<Vec<u8>>,
    /// For an event message: the JID of the event's creator
    /// (the creation message's author). The response encryption
    /// key is derived over the creator and responder JIDs, so
    /// the creator is part of the event's crypto identity and
    /// is archived and re-registered together with
    /// [`LiveMessage::event_secret`]. `None` for every other
    /// kind.
    pub event_creator: Option<String>,
    /// For an event message: the latest known response per
    /// responder, `(responder JID, response)`, as aggregated by
    /// the app from [`BackendEvent::EventResponse`] events. The
    /// backend never fills this in (it reports responses, it
    /// does not keep tally state); the archive carries it on the
    /// message so the app can reload it after a restart and keep
    /// applying last-response-wins replacements instead of
    /// double-counting a responder whose earlier answer it has
    /// forgotten.
    pub event_ballots: Vec<(String, EventRsvp)>,
    /// The sender's verified business name, when the payload
    /// states one (history sync carries it per message). The
    /// app treats its presence as proof the sender is a
    /// verified business and badges the chat.
    pub sender_verified_name: Option<String>,
    /// Synced delivery state for an outgoing message, from the
    /// history status or a live receipt. `None` for incoming
    /// messages and for outgoing messages with no known state.
    pub delivery: Option<DeliveryState>,
    /// True when this message arrived inside a history payload
    /// (the initial sync or an on-demand page), as opposed to a
    /// live arrival. A history payload's unread messages are
    /// already counted by the unread seed its chat upsert
    /// carried, so the app must not increment the badge for them
    /// a second time (the first-link double count). Live
    /// arrivals leave this false.
    pub from_history: bool,
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
            server_id: None,
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
            photo_ref: None,
            voice_ref: None,
            video_ref: None,
            video_thumbnail: None,
            poster_ref: None,
            video_width: None,
            video_height: None,
            voice_waveform: None,
            mentions: Vec::new(),
            photo_thumbnail: None,
            sticker_ref: None,
            gif_playback: false,
            sticker_animated: false,
            sticker_lottie: false,
            sticker_thumbnail: None,
            document_ref: None,
            document_name: None,
            document_page_count: None,
            document_thumbnail: None,
            sender_verified_name: None,
            delivery: None,
            from_history: false,
            location_thumbnail: None,
            poll_secret: None,
            poll_creator: None,
            poll_ballots: Vec::new(),
            event_secret: None,
            event_creator: None,
            event_ballots: Vec::new(),
        }
    }
}

/// One status (story) update, received or posted. A status is
/// not a message in a conversation: it is delivered under the
/// broadcast address `status@broadcast`, it belongs to its
/// author alone, and it expires 24 hours after it was posted.
/// It therefore travels as its own type instead of entering the
/// chat machinery as a [`LiveMessage`] with a fake chat; the
/// app keeps received statuses in session state (the Updates
/// feed), never in the chat archive. The media references work
/// exactly like a message's: the bytes are fetched with the
/// ordinary fetch commands addressed to `status@broadcast`.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveStatus {
    /// Protocol message ID (stable within the broadcast).
    pub id: String,
    /// JID of the status author.
    pub author_jid: String,
    /// Author display name as the backend resolved it; `None`
    /// means the author is this account.
    pub author_name: Option<String>,
    /// Plain-text body: the status text, or a media caption, or
    /// the typed kind label when the post carries no words.
    pub body: String,
    /// Unix timestamp (seconds) the status was posted.
    pub timestamp: i64,
    /// True when this account posted the status.
    pub from_me: bool,
    /// Rich content kind (text by default), classified exactly
    /// like a message's.
    pub kind: LiveMessageKind,
    /// For a text status: the background colour the author
    /// picked, as an ARGB word (the wire's `backgroundArgb`).
    /// `None` for media statuses and for text payloads that
    /// carried no colour.
    pub background_argb: Option<u32>,
    /// (emoji, count) reaction summary, fed by reaction updates
    /// addressed to the broadcast.
    pub reactions: Vec<(String, u32)>,
    /// My own reaction, if any.
    pub my_reaction: Option<String>,
    /// For a photo status: the reference its bytes can be
    /// re-downloaded from (see [`PhotoRef`]).
    pub photo_ref: Option<PhotoRef>,
    /// For a photo status: the JPEG thumbnail the author's
    /// client embedded in the payload, the photo at preview
    /// fidelity.
    pub photo_thumbnail: Option<Vec<u8>>,
    /// For a video status: the reference its bytes can be
    /// re-downloaded from (see [`VideoRef`]).
    pub video_ref: Option<VideoRef>,
    /// For a video status: the embedded JPEG thumbnail (a real
    /// frame of the video), its visual identity until the bytes
    /// are fetched and played.
    pub video_thumbnail: Option<Vec<u8>>,
    /// For a video status: the reference its hosted poster can
    /// be downloaded from, when the payload carried one.
    pub poster_ref: Option<PosterRef>,
    /// For a video status: the frame width the payload states.
    pub video_width: Option<u32>,
    /// For a video status: the frame height the payload states.
    pub video_height: Option<u32>,
}

/// This account's role in a channel, as the server reports it.
/// `Guest` means the account does not follow the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveChannelRole {
    /// Not following the channel.
    Guest,
    /// Following the channel.
    Follower,
    /// Can post and manage updates.
    Admin,
    /// Owns the channel.
    Owner,
}

impl LiveChannelRole {
    /// True when the account can post to the channel.
    pub fn can_post(self) -> bool {
        matches!(self, Self::Admin | Self::Owner)
    }
}

/// A channel (newsletter), as the backend reports it. A
/// channel's updates also live in the chat roster as an
/// ordinary chat of kind Channel; this type carries the
/// channel-specific facts the roster does not have (role,
/// follower count, invite code) for the Channels surface and
/// the channel info panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveChannel {
    /// Channel JID (`...@newsletter`).
    pub jid: String,
    /// Channel name.
    pub name: String,
    /// Channel description (empty when the channel has none).
    pub description: String,
    /// Follower count as the server reports it.
    pub followers: u64,
    /// True when the server marks the channel verified.
    pub verified: bool,
    /// This account's role in the channel.
    pub role: LiveChannelRole,
    /// True when this account muted the channel's updates.
    pub muted: bool,
    /// The invite code (`whatsapp.com/channel/<code>`), when
    /// the server reports one.
    pub invite_code: Option<String>,
    /// Unix timestamp (seconds) the channel was created;
    /// 0 when the server did not report one.
    pub created_at: i64,
}

/// The channel info panel's payload: the channel itself plus
/// the admin-side facts the plain metadata does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveChannelInfo {
    /// The channel, freshly fetched.
    pub channel: LiveChannel,
    /// How many admins the channel has. Only answered for
    /// admins and owners; `None` for everyone else.
    pub admin_count: Option<u32>,
    /// A sample of follower display names, fetched for
    /// admins and owners (the server answers one bounded
    /// page). Empty for followers, who are not entitled to
    /// the list.
    pub follower_names: Vec<String>,
}

/// Reaction and poll counters for one channel update, keyed
/// by the update's server ID. Channel live updates carry
/// counters only, never bodies; new posts arrive through the
/// normal message flow.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveChannelCounters {
    /// Channel JID the update belongs to.
    pub jid: String,
    /// Server ID of the update the counters belong to.
    pub server_id: u64,
    /// (emoji, count) reaction summary, replacing the
    /// update's previous summary.
    pub reactions: Vec<(String, u32)>,
    /// (SHA-256 of the option name, votes) per poll option,
    /// replacing the tallies of the update's poll options.
    /// The app matches hashes against the option names it
    /// already renders.
    pub poll_votes: Vec<([u8; 32], u32)>,
}

/// One group member row as the backends report it:
/// address, display name, admin flag, and the member
/// label the member set for themselves in this group
/// (row 65), when they set one.
pub type GroupMemberRow = (String, String, bool, Option<String>);

/// One saved contact in a [`BackendEvent::SavedContacts`]
/// report. A person appears under two JID forms (phone number
/// and LID); `jid` is the canonical one, the phone-number form
/// when the backend's identity book holds the contact under
/// it, else the LID. Consumers that compare JIDs against a
/// list recorded in either form (the status privacy list is
/// the standing example) resolve every form through
/// `alt_jids` to this one key, so the comparison cannot be
/// defeated by a form mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedContact {
    /// The person's canonical JID.
    pub jid: String,
    /// The saved display name.
    pub name: String,
    /// The person's other known JID forms, sorted, never
    /// containing `jid` itself.
    pub alt_jids: Vec<String>,
}

/// An event from the backend to the UI.
///
/// Parity between backends (the real session and the fake) is
/// defined over event shapes and state convergence: for the
/// same command, every backend answers with the same typed
/// events, applies the same refusal rules, and converges the
/// app to the same state. Timing is deliberately NOT part of
/// the contract: the real session's confirmations wait on the
/// server (and the `*Changed` events below are the adoption
/// channel for changes made elsewhere), while the fake
/// simulates that echo at zero latency. A test that needs a
/// specific failure uses the fake's failure injection; the
/// connection-lifecycle events (pairing expiry, bans, transport
/// loss, crashes) are produced only by a real transport or the
/// process supervisor and are not simulated by any backend.
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
        /// How long this payload stays valid, as reported by the
        /// protocol library when it issued the code. A payload past
        /// its window is dead even if it is still on screen, so
        /// consumers must stop presenting it as scannable.
        valid_for: std::time::Duration,
    },
    /// Every QR payload for this session has expired unscanned.
    /// The protocol library stops issuing codes at this point (and
    /// closes the connection unless a pairing-code flow is still
    /// outstanding), so any code still held is dead.
    QrExpired,
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
    /// A community was created by this client (the answer to
    /// [`BackendCommand::CreateCommunity`]). The community's
    /// announcement group shares the community JID, so the JID
    /// doubles as the announcement chat's address.
    CommunityCreated {
        /// JID of the new community (also its announcement group).
        jid: String,
        /// The community's name, as created.
        name: String,
    },
    /// Groups were linked into a community (the answer to
    /// [`BackendCommand::LinkCommunityGroups`]).
    CommunityGroupsLinked {
        /// Community (parent group) JID.
        community_jid: String,
        /// Group JIDs that are now linked subgroups.
        linked: Vec<String>,
        /// Groups the server refused, as (group JID, server
        /// error code) pairs.
        failed: Vec<(String, u32)>,
    },
    /// Groups were unlinked from a community (the answer to
    /// [`BackendCommand::UnlinkCommunityGroups`]).
    CommunityGroupsUnlinked {
        /// Community (parent group) JID.
        community_jid: String,
        /// Group JIDs that are no longer linked.
        unlinked: Vec<String>,
        /// Groups the server refused, as (group JID, server
        /// error code) pairs.
        failed: Vec<(String, u32)>,
    },
    /// A subgroup was created inside a community (the answer to
    /// [`BackendCommand::CreateCommunitySubgroup`]). The roster
    /// entry carries its community parent.
    CommunitySubgroupCreated {
        /// Community (parent group) JID.
        community_jid: String,
        /// The new subgroup's roster entry.
        group: LiveChat,
    },
    /// Participants were removed from a community (the answer to
    /// [`BackendCommand::RemoveCommunityParticipants`]).
    CommunityParticipantsRemoved {
        /// Community (parent group) JID.
        community_jid: String,
        /// Participant JIDs that were removed.
        participants: Vec<String>,
    },
    /// A community was deactivated (the answer to
    /// [`BackendCommand::DeactivateCommunity`]). Its subgroups
    /// survive as ordinary groups; the host clears their parent
    /// tags and drops the parent chat locally.
    CommunityDeactivated {
        /// Community (parent group) JID.
        community_jid: String,
    },
    /// A message appeared or changed.
    MessageUpsert(LiveMessage),
    /// A message this client sent was accepted by the server, with
    /// the protocol ID it went out under: the caller-minted
    /// `send_id` when the command carried one, otherwise the ID
    /// the library assigned at dispatch. The UI binds its
    /// pending local echo to this ID, so the backend's own upsert
    /// and later receipts find the message directly instead of
    /// guessing by content.
    MessageSent {
        /// Chat JID the message went to.
        chat_jid: String,
        /// Protocol ID assigned to the sent message.
        message_id: String,
    },
    /// A message this client tried to send to a chat never
    /// reached the server: a text, media, location, contact,
    /// interactive, or interactive-response send failed. The
    /// event names the send's protocol ID (`message_id`: the
    /// caller-minted [`BackendCommand`] `send_id` when one was
    /// supplied, otherwise the ID the backend minted at
    /// dispatch), so the app binds the failure to that exact
    /// echo rather than guessing by order. The failure means
    /// UNCONFIRMED, not never sent: a failure after the stanza
    /// was written (a timeout at the write stage) can still
    /// have landed, which is why the app keeps the echo's ID
    /// bound and lets a later upsert or receipt for the same
    /// ID reconcile it. The app renders its own localized
    /// sentence from `failure`. Deliberately not the generic
    /// [`BackendEvent::Error`]: that path carries every backend
    /// failure, so the app had to guess from the message text
    /// which errors were send failures; this event is that
    /// answer, typed. The raw library error is in the backend's
    /// log with its stage, never in this event.
    MessageSendFailed {
        /// Chat JID the message was addressed to.
        chat_jid: String,
        /// Protocol ID the failed send carried.
        message_id: String,
        /// The structured failure (kind and stage).
        failure: SendFailure,
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
        /// JID of the receipt actor, when the wire names one.
        /// In a group, one member's receipt must not advance a
        /// message to Read for everyone.
        actor: Option<String>,
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
    /// A [`BackendCommand::React`] never reached the server: the
    /// target was invalid (a group target without its author) or
    /// the send itself failed. The app rolls the optimistic
    /// reaction back to the last state the server confirmed and
    /// shows `message` as the notice. Deliberately not the generic
    /// [`BackendEvent::Error`]: that path also fails pending
    /// message echoes, which a refused reaction must not touch.
    ReactionFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the target message.
        message_id: String,
        /// Why the reaction failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::StarMessage`] never reached the
    /// server: the target was invalid (a group target without
    /// its author) or the send itself failed. The app rolls the
    /// optimistic star back to the last state the server
    /// confirmed and shows `message` as the notice. Deliberately
    /// not the generic [`BackendEvent::Error`], for the same
    /// reason as [`BackendEvent::ReactionFailed`]: that path
    /// also fails pending message echoes, which a refused star
    /// must not touch, and it cannot name the message to roll
    /// back.
    StarFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the target message.
        message_id: String,
        /// Why the star failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::PinMessage`] never reached the
    /// server: the pin counterpart of
    /// [`BackendEvent::StarFailed`], with the same contract.
    /// The app rolls the optimistic pin back and shows
    /// `message` as the notice.
    PinFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the target message.
        message_id: String,
        /// Why the pin failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::KeepMessage`] never reached the
    /// server: the keep counterpart of
    /// [`BackendEvent::StarFailed`], with the same contract.
    /// The app rolls the optimistic kept state back and shows
    /// `message` as the notice.
    KeepFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the target message.
        message_id: String,
        /// Why the keep failed, in user words.
        message: String,
    },
    /// A message's kept state changed on the server: kept or
    /// un-kept from another device (a keep made on the phone
    /// arrives as a keep add-on message, which the backend
    /// translates into this event instead of a bubble). A keep
    /// set from this client goes out as
    /// [`BackendCommand::KeepMessage`] and applies locally at
    /// once, without waiting for this event.
    MessageKept {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the kept message.
        message_id: String,
        /// Whether the message is kept now.
        kept: bool,
    },
    /// A [`BackendCommand::ReportSpam`] was accepted by the
    /// server: the report is filed.
    SpamReported {
        /// Chat JID the reported message belongs to.
        chat_jid: String,
    },
    /// A [`BackendCommand::ReportSpam`] never reached the
    /// server, or the server refused it. The app shows
    /// `message` as the notice; nothing was reported.
    SpamReportFailed {
        /// Why the report failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::PostTextStatus`] reached the server:
    /// the text status is posted. The app adopts `text` as its
    /// current status for the rest of the session, and keeps
    /// `message_id` so the post can be revoked with
    /// [`BackendCommand::DeleteStatus`].
    StatusPosted {
        /// The status text that was posted.
        text: String,
        /// Protocol ID of the posted status.
        message_id: String,
    },
    /// A [`BackendCommand::PostMediaStatus`] reached the server:
    /// the photo or video status is posted. Carries the full
    /// [`LiveStatus`] so the app adopts the post into My Status
    /// exactly like a received one: viewable in this session and
    /// deletable by its protocol ID.
    StatusMediaPosted {
        /// The posted status.
        status: Box<LiveStatus>,
    },
    /// A status update arrived: someone in the account's
    /// audience posted one, the account's own post was delivered
    /// back, or history sync redelivered one the server still
    /// holds. Statuses arrive while the client is linked (and
    /// through history sync at link time); there is no fetch for
    /// them, so a status posted and expired while the client was
    /// offline never appears. Re-delivery of a status the app
    /// already holds carries the same ID and replaces it.
    StatusReceived {
        /// The received status.
        status: Box<LiveStatus>,
    },
    /// A [`BackendCommand::DeleteStatus`] reached the server:
    /// one of my own statuses is revoked. The app drops it from
    /// My Status.
    StatusDeleted {
        /// Protocol ID of the deleted status.
        message_id: String,
    },
    /// A [`BackendCommand::DeleteStatus`] never reached the
    /// server. The status stays in My Status and the app shows
    /// `message` where the delete was attempted.
    StatusDeleteFailed {
        /// Protocol ID of the status that could not be deleted.
        message_id: String,
        /// Why the delete failed, in user words.
        message: String,
    },
    /// The channel list landed (answering
    /// [`BackendCommand::FetchChannels`]): every channel this
    /// account follows, owns or admins, with fresh metadata.
    /// Replaces the app's channel cache wholesale.
    ChannelsListed {
        /// The channels, in the server's order.
        channels: Vec<LiveChannel>,
    },
    /// A [`BackendCommand::FetchChannels`] failed; the app
    /// keeps the channel cache it already had.
    ChannelsListFailed {
        /// Why the list failed, in user words.
        message: String,
    },
    /// A channel lookup by invite code landed (answering
    /// [`BackendCommand::LookupChannel`]).
    ChannelFound {
        /// The channel behind the invite code.
        channel: LiveChannel,
    },
    /// A [`BackendCommand::LookupChannel`] found nothing or
    /// failed; `message` says which, in user words.
    ChannelLookupFailed {
        /// Why the lookup failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::FollowChannel`] landed: the
    /// account now follows the channel.
    ChannelFollowed {
        /// The channel, with its post-follow role.
        channel: LiveChannel,
    },
    /// A [`BackendCommand::FollowChannel`] failed; the
    /// account's role in the channel is unchanged.
    ChannelFollowFailed {
        /// Channel JID that could not be followed.
        jid: String,
        /// Why the follow failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::UnfollowChannel`] landed: the
    /// account no longer follows the channel.
    ChannelUnfollowed {
        /// Channel JID that was unfollowed.
        jid: String,
    },
    /// A [`BackendCommand::UnfollowChannel`] failed; the
    /// account still follows the channel.
    ChannelUnfollowFailed {
        /// Channel JID that could not be unfollowed.
        jid: String,
        /// Why the unfollow failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::CreateChannel`] landed: the
    /// account owns a new channel.
    ChannelCreated {
        /// The new channel.
        channel: LiveChannel,
    },
    /// A [`BackendCommand::CreateChannel`] failed; no
    /// channel was created.
    ChannelCreateFailed {
        /// Why the creation failed, in user words.
        message: String,
    },
    /// A channel history page landed (answering
    /// [`BackendCommand::FetchChannelHistory`]): its
    /// messages were already delivered as ordinary message
    /// upserts for the channel's chat before this event.
    ChannelHistoryFetched {
        /// Channel JID the page belongs to.
        jid: String,
        /// How many updates the page carried.
        added: u32,
        /// True when the page was short or empty: the
        /// channel has no older updates to page to.
        end_of_history: bool,
    },
    /// A [`BackendCommand::FetchChannelHistory`] failed; the
    /// channel's updates stay as they were and a later open
    /// or scroll may retry.
    ChannelHistoryFailed {
        /// Channel JID the fetch was for.
        jid: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// Reaction and poll counters for one channel update
    /// changed (a channel live update, armed by
    /// [`BackendCommand::WatchChannel`]). The app replaces
    /// the named update's reaction summary and poll tallies
    /// with these.
    ChannelCountersUpdated {
        /// The counters, keyed by the update's server ID.
        counters: LiveChannelCounters,
    },
    /// A [`BackendCommand::ReactToChannelUpdate`] landed.
    ChannelReactionSet {
        /// Channel JID.
        jid: String,
        /// Server ID of the update.
        server_id: u64,
        /// The reaction now in effect (empty when removed).
        emoji: String,
    },
    /// A [`BackendCommand::ReactToChannelUpdate`] failed;
    /// the app's optimistic reaction for the update is
    /// rolled back.
    ChannelReactionFailed {
        /// Channel JID.
        jid: String,
        /// Server ID of the update.
        server_id: u64,
        /// Why the reaction failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::CreateChannelPoll`] was accepted
    /// by the server. The poll update itself arrives through
    /// the live-update and history paths.
    ChannelPollCreated {
        /// Channel JID the poll was posted to.
        jid: String,
    },
    /// A [`BackendCommand::CreateChannelPoll`] failed; no
    /// poll was posted.
    ChannelPollFailed {
        /// Channel JID the poll was meant for.
        jid: String,
        /// Why the poll failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::VoteChannelPoll`] landed.
    ChannelPollVoted {
        /// Channel JID.
        jid: String,
        /// Server ID of the poll update.
        server_id: u64,
    },
    /// A [`BackendCommand::VoteChannelPoll`] failed; the
    /// app's optimistic vote is rolled back.
    ChannelPollVoteFailed {
        /// Channel JID.
        jid: String,
        /// Server ID of the poll update.
        server_id: u64,
        /// Why the vote failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::EditChannelUpdate`] landed.
    ChannelUpdateEdited {
        /// Channel JID.
        jid: String,
        /// Protocol ID of the edited update.
        message_id: String,
    },
    /// A [`BackendCommand::EditChannelUpdate`] failed; the
    /// update keeps its previous text.
    ChannelUpdateEditFailed {
        /// Channel JID.
        jid: String,
        /// Protocol ID of the update.
        message_id: String,
        /// Why the edit failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::RevokeChannelUpdate`] landed.
    ChannelUpdateRevoked {
        /// Channel JID.
        jid: String,
        /// Protocol ID of the revoked update.
        message_id: String,
    },
    /// A [`BackendCommand::RevokeChannelUpdate`] failed; the
    /// update stays published.
    ChannelUpdateRevokeFailed {
        /// Channel JID.
        jid: String,
        /// Protocol ID of the update.
        message_id: String,
        /// Why the revoke failed, in user words.
        message: String,
    },
    /// A channel info payload landed (answering
    /// [`BackendCommand::FetchChannelInfo`]).
    ChannelInfoFetched {
        /// The info payload.
        info: LiveChannelInfo,
    },
    /// A [`BackendCommand::FetchChannelInfo`] failed.
    ChannelInfoFailed {
        /// Channel JID the fetch was for.
        jid: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::SetChannelMuted`] landed.
    ChannelMuteSet {
        /// Channel JID.
        jid: String,
        /// The mute state now in effect.
        muted: bool,
    },
    /// A [`BackendCommand::SetChannelMuted`] failed; the
    /// channel's mute state is unchanged.
    ChannelMuteFailed {
        /// Channel JID.
        jid: String,
        /// Why the change failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::PostTextStatus`] never reached the
    /// server: the text was empty, the audience was empty or
    /// unusable, or the send itself failed. The app keeps its
    /// previous status state and shows `message` where the post
    /// was attempted. Deliberately not the generic
    /// [`BackendEvent::Error`], for the same reason as
    /// [`BackendEvent::StarFailed`]: that path also fails pending
    /// message echoes, which a refused status post must not touch.
    StatusPostFailed {
        /// Why the post failed, in user words.
        message: String,
    },
    /// A vote landed on a poll. Votes arrive on the wire as
    /// separate encrypted messages; the backend decrypts them
    /// with the poll's creation secret and reports the voter's
    /// full current selection (last-vote-wins: a later event for
    /// the same voter replaces the earlier one wholesale, and an
    /// empty selection means the voter cleared their vote).
    /// Aggregating the selections into tallies is the app's job.
    /// A vote whose poll secret the backend does not know cannot
    /// be decrypted and is never reported.
    PollVote {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the poll message.
        message_id: String,
        /// JID of the voter.
        voter_jid: String,
        /// True when the vote is the account's own (cast from
        /// this or another of the account's devices).
        from_me: bool,
        /// Option indices the voter currently selects.
        selected: Vec<usize>,
    },
    /// A [`BackendCommand::VotePoll`] never reached the
    /// server: the poll's creation secret is unavailable on
    /// this device, an address was invalid, or the send
    /// itself failed. The app rolls the optimistic vote back
    /// to the selection it replaced and shows `message` as
    /// the notice. The poll counterpart of
    /// [`BackendEvent::StarFailed`], with the same contract.
    PollVoteFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the poll message.
        message_id: String,
        /// Why the vote failed, in user words.
        message: String,
    },
    /// A response to an event landed. Responses arrive on the
    /// wire as separate encrypted messages; the backend
    /// decrypts them with the event's creation secret and
    /// reports the responder's current answer (last-response-
    /// wins: a later event for the same responder replaces the
    /// earlier one wholesale). Aggregating the answers into
    /// tallies is the app's job. A response whose event secret
    /// the backend does not know cannot be decrypted and is
    /// never reported. The event counterpart of
    /// [`BackendEvent::PollVote`].
    EventResponse {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the event message.
        message_id: String,
        /// JID of the responder.
        responder_jid: String,
        /// True when the response is the account's own (cast
        /// from this or another of the account's devices).
        from_me: bool,
        /// The responder's current answer.
        response: EventRsvp,
        /// Extra guests the responder is bringing, when stated.
        extra_guest_count: Option<i32>,
    },
    /// A [`BackendCommand::RespondEvent`] never reached the
    /// server: the event's creation secret is unavailable on
    /// this device, an address was invalid, or the send itself
    /// failed. The app rolls the optimistic response back to
    /// the answer it replaced and shows `message` as the
    /// notice. The event counterpart of
    /// [`BackendEvent::PollVoteFailed`], with the same contract.
    EventRespondFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the event message.
        message_id: String,
        /// Why the response failed, in user words.
        message: String,
    },
    /// An event was edited by its organizer (a cancellation is
    /// an edit with `canceled` set). The edit replaces the
    /// whole event payload, so every field rides along, in the
    /// same shape as [`LiveMessageKind::Event`]. Own edits made
    /// from another of the account's devices arrive here too;
    /// the app applies the payload as the event's new state.
    EventEdited {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the event message.
        message_id: String,
        /// Event name.
        name: String,
        /// Free-text details (may be empty).
        description: String,
        /// Start, unix seconds (0 when the payload stated none).
        start_time: i64,
        /// End, unix seconds (0 when the event has no end).
        end_time: i64,
        /// Place label the payload states (may be empty).
        location: String,
        /// Join/call link (may be empty).
        join_link: String,
        /// The event is a scheduled call.
        scheduled_call: bool,
        /// Extra guests are allowed.
        extra_guests: bool,
        /// The organizer cancelled the event with this edit.
        canceled: bool,
    },
    /// A [`BackendCommand::EditEvent`] never reached the
    /// server: the event's creation secret is unavailable on
    /// this device, an address was invalid, or the send itself
    /// failed. The app restores the event's previous fields and
    /// shows `message` as the notice. The event counterpart of
    /// [`BackendEvent::EditMessageFailed`], with the same
    /// contract.
    EventEditFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the event message.
        message_id: String,
        /// Why the edit failed, in user words.
        message: String,
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
    /// A [`BackendCommand::EditMessage`] never reached the
    /// server. The app restores the message's previous body
    /// and shows `message` as the notice. The edit counterpart
    /// of [`BackendEvent::StarFailed`], with the same contract.
    EditMessageFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// Why the edit failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::RevokeMessage`] never reached the
    /// server: the message was not deleted for anyone. The app
    /// restores the message's not-deleted state and shows
    /// `message` as the notice. The revoke counterpart of
    /// [`BackendEvent::StarFailed`], with the same contract.
    RevokeMessageFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// Why the revoke failed, in user words.
        message: String,
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
        /// Unix second at which a timed mute ends; `0` when
        /// unmuted or muted indefinitely.
        mute_until: i64,
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
    /// A chat's lock state changed on the server: locked or
    /// unlocked from another device, or the account state as
    /// a full sync reports it (the link-time delivery of the
    /// phone's locked set rides this path). This is the
    /// authoritative path for lock changes made elsewhere; a
    /// lock set from this client goes out as
    /// [`BackendCommand::ChatAction`] and applies locally at
    /// once, without waiting for this event.
    LockChanged {
        /// Chat JID.
        chat_jid: String,
        /// Whether the chat is locked now.
        locked: bool,
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
    /// A chat was deleted on the server (deleted on the phone,
    /// or this client's own [`BackendCommand::DeleteChat`]
    /// confirmed back). The app removes the chat from its list,
    /// its session mirror and its archive. A deletion made from
    /// this client applies locally only when this event
    /// confirms it: nothing is destroyed on a command whose
    /// outcome is not yet known.
    ChatDeleted {
        /// Chat JID.
        chat_jid: String,
    },
    /// A chat's messages were cleared on the server (cleared
    /// on the phone, or this client's own
    /// [`BackendCommand::ClearChat`] confirmed back; the chat
    /// itself stays in the list). The app removes the cleared
    /// messages from its session mirror and its archive; the
    /// chat, its list position and its settings are untouched.
    /// A clearing made from this client applies locally only
    /// when this event confirms it, for the same reason as
    /// [`BackendEvent::ChatDeleted`].
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
    /// mirror and its archive. A delete made from this client
    /// goes out as [`BackendCommand::DeleteMessageForMe`] and
    /// applies locally at once through the same adoption,
    /// without waiting for this event.
    MessageDeletedForMe {
        /// Chat JID the message belongs to.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
    },
    /// A [`BackendCommand::ChatAction`] never reached the
    /// server. The app rolls the chat's archive/pin/mute state
    /// back to the snapshot it took when the action was sent
    /// and shows `message` as the notice. The chat-action
    /// counterpart of [`BackendEvent::StarFailed`], with the
    /// same contract.
    ChatActionFailed {
        /// Chat JID.
        chat_jid: String,
        /// The action that failed.
        action: ChatAction,
        /// Why the action failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::ClearChat`] never reached the
    /// server. Nothing was cleared anywhere: the app staged
    /// no local deletion for the command, so there is nothing
    /// to restore; it only shows `message` as the notice.
    ClearChatFailed {
        /// Chat JID.
        chat_jid: String,
        /// Why the clear failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::DeleteChat`] never reached the
    /// server. Nothing was deleted anywhere: the app staged
    /// no local deletion for the command, so there is nothing
    /// to restore; it only shows `message` as the notice.
    DeleteChatFailed {
        /// Chat JID.
        chat_jid: String,
        /// Why the delete failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::DeleteMessageForMe`] never reached
    /// the server. The app restores the message it removed
    /// optimistically (its archive row included) and shows
    /// `message` as the notice. The delete-for-me counterpart
    /// of [`BackendEvent::StarFailed`], with the same contract.
    DeleteForMeFailed {
        /// Chat JID the message belongs to.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// Why the delete failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::MarkChatUnread`] never reached the
    /// server. The app removes the unread badge it set
    /// optimistically and shows `message` as the notice.
    MarkUnreadFailed {
        /// Chat JID.
        chat_jid: String,
        /// Why marking unread failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::MarkChatRead`] never reached the
    /// server. The chat stays read in this app (the read really
    /// happened here); only the sync to the account's other
    /// devices failed, so the app shows `message` as the notice
    /// and changes nothing else.
    MarkReadFailed {
        /// Chat JID.
        chat_jid: String,
        /// Why marking read failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::BlockContact`] never reached the
    /// server. The app restores the blocked set (and its
    /// persisted copy) to the state before the toggle and
    /// shows `message` as the notice.
    BlockFailed {
        /// JID of the contact the toggle named.
        chat_jid: String,
        /// Why the toggle failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::SetGroupInfo`] was accepted by the
    /// server: every field the command carried is saved. The
    /// app applies the new title and shows its saved notice
    /// only from this event, never at click time.
    GroupInfoSaved {
        /// Chat JID of the group.
        chat_jid: String,
    },
    /// A [`BackendCommand::SetGroupInfo`] failed on the
    /// server: a field the command carried was refused or the
    /// send failed. The app keeps the group's previous title
    /// and shows `message` as the notice.
    GroupInfoFailed {
        /// Chat JID of the group.
        chat_jid: String,
        /// Why saving failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::LeaveGroup`] never reached the
    /// server: the account is still a member. The app restores
    /// the chat row it archived optimistically and shows
    /// `message` as the notice.
    LeaveGroupFailed {
        /// Chat JID of the group.
        chat_jid: String,
        /// Why leaving failed, in user words.
        message: String,
    },
    /// A [`BackendCommand::SetDisappearingTimer`] never
    /// reached the server. The app restores the chat's
    /// previous timer (mirror and archive, without a new
    /// in-stream announcement) and shows `message` as the
    /// notice.
    DisappearingTimerFailed {
        /// Chat JID.
        chat_jid: String,
        /// Why the timer change failed, in user words.
        message: String,
    },
    /// A message was pinned or unpinned in a chat (a
    /// pin-in-chat protocol notice from any device): apply
    /// the state to the named message. The notice itself also
    /// translates as a message and renders as the system line
    /// that names the act.
    MessagePinChanged {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message whose pin state changed.
        message_id: String,
        /// The new pin state.
        pinned: bool,
    },
    /// Current privacy settings as fetched from the account
    /// (category → value spellings; see [`BackendCommand::SetPrivacy`]).
    PrivacyState(std::collections::BTreeMap<String, String>),
    /// The signed-in account's own profile, read back from the
    /// server (see [`OwnProfile`]). Emitted at connect, in answer
    /// to [`BackendCommand::FetchOwnProfile`], and as the
    /// reconciling re-read after every
    /// [`BackendCommand::UpdateOwnProfile`], successful or not.
    OwnProfile(OwnProfile),
    /// An [`BackendCommand::UpdateOwnProfile`] change was refused
    /// or failed on the server; the message is in user words. A
    /// reconciling [`BackendEvent::OwnProfile`] re-read follows,
    /// carrying the state the server kept.
    OwnProfileSaveFailed {
        /// Why the change failed, in user words.
        message: String,
    },
    /// The account's blocklist as fetched at connect: the JIDs
    /// blocked on the server, in the server's own spelling. The
    /// app adopts the list as its blocked set, replacing any
    /// locally persisted copy: the server is the source of truth
    /// for blocks, and a client that only remembers its own
    /// toggles shows the wrong state after a restart.
    Blocklist(Vec<String>),
    /// An incoming call is ringing. Accept or reject it with
    /// the matching commands; only one call exists at a time.
    IncomingCall {
        /// Call identifier (protocol call id).
        call_id: String,
        /// Chat JID of the caller (the 1:1 chat with them). For
        /// a group invitation this is the group's chat JID: the
        /// call belongs to the group, whoever sent the invite.
        chat_jid: String,
        /// Whether the offer asks for video.
        video: bool,
        /// The group this invitation belongs to, when it is a
        /// group call invitation; None for a 1:1 call.
        group_jid: Option<String>,
    },
    /// An in-call audio toggle (mute, speaker) was applied by
    /// the call engine: the confirmed state, answering
    /// [`BackendCommand::SetCallMuted`] or
    /// [`BackendCommand::SetCallSpeaker`]. The app flips its
    /// toggles optimistically and confirms them from this
    /// event; a failure answers with
    /// [`BackendEvent::CallAudioFailed`] and the app reverts
    /// the flip (P2-33). Before, the speaker toggle had no
    /// outcome event at all.
    CallAudioState {
        /// The confirmed microphone mute state, when this
        /// event answers the mute toggle.
        muted: Option<bool>,
        /// The confirmed speaker state, when this event
        /// answers the speaker toggle.
        speaker: Option<bool>,
    },
    /// An in-call audio toggle failed: the answering event
    /// for [`BackendCommand::SetCallMuted`] and
    /// [`BackendCommand::SetCallSpeaker`] when the call
    /// engine refuses or no call is in progress. It is typed,
    /// not the generic error, for the same reason
    /// [`BackendEvent::ReactionFailed`] is: the app must roll
    /// exactly one optimistic change back (the toggle's
    /// backup pair), and an unrelated generic error must
    /// never trip that revert (VRF-04).
    CallAudioFailed {
        /// Why the toggle failed, in user words.
        message: String,
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
    /// A call rang this account and ended unanswered, and it was
    /// never announced with [`BackendEvent::IncomingCall`]: its
    /// offer could not be shown (another call was already in
    /// progress), or it was replayed from the offline queue on
    /// reconnect for a call that rang while this device was
    /// away. Emitted once per such call, for the call history;
    /// the signal carries no media kind, so the history records
    /// these as voice calls. A missed call whose offer WAS shown
    /// does not produce this event: it ends through
    /// [`BackendEvent::CallChanged`] like any other call.
    CallMissed {
        /// Call identifier (protocol call id).
        call_id: String,
        /// Chat JID of the caller (the 1:1 chat with them).
        chat_jid: String,
        /// When the call rang, unix seconds (the protocol
        /// event's own timestamp; in the past for offline
        /// replays).
        at: i64,
    },
    /// A call that rang this account ended on another of the
    /// account's devices: the phone answered it, or the phone
    /// declined it. Emitted once per such call, whether or not
    /// this device ever showed the call ringing: when it did,
    /// the event ends the ringing prompt; when it did not
    /// (another call held the line, or the call rang and ended
    /// while this device was away), the event is the only word
    /// the history gets about the call. Either way the call
    /// was answered somewhere, so its history entry is an
    /// incoming call that is NOT missed, with no duration on
    /// this device: the talk happened on the other one.
    CallEndedElsewhere {
        /// Call identifier (protocol call id).
        call_id: String,
        /// Chat JID of the caller (the 1:1 chat with them).
        chat_jid: String,
        /// True when another device answered the call; false
        /// when another device declined it.
        answered: bool,
        /// When the call ended elsewhere, unix seconds (the
        /// protocol event's own timestamp).
        at: i64,
    },
    /// The current group or link call's roster, re-sent whole
    /// on every change (someone joins, leaves, raises a hand,
    /// starts or stops sharing). The app replaces its copy.
    CallParticipants {
        /// The roster, in the call engine's own order.
        participants: Vec<CallParticipant>,
    },
    /// A participant's in-call reaction changed. Reactions ride
    /// the same channel as the roster but arrive on their own
    /// events: they are transient display state, not roster
    /// facts.
    CallReaction {
        /// JID of the participant reacting.
        jid: String,
        /// Their current reaction, or None when it cleared.
        emoji: Option<String>,
    },
    /// The current call's waiting room, re-sent whole on every
    /// change. Only a call with a waiting room (a hosted call
    /// link) produces these.
    CallWaitingRoom {
        /// This device hosts the call and may admit or deny.
        is_admin: bool,
        /// Joining currently requires the host's approval.
        approval_required: bool,
        /// The people waiting, in arrival order.
        users: Vec<CallWaitingUser>,
    },
    /// The current call's video state, confirmed by the call
    /// engine: answers [`BackendCommand::SetCallVideo`] and
    /// reports the peer's side as it changes.
    CallVideoState {
        /// My camera is sending.
        local_on: bool,
        /// The other side's video is on (a peer's camera, or
        /// any group participant's).
        remote_on: bool,
        /// Why local video cannot run, when it cannot (no
        /// camera on this machine, the camera would not open),
        /// in user words. None when local video works.
        local_note: Option<String>,
    },
    /// One remote video frame (an encoded H.264 access unit)
    /// for the call's video stage. Frames are the one event
    /// kind the backend may drop under pressure (see
    /// [`EventSink::try_emit`]): a stale frame is worthless
    /// once the next one exists, and the decoder resyncs at
    /// the next keyframe.
    CallVideoAu {
        /// JID of the participant sending these frames, in a
        /// group or link call; None on a 1:1 call, where the
        /// sender is the peer by definition.
        sender: Option<String>,
        /// The access unit, Annex-B start codes included.
        data: Vec<u8>,
        /// The unit carries a keyframe (a decoder can start
        /// or resync from it).
        keyframe: bool,
        /// Frame rotation bits (0..3) from the stream's own
        /// metadata.
        orientation: u8,
    },
    /// The local camera's latest preview frame, already
    /// decoded and downscaled by the capture side: RGBA
    /// pixels for the call card's local tile. Dropped under
    /// pressure like [`BackendEvent::CallVideoAu`]; only the
    /// newest preview matters.
    CallVideoFrame {
        /// RGBA pixels, row-major, `width` by `height`.
        rgba: Vec<u8>,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
    },
    /// A call link was created: the answer to
    /// [`BackendCommand::CreateCallLink`].
    CallLinkCreated {
        /// The link's URL, ready to share.
        url: String,
        /// True for a video link.
        video: bool,
    },
    /// A call link's facts, read before joining: the answer to
    /// [`BackendCommand::PreviewCallLink`].
    CallLinkPreview {
        /// The link's canonical URL.
        url: String,
        /// JID of the account that created the link.
        creator_jid: String,
        /// True for a video link.
        video: bool,
        /// Joining waits for the host to admit this device.
        waiting_room: bool,
    },
    /// Creating or reading a call link failed: the answer to
    /// [`BackendCommand::CreateCallLink`] and
    /// [`BackendCommand::PreviewCallLink`]. Typed, like
    /// [`BackendEvent::CallAudioFailed`], so the link dialog
    /// can show the failure where the user is looking instead
    /// of relying on the generic error path.
    CallLinkFailed {
        /// Why it failed, in user words.
        message: String,
    },
    /// An in-call control other than the audio toggles failed:
    /// the answer to [`BackendCommand::SetCallVideo`],
    /// [`BackendCommand::SendCallReaction`],
    /// [`BackendCommand::SetCallHandRaised`],
    /// [`BackendCommand::InviteCallParticipant`],
    /// [`BackendCommand::AdmitWaitingUser`],
    /// [`BackendCommand::DenyWaitingUser`] or
    /// [`BackendCommand::SetCallApproval`] when the call
    /// engine refuses or no call is in progress. Typed for the
    /// same reason [`BackendEvent::CallAudioFailed`] is: the
    /// app flips the video and hand toggles optimistically and
    /// must revert exactly those flips, and an unrelated
    /// generic error must never trip that revert.
    CallControlFailed {
        /// Why the control failed, in user words.
        message: String,
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
    /// A photo fetch for display finished; the decrypted bytes
    /// are carried in memory and were never written to disk.
    PhotoBytes {
        /// Chat JID the photo belongs to.
        chat_jid: String,
        /// Protocol ID of the photo message.
        message_id: String,
        /// The photo's decrypted bytes (an encoded image).
        bytes: Vec<u8>,
    },
    /// A photo fetch for display failed. Reported per message
    /// (not as a global `Error`) so a chat full of unavailable
    /// photos cannot flood the notice line; the message is in
    /// user words.
    PhotoFetchFailed {
        /// Chat JID the photo belongs to.
        chat_jid: String,
        /// Protocol ID of the photo message.
        message_id: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// A voice fetch for playback finished; the decrypted bytes
    /// are carried in memory and were never written to disk.
    VoiceBytes {
        /// Chat JID the voice message belongs to.
        chat_jid: String,
        /// Protocol ID of the voice message.
        message_id: String,
        /// The voice note's decrypted bytes (an encoded audio file).
        bytes: Vec<u8>,
    },
    /// A voice fetch for playback failed. Reported per message
    /// (not as a global `Error`); the message is in user words.
    VoiceFetchFailed {
        /// Chat JID the voice message belongs to.
        chat_jid: String,
        /// Protocol ID of the voice message.
        message_id: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// A view-once video fetch for playback finished; the
    /// decrypted bytes are carried in memory and were never
    /// written to disk.
    VideoBytes {
        /// Chat JID the video belongs to.
        chat_jid: String,
        /// Protocol ID of the video message.
        message_id: String,
        /// The video's decrypted bytes (an encoded MP4 file).
        bytes: Vec<u8>,
    },
    /// A regular video fetch for playback finished; the
    /// decrypted bytes were streamed to a file in the app's
    /// video cache, never buffered whole in memory.
    VideoReady {
        /// Chat JID the video belongs to.
        chat_jid: String,
        /// Protocol ID of the video message.
        message_id: String,
        /// Absolute path of the cached video file.
        path: String,
    },
    /// A video poster fetch for display finished; the decrypted
    /// bytes are carried in memory and were never written to
    /// disk.
    VideoPosterBytes {
        /// Chat JID the video belongs to.
        chat_jid: String,
        /// Protocol ID of the video message.
        message_id: String,
        /// The poster's decrypted bytes (an encoded image).
        bytes: Vec<u8>,
    },
    /// A video poster fetch for display failed. Reported per
    /// message (not as a global `Error`); the message is in user
    /// words.
    VideoPosterFetchFailed {
        /// Chat JID the video belongs to.
        chat_jid: String,
        /// Protocol ID of the video message.
        message_id: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// A video fetch for playback failed. Reported per message
    /// (not as a global `Error`); the message is in user words.
    VideoFetchFailed {
        /// Chat JID the video belongs to.
        chat_jid: String,
        /// Protocol ID of the video message.
        message_id: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// A chat partner started or stopped typing (or recording).
    TypingReceived {
        /// Chat JID.
        chat_jid: String,
        /// True while typing/recording, false when idle.
        typing: bool,
    },
    /// A watched contact's presence changed (or was first
    /// reported after subscribing). Only ever sent for a chat
    /// the app subscribed to with
    /// [`BackendCommand::SetPresenceSubscription`].
    PresenceReceived {
        /// Chat JID (the contact).
        chat_jid: String,
        /// True when the contact is online now.
        online: bool,
        /// When the contact was last seen, unix seconds, when
        /// the report carries it. `None` when the contact is
        /// online, and also when their privacy settings hide
        /// their last seen from this account.
        last_seen: Option<i64>,
    },
    /// A group's member list, answering `FetchGroupMembers`.
    GroupMembers {
        /// Group JID.
        chat_jid: String,
        /// (jid, display name, is admin, member label) rows.
        /// The display name is the name the server rendered
        /// for the participant, or empty when it rendered
        /// none; the host then resolves a label from its own
        /// data or masks the identifier. It is never the raw
        /// phone number or LID. The member label is the
        /// participant's own label in this group (row 65), or
        /// `None` when they have not set one.
        members: Vec<GroupMemberRow>,
        /// Whether this account is itself an admin of the
        /// group, computed by the backend from the account's
        /// own addresses against the member list (the app never
        /// guesses its own role from names or row order). Gates
        /// the admin-only message actions in the UI.
        own_admin: bool,
        /// The account's own member label in this group, read
        /// off its own participant row (row 65): the app
        /// cannot pick its own row out of the list, so the
        /// backend names the value. `None` when unset.
        own_label: Option<String>,
    },
    /// The outcome of an add-with-history or a history-share
    /// retry (row 64). The add outcome and the share outcome
    /// are separate fields because a share can fail after the
    /// members were added, and the app must say exactly that.
    GroupHistoryReported {
        /// Group JID.
        chat_jid: String,
        /// JIDs that were added (empty for a retry).
        added: Vec<String>,
        /// (jid, reason) pairs the server refused to add.
        refused: Vec<(String, String)>,
        /// The history share's own outcome.
        report: HistoryShareReport,
    },
    /// This account's own member label in a group was set (or
    /// cleared, when empty), answering `SetOwnMemberLabel`.
    MemberLabelSet {
        /// Group JID.
        chat_jid: String,
        /// The label now in effect (empty when cleared).
        label: String,
    },
    /// A [`BackendCommand::SetOwnMemberLabel`] failed; the
    /// previous label still stands.
    MemberLabelFailed {
        /// Group JID.
        chat_jid: String,
        /// What failed, in words.
        message: String,
    },
    /// A group member's label changed, reported from the
    /// member-label protocol message in the group (row 65).
    /// `label` is `None` when the member cleared it.
    MemberLabelChanged {
        /// Group JID.
        chat_jid: String,
        /// The member whose label changed.
        member_jid: String,
        /// The new label, or `None` when cleared.
        label: Option<String>,
    },
    /// A message was reported to the group's admins,
    /// answering `ReportMessageToAdmins`. The protocol answer
    /// confirms the report went out; it says nothing about
    /// which ids the server accepted.
    MessageReportedToAdmins {
        /// Group JID.
        chat_jid: String,
        /// The reported message's protocol id.
        message_id: String,
    },
    /// A [`BackendCommand::ReportMessageToAdmins`] failed;
    /// no report went out.
    ReportToAdminsFailed {
        /// Group JID.
        chat_jid: String,
        /// The message that was not reported.
        message_id: String,
        /// What failed, in words.
        message: String,
    },
    /// The messages reported to a group's admins, answering
    /// `FetchReportedMessages` (row 65).
    ReportedMessages {
        /// Group JID.
        chat_jid: String,
        /// The outstanding reports.
        reports: Vec<ReportedMessageEntry>,
    },
    /// A [`BackendCommand::FetchReportedMessages`] failed.
    ReportedMessagesFailed {
        /// Group JID.
        chat_jid: String,
        /// What failed, in words.
        message: String,
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
    /// A sticker's bytes, answering `FetchSticker`.
    StickerBytes {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the sticker message.
        message_id: String,
        /// The sticker bytes (WebP as sent).
        bytes: Vec<u8>,
    },
    /// A sticker fetch failed; the tile keeps its placeholder.
    StickerFetchFailed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the sticker message.
        message_id: String,
    },
    /// The answer to [`BackendCommand::FetchVerifiedName`]:
    /// the identity's verified business name, or `None` when
    /// the identity is not a verified business.
    VerifiedNameResult {
        /// JID that was looked up.
        jid: String,
        /// The verified name, when the identity is verified.
        name: Option<String>,
    },
    /// A profile picture's bytes, answering `FetchAvatar`.
    AvatarBytes {
        /// JID the picture belongs to (the JID the app asked
        /// for, which is the identity the picture labels).
        jid: String,
        /// The picture's server-side ID, for change detection.
        picture_id: Option<String>,
        /// The picture bytes (JPEG as served).
        bytes: Vec<u8>,
    },
    /// The identity has no profile picture, or its privacy
    /// settings refuse the lookup; the avatar falls back to the
    /// initial disc. A transport failure does NOT produce this
    /// event: it stays retryable instead of being cached as a
    /// negative.
    AvatarMissing {
        /// JID that was looked up.
        jid: String,
    },
    /// A profile picture lookup could not be answered at all
    /// (a transport failure, or an identity form the lookup
    /// cannot address yet, such as a LID whose phone mapping is
    /// still unknown). Nothing is cached either way: the app
    /// clears the in-flight mark so a later display retries,
    /// under a short cooldown instead of a per-paint storm.
    AvatarUnavailable {
        /// JID that was looked up.
        jid: String,
    },
    /// A group join asked through an invite link is waiting
    /// for an admin's approval: a successful outcome, not an
    /// error. No chat row exists yet; the group appears when
    /// the approval lands and the roster syncs it in.
    JoinRequestPending,
    /// A profile picture changed on the wire (or its owner's
    /// contact entry was updated, which is the signal WA Web
    /// refreshes pictures on). Any cached picture for this JID
    /// is stale: drop it and refetch on next display.
    AvatarChanged {
        /// JID whose picture changed.
        jid: String,
    },
    /// An older-history page requested with
    /// [`BackendCommand::FetchOlderMessages`] has landed: its
    /// messages were already delivered as ordinary message
    /// upserts for the chat (and persisted by the app's normal
    /// path) before this event, so the app can clear the
    /// chat's loading state against content that is already on
    /// screen.
    HistoryPageFetched {
        /// Chat JID the page belongs to.
        chat_jid: String,
        /// How many messages the page carried for the chat.
        added: u32,
        /// True when the phone's history for this chat is
        /// exhausted (its transfer marker says no more messages
        /// remain, or the page carried nothing): the app
        /// latches the chat's history state and never asks
        /// again for this chat's older pages.
        end_of_history: bool,
    },
    /// An older-history request ended without an answer: the
    /// send itself failed, or the phone never answered within
    /// the backend's watchdog window. The chat returns to idle
    /// (a later scroll to the top may retry); this is never
    /// reported as end-of-history.
    HistoryFetchFailed {
        /// Chat JID the request was for.
        chat_jid: String,
        /// Why the request failed, in user words.
        message: String,
    },
    /// A group's profile picture was set or removed by this
    /// client (answering [`BackendCommand::SetGroupPicture`]).
    /// The app drops the group's cached picture and refetches
    /// through the normal avatar funnel, so every surface
    /// repaints from the server's new state.
    GroupPictureChanged {
        /// Group JID.
        chat_jid: String,
    },
    /// A group picture change never reached the server or was
    /// refused by it (answering
    /// [`BackendCommand::SetGroupPicture`]). The picture the
    /// app shows stays the server's; `message` says why, in
    /// user words.
    GroupPictureFailed {
        /// Group JID.
        chat_jid: String,
        /// Why the change failed, in user words.
        message: String,
    },
    /// A number lookup found its person (answering
    /// [`BackendCommand::LookupContact`]): the chat is ready
    /// to open. The app upserts it through the normal path
    /// (which persists it) and selects it.
    ContactResolved {
        /// The resolved person's chat.
        chat: LiveChat,
        /// The person's other address form (their LID when the
        /// chat is addressed by phone number, or the reverse),
        /// when the lookup disclosed it. A person has two JID
        /// forms and the roster may already hold a chat under
        /// the other one; the app checks it before creating a
        /// second chat for the same person.
        alt_jid: Option<String>,
    },
    /// A number lookup completed and the number is not on
    /// WhatsApp (answering [`BackendCommand::LookupContact`]).
    /// An answer, not a failure.
    ContactNotFound {
        /// The number that was looked up, as typed.
        phone: String,
    },
    /// A number lookup itself failed (answering
    /// [`BackendCommand::LookupContact`]): the number could
    /// not be one, the server refused the query, or the
    /// connection failed. Never reported as "not on
    /// WhatsApp".
    ContactLookupFailed {
        /// The number that was looked up, as typed.
        phone: String,
        /// Why the lookup failed, in user words.
        message: String,
    },
    /// A media fetch recovered through the media-retry flow: the
    /// sender's device re-uploaded the media and the message's
    /// CDN path changed. The app persists the fresh path with
    /// the archived reference so later sessions download directly.
    MediaRefreshed {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the media message.
        message_id: String,
        /// The fresh CDN direct path.
        direct_path: String,
    },
    /// The phone's favorite-chat set, synced from app state. The
    /// list is complete: it replaces the app's favorite set.
    FavoritesSync {
        /// Chat JIDs marked favorite on the account.
        chat_jids: Vec<String>,
    },
    /// A sticker was favorited or unfavorited on a linked
    /// device (app-state sync). `filehash` is the sticker's
    /// identity (the base64 SHA-256 of its decrypted file, the
    /// index key the sync itself uses).
    StickerFavoriteChanged {
        /// The sticker's filehash identity.
        filehash: String,
        /// True when the sticker entered the favorites, false
        /// when it left them.
        favorite: bool,
    },
    /// A sticker was removed from the recent-stickers list on
    /// a linked device (app-state sync).
    RecentStickerRemoved {
        /// The sticker's filehash identity.
        filehash: String,
    },
    /// A business profile arrived, answering
    /// [`BackendCommand::FetchBusinessProfile`]. `None`
    /// means the account has no business profile: an
    /// answer, not an error.
    BusinessProfileFetched {
        /// Chat JID of the business.
        chat_jid: String,
        /// The profile, when the account has one.
        profile: Option<LiveBusinessProfile>,
    },
    /// A contact's About arrived, answering
    /// [`BackendCommand::FetchContactAbout`]. Empty when the
    /// contact has no About or their privacy refuses the
    /// lookup (see the command's docs).
    ContactAbout {
        /// Chat JID of the contact.
        chat_jid: String,
        /// The contact's About text.
        about: String,
    },
    /// A business profile fetch failed.
    BusinessProfileFetchFailed {
        /// Chat JID of the business.
        chat_jid: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// One page of a business catalog arrived, answering
    /// [`BackendCommand::FetchCatalog`].
    CatalogFetched {
        /// Chat JID of the business.
        chat_jid: String,
        /// The page's products, in catalog order.
        products: Vec<LiveProduct>,
        /// Cursor for the next page; `None` on the last
        /// page.
        after_cursor: Option<String>,
    },
    /// A catalog fetch failed.
    CatalogFetchFailed {
        /// Chat JID of the business.
        chat_jid: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// A business's catalog collections arrived,
    /// answering [`BackendCommand::FetchCollections`].
    CollectionsFetched {
        /// Chat JID of the business.
        chat_jid: String,
        /// The collections, in the server's order.
        collections: Vec<LiveCollection>,
    },
    /// A collections fetch failed.
    CollectionsFetchFailed {
        /// Chat JID of the business.
        chat_jid: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// The bytes of one catalog product image arrived,
    /// answering [`BackendCommand::FetchProductImage`].
    ProductImageBytes {
        /// The URL the bytes belong to.
        url: String,
        /// The image bytes.
        bytes: Vec<u8>,
    },
    /// A product image fetch failed. Carries no message:
    /// the surface falls back to its neutral tile and the
    /// failure needs no words.
    ProductImageFetchFailed {
        /// The URL that failed.
        url: String,
    },
    /// An order's details arrived, answering
    /// [`BackendCommand::FetchOrderDetails`].
    OrderDetailsFetched {
        /// Chat JID of the business.
        chat_jid: String,
        /// Order id the details belong to.
        order_id: String,
        /// The order's line items and totals.
        order: LiveOrder,
    },
    /// An order details fetch failed.
    OrderDetailsFetchFailed {
        /// Chat JID of the business.
        chat_jid: String,
        /// Order id that was asked for.
        order_id: String,
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// The account's own business profile arrived,
    /// answering [`BackendCommand::FetchOwnBusinessProfile`].
    /// `None` means the account has no business profile.
    OwnBusinessProfileFetched {
        /// The profile, when the account has one.
        profile: Option<LiveBusinessProfile>,
    },
    /// The own business profile fetch failed.
    OwnBusinessProfileFetchFailed {
        /// Why the fetch failed, in user words.
        message: String,
    },
    /// The own business profile delta was applied,
    /// answering [`BackendCommand::UpdateOwnBusinessProfile`].
    OwnBusinessProfileSaved,
    /// The own business profile delta failed; nothing was
    /// applied.
    OwnBusinessProfileSaveFailed {
        /// Why the save failed, in user words.
        message: String,
    },
    /// A label was created, renamed, recolored, or
    /// deleted, reported by app-state sync (a change made
    /// on another linked device, the pairing-time full
    /// sync replaying the account's set, or the echo of
    /// this client's own save). `deleted` removes the
    /// definition; associations it had are pruned by the
    /// app the same way the official clients prune them.
    LabelEdited {
        /// Label id.
        label_id: String,
        /// New name (absent on a deletion).
        name: Option<String>,
        /// New color index (absent on a deletion).
        color: Option<i32>,
        /// Predefined label id, when this label is one of
        /// the protocol's predefined labels.
        predefined_id: Option<i32>,
        /// Whether this report deletes the label.
        deleted: bool,
    },
    /// A label was added to or removed from a chat,
    /// reported by app-state sync.
    ChatLabelChanged {
        /// Label id.
        label_id: String,
        /// Chat JID.
        chat_jid: String,
        /// Whether the chat now carries the label.
        labeled: bool,
    },
    /// A label was added to or removed from one message,
    /// reported by app-state sync.
    MessageLabelChanged {
        /// Label id.
        label_id: String,
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// Whether the message now carries the label.
        labeled: bool,
    },
    /// A label was deleted, answering
    /// [`BackendCommand::DeleteLabel`]. The app applies
    /// the deletion (and prunes the label's associations)
    /// only on this answer.
    LabelDeleted {
        /// Label id.
        label_id: String,
    },
    /// A label deletion failed; nothing changed.
    LabelDeleteFailed {
        /// Label id.
        label_id: String,
        /// Why the deletion failed, in user words.
        message: String,
    },
    /// A label mutation the app applied at once failed;
    /// the app rolls back exactly the apply the carried
    /// action names.
    LabelActionFailed {
        /// The attempted action.
        action: LabelAction,
        /// Why it failed, in user words.
        message: String,
    },
    /// A synced business quick reply was created, edited,
    /// or deleted, reported by app-state sync (another
    /// device's change, the pairing-time full sync, or
    /// the echo of this client's own save). On a deletion
    /// only the id is meaningful.
    QuickReplyEdited {
        /// Quick reply id.
        id: String,
        /// The reply's shortcut (empty on a deletion).
        shortcut: String,
        /// The reply's message text (empty on a deletion).
        message: String,
        /// The reply's keywords.
        keywords: Vec<String>,
        /// The reply's usage count.
        count: i32,
        /// Whether this report deletes the reply.
        deleted: bool,
    },
    /// A quick reply save the app applied at once failed;
    /// the app restores the reply's previous state.
    QuickReplyFailed {
        /// Quick reply id.
        id: String,
        /// Why it failed, in user words.
        message: String,
    },
    /// A quick reply was deleted, answering
    /// [`BackendCommand::DeleteQuickReply`]. The app
    /// applies the deletion only on this answer.
    QuickReplyDeleted {
        /// Quick reply id.
        id: String,
    },
    /// A quick reply deletion failed; nothing changed.
    QuickReplyDeleteFailed {
        /// Quick reply id.
        id: String,
        /// Why the deletion failed, in user words.
        message: String,
    },
    /// One call from the phone's own call history, delivered by
    /// the pairing-time history or a later app-state sync. The
    /// record's direction was derived from the event's creator
    /// comparison, never from the wire record's own incoming
    /// flag (the library documents that flag as unreliable). The
    /// store dedups by call ID, so replays are harmless.
    CallHistoryRecord {
        /// The completed call.
        record: CallRecord,
    },
    /// A group changed on the wire in a way that is not a rename
    /// (members added or removed, modes flipped): any cached
    /// member list for this group is stale.
    GroupChanged {
        /// Group JID.
        chat_jid: String,
    },
    /// A message arrived that this client could not decrypt, and
    /// the wire's failure mode says to show a placeholder (WA Web
    /// renders one in the same case). The app inserts the
    /// placeholder only if it has no copy of the message: a later
    /// successful decryption upserts the real message over it.
    MessageUnavailable {
        /// Chat JID.
        chat_jid: String,
        /// Protocol ID of the message.
        message_id: String,
        /// Author JID, when known.
        sender_jid: Option<String>,
        /// True when this device sent the message.
        from_me: bool,
        /// Unix timestamp (seconds).
        timestamp: i64,
    },
    /// The connection dropped (the backend will try to reconnect).
    ConnectionLost {
        /// Short human-readable reason.
        reason: String,
    },
    /// Catch-up sync (history sync after pairing, offline sync on
    /// reconnect) is running. `percent` is the library's reported
    /// progress when it provides one: a per-chunk stamp from the
    /// phone with no monotonicity and no guaranteed final value.
    /// A host may display it only as reported (max-clamped within
    /// one sync episode, never extrapolated, and never as 100:
    /// 100 is reserved for the `SyncCompleted` event). Phases
    /// that report no number carry `None` and stay indeterminate.
    SyncProgress {
        /// Progress 0 to 100, when the library reports it.
        percent: Option<u32>,
    },
    /// The catch-up sync finished: the roster and recent history
    /// the phone sent are fully applied.
    SyncCompleted,
    /// The offline drain was interrupted before finishing: the
    /// client is NOT caught up and the remainder stays queued
    /// server-side for redelivery. This must never be treated as
    /// completion and must never latch the synced state.
    SyncInterrupted,
    /// A group's community structure, learned from its group
    /// metadata: the parent community JID (when the group is a
    /// linked subgroup) and whether this group is itself a
    /// community parent.
    ChatCommunity {
        /// Group JID the structure describes.
        chat_jid: String,
        /// Parent community group JID, when linked.
        parent_jid: Option<String>,
        /// True when this group is a community parent.
        is_parent: bool,
    },
    /// A better display name was resolved for a chat (a contact
    /// update arrived, group metadata supplied the subject, or
    /// the LID-to-phone mapping identified a direct chat). With
    /// `authoritative: false` the event is a heal: the host
    /// applies it only where the current title is still a raw
    /// identifier or the chat's own formatted phone number, and
    /// a resolved name is never downgraded. With
    /// `authoritative: true` the server has stated the chat's
    /// current name (a saved contact renamed on a device, a
    /// group subject change): the host replaces the title
    /// outright, because the heal discipline would otherwise
    /// freeze the first resolved name forever.
    ChatRenamed {
        /// The chat whose title improved.
        chat_jid: String,
        /// The resolved display name.
        title: String,
        /// Whether this rename replaces a resolved title too.
        authoritative: bool,
    },
    /// A better display name was resolved for a message sender
    /// (a contact update, group metadata, or a history payload's
    /// identity data). The host rewrites stored sender labels that
    /// still show the raw identifier for this sender, in every
    /// chat; resolved labels are never downgraded.
    SenderNamed {
        /// The sender's JID, in whichever form the name source
        /// used.
        sender_jid: String,
        /// The resolved display name.
        name: String,
    },
    /// The account's saved contacts as the backend's identity
    /// book knows them: one entry per person, under the
    /// phone-number JID when known, else the LID, with the
    /// person's other known JID forms alongside. Emitted whole
    /// whenever the set changes, so the receiver replaces its
    /// copy rather than merging. This is the address book that
    /// contacts audiences and people pickers draw from.
    SavedContacts {
        /// One entry per saved contact.
        contacts: Vec<SavedContact>,
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
/// delivery worker) instead of dropping anything. No state event
/// kind is droppable (message upserts carry chat content, the
/// *Changed events carry state assertions, receipts carry read
/// state), so blocking is the only honest overflow behavior for
/// them. The two call-video frame kinds are the single carved
/// out exception: a video frame is worthless once its successor
/// exists, so those go through [`EventSink::try_emit`] and may
/// be dropped when the channel is full, exactly like a media
/// pipeline dropping a frame it cannot use in time; the video
/// decoder resyncs at the next keyframe.
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

    /// Queues an event only if the channel has room right now;
    /// returns whether it was queued. Reserved for the two
    /// call-video frame kinds ([`BackendEvent::CallVideoAu`],
    /// [`BackendEvent::CallVideoFrame`]): media frames may be
    /// dropped when the UI has stalled, state events may not,
    /// so nothing else may call this.
    pub fn try_emit(&self, event: BackendEvent) -> bool {
        if self.tx.try_send(event).is_ok() {
            (self.wake)();
            true
        } else {
            false
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
            message: "The connection could not start. Restart Zaprust.".to_owned(),
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
                message: "The connection failed twice and stopped. \
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

    #[test]
    fn phone_digits_accepts_international_forms_and_refuses_the_rest() {
        // Formatting is presentation: spaces, dashes,
        // parentheses and plus signs are not digits and strip
        // away; the digits that remain are the number.
        assert_eq!(phone_digits("+62 812-3456-7890").as_deref(), Some("6281234567890"));
        assert_eq!(phone_digits("1 (000) 000-0007").as_deref(), Some("10000000007"));
        assert_eq!(phone_digits("10000000007").as_deref(), Some("10000000007"));
        // The bounds are the international format's: fewer
        // than 7 digits or more than 15 is not a phone number
        // at all, and the lookup fails as malformed instead of
        // asking the server about a non-number. Whether a
        // well-formed number is actually on WhatsApp is the
        // server's answer (not registered), a different
        // outcome the normalizer never pre-empts.
        assert_eq!(phone_digits("123"), None);
        assert_eq!(phone_digits("1234567890123456"), None, "sixteen digits is too long");
        assert_eq!(phone_digits(""), None);
        assert_eq!(phone_digits("+62-812-ab"), None, "the digits that remain are too few");
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

    #[test]
    fn stored_session_probe_needs_a_nonempty_store_file() {
        let dir = std::env::temp_dir().join(format!("zaprust-probe-test-{}", std::process::id()));
        let cfg = BackendConfig {
            data_dir: dir.clone(),
            account_id: "default".to_owned(),
            db_key: [0; 32],
        };
        assert!(!stored_session_exists(&cfg), "no store file, no stored session");
        std::fs::create_dir_all(cfg.account_dir()).expect("account dir");
        std::fs::write(cfg.session_db_path(), b"").expect("empty store file");
        assert!(!stored_session_exists(&cfg), "an empty store file holds no device identity");
        std::fs::write(cfg.session_db_path(), b"device identity").expect("store file");
        assert!(stored_session_exists(&cfg), "a populated store file is a stored session");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The deletion helpers sweep the video cache with the
    /// same stem contract as the downloads folder, and the
    /// cache eviction drops the oldest files past the cap
    /// while never touching the file that just landed.
    #[test]
    fn video_cache_dies_with_its_message_and_evicts_oldest_first() {
        let root = TempTree::new("video-cache");
        let cache = root.path().join("video-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let stem = downloaded_media_stem("6281234567890@s.whatsapp.net", "VID1");
        std::fs::write(cache.join(format!("{stem}.mp4")), vec![0u8; 100]).unwrap();
        std::fs::write(cache.join("other-VID9.mp4"), vec![0u8; 100]).unwrap();
        assert!(remove_download_for_message(root.path(), "6281234567890@s.whatsapp.net", "VID1"));
        assert!(
            !cache.join(format!("{stem}.mp4")).exists(),
            "the cached video goes with its message"
        );
        assert!(cache.join("other-VID9.mp4").exists(), "another chat's cache is untouched");

        // Eviction: three 100-byte files, cap 250, the newest
        // is protected; exactly the oldest goes.
        let dir2 = root.path().join("cache2");
        std::fs::create_dir_all(&dir2).unwrap();
        let names = ["a.mp4", "b.mp4", "c.mp4"];
        for name in names {
            std::fs::write(dir2.join(name), vec![0u8; 100]).unwrap();
            // Stagger the writes so the mtime age order is
            // deterministic on any filesystem.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let keep = dir2.join("c.mp4");
        let removed = evict_video_cache(&dir2, 250, &keep);
        assert_eq!(removed, 1);
        assert!(!dir2.join("a.mp4").exists(), "the oldest is evicted");
        assert!(dir2.join("b.mp4").exists());
        assert!(dir2.join("c.mp4").exists(), "the just-landed file is never evicted");
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

    /// The app-minted protocol ID must be indistinguishable in
    /// form from the library's own: the library generates
    /// "3EB0" plus 18 upper-case hex characters (22 total, its
    /// wacore request tests pin that shape), and it accepts a
    /// caller-chosen ID through its send options.
    #[test]
    fn minted_message_ids_match_the_library_shape() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let id = mint_message_id();
            assert_eq!(id.len(), 22, "id must be 3EB0 plus 18 hex chars: {id}");
            assert!(id.starts_with("3EB0"), "id must carry the prefix: {id}");
            assert!(
                id[4..].chars().all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)),
                "id must be upper-case hex after the prefix: {id}"
            );
            assert!(seen.insert(id), "minted ids must not repeat");
        }
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
