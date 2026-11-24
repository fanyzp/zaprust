//! Store, Zaprust's own chat archive.
//!
//! The protocol library's session store holds device keys and
//! protocol state only; it is not a chat database. This crate is the
//! app's archive: chats and messages, written through from backend
//! events and loaded on start in live mode.
//!
//! - SQLite via `rusqlite` with the bundled SQLCipher engine
//!   (builds the same on Linux, Windows, and macOS).
//! - Schema versioning via `PRAGMA user_version` with explicit
//!   migrations (see [`migrate`]).
//! - **Encrypted at rest** (security audit SEC-F01): the archive
//!   and the protocol library's session store are both SQLCipher
//!   databases under one key whose durable home is an owner-only
//!   key file beside the data, mirrored to the OS keyring (see
//!   [`dbkey`]). Archives written by older, plaintext builds are
//!   converted in place on first open ([`encrypt_file_in_place`]).
//! - **Physical deletion** (privacy audit A2): connections run
//!   with `secure_delete` on, so a deleted row's content is
//!   zeroed as its page is freed instead of lingering in the
//!   file; mass deletions additionally checkpoint and VACUUM
//!   (see [`Store::delete_chat`]), and a one-time purge on first
//!   open clears the free pages archives accumulated before
//!   this discipline existed (see [`Store::open`]).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;
use std::path::Path;

pub mod dbkey;

pub use dbkey::load_or_create_key;

use backend::{LiveChat, LiveChatKind, LiveMessage};
use rusqlite::{params, Connection};

/// Errors from the archive store.
#[derive(Debug)]
pub enum StoreError {
    /// Underlying SQLite error.
    Sqlite(rusqlite::Error),
    /// Filesystem error (for example while writing a backup copy).
    Io(std::io::Error),
    /// The database schema is newer than this build understands.
    SchemaTooNew {
        /// The schema version found in the file.
        found: i64,
        /// The newest schema version this build can migrate to.
        supported: i64,
    },
    /// The database carries a schema version this build cannot
    /// interpret at all (negative or otherwise invalid).
    BadSchemaVersion(i64),
    /// One message row's rich payload is not decodable. Named
    /// by chat and message so the failure is diagnosable;
    /// silently treating it as empty would erase the row's
    /// references and thumbnails on the next rewrite.
    CorruptData {
        /// Chat the corrupt row belongs to.
        chat_jid: String,
        /// The corrupt row's message ID.
        id: String,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "archive database error: {e}"),
            Self::Io(e) => write!(f, "archive file error: {e}"),
            Self::BadSchemaVersion(v) => {
                write!(f, "archive schema version {v} is not a version this build understands")
            }
            Self::SchemaTooNew { found, supported } => write!(
                f,
                "archive schema version {found} is newer than this build supports ({supported})"
            ),
            Self::CorruptData { chat_jid, id } => {
                write!(f, "archive message data is corrupt (chat {chat_jid}, message {id})")
            }
        }
    }
}

impl std::error::Error for StoreError {}

/// Applies the SQLCipher key to a fresh connection and proves it
/// by reading the schema: on a wrongly keyed database that read
/// fails, which is exactly the error the caller should surface.
fn apply_key(conn: &Connection, key: &[u8; 32]) -> Result<(), StoreError> {
    conn.execute_batch(&format!("PRAGMA key = \"{}\";", dbkey::key_pragma(key)))?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
    Ok(())
}

/// Converts a plaintext SQLite database file to a SQLCipher
/// database under `key`, in place. A missing file, an empty file,
/// and a file that does not start with the plaintext SQLite header
/// (already encrypted, or not a database at all) are left alone,
/// so callers can run this unconditionally before opening. Used
/// for the archive itself and, by the app, for the protocol
/// session store before the backend opens it.
///
/// # Errors
/// [`StoreError`] on filesystem or SQLite failures during the
/// conversion; the original file is replaced only after the
/// encrypted copy is complete.
pub fn encrypt_file_in_place(path: &Path, key: &[u8; 32]) -> Result<(), StoreError> {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return Ok(());
    };
    let mut header = [0u8; 16];
    let is_plaintext = file.read_exact(&mut header).is_ok() && &header == b"SQLite format 3\0";
    drop(file);
    if !is_plaintext {
        return Ok(());
    }
    let mut tmp_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(".encrypting");
    let tmp = path.with_file_name(tmp_name);
    let _ = std::fs::remove_file(&tmp);
    {
        let conn = Connection::open(path)?;
        // `sqlcipher_export` copies schema and data but not the
        // header's `user_version`, which the archive's migration
        // ladder keys on; carry it across explicitly or a legacy
        // database would restart the ladder from zero and collide
        // with its own tables.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let escaped = tmp.to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!(
            "ATTACH DATABASE '{escaped}' AS encrypted KEY \"{}\";",
            dbkey::key_pragma(key)
        ))?;
        conn.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))?;
        conn.execute_batch(&format!("PRAGMA encrypted.user_version = {version};"))?;
        conn.execute_batch("DETACH DATABASE encrypted;")?;
    }
    // The export read the plaintext database through its
    // connection, write-ahead log included; the sidecars of both
    // files are spent once the connections are closed.
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        let _ = std::fs::remove_file(format!("{}{suffix}", tmp.display()));
    }
    std::fs::rename(&tmp, path).map_err(StoreError::Io)?;
    Ok(())
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The newest schema version this build migrates to.
pub const SCHEMA_VERSION: i64 = 12;

/// `store_meta` key recording that the one-time freelist purge
/// (see [`Store::open`]) has run for this archive. The purge
/// rewrites the whole file, so it must happen exactly once per
/// archive, not on every startup; the flag is written only
/// after the purge completes, so a crash mid-purge simply
/// retries on the next open.
const FREELIST_PURGED_KEY: &str = "freelist_purged";

/// `store_meta` key recording that the one-time view-once
/// caption scrub has run for this archive. Same discipline as
/// [`FREELIST_PURGED_KEY`]: exactly once per archive, flag
/// written only after the scrub completes.
const VIEW_ONCE_CAPTIONS_SCRUBBED_KEY: &str = "view_once_captions_scrubbed";

/// `store_meta` key holding the persisted blocked-chat set (a
/// JSON array of chat ids). The set lives in this encrypted
/// archive rather than in the plaintext settings file: a chat id
/// is a fast non-cryptographic hash of a phone-number JID, and
/// the phone-number space is small enough to reverse by
/// dictionary, so the settings file would hand anyone who copies
/// it the user's blocked phone numbers (privacy audit A6). The
/// server's blocklist, fetched at every connect, stays the
/// source of truth; this copy only covers the time before that
/// fetch lands.
const BLOCKED_CHATS_KEY: &str = "blocked_chats";

/// Message flag bit 1 was the retired AI-generated marker; the
/// bit stays unassigned so existing archives keep their meaning.
/// Message flag bit: the sender deleted this message for everyone
/// while the archived body was retained locally. No longer set
/// (a revoke now always lands as FLAG_DELETED); the bit stays
/// assigned so existing archives keep their meaning.
pub const FLAG_DELETED_BY_SENDER: i64 = 2;
/// Message flag bit: the message was deleted for everyone and the
/// body replaced by the placeholder.
pub const FLAG_DELETED: i64 = 4;

/// Message flag: I starred this message.
pub const FLAG_STARRED: i64 = 8;

/// Message flag: this message is pinned in its chat.
pub const FLAG_PINNED_MESSAGE: i64 = 16;

/// Message flag: this view-once message has been opened. Once
/// opened it stays opened, so the state is persisted with the
/// message instead of living only in the session.
pub const FLAG_VIEW_ONCE_OPENED: i64 = 32;

/// Message flag: this disappearing message is kept in the chat
/// (keep in chat). A kept message does not disappear when its
/// timer runs out, so the state persists with the message like
/// the star and pin flags.
pub const FLAG_KEPT: i64 = 64;

/// The rich payload of one message, stored as JSON in the
/// `messages.data` column (schema v2). Absence (NULL) means a plain
/// text message with no extras.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct MessageData {
    #[serde(default)]
    kind: Option<backend::LiveMessageKind>,
    /// The author's JID, archived with the message so actions
    /// that must name the author (a group reaction above all)
    /// still work on history after a restart, not only on
    /// messages seen live this session. Rows written before
    /// this field existed read back as None.
    #[serde(default)]
    sender_jid: Option<String>,
    #[serde(default)]
    reactions: Vec<(String, u32)>,
    #[serde(default)]
    my_reaction: Option<String>,
    #[serde(default)]
    quoted_sender: Option<String>,
    #[serde(default)]
    quoted_body: Option<String>,
    #[serde(default)]
    link_preview: Option<backend::LivePreview>,
    #[serde(default)]
    edited: bool,
    #[serde(default)]
    forwarded: bool,
    #[serde(default)]
    view_once: bool,
    /// A photo's re-download reference, archived with the message
    /// so its bytes stay fetchable in later sessions. Dropped for
    /// a view-once photo once its single view has been spent (see
    /// [`Store::clear_photo_ref`]).
    #[serde(default)]
    photo_ref: Option<backend::PhotoRef>,
    /// A voice message's re-download reference, archived with the
    /// message so the note stays fetchable for playback in later
    /// sessions.
    #[serde(default)]
    voice_ref: Option<backend::VoiceRef>,
    /// A video's re-download reference, archived with the message
    /// so its bytes stay fetchable in later sessions. Dropped for
    /// a view-once video once its single view has been spent (see
    /// [`Store::clear_video_ref`]).
    #[serde(default)]
    video_ref: Option<backend::VideoRef>,
    /// A video's embedded JPEG thumbnail (a real frame of the
    /// video), archived with the message so its bubble and
    /// viewer can present the video's own image in later
    /// sessions without fetching its bytes. Dropped together
    /// with the reference for a view-once video once its view
    /// has been spent.
    #[serde(default)]
    video_thumbnail: Option<Vec<u8>>,
    /// A video's hosted poster reference, archived with the
    /// message so the poster stays fetchable in later sessions
    /// for videos whose payload carried no embedded thumbnail.
    /// Dropped together with the video reference for a view-once
    /// video once its single view has been spent (see
    /// [`Store::clear_video_ref`]): the poster is a real frame
    /// of the sealed content.
    #[serde(default)]
    poster_ref: Option<backend::PosterRef>,
    /// A video's wire-stated frame width, archived with the
    /// message so its tile can take the video's own proportions
    /// in later sessions before any bytes are fetched.
    #[serde(default)]
    video_width: Option<u32>,
    /// A video's wire-stated frame height. See
    /// [`MessageData::video_width`].
    #[serde(default)]
    video_height: Option<u32>,
    /// A voice note's wire waveform bytes, archived with the
    /// message so its bubble renders the sender's own waveform
    /// in later sessions, never a locally invented shape.
    #[serde(default)]
    voice_waveform: Option<Vec<u8>>,
    /// The JIDs the message mentions, archived so the app can
    /// keep resolving them to display names after a restart.
    #[serde(default)]
    mentions: Vec<String>,
    /// A photo's embedded JPEG thumbnail (the photo itself at
    /// preview fidelity), archived like the video thumbnail so
    /// the tile can paint the message's own image in later
    /// sessions without fetching its bytes.
    #[serde(default)]
    photo_thumbnail: Option<Vec<u8>>,
    /// A sticker's re-download reference, archived with the
    /// message so its bytes stay fetchable in later sessions.
    #[serde(default)]
    sticker_ref: Option<backend::StickerRef>,
    /// A video's GIF flag, archived with the message so the
    /// clip still loops in later sessions.
    #[serde(default)]
    gif_playback: bool,
    /// A sticker's animation flags, archived with the message
    /// so its badge stays honest in later sessions.
    #[serde(default)]
    sticker_animated: bool,
    /// A sticker's Lottie flag. See [`MessageData::sticker_animated`].
    #[serde(default)]
    sticker_lottie: bool,
    /// A sticker's embedded static frame, archived with the
    /// message (a Lottie sticker's only picture).
    #[serde(default)]
    sticker_thumbnail: Option<Vec<u8>>,
    /// A document's re-download reference, archived with the
    /// message so a document received in an earlier session
    /// stays downloadable after a restart. Dropped for a
    /// view-once document once its single view has been spent
    /// (see [`Store::clear_document_ref`]).
    #[serde(default)]
    document_ref: Option<backend::DocumentRef>,
    /// A document's own file name, archived with the message
    /// in its own field (the kind's caption slot holds only
    /// the sender's caption).
    #[serde(default)]
    document_name: Option<String>,
    /// A document's stated page count.
    #[serde(default)]
    document_page_count: Option<u32>,
    /// A document's embedded JPEG thumbnail, archived with
    /// the message so its card can present the document's own
    /// preview image in later sessions.
    #[serde(default)]
    document_thumbnail: Option<Vec<u8>>,
    /// A location's embedded map snapshot, archived with the
    /// message so its card can present the place's own picture
    /// in later sessions.
    #[serde(default)]
    location_thumbnail: Option<Vec<u8>>,
    /// A poll's creation secret, archived with the poll so the
    /// app can hand it back to the backend in later sessions
    /// (votes are encrypted under it; without it the poll could
    /// neither be voted on nor tallied after a restart). It sits
    /// in this encrypted archive, never in plaintext settings.
    #[serde(default)]
    poll_secret: Option<Vec<u8>>,
    /// A poll's creator JID, archived alongside the secret (the
    /// vote key derivation needs both).
    #[serde(default)]
    poll_creator: Option<String>,
    /// A poll's latest ballot per voter, `(voter JID, selected
    /// option indices)`, as aggregated by the app. Kept so a
    /// voter's changed vote after a restart replaces their old
    /// ballot instead of counting twice.
    #[serde(default)]
    poll_ballots: Vec<(String, Vec<usize>)>,
    /// An event's creation secret, archived with the event so
    /// the app can hand it back to the backend in later
    /// sessions (responses are encrypted under it; without it
    /// the event could neither be answered nor tallied after a
    /// restart). It sits in this encrypted archive, never in
    /// plaintext settings.
    #[serde(default)]
    event_secret: Option<Vec<u8>>,
    /// An event's creator JID, archived alongside the secret
    /// (the response key derivation needs both).
    #[serde(default)]
    event_creator: Option<String>,
    /// An event's latest response per responder, `(responder
    /// JID, response)`, as aggregated by the app. Kept so a
    /// responder's changed answer after a restart replaces
    /// their old one instead of counting twice.
    #[serde(default)]
    event_ballots: Vec<(String, backend::EventRsvp)>,
    /// The sender's verified business name, when the payload
    /// stated one; the app badges the chat from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verified_name: Option<String>,
    /// Synced delivery state of an outgoing message,
    /// archived so read ticks survive a restart. Merges are
    /// monotonic: a rewrite never lowers it.
    #[serde(default)]
    delivery: Option<backend::DeliveryState>,
}

/// One stored profile picture row: (picture_id, bytes,
/// fetched_at). Bytes are `None` on a negative row (the identity
/// has no picture, or privacy refused it).
pub type AvatarRow = (Option<String>, Option<Vec<u8>>, i64);

/// [`Store::messages_reporting`] result: the decoded rows plus
/// the IDs of rows skipped because their payload was corrupt.
pub type MessagesReport = (Vec<(LiveMessage, i64)>, Vec<String>);

/// One row of the full avatar listing: the identity's JID plus
/// its [`AvatarRow`].
pub type AvatarListing = (String, Option<String>, Option<Vec<u8>>, i64);

/// One hit of the global message search (design standard
/// section 25): an archived message whose visible text matched
/// the query, with the context the result list renders. The
/// body is the message's own text or its media caption, never
/// a kind label.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// JID of the chat the message belongs to.
    pub chat_jid: String,
    /// The message's id inside its chat.
    pub message_id: String,
    /// The sender's display name as archived.
    pub sender: String,
    /// The matched text (message body or media caption).
    pub body: String,
    /// Unix seconds.
    pub timestamp: i64,
    /// Whether the account owner sent the message.
    pub from_me: bool,
}

/// The archive store: one SQLite file, versioned schema.
pub struct Store {
    conn: Connection,
}

/// Runs one `ALTER TABLE ... ADD COLUMN` unless the table
/// already has the column.
///
/// Migration steps must converge from states the ladder itself
/// did not produce: a file whose `user_version` disagrees with
/// its actual tables (a pre-versioning archive, a downgraded or
/// hand-repaired file) would otherwise fail every launch with
/// "duplicate column name" and never open again. The version
/// bookkeeping stays honest: the step still records its version
/// bump in the same transaction, it just tolerates its own
/// effects already being present.
fn add_column_if_missing(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
    ddl: &str,
) -> Result<(), StoreError> {
    let exists = {
        let mut stmt = tx.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
        names.collect::<Result<Vec<String>, _>>()?
    };
    if !exists.iter().any(|name| name == column) {
        tx.execute_batch(ddl)?;
    }
    Ok(())
}

impl Store {
    /// Opens (or creates) the encrypted archive at `path` and runs
    /// migrations. A plaintext archive left by an older build is
    /// converted in place first (see [`encrypt_file_in_place`]).
    /// The first open under a build with physical deletion also
    /// runs the one-time freelist purge described in the crate
    /// docs; later opens skip it.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures (including a wrong `key`,
    /// which SQLCipher reports as an unreadable database) or a
    /// too-new schema.
    pub fn open(path: &Path, key: &[u8; 32]) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            // A directory failure is an I/O failure, not a SQL
            // conversion error (P2-42): the old mapping made the
            // user-facing string blame the database format for
            // what was a filesystem problem.
            std::fs::create_dir_all(parent).map_err(StoreError::Io)?;
        }
        encrypt_file_in_place(path, key)?;
        let conn = Connection::open(path)?;
        apply_key(&conn, key)?;
        Self::init(conn)
    }

    /// Opens a throwaway in-memory archive (tests, previews).
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        // secure_delete is per-connection, and this connection
        // is the only one the store ever uses, so setting it
        // here covers every delete the archive performs: freed
        // pages are zeroed as they are freed, instead of keeping
        // the deleted rows' bytes until the page happens to be
        // reused (privacy audit A2).
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA secure_delete = ON;",
        )?;
        let store = Self { conn };
        store.migrate()?;
        store.purge_freelist_once()?;
        store.scrub_view_once_captions_once()?;
        store.prune_unbounded_tables()?;
        Ok(store)
    }

    /// Bounds the two tables that otherwise grow forever
    /// (P2-39): avatar rows are one full picture per identity,
    /// and the calls table accretes one row per call. Aged
    /// NEGATIVE avatar rows (no bytes: a "no picture" answer)
    /// older than 30 days are dropped; the app re-asks on its
    /// own 24-hour negative rule anyway, so nothing current is
    /// lost. Positive rows stay: they are the offline picture
    /// cache. Call history is capped at the newest 1,000 rows,
    /// far beyond what the Calls panel can show.
    fn prune_unbounded_tables(&self) -> Result<(), StoreError> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
            - 30 * 86_400;
        self.conn.execute(
            "DELETE FROM avatars WHERE bytes IS NULL AND fetched_at > 0 AND fetched_at < ?1",
            params![cutoff],
        )?;
        self.conn.execute(
            "DELETE FROM calls WHERE id NOT IN (SELECT id FROM calls ORDER BY started_at DESC, id DESC LIMIT 1000)",
            [],
        )?;
        Ok(())
    }

    /// The one-time purge of free pages accumulated before
    /// `secure_delete` was enabled: archives written by older
    /// builds deleted rows logically only, so their bodies can
    /// still sit in the file's free pages (ciphertext, but
    /// recoverable by anyone holding file and key together).
    /// VACUUM drops every free page, so one pass clears them
    /// all. Gated on the [`FREELIST_PURGED_KEY`] flag in
    /// `store_meta`: the flag is written after the purge, so
    /// the purge runs exactly once per archive and a crash
    /// mid-purge retries on the next open instead of marking
    /// work done that never happened.
    fn purge_freelist_once(&self) -> Result<(), StoreError> {
        let done: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM store_meta WHERE key = ?1)",
            params![FREELIST_PURGED_KEY],
            |row| row.get(0),
        )?;
        if done {
            return Ok(());
        }
        self.reclaim_deleted()?;
        self.conn.execute(
            "INSERT INTO store_meta (key, value) VALUES (?1, '1')",
            params![FREELIST_PURGED_KEY],
        )?;
        Ok(())
    }

    /// The one-time scrub of view-once captions archived by
    /// builds that still stored them: those builds wrote the
    /// caption into the message row's body and kind payload, so
    /// it survived restarts indefinitely, defeating the
    /// sender's view-once intent. The rows are identifiable
    /// (the view-once flag is part of the stored data), so the
    /// scrub empties body and kind caption on exactly those
    /// rows, and recomputes the preview of every chat whose
    /// newest message is view-once: under the old builds that
    /// preview was the caption itself. `secure_delete` zeroes
    /// the freed pages as the rows are rewritten, and the WAL
    /// is truncated afterwards. Gated on the
    /// [`VIEW_ONCE_CAPTIONS_SCRUBBED_KEY`] flag, written after
    /// the scrub completes; the scrub is idempotent, so a crash
    /// mid-scrub simply retries on the next open.
    fn scrub_view_once_captions_once(&self) -> Result<(), StoreError> {
        let done: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM store_meta WHERE key = ?1)",
            params![VIEW_ONCE_CAPTIONS_SCRUBBED_KEY],
            |row| row.get(0),
        )?;
        if done {
            return Ok(());
        }
        // Archives converge from states the ladder itself did
        // not produce (see the migration steps): a hand-built or
        // partial file can lack the messages table entirely, in
        // which case there is nothing to scrub.
        let has_messages: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'messages')",
            [],
            |row| row.get(0),
        )?;
        if !has_messages {
            self.conn.execute(
                "INSERT INTO store_meta (key, value) VALUES (?1, '1')",
                params![VIEW_ONCE_CAPTIONS_SCRUBBED_KEY],
            )?;
            return Ok(());
        }
        let rows: Vec<(String, String, i64, String, Option<String>)> = {
            let mut stmt =
                self.conn.prepare("SELECT chat_jid, id, timestamp, body, data FROM messages")?;
            let mapped = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?;
            mapped.collect::<Result<_, _>>()?
        };
        // The newest message of each chat, in the same order
        // `messages` reads them (timestamp, then id): the chat
        // preview derives from it, so it decides whether the
        // stored preview can be a leaked caption.
        let mut newest: std::collections::HashMap<&str, (i64, &str, bool)> =
            std::collections::HashMap::new();
        let mut scrubbed_any = false;
        let tx = self.conn.unchecked_transaction()?;
        for (chat_jid, id, timestamp, body, data_text) in &rows {
            let data: MessageData = match data_text.as_deref() {
                Some(text) => Self::parse_message_data(text, chat_jid, id)?,
                None => MessageData::default(),
            };
            let entry = newest.entry(chat_jid.as_str()).or_insert((*timestamp, id.as_str(), false));
            if (*timestamp, id.as_str()) >= (entry.0, entry.1) {
                *entry = (*timestamp, id.as_str(), data.view_once);
            }
            if !data.view_once {
                continue;
            }
            let mut data = data;
            let mut changed = !body.is_empty();
            if let Some(backend::LiveMessageKind::Media { caption, .. }) = &mut data.kind {
                if !caption.is_empty() {
                    caption.clear();
                    changed = true;
                }
            }
            if changed {
                let data_json = serde_json::to_string(&data).map_err(|e| {
                    StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
                })?;
                tx.execute(
                    "UPDATE messages SET body = '', data = ?3 WHERE chat_jid = ?1 AND id = ?2",
                    params![chat_jid, id, data_json],
                )?;
                scrubbed_any = true;
            }
        }
        for (chat_jid, (_, _, view_once)) in &newest {
            if *view_once {
                tx.execute(
                    "UPDATE chats SET preview = ?2 WHERE jid = ?1",
                    params![chat_jid, core::model::VIEW_ONCE_PREVIEW],
                )?;
            }
        }
        tx.commit()?;
        if scrubbed_any {
            self.checkpoint_truncate()?;
        }
        self.conn.execute(
            "INSERT INTO store_meta (key, value) VALUES (?1, '1')",
            params![VIEW_ONCE_CAPTIONS_SCRUBBED_KEY],
        )?;
        Ok(())
    }

    /// Physically removes what deletions freed: truncates the
    /// WAL (its frames still carry pre-delete page images) and
    /// VACUUMs, which rebuilds the database file from live
    /// pages only and drops every free page. VACUUM rewrites
    /// the whole encrypted file, so it is tied to mass
    /// deletions (a chat deleted or cleared, an expiry batch)
    /// and to the one-time purge, never to single-row deletes;
    /// those rely on `secure_delete` zeroing plus the WAL
    /// truncation in [`Store::delete_message`].
    fn reclaim_deleted(&self) -> Result<(), StoreError> {
        self.checkpoint_truncate()?;
        self.conn.execute_batch("VACUUM;")?;
        self.checkpoint_truncate()?;
        Ok(())
    }

    /// Flushes the WAL into the database file and truncates
    /// the WAL to zero length, so frames written before a
    /// deletion do not linger for the rest of the session.
    fn checkpoint_truncate(&self) -> Result<(), StoreError> {
        self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// The schema version stored in the database file.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Applies migrations until the schema reaches
    /// [`SCHEMA_VERSION`]. Idempotent, and tolerant of a file
    /// whose recorded version disagrees with its actual tables:
    /// each step converges from any partial state instead of
    /// failing on its own already-applied effects.
    ///
    /// # Errors
    /// [`StoreError::SchemaTooNew`] if the file is ahead of this
    /// build; [`StoreError::Sqlite`] on migration failures.
    pub fn migrate(&self) -> Result<(), StoreError> {
        let mut version = self.schema_version()?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew { found: version, supported: SCHEMA_VERSION });
        }
        while version < SCHEMA_VERSION {
            // Each step's DDL and its version bump commit together:
            // a crash mid-migration can no longer leave a
            // half-migrated file at the old version.
            let tx = self.conn.unchecked_transaction()?;
            match version {
                0 => {
                    // IF NOT EXISTS (and the per-column checks in
                    // the ALTER steps below): a file whose
                    // user_version disagrees with its tables, like
                    // a pre-versioning archive that already has
                    // them, must converge instead of failing every
                    // launch with "table already exists".
                    tx.execute_batch(
                        "CREATE TABLE IF NOT EXISTS chats (
                            jid TEXT PRIMARY KEY,
                            title TEXT NOT NULL,
                            kind TEXT NOT NULL,
                            preview TEXT NOT NULL DEFAULT '',
                            preview_sender TEXT,
                            timestamp INTEGER NOT NULL DEFAULT 0,
                            unread INTEGER NOT NULL DEFAULT 0
                        );
                        CREATE TABLE IF NOT EXISTS messages (
                            chat_jid TEXT NOT NULL,
                            id TEXT NOT NULL,
                            sender TEXT,
                            body TEXT NOT NULL,
                            timestamp INTEGER NOT NULL,
                            from_me INTEGER NOT NULL DEFAULT 0,
                            PRIMARY KEY (chat_jid, id)
                        );
                        CREATE INDEX IF NOT EXISTS idx_messages_chat_time
                            ON messages (chat_jid, timestamp);",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 1;")?;
                }
                1 => {
                    // v2: rich payloads (kind/reactions/quote/preview
                    // as JSON) + a flags bitfield (AI-generated,
                    // deleted-by-sender, deleted).
                    add_column_if_missing(
                        &tx,
                        "messages",
                        "data",
                        "ALTER TABLE messages ADD COLUMN data TEXT",
                    )?;
                    add_column_if_missing(
                        &tx,
                        "messages",
                        "flags",
                        "ALTER TABLE messages ADD COLUMN flags INTEGER NOT NULL DEFAULT 0",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 2;")?;
                }
                2 => {
                    // v3 originally created the agent platform
                    // tables (memory_chunks, reminders). Those
                    // features were retired and v7 drops the
                    // tables, so this step no longer creates them;
                    // archives that already carry them lose them
                    // at the v7 step.
                    tx.execute_batch("PRAGMA user_version = 3;")?;
                }
                3 => {
                    // v4: chat list state (archived / pinned) that
                    // the chat-actions surface writes through.
                    add_column_if_missing(
                        &tx,
                        "chats",
                        "archived",
                        "ALTER TABLE chats ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
                    )?;
                    add_column_if_missing(
                        &tx,
                        "chats",
                        "pinned",
                        "ALTER TABLE chats ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 4;")?;
                }
                4 => {
                    // v5: disappearing messages. The per-chat
                    // timer lives on the chat row; each message
                    // carries the unix second at which it expires
                    // (0 = never), computed by the backend from
                    // the wire expiration.
                    add_column_if_missing(
                        &tx,
                        "chats",
                        "ephemeral_secs",
                        "ALTER TABLE chats ADD COLUMN ephemeral_secs INTEGER NOT NULL DEFAULT 0",
                    )?;
                    add_column_if_missing(
                        &tx,
                        "messages",
                        "expires_at",
                        "ALTER TABLE messages ADD COLUMN expires_at INTEGER NOT NULL DEFAULT 0",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 5;")?;
                }
                5 => {
                    // v6: chat mute joins archive and pin in the
                    // archive, so a muted chat stays muted across
                    // restarts (it previously lived only in the
                    // running app's memory).
                    add_column_if_missing(
                        &tx,
                        "chats",
                        "muted",
                        "ALTER TABLE chats ADD COLUMN muted INTEGER NOT NULL DEFAULT 0",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 6;")?;
                }
                6 => {
                    // v7: the retired agent platform tables go
                    // away. Archives written by builds that still
                    // created them (schema v3 to v6) may carry
                    // both tables; fresh archives never had them.
                    tx.execute_batch(
                        "DROP TABLE IF EXISTS memory_chunks;
                         DROP TABLE IF EXISTS reminders;",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 7;")?;
                }
                7 => {
                    // v8: the store_meta key/value table. Its
                    // first resident records that the one-time
                    // freelist purge has run (see
                    // FREELIST_PURGED_KEY); the purge itself
                    // cannot live in this ladder because VACUUM
                    // is not allowed inside a transaction, so it
                    // runs from the open path after migration.
                    tx.execute_batch(
                        "CREATE TABLE IF NOT EXISTS store_meta (
                            key TEXT PRIMARY KEY,
                            value TEXT NOT NULL
                        );",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 8;")?;
                }
                8 => {
                    // v9: the call history. One row per completed
                    // call, keyed by the protocol call id: a call
                    // reported twice (its end event and a
                    // missed-call event) is stored once. The rows
                    // are metadata only (peer, direction, outcome,
                    // duration); call media is never recorded.
                    tx.execute_batch(
                        "CREATE TABLE IF NOT EXISTS calls (
                            id INTEGER PRIMARY KEY AUTOINCREMENT,
                            call_id TEXT NOT NULL UNIQUE,
                            chat_jid TEXT NOT NULL,
                            video INTEGER NOT NULL DEFAULT 0,
                            outgoing INTEGER NOT NULL DEFAULT 0,
                            missed INTEGER NOT NULL DEFAULT 0,
                            duration_secs INTEGER NOT NULL DEFAULT 0,
                            started_at INTEGER NOT NULL DEFAULT 0
                        );
                        CREATE INDEX IF NOT EXISTS idx_calls_started
                            ON calls (started_at);",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 9;")?;
                }
                9 => {
                    // v10: per-chat favorites. The flag gets its
                    // own column with dedicated accessors
                    // (`set_chat_favorite` / `chat_favorites`)
                    // and deliberately stays out of the column
                    // lists `upsert_chat` and `chats` use: no
                    // chat-list sync write can ever carry, and
                    // therefore can never wipe, a favorite flag.
                    // (The set itself is the account's, synced
                    // from the phone's app state since v1.6.7;
                    // this column is its local persistence.)
                    add_column_if_missing(
                        &tx,
                        "chats",
                        "favorite",
                        "ALTER TABLE chats ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 10;")?;
                }
                10 => {
                    // v11: profile pictures. One row per identity
                    // (contact, group, community): the picture
                    // bytes when the identity has one, or a
                    // negative row (bytes NULL) when it has none
                    // or privacy refused, so a launch does not
                    // re-ask the server for every initial disc.
                    // fetched_at (unix seconds) ages negatives
                    // out; positives are refreshed by the wire's
                    // picture-change events, not by age.
                    tx.execute_batch(
                        "CREATE TABLE IF NOT EXISTS avatars (
                            jid TEXT PRIMARY KEY,
                            picture_id TEXT,
                            bytes BLOB,
                            fetched_at INTEGER NOT NULL DEFAULT 0
                        );",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 11;")?;
                }
                11 => {
                    // v12: the end of a timed mute (unix
                    // seconds, 0 = no timed end) joins the mute
                    // flag, so a mute with a deadline survives a
                    // restart and can lift itself when it passes.
                    add_column_if_missing(
                        &tx,
                        "chats",
                        "mute_until",
                        "ALTER TABLE chats ADD COLUMN mute_until INTEGER NOT NULL DEFAULT 0",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 12;")?;
                }
                _ => return Err(StoreError::BadSchemaVersion(version)),
            }
            tx.commit()?;
            version = self.schema_version()?;
        }
        Ok(())
    }

    /// Inserts or replaces a chat row.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    /// The preview group (preview, preview sender, timestamp,
    /// unread) is monotonic in the store itself (P2-38): an
    /// upsert older than the stored row cannot move the
    /// preview backwards or resurrect a read chat's unread
    /// count, whichever writer produced it. The title guard
    /// above is the same idea for names.
    pub fn upsert_chat(&self, chat: &LiveChat) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO chats (jid, title, kind, preview, preview_sender, timestamp, unread, archived, pinned, ephemeral_secs, muted, mute_until)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(jid) DO UPDATE SET
                title = CASE WHEN (length(excluded.title) >= 9 AND excluded.title NOT GLOB '*[^0-9]*')
                                  AND NOT (length(chats.title) >= 9 AND chats.title NOT GLOB '*[^0-9]*')
                             THEN chats.title ELSE excluded.title END,
                kind = excluded.kind,
                preview = CASE WHEN excluded.timestamp >= chats.timestamp
                               THEN excluded.preview ELSE chats.preview END,
                preview_sender = CASE WHEN excluded.timestamp >= chats.timestamp
                               THEN excluded.preview_sender ELSE chats.preview_sender END,
                timestamp = MAX(chats.timestamp, excluded.timestamp),
                unread = CASE WHEN excluded.timestamp >= chats.timestamp
                               THEN excluded.unread ELSE chats.unread END,
                archived = excluded.archived,
                pinned = excluded.pinned,
                ephemeral_secs = excluded.ephemeral_secs,
                muted = excluded.muted,
                mute_until = excluded.mute_until",
            params![
                chat.jid,
                chat.title,
                kind_str(chat.kind),
                chat.preview,
                chat.preview_sender,
                chat.timestamp,
                i64::from(chat.unread),
                i64::from(chat.archived),
                i64::from(chat.pinned),
                i64::from(chat.ephemeral_secs),
                i64::from(chat.muted),
                chat.mute_until,
            ],
        )?;
        Ok(())
    }

    /// Applies a resolved display name to a chat whose stored
    /// title is still a raw identifier (or empty, or the chat's
    /// own formatted phone number from the LID-to-phone heal).
    /// Returns whether a row changed. A resolved title is never
    /// overwritten, and a raw identifier is never accepted as the
    /// new title: renames only ever move a title upward from an
    /// identifier or a bare number to a name. The formatted-phone
    /// state counts as healable, not resolved: a chat healed to
    /// "+phone" must still be able to upgrade when the contact's
    /// name arrives later.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn rename_chat(&self, chat_jid: &str, title: &str) -> Result<bool, StoreError> {
        if title.trim().is_empty() || is_raw_identifier(title) {
            return Ok(false);
        }
        let changed = self.conn.execute(
            "UPDATE chats SET title = ?2 WHERE jid = ?1
             AND (title = ''
                  OR (length(title) >= 9 AND title NOT GLOB '*[^0-9]*')
                  OR (length(title) >= 10 AND title LIKE '+%'
                      AND substr(title, 2) NOT GLOB '*[^0-9]*'))",
            params![chat_jid, title],
        )?;
        Ok(changed > 0)
    }

    /// Applies an authoritative rename: the server has stated
    /// the chat's current name (a saved contact renamed on a
    /// device, a group subject change), so the stored title is
    /// replaced outright, resolved titles included. Without this
    /// path the heal guard above would freeze the first name a
    /// chat ever resolved, and real renames would never reach
    /// the archive. An empty or raw-identifier title is still
    /// refused: no authoritative source states those.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn rename_chat_authoritative(
        &self,
        chat_jid: &str,
        title: &str,
    ) -> Result<bool, StoreError> {
        if title.trim().is_empty() || is_raw_identifier(title) {
            return Ok(false);
        }
        let changed = self
            .conn
            .execute("UPDATE chats SET title = ?2 WHERE jid = ?1", params![chat_jid, title])?;
        Ok(changed > 0)
    }

    /// Applies a resolved display name to every stored message
    /// whose sender label is still that sender's raw identifier,
    /// across all chats, and to chat previews labelled with the
    /// same raw identifier. Returns how many message rows changed.
    /// Resolved labels are never overwritten, and a raw identifier
    /// is never accepted as the new name.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn rename_sender(&self, sender_jid: &str, name: &str) -> Result<usize, StoreError> {
        if name.trim().is_empty() || is_raw_identifier(name) {
            return Ok(0);
        }
        let changed = self.conn.execute(
            "UPDATE messages SET sender = ?2
             WHERE json_extract(data, '$.sender_jid') = ?1
             AND sender IS NOT NULL
             AND length(sender) >= 9 AND sender NOT GLOB '*[^0-9]*'",
            params![sender_jid, name],
        )?;
        let raw = sender_jid.split('@').next().unwrap_or(sender_jid);
        self.conn.execute(
            "UPDATE chats SET preview_sender = ?2 WHERE preview_sender = ?1",
            params![raw, name],
        )?;
        Ok(changed)
    }

    /// Inserts or replaces a message row. `flags` is a bitfield of
    /// the `FLAG_*` constants (deleted, starred, pinned states).
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn upsert_message(&self, msg: &LiveMessage, flags: i64) -> Result<(), StoreError> {
        Self::write_message(&self.conn, msg, flags)
    }

    /// Inserts or replaces many message rows in a single
    /// transaction. A history-sync burst can carry thousands of
    /// upserts; paying one commit per row made the burst the
    /// slowest part of linking. Either every row lands or none
    /// does, matching the all-or-nothing shape callers expect
    /// from a batch.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn upsert_messages(&self, msgs: &[(LiveMessage, i64)]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        for (msg, flags) in msgs {
            Self::write_message(&tx, msg, *flags)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Parses one message row's rich payload. A payload that
    /// does not decode is a loud failure, never a silent
    /// default: treating it as empty would erase the row's
    /// references and thumbnails on the next rewrite.
    fn parse_message_data(text: &str, chat_jid: &str, id: &str) -> Result<MessageData, StoreError> {
        serde_json::from_str(text).map_err(|_| StoreError::CorruptData {
            chat_jid: chat_jid.to_owned(),
            id: id.to_owned(),
        })
    }

    /// Reads and parses one message row's rich payload, or
    /// `None` when the row does not exist or carries none.
    fn read_message_data(
        conn: &Connection,
        chat_jid: &str,
        id: &str,
    ) -> Result<Option<MessageData>, StoreError> {
        use rusqlite::OptionalExtension;
        let data_text: Option<String> = conn
            .query_row(
                "SELECT data FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        match data_text {
            Some(text) => Ok(Some(Self::parse_message_data(&text, chat_jid, id)?)),
            None => Ok(None),
        }
    }

    /// The single-row write behind [`Store::upsert_message`] and
    /// [`Store::upsert_messages`], against any connection handle
    /// (the store's own, or a transaction borrowing it).
    fn write_message(conn: &Connection, msg: &LiveMessage, flags: i64) -> Result<(), StoreError> {
        // A view-once message's caption is part of its sealed
        // payload: the live mirror keeps it (the viewer shows it
        // with the media itself), but the archive must not. A
        // view-once row is stored with an empty body and an
        // empty kind caption; the kind and the view-once flag
        // survive, so after a restart the message still renders
        // as the view-once placeholder, with no caption left to
        // leak. Rows written before this rule are scrubbed by
        // the one-time pass in `init` (they are identifiable:
        // the flag is part of the stored data).
        let stored_kind = match &msg.kind {
            backend::LiveMessageKind::Text => None,
            backend::LiveMessageKind::Media { media, seconds, size_bytes, .. } if msg.view_once => {
                Some(backend::LiveMessageKind::Media {
                    media: *media,
                    caption: String::new(),
                    seconds: *seconds,
                    size_bytes: *size_bytes,
                })
            }
            other => Some(other.clone()),
        };
        let stored_body = if msg.view_once { "" } else { msg.body.as_str() };
        // A poll's tallies, my votes and per-voter ballots are
        // aggregated state the app builds from vote events; a
        // re-delivered creation payload always carries zero
        // counts and no ballots, so letting it overwrite the row
        // would erase every vote the poll has collected. When the
        // stored row already holds this poll, its tally state and
        // ballots survive the rewrite (the creation's own fields
        // still refresh), and a secret or creator the rewrite
        // does not carry falls back to the stored one.
        let mut stored_kind = stored_kind;
        // A fetched poster lands in the archive through
        // set_video_thumbnail, not through the message mirror
        // (whose thumbnail field stays empty for a video whose
        // payload carried none). A later rewrite of the row (a
        // reaction, an edit) must not erase the poster the fetch
        // archived: when the incoming message carries no
        // thumbnail, the stored one survives.
        let preserved_thumbnail = if msg.video_thumbnail.is_none()
            && matches!(
                msg.kind,
                backend::LiveMessageKind::Media { media: backend::MediaKind::Video, .. }
            ) {
            Self::read_message_data(conn, &msg.chat_jid, &msg.id)?
                .and_then(|data| data.video_thumbnail)
        } else {
            None
        };
        // Delivery is synced state: a rewrite carrying a lesser
        // (or no) state must never lower what the archive knows.
        let stored_delivery = Self::read_message_data(conn, &msg.chat_jid, &msg.id)
            .ok()
            .flatten()
            .and_then(|data| data.delivery);
        let delivery = backend::DeliveryState::max_opt(msg.delivery, stored_delivery);
        let mut poll_ballots: Vec<(String, Vec<usize>)> = msg.poll_ballots.clone();
        let existing_poll = if matches!(stored_kind, Some(backend::LiveMessageKind::Poll { .. })) {
            Self::read_message_data(conn, &msg.chat_jid, &msg.id)?
                .filter(|data| matches!(data.kind, Some(backend::LiveMessageKind::Poll { .. })))
        } else {
            None
        };
        if let Some(existing) = &existing_poll {
            if let (
                Some(backend::LiveMessageKind::Poll {
                    options: old_options,
                    my_votes: old_mine,
                    ..
                }),
                Some(backend::LiveMessageKind::Poll { options, my_votes, .. }),
            ) = (&existing.kind, &mut stored_kind)
            {
                if options.len() == old_options.len() {
                    *options = old_options.clone();
                }
                *my_votes = old_mine.clone();
            }
            poll_ballots = existing.poll_ballots.clone();
        }
        // Events merge like polls: a rewrite of the creation
        // (a history re-sync, an echo) carries no responses, so
        // the archived tallies, my answer and the ballots are
        // the state of record and survive it.
        let mut event_ballots: Vec<(String, backend::EventRsvp)> = msg.event_ballots.clone();
        let existing_event = if matches!(stored_kind, Some(backend::LiveMessageKind::Event { .. }))
        {
            Self::read_message_data(conn, &msg.chat_jid, &msg.id)?
                .filter(|data| matches!(data.kind, Some(backend::LiveMessageKind::Event { .. })))
        } else {
            None
        };
        if let Some(existing) = &existing_event {
            if let (
                Some(backend::LiveMessageKind::Event {
                    going: old_going,
                    maybe: old_maybe,
                    not_going: old_not_going,
                    my_rsvp: old_mine,
                    ..
                }),
                Some(backend::LiveMessageKind::Event { going, maybe, not_going, my_rsvp, .. }),
            ) = (&existing.kind, &mut stored_kind)
            {
                *going = *old_going;
                *maybe = *old_maybe;
                *not_going = *old_not_going;
                *my_rsvp = *old_mine;
            }
            event_ballots = existing.event_ballots.clone();
        }
        let data = MessageData {
            kind: stored_kind,
            sender_jid: msg.sender_jid.clone(),
            reactions: msg.reactions.clone(),
            my_reaction: msg.my_reaction.clone(),
            quoted_sender: msg.quoted_sender.clone(),
            quoted_body: msg.quoted_body.clone(),
            link_preview: msg.link_preview.clone(),
            edited: msg.edited,
            forwarded: msg.forwarded,
            view_once: msg.view_once,
            photo_ref: msg.photo_ref.clone(),
            voice_ref: msg.voice_ref.clone(),
            video_ref: msg.video_ref.clone(),
            video_thumbnail: msg.video_thumbnail.clone().or(preserved_thumbnail),
            poster_ref: msg.poster_ref.clone(),
            video_width: msg.video_width,
            video_height: msg.video_height,
            voice_waveform: msg.voice_waveform.clone(),
            mentions: msg.mentions.clone(),
            photo_thumbnail: msg.photo_thumbnail.clone(),
            sticker_ref: msg.sticker_ref.clone(),
            gif_playback: msg.gif_playback,
            sticker_animated: msg.sticker_animated,
            sticker_lottie: msg.sticker_lottie,
            sticker_thumbnail: msg.sticker_thumbnail.clone(),
            document_ref: msg.document_ref.clone(),
            document_name: msg.document_name.clone(),
            document_page_count: msg.document_page_count,
            document_thumbnail: msg.document_thumbnail.clone(),
            location_thumbnail: msg.location_thumbnail.clone(),
            verified_name: msg.sender_verified_name.clone(),
            delivery,
            poll_secret: msg
                .poll_secret
                .clone()
                .or_else(|| existing_poll.as_ref().and_then(|data| data.poll_secret.clone())),
            poll_creator: msg
                .poll_creator
                .clone()
                .or_else(|| existing_poll.as_ref().and_then(|data| data.poll_creator.clone())),
            poll_ballots,
            event_secret: msg
                .event_secret
                .clone()
                .or_else(|| existing_event.as_ref().and_then(|data| data.event_secret.clone())),
            event_creator: msg
                .event_creator
                .clone()
                .or_else(|| existing_event.as_ref().and_then(|data| data.event_creator.clone())),
            event_ballots,
        };
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        conn.execute(
            "INSERT INTO messages (chat_jid, id, sender, body, timestamp, from_me, data, flags, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(chat_jid, id) DO UPDATE SET
                sender = CASE WHEN excluded.sender IS NOT NULL
                                   AND (length(excluded.sender) >= 9 AND excluded.sender NOT GLOB '*[^0-9]*')
                                   AND messages.sender IS NOT NULL
                                   AND NOT (length(messages.sender) >= 9 AND messages.sender NOT GLOB '*[^0-9]*')
                              THEN messages.sender ELSE excluded.sender END,
                body = excluded.body,
                timestamp = excluded.timestamp,
                from_me = excluded.from_me,
                data = excluded.data,
                flags = excluded.flags,
                expires_at = excluded.expires_at",
            params![
                msg.chat_jid,
                msg.id,
                msg.sender,
                stored_body,
                msg.timestamp,
                i64::from(msg.from_me),
                data_json,
                flags,
                msg.expires_at,
            ],
        )?;
        Ok(())
    }

    /// Deletes one stored message. Returns whether a row
    /// existed. The deletion is physical, not just logical:
    /// `secure_delete` zeroes the freed page in the database
    /// file, and the WAL is truncated so the frames written
    /// before the deletion do not keep the body for the rest
    /// of the session. (The freed page itself is dropped by
    /// the next mass deletion's VACUUM.) Single deletions like
    /// a delete-for-me landing from another device take this
    /// path; the disappearing-messages expiry pass removes its
    /// whole catch through [`Store::delete_messages`] instead,
    /// so a pass pays one commit and one reclaim, not one per
    /// message.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn delete_message(&self, chat_jid: &str, id: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM messages WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id],
        )?;
        if changed > 0 {
            self.checkpoint_truncate()?;
        }
        Ok(changed > 0)
    }

    /// Deletes many stored messages, given as (chat JID,
    /// message id) pairs, in a single transaction, and
    /// reclaims what they freed (WAL truncation plus VACUUM;
    /// see [`Store::delete_chat`] for why mass deletions pay
    /// for the rewrite). Returns how many rows existed. An
    /// empty batch is a no-op, matching
    /// [`Store::upsert_messages`].
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn delete_messages(&self, keys: &[(String, String)]) -> Result<usize, StoreError> {
        if keys.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut deleted = 0;
        for (chat_jid, id) in keys {
            deleted += tx.execute(
                "DELETE FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
            )?;
        }
        tx.commit()?;
        if deleted > 0 {
            self.reclaim_deleted()?;
        }
        Ok(deleted)
    }

    /// Deletes one stored chat and all its messages. Returns
    /// whether a chat row existed. A chat deleted on another
    /// device leaves the archive whole: the chat row and its
    /// message rows go in one transaction, so the archive never
    /// holds a chat without its messages or messages without
    /// their chat. When anything was removed, the freed space
    /// is reclaimed (WAL truncation plus VACUUM): a whole chat
    /// is exactly the deletion whose bodies must not linger
    /// in free pages, and it is rare and user-initiated, so
    /// the file rewrite is worth its cost here and only here.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn delete_chat(&self, chat_jid: &str) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let messages = tx.execute("DELETE FROM messages WHERE chat_jid = ?1", params![chat_jid])?;
        let changed = tx.execute("DELETE FROM chats WHERE jid = ?1", params![chat_jid])?;
        tx.commit()?;
        if messages + changed > 0 {
            self.reclaim_deleted()?;
        }
        Ok(changed > 0)
    }

    /// Deletes a chat's stored messages while keeping the chat
    /// row itself. A chat cleared on another device empties
    /// here the way it empties there: `up_to` bounds the clear
    /// to messages stamped at or before it (Unix seconds;
    /// `None` clears everything), and `keep_starred` spares
    /// rows whose flags carry [`FLAG_STARRED`], the clearing
    /// device's keep-starred choice. Returns how many message
    /// rows were deleted. A clear that removed anything
    /// reclaims the freed space like [`Store::delete_chat`]
    /// does.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn delete_chat_messages(
        &self,
        chat_jid: &str,
        keep_starred: bool,
        up_to: Option<i64>,
    ) -> Result<usize, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM messages
             WHERE chat_jid = ?1
               AND (?2 IS NULL OR timestamp <= ?2)
               AND (?3 = 0 OR (flags & ?4) = 0)",
            params![chat_jid, up_to, i64::from(keep_starred), FLAG_STARRED],
        )?;
        if changed > 0 {
            self.reclaim_deleted()?;
        }
        Ok(changed)
    }

    /// Updates ONLY the flags bitfield of one stored message.
    /// Returns whether a row existed. This is the only correct way
    /// to star/pin/flag a message after the fact: a full re-upsert
    /// would have to rebuild the row from the UI model, which does
    /// not carry every stored field (v0.25 logic review F1: the old
    /// re-upsert path zeroed timestamps and erased quote, preview
    /// and view-once data).
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn set_message_flags(
        &self,
        chat_jid: &str,
        id: &str,
        flags: i64,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE messages SET flags = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, flags],
        )?;
        Ok(changed > 0)
    }

    /// Drops the stored photo reference of one message, leaving
    /// the rest of the row untouched. This is how a view-once
    /// photo's re-download capability ends: the reference is what
    /// makes the bytes fetchable, so once the single view has
    /// been spent the archive must not keep a way back to them.
    /// A message without a reference (or no such message) is a
    /// quiet no-op.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn clear_photo_ref(&self, chat_jid: &str, id: &str) -> Result<(), StoreError> {
        use rusqlite::OptionalExtension;
        let data_text: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(data_text) = data_text else { return Ok(()) };
        let mut data: MessageData = Self::parse_message_data(&data_text, chat_jid, id)?;
        if data.photo_ref.is_none() && data.photo_thumbnail.is_none() {
            return Ok(());
        }
        data.photo_ref = None;
        // The thumbnail is part of the same sealed payload as
        // the reference: a spent view-once photo keeps neither.
        data.photo_thumbnail = None;
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Replaces one poll's aggregated tally state: the per-option
    /// counts and my votes inside its stored kind, plus the
    /// per-voter ballots beside it. This is how a vote the app
    /// aggregated (from a backend vote event or a local vote)
    /// becomes durable without rewriting the whole message row.
    /// A message that is not an archived poll (or does not exist)
    /// is a quiet no-op. `counts` is in option order; a count list
    /// whose length disagrees with the poll's options is refused
    /// the same quiet way, since it cannot belong to this poll.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn set_poll_tally(
        &self,
        chat_jid: &str,
        id: &str,
        counts: &[u32],
        my_votes: &[usize],
        ballots: &[(String, Vec<usize>)],
    ) -> Result<(), StoreError> {
        let Some(mut data) = Self::read_message_data(&self.conn, chat_jid, id)? else {
            return Ok(());
        };
        let Some(backend::LiveMessageKind::Poll { options, my_votes: mine, .. }) = &mut data.kind
        else {
            return Ok(());
        };
        if counts.len() != options.len() {
            return Ok(());
        }
        for (entry, count) in options.iter_mut().zip(counts) {
            entry.1 = *count;
        }
        *mine = my_votes.to_vec();
        data.poll_ballots = ballots.to_vec();
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Records an event's aggregated state in the archive: the
    /// tallies, my own response, and the per-responder ballots
    /// they were computed from. The event counterpart of
    /// [`Store::set_poll_tally`], with the same contract: a
    /// message that is not an archived event is left alone.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    #[allow(clippy::too_many_arguments)]
    pub fn set_event_tally(
        &self,
        chat_jid: &str,
        id: &str,
        going: u32,
        maybe: u32,
        not_going: u32,
        my_rsvp: Option<backend::EventRsvp>,
        ballots: &[(String, backend::EventRsvp)],
    ) -> Result<(), StoreError> {
        let Some(mut data) = Self::read_message_data(&self.conn, chat_jid, id)? else {
            return Ok(());
        };
        let Some(backend::LiveMessageKind::Event {
            going: g,
            maybe: m,
            not_going: n,
            my_rsvp: mine,
            ..
        }) = &mut data.kind
        else {
            return Ok(());
        };
        *g = going;
        *m = maybe;
        *n = not_going;
        *mine = my_rsvp;
        data.event_ballots = ballots.to_vec();
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Replaces the descriptive fields of one stored event
    /// (an edit or a cancellation landing from the wire, or my
    /// own confirmed edit), leaving its tallies, my response
    /// and its ballots untouched. A message without any (or no
    /// such message) is a quiet no-op.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    #[allow(clippy::too_many_arguments)]
    pub fn set_event_fields(
        &self,
        chat_jid: &str,
        id: &str,
        name: &str,
        description: &str,
        start_time: i64,
        end_time: i64,
        location: &str,
        join_link: &str,
        scheduled_call: bool,
        extra_guests: bool,
        canceled: bool,
    ) -> Result<(), StoreError> {
        let Some(mut data) = Self::read_message_data(&self.conn, chat_jid, id)? else {
            return Ok(());
        };
        let Some(backend::LiveMessageKind::Event {
            name: n,
            description: d,
            start_time: s,
            end_time: e,
            location: l,
            join_link: j,
            scheduled_call: sc,
            extra_guests: eg,
            canceled: c,
            ..
        }) = &mut data.kind
        else {
            return Ok(());
        };
        *n = name.to_owned();
        *d = description.to_owned();
        *s = start_time;
        *e = end_time;
        *l = location.to_owned();
        *j = join_link.to_owned();
        *sc = scheduled_call;
        *eg = extra_guests;
        *c = canceled;
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Drops the stored video reference, thumbnail and poster
    /// reference of one message, leaving the rest of the row
    /// untouched. This is how a view-once video's re-download
    /// capability ends: the reference is what makes the bytes
    /// fetchable, and the thumbnail and poster are real frames
    /// of the sealed content, so once the single view has been
    /// spent the archive must keep none of them. A message
    /// without any (or no such message) is a quiet no-op.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn clear_video_ref(&self, chat_jid: &str, id: &str) -> Result<(), StoreError> {
        use rusqlite::OptionalExtension;
        let data_text: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(data_text) = data_text else { return Ok(()) };
        let mut data: MessageData = Self::parse_message_data(&data_text, chat_jid, id)?;
        if data.video_ref.is_none() && data.video_thumbnail.is_none() && data.poster_ref.is_none() {
            return Ok(());
        }
        data.video_ref = None;
        data.video_thumbnail = None;
        data.poster_ref = None;
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Clears one view-once voice message's re-download
    /// reference (and its waveform) from the archive when its
    /// single play has been spent: the reference is what makes
    /// the bytes fetchable, and the waveform is the sealed
    /// audio's own shape, so once the play has been spent the
    /// archive must keep neither. A message without any (or no
    /// such message) is a quiet no-op.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn clear_voice_ref(&self, chat_jid: &str, id: &str) -> Result<(), StoreError> {
        use rusqlite::OptionalExtension;
        let data_text: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(data_text) = data_text else { return Ok(()) };
        let mut data: MessageData = Self::parse_message_data(&data_text, chat_jid, id)?;
        if data.voice_ref.is_none() && data.voice_waveform.is_none() {
            return Ok(());
        }
        data.voice_ref = None;
        data.voice_waveform = None;
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Clears one view-once sticker message's re-download
    /// reference from the archive when its single view has been
    /// spent. A message without one (or no such message) is a
    /// quiet no-op.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn clear_sticker_ref(&self, chat_jid: &str, id: &str) -> Result<(), StoreError> {
        use rusqlite::OptionalExtension;
        let data_text: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(data_text) = data_text else { return Ok(()) };
        let mut data: MessageData = Self::parse_message_data(&data_text, chat_jid, id)?;
        if data.sticker_ref.is_none() {
            return Ok(());
        }
        data.sticker_ref = None;
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Clears one view-once document message's re-download
    /// reference from the archive. A message without one (or no
    /// such message) is a quiet no-op.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn clear_document_ref(&self, chat_jid: &str, id: &str) -> Result<(), StoreError> {
        use rusqlite::OptionalExtension;
        let data_text: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(data_text) = data_text else { return Ok(()) };
        let mut data: MessageData = Self::parse_message_data(&data_text, chat_jid, id)?;
        if data.document_ref.is_none() {
            return Ok(());
        }
        data.document_ref = None;
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Stores fetched poster bytes as one message's video
    /// thumbnail, leaving the rest of the row untouched. The
    /// poster a video's payload did not embed is downloaded on
    /// demand; archiving it with the message means later
    /// sessions present the video's own frame without fetching
    /// again, exactly like an embedded thumbnail. A message
    /// that is not archived is a quiet no-op.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn set_video_thumbnail(
        &self,
        chat_jid: &str,
        id: &str,
        thumbnail: &[u8],
    ) -> Result<(), StoreError> {
        use rusqlite::OptionalExtension;
        let data_text: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(data_text) = data_text else { return Ok(()) };
        let mut data: MessageData = Self::parse_message_data(&data_text, chat_jid, id)?;
        if data.video_thumbnail.as_deref() == Some(thumbnail) {
            return Ok(());
        }
        data.video_thumbnail = Some(thumbnail.to_vec());
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Replaces the CDN path of a message's archived media
    /// reference (photo, voice, video, or sticker, whichever it
    /// carries) with a fresh one. The media-retry flow returns a
    /// new path when the author's device re-uploads expired
    /// media; persisting it means later sessions download
    /// directly instead of recovering again. A message without
    /// a reference, or one that is not archived, is a no-op.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn update_media_direct_path(
        &self,
        chat_jid: &str,
        id: &str,
        direct_path: &str,
    ) -> Result<(), StoreError> {
        use rusqlite::OptionalExtension;
        let data_text: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM messages WHERE chat_jid = ?1 AND id = ?2",
                params![chat_jid, id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(data_text) = data_text else { return Ok(()) };
        let mut data: MessageData = Self::parse_message_data(&data_text, chat_jid, id)?;
        let mut changed = false;
        if let Some(reference) = &mut data.photo_ref {
            reference.direct_path = direct_path.to_owned();
            changed = true;
        }
        if let Some(reference) = &mut data.voice_ref {
            reference.direct_path = direct_path.to_owned();
            changed = true;
        }
        if let Some(reference) = &mut data.video_ref {
            reference.direct_path = direct_path.to_owned();
            changed = true;
        }
        if let Some(reference) = &mut data.sticker_ref {
            reference.direct_path = direct_path.to_owned();
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id, data_json],
        )?;
        Ok(())
    }

    /// Whether a message is archived under this chat and ID.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn has_message(&self, chat_jid: &str, id: &str) -> Result<bool, StoreError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Stores one identity's profile picture row: the bytes
    /// when the identity has a picture, or `None` bytes for the
    /// negative (no picture, or privacy refused). `fetched_at`
    /// is unix seconds; the app ages negatives out and refetches
    /// them, while positives live until a picture-change event
    /// drops the row.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn put_avatar(
        &self,
        jid: &str,
        picture_id: Option<&str>,
        bytes: Option<&[u8]>,
        fetched_at: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO avatars (jid, picture_id, bytes, fetched_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(jid) DO UPDATE SET picture_id = excluded.picture_id,
                bytes = excluded.bytes, fetched_at = excluded.fetched_at",
            params![jid, picture_id, bytes, fetched_at],
        )?;
        Ok(())
    }

    /// One identity's stored profile picture row, if any:
    /// `(picture_id, bytes, fetched_at)`. Bytes are `None` on a
    /// negative row.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn avatar(&self, jid: &str) -> Result<Option<AvatarRow>, StoreError> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT picture_id, bytes, fetched_at FROM avatars WHERE jid = ?1",
                params![jid],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Every stored profile picture row, for the app's launch
    /// seeding of its avatar cache.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn avatars(&self) -> Result<Vec<AvatarListing>, StoreError> {
        let mut stmt =
            self.conn.prepare("SELECT jid, picture_id, bytes, fetched_at FROM avatars")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Drops one identity's stored profile picture (a
    /// picture-change event made it stale).
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn delete_avatar(&self, jid: &str) -> Result<(), StoreError> {
        self.conn.execute("DELETE FROM avatars WHERE jid = ?1", params![jid])?;
        Ok(())
    }

    /// All archived chats, most recent first.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn chats(&self) -> Result<Vec<LiveChat>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT jid, title, kind, preview, preview_sender, timestamp, unread, archived, pinned, ephemeral_secs, muted, mute_until
             FROM chats ORDER BY timestamp DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(LiveChat {
                jid: row.get(0)?,
                title: row.get(1)?,
                kind: kind_of(&row.get::<_, String>(2)?),
                preview: row.get(3)?,
                preview_sender: row.get(4)?,
                timestamp: row.get(5)?,
                unread: u32::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
                archived: row.get::<_, i64>(7)? != 0,
                pinned: row.get::<_, i64>(8)? != 0,
                ephemeral_secs: u32::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
                muted: row.get::<_, i64>(10)? != 0,
                mute_until: row.get(11)?,
                parent_jid: None,
                is_parent: false,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The JIDs of the chats the user marked as favorites on
    /// this device, in JID order. Favorites are a local app
    /// concept with no server counterpart; the flags live in
    /// their own column (schema v10) that the chat sync paths
    /// never read or write, so this set changes only through
    /// [`Store::set_chat_favorite`].
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn chat_favorites(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt =
            self.conn.prepare("SELECT jid FROM chats WHERE favorite != 0 ORDER BY jid")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Sets or clears one chat's favorite flag. A JID with no
    /// archived chat row is a quiet no-op: the flag belongs to
    /// the row, so a chat the archive does not hold cannot be a
    /// favorite (and deleting the chat takes its flag with it).
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn set_chat_favorite(&self, chat_jid: &str, favorite: bool) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE chats SET favorite = ?2 WHERE jid = ?1",
            params![chat_jid, i64::from(favorite)],
        )?;
        Ok(())
    }

    /// Records one completed call in the call history. The
    /// protocol call ID is unique: recording the same call twice
    /// (its end event and a missed-call event can both report it)
    /// keeps the first record and reports `Ok(None)`. On a fresh
    /// record, returns its archive row ID.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn record_call(&self, record: &backend::CallRecord) -> Result<Option<u64>, StoreError> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO calls
                (call_id, chat_jid, video, outgoing, missed, duration_secs, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.call_id,
                record.chat_jid,
                i64::from(record.video),
                i64::from(record.outgoing),
                i64::from(record.missed),
                i64::from(record.duration_secs),
                record.started_at,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let rowid = self.conn.last_insert_rowid();
        Ok(u64::try_from(rowid).ok())
    }

    /// The recorded call history, newest first (by ring/placement
    /// time; ties break toward the most recently recorded).
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn call_history(&self) -> Result<Vec<backend::CallRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, call_id, chat_jid, video, outgoing, missed, duration_secs, started_at
             FROM calls ORDER BY started_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(backend::CallRecord {
                id: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                call_id: row.get(1)?,
                chat_jid: row.get(2)?,
                video: row.get::<_, i64>(3)? != 0,
                outgoing: row.get::<_, i64>(4)? != 0,
                missed: row.get::<_, i64>(5)? != 0,
                duration_secs: u32::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
                started_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// All archived messages of one chat, oldest first, each with
    /// its flags bitfield.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn messages(&self, chat_jid: &str) -> Result<Vec<(LiveMessage, i64)>, StoreError> {
        Ok(self.messages_reporting(chat_jid)?.0)
    }

    /// [`Store::messages`] with the load report: the IDs of
    /// rows whose rich payload did not decode. Those rows are
    /// skipped (the rest of the chat loads), and the caller
    /// surfaces the loss instead of the archive silently
    /// coming back short.
    pub fn messages_reporting(&self, chat_jid: &str) -> Result<MessagesReport, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT chat_jid, id, sender, body, timestamp, from_me, data, flags, expires_at
             FROM messages WHERE chat_jid = ?1 ORDER BY timestamp ASC, id ASC",
        )?;
        let mut skipped: Vec<String> = Vec::new();
        let rows = stmt.query_map(params![chat_jid], |row| {
            let data_text: Option<String> = row.get(6)?;
            let row_id: String = row.get(1)?;
            let data: MessageData = match data_text.as_deref() {
                Some(text) => match Self::parse_message_data(text, chat_jid, &row_id) {
                    Ok(data) => data,
                    Err(_) => {
                        // Marked by a sentinel the caller
                        // filters below: the row is skipped,
                        // its ID reported.
                        MessageData {
                            sender_jid: Some("\u{1}corrupt".to_owned()),
                            ..MessageData::default()
                        }
                    }
                },
                None => MessageData::default(),
            };
            let mut msg = LiveMessage::text(
                row.get::<_, String>(1)?,
                row.get::<_, String>(0)?,
                row.get(2)?,
                row.get::<_, String>(3)?,
                row.get(4)?,
                row.get::<_, i64>(5)? != 0,
            );
            msg.kind = data.kind.unwrap_or(backend::LiveMessageKind::Text);
            msg.sender_jid = data.sender_jid;
            msg.reactions = data.reactions;
            msg.my_reaction = data.my_reaction;
            msg.quoted_sender = data.quoted_sender;
            msg.quoted_body = data.quoted_body;
            msg.link_preview = data.link_preview;
            msg.edited = data.edited;
            msg.forwarded = data.forwarded;
            msg.view_once = data.view_once;
            msg.photo_ref = data.photo_ref;
            msg.voice_ref = data.voice_ref;
            msg.video_ref = data.video_ref;
            msg.video_thumbnail = data.video_thumbnail;
            msg.poster_ref = data.poster_ref;
            msg.video_width = data.video_width;
            msg.video_height = data.video_height;
            msg.voice_waveform = data.voice_waveform;
            msg.mentions = data.mentions;
            msg.photo_thumbnail = data.photo_thumbnail;
            msg.sticker_ref = data.sticker_ref;
            msg.gif_playback = data.gif_playback;
            msg.sticker_animated = data.sticker_animated;
            msg.sticker_lottie = data.sticker_lottie;
            msg.sticker_thumbnail = data.sticker_thumbnail;
            msg.document_ref = data.document_ref;
            msg.document_name = data.document_name;
            msg.document_page_count = data.document_page_count;
            msg.document_thumbnail = data.document_thumbnail;
            msg.location_thumbnail = data.location_thumbnail;
            msg.sender_verified_name = data.verified_name;
            msg.delivery = data.delivery;
            msg.poll_secret = data.poll_secret;
            msg.poll_creator = data.poll_creator;
            msg.poll_ballots = data.poll_ballots;
            msg.event_secret = data.event_secret;
            msg.event_creator = data.event_creator;
            msg.event_ballots = data.event_ballots;
            msg.expires_at = row.get(8)?;
            let flags: i64 = row.get(7)?;
            Ok((msg, flags))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (msg, flags) = row?;
            if msg.sender_jid.as_deref() == Some("\u{1}corrupt") {
                skipped.push(msg.id.clone());
                continue;
            }
            out.push((msg, flags));
        }
        Ok((out, skipped))
    }

    /// Sets one message's archived delivery state, monotonically:
    /// a lesser state never overwrites a greater one. Used by the
    /// live receipt path, which advances ticks without a full
    /// message rewrite.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures or a corrupt payload.
    pub fn set_message_delivery(
        &self,
        chat_jid: &str,
        id: &str,
        delivery: backend::DeliveryState,
    ) -> Result<bool, StoreError> {
        let mut data = match Self::read_message_data(&self.conn, chat_jid, id)? {
            Some(data) => data,
            None => {
                // A plain-text row carries no payload yet; the
                // message exists, so start one for it. A missing
                // row is not an error, just nothing to advance.
                let exists: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE chat_jid = ?1 AND id = ?2)",
                    params![chat_jid, id],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Ok(false);
                }
                MessageData::default()
            }
        };
        if backend::DeliveryState::max_opt(data.delivery, Some(delivery)) == data.delivery {
            return Ok(false);
        }
        data.delivery = Some(delivery);
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "UPDATE messages SET data = ?1 WHERE chat_jid = ?2 AND id = ?3",
            params![data_json, chat_jid, id],
        )?;
        Ok(true)
    }

    /// Every starred message in the archive, across all chats,
    /// newest first: the Starred panel's live data source. Rows
    /// are returned with their flags so the caller can filter
    /// and render exactly as the per-chat loads do.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn starred_messages(&self) -> Result<Vec<(LiveMessage, i64)>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT chat_jid FROM messages WHERE flags & ?1 != 0 GROUP BY chat_jid")?;
        let chat_rows = stmt.query_map(params![FLAG_STARRED], |row| row.get::<_, String>(0))?;
        let mut chat_jids = Vec::new();
        for row in chat_rows {
            chat_jids.push(row?);
        }
        drop(stmt);
        let mut out = Vec::new();
        for jid in &chat_jids {
            for (msg, flags) in self.messages(jid)? {
                if flags & FLAG_STARRED != 0 {
                    out.push((msg, flags));
                }
            }
        }
        out.sort_by(|a, b| b.0.timestamp.cmp(&a.0.timestamp).then_with(|| b.0.id.cmp(&a.0.id)));
        Ok(out)
    }

    /// Global message search over the archive (design standard
    /// section 25): every message, in any chat, whose visible
    /// text contains `query` as a case-insensitive substring,
    /// newest first, at most `limit` hits.
    ///
    /// Only text a reader of the conversation can see counts:
    /// plain text messages, and media messages with a real
    /// caption (a document's file name is its caption). A
    /// captionless media row's stored body is a kind label
    /// written by the backend, not content, so it never
    /// matches; deleted rows and view-once rows (stored with
    /// an empty body by construction) never match either.
    ///
    /// The query runs as a LIKE scan with the query's LIKE
    /// metacharacters escaped, so the match is literal. The
    /// scan reads newest-first and over-reads past excluded
    /// rows up to a bounded multiple of `limit`, so a query
    /// that mostly hits excluded rows still terminates
    /// quickly; the recorded trade-off is that hits older
    /// than that bound can be missed when the newest matches
    /// are overwhelmingly excluded rows. An index-backed
    /// query can replace the scan behind this same signature
    /// if archives outgrow it.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, StoreError> {
        let needle = query.trim();
        if needle.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut pattern = String::with_capacity(needle.len() + 2);
        pattern.push('%');
        for c in needle.chars() {
            if matches!(c, '%' | '_' | '\\') {
                pattern.push('\\');
            }
            pattern.push(c);
        }
        pattern.push('%');
        let scan = i64::try_from(limit.saturating_mul(8).max(64)).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT chat_jid, id, sender, body, timestamp, from_me, flags, data
             FROM messages WHERE body LIKE ?1 ESCAPE '\\'
             ORDER BY timestamp DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, scan], |row| {
            let data_text: Option<String> = row.get(7)?;
            let sender: Option<String> = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                sender.unwrap_or_default(),
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)? != 0,
                row.get::<_, i64>(6)?,
                data_text,
            ))
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let (chat_jid, message_id, sender, body, timestamp, from_me, flags, data_text) = row?;
            if flags & FLAG_DELETED != 0 {
                continue;
            }
            let data: MessageData =
                data_text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            if data.view_once {
                continue;
            }
            // A message matches on the text its reader sees:
            // a plain text body, or a media caption. Any other
            // kind's stored body is a label or a summary, not
            // searchable content.
            let searchable = match &data.kind {
                None => true,
                Some(backend::LiveMessageKind::Media { caption, .. }) => !caption.is_empty(),
                Some(_) => false,
            };
            if !searchable {
                continue;
            }
            hits.push(SearchHit { chat_jid, message_id, sender, body, timestamp, from_me });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    /// A portable JSON export of the whole archive (used by
    /// support exports).
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn export_json(&self) -> Result<String, StoreError> {
        let mut chats_json = Vec::new();
        for chat in self.chats()? {
            let mut messages_json = Vec::new();
            for (msg, flags) in self.messages(&chat.jid)? {
                messages_json.push(serde_json::json!({
                    "id": msg.id,
                    "sender": msg.sender,
                    "body": msg.body,
                    "timestamp": msg.timestamp,
                    "from_me": msg.from_me,
                    "flags": flags,
                }));
            }
            chats_json.push(serde_json::json!({
                "jid": chat.jid,
                "title": chat.title,
                "messages": messages_json,
            }));
        }
        serde_json::to_string(&serde_json::json!({ "chats": chats_json }))
            .map_err(|e| StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
    }

    /// The persisted blocked-chat set (chat ids), or `None` when
    /// no set has ever been stored in this archive. The
    /// distinction matters to the caller: a stored empty set is
    /// the server's word that nothing is blocked, while `None`
    /// means the archive has nothing to say and a legacy copy
    /// elsewhere (the pre-A6 settings file) may still need
    /// migrating in. See [`BLOCKED_CHATS_KEY`] for why the set
    /// lives here and not in the settings file.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures or a stored value that
    /// is not the JSON array [`Store::set_blocked_chats`] wrote.
    pub fn blocked_chats(&self) -> Result<Option<Vec<u64>>, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM store_meta WHERE key = ?1",
                params![BLOCKED_CHATS_KEY],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let ids: Vec<u64> = serde_json::from_str(&raw).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        Ok(Some(ids))
    }

    /// Replaces the persisted blocked-chat set. Called whenever
    /// the set changes locally and whenever the server's
    /// connect-time blocklist is adopted, so the stored copy
    /// always mirrors the last known truth.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn set_blocked_chats(&self, ids: &[u64]) -> Result<(), StoreError> {
        let raw = serde_json::to_string(&ids).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "INSERT INTO store_meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![BLOCKED_CHATS_KEY, raw],
        )?;
        Ok(())
    }
}

/// Whether a display string is a raw identifier rather than a
/// name: nine or more characters, all digits. The SQL guards in
/// this module express the same predicate with GLOB; the two must
/// stay in step.
fn is_raw_identifier(text: &str) -> bool {
    text.len() >= 9 && text.bytes().all(|b| b.is_ascii_digit())
}

fn kind_str(kind: LiveChatKind) -> &'static str {
    match kind {
        LiveChatKind::Dm => "dm",
        LiveChatKind::Group => "group",
        LiveChatKind::Channel => "channel",
        LiveChatKind::Broadcast => "broadcast",
    }
}

fn kind_of(s: &str) -> LiveChatKind {
    match s {
        "group" => LiveChatKind::Group,
        "channel" => LiveChatKind::Channel,
        "broadcast" => LiveChatKind::Broadcast,
        _ => LiveChatKind::Dm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backend::fake::{scripted_chats, scripted_messages};

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zaprust-store-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("archive.db")
    }

    fn call_record(call_id: &str, started_at: i64) -> backend::CallRecord {
        backend::CallRecord {
            id: 0,
            call_id: call_id.to_owned(),
            chat_jid: "12025550111@s.whatsapp.net".to_owned(),
            video: false,
            outgoing: false,
            missed: true,
            duration_secs: 0,
            started_at,
        }
    }

    /// The call history round-trips every field and reads back
    /// newest first, the order the Calls panel renders.
    #[test]
    fn call_history_round_trips_newest_first() {
        let store = Store::open_in_memory().unwrap();
        let mut connected = call_record("call-old", 1_700_000_000);
        connected.missed = false;
        connected.outgoing = true;
        connected.video = true;
        connected.duration_secs = 125;
        store.record_call(&connected).unwrap();
        store.record_call(&call_record("call-new", 1_700_003_600)).unwrap();
        let history = store.call_history().unwrap();
        assert_eq!(history.len(), 2);
        let newest = &history[0];
        assert_eq!(newest.call_id, "call-new");
        assert!(newest.missed && !newest.outgoing && !newest.video);
        assert_ne!(newest.id, 0, "read-back records carry their row id");
        let oldest = &history[1];
        assert_eq!(oldest.call_id, "call-old");
        assert_eq!(oldest.chat_jid, "12025550111@s.whatsapp.net");
        assert!(oldest.video && oldest.outgoing && !oldest.missed);
        assert_eq!(oldest.duration_secs, 125);
        assert_eq!(oldest.started_at, 1_700_000_000);
    }

    /// A call reported twice (its end event and a missed-call
    /// event) is stored once, under the first record.
    #[test]
    fn record_call_keeps_only_the_first_record_of_a_call() {
        let store = Store::open_in_memory().unwrap();
        let first = store.record_call(&call_record("call-1", 1_700_000_000)).unwrap();
        assert!(first.is_some(), "the first record is stored");
        let mut duplicate = call_record("call-1", 1_700_000_000);
        duplicate.missed = false;
        duplicate.duration_secs = 30;
        let second = store.record_call(&duplicate).unwrap();
        assert!(second.is_none(), "the duplicate is ignored");
        let history = store.call_history().unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].missed, "the first record wins");
    }

    /// The history is durable: records written by one open are
    /// read back by the next, so the Calls panel survives an app
    /// restart.
    #[test]
    fn call_history_survives_a_reopen() {
        let path = temp_db_path("calls");
        let _ = std::fs::remove_file(&path);
        let key = [7u8; 32];
        {
            let store = Store::open(&path, &key).unwrap();
            store.record_call(&call_record("call-1", 1_700_000_000)).unwrap();
        }
        let store = Store::open(&path, &key).unwrap();
        let history = store.call_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].call_id, "call-1");
        let _ = std::fs::remove_file(&path);
    }

    /// Item 116: favorites round-trip through their dedicated
    /// accessors, and a chat-list sync (which writes rows
    /// through `upsert_chat`, the only chat write the sync
    /// paths have) cannot wipe a flag: the favorite column is
    /// not part of that write, by construction.
    #[test]
    fn chat_favorites_round_trip_and_survive_chat_upserts() {
        let store = Store::open_in_memory().unwrap();
        let chats = scripted_chats();
        for chat in &chats {
            store.upsert_chat(chat).unwrap();
        }
        assert!(store.chat_favorites().unwrap().is_empty(), "nothing is a favorite by default");

        store.set_chat_favorite(&chats[0].jid, true).unwrap();
        store.set_chat_favorite(&chats[2].jid, true).unwrap();
        // JID order, not insertion order.
        let mut expected = vec![chats[0].jid.clone(), chats[2].jid.clone()];
        expected.sort();
        assert_eq!(store.chat_favorites().unwrap(), expected);

        // A re-sync rewrites every chat row (new previews, new
        // timestamps); the favorites must come through intact.
        for chat in &chats {
            let mut resynced = chat.clone();
            resynced.preview = "a newer preview".to_owned();
            resynced.timestamp += 60;
            store.upsert_chat(&resynced).unwrap();
        }
        assert_eq!(store.chat_favorites().unwrap(), expected, "a sync must not wipe favorites");

        store.set_chat_favorite(&chats[0].jid, false).unwrap();
        assert_eq!(store.chat_favorites().unwrap(), vec![chats[2].jid.clone()]);

        // A JID the archive does not hold cannot become a
        // favorite, and setting one is not an error.
        store.set_chat_favorite("0@s.whatsapp.net", true).unwrap();
        assert_eq!(store.chat_favorites().unwrap(), vec![chats[2].jid.clone()]);
    }

    /// Item 116: the flags are durable state, not session
    /// state: they read back after the archive is closed and
    /// reopened.
    #[test]
    fn chat_favorites_survive_a_reopen() {
        let path = temp_db_path("favorites");
        let _ = std::fs::remove_file(&path);
        let key = [7u8; 32];
        let jid = scripted_chats()[1].jid.clone();
        {
            let store = Store::open(&path, &key).unwrap();
            store.upsert_chat(&scripted_chats()[1]).unwrap();
            store.set_chat_favorite(&jid, true).unwrap();
        }
        let store = Store::open(&path, &key).unwrap();
        assert_eq!(store.chat_favorites().unwrap(), vec![jid]);
        let _ = std::fs::remove_file(&path);
    }

    /// Item 116: an archive written by the previous build
    /// (schema v9: every current table, no favorite column)
    /// migrates to v10 with its rows intact and no favorites,
    /// and the migrated archive takes flags.
    #[test]
    fn migration_from_v9_adds_an_unset_favorite_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE chats (
                jid TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                kind TEXT NOT NULL,
                preview TEXT NOT NULL DEFAULT '',
                preview_sender TEXT,
                timestamp INTEGER NOT NULL DEFAULT 0,
                unread INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                ephemeral_secs INTEGER NOT NULL DEFAULT 0,
                muted INTEGER NOT NULL DEFAULT 0,
                mute_until INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE messages (
                chat_jid TEXT NOT NULL,
                id TEXT NOT NULL,
                sender TEXT,
                body TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                from_me INTEGER NOT NULL DEFAULT 0,
                data TEXT,
                flags INTEGER NOT NULL DEFAULT 0,
                expires_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (chat_jid, id)
            );
            CREATE TABLE store_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE calls (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                call_id TEXT NOT NULL UNIQUE,
                chat_jid TEXT NOT NULL,
                video INTEGER NOT NULL DEFAULT 0,
                outgoing INTEGER NOT NULL DEFAULT 0,
                missed INTEGER NOT NULL DEFAULT 0,
                duration_secs INTEGER NOT NULL DEFAULT 0,
                started_at INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO chats (jid, title, kind) VALUES ('123@s.whatsapp.net', 'Ana', 'dm');
            PRAGMA user_version = 9;",
        )
        .unwrap();
        let store = Store { conn };
        store.migrate().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(store.chat_favorites().unwrap().is_empty(), "existing chats start unfavorited");
        let chats = store.chats().unwrap();
        assert_eq!(chats.len(), 1, "the v9 row survived the migration");
        assert_eq!(chats[0].title, "Ana");
        store.set_chat_favorite("123@s.whatsapp.net", true).unwrap();
        assert_eq!(store.chat_favorites().unwrap(), vec!["123@s.whatsapp.net".to_owned()]);
    }

    #[test]
    fn encrypted_archive_opens_only_with_its_key() {
        let path = temp_db_path("encrypted");
        let key = [7u8; 32];
        {
            let store = Store::open(&path, &key).unwrap();
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        }
        // The file on disk is not a plaintext SQLite database.
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3\0"));
        // The right key reopens it; a wrong key is refused.
        assert!(Store::open(&path, &key).is_ok());
        assert!(Store::open(&path, &[9u8; 32]).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn plaintext_archive_is_converted_on_first_open() {
        let path = temp_db_path("migrate");
        {
            // A legacy plaintext archive with a row of user data.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE legacy (body TEXT); INSERT INTO legacy VALUES ('hello');",
            )
            .unwrap();
        }
        let key = [3u8; 32];
        let store = Store::open(&path, &key).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3\0"));
        // The legacy table survived the conversion, readable only
        // through a keyed connection.
        let conn = Connection::open(&path).unwrap();
        apply_key(&conn, &key).unwrap();
        let body: String = conn.query_row("SELECT body FROM legacy", [], |row| row.get(0)).unwrap();
        assert_eq!(body, "hello");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn migration_reaches_current_version_and_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        store.migrate().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migration_converges_when_version_zero_already_has_tables() {
        // A pre-versioning archive: the v1 tables exist but
        // user_version was never stamped, so it reads 0. The
        // ladder must not die on "table already exists"; it adds
        // the missing columns and reaches the current version.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE chats (
                jid TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                kind TEXT NOT NULL,
                preview TEXT NOT NULL DEFAULT '',
                preview_sender TEXT,
                timestamp INTEGER NOT NULL DEFAULT 0,
                unread INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE messages (
                chat_jid TEXT NOT NULL,
                id TEXT NOT NULL,
                sender TEXT,
                body TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                from_me INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (chat_jid, id)
            );
            CREATE INDEX idx_messages_chat_time ON messages (chat_jid, timestamp);
            INSERT INTO chats (jid, title, kind) VALUES ('123@s.whatsapp.net', 'Ana', 'dm');",
        )
        .unwrap();
        let store = Store { conn };
        store.migrate().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The converged schema is fully usable and the row kept.
        store.upsert_chat(&scripted_chats()[0]).unwrap();
        let chats = store.chats().unwrap();
        assert!(chats.iter().any(|c| c.title == "Ana"), "pre-existing row survived: {chats:?}");
        store.migrate().unwrap();
    }

    #[test]
    fn migration_converges_when_a_step_was_already_applied() {
        // Version says 3, but the v4 columns are already present
        // (an interrupted or hand-repaired file): the v4 step
        // must not die on "duplicate column name".
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE chats (
                jid TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                kind TEXT NOT NULL,
                preview TEXT NOT NULL DEFAULT '',
                preview_sender TEXT,
                timestamp INTEGER NOT NULL DEFAULT 0,
                unread INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE messages (
                chat_jid TEXT NOT NULL,
                id TEXT NOT NULL,
                sender TEXT,
                body TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                from_me INTEGER NOT NULL DEFAULT 0,
                data TEXT,
                flags INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (chat_jid, id)
            );
            ALTER TABLE chats ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE chats ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
            PRAGMA user_version = 3;",
        )
        .unwrap();
        let store = Store { conn };
        store.migrate().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        store.upsert_chat(&scripted_chats()[0]).unwrap();
    }

    #[test]
    fn chat_and_message_round_trip() {
        let store = Store::open_in_memory().unwrap();
        for chat in scripted_chats() {
            store.upsert_chat(&chat).unwrap();
        }
        for msg in scripted_messages() {
            store.upsert_message(&msg, 0).unwrap();
        }
        let chats = store.chats().unwrap();
        assert_eq!(chats.len(), 6);
        assert_eq!(chats[0].jid, scripted_chats()[0].jid, "most recent first");
        let msgs: Vec<LiveMessage> =
            store.messages(&scripted_chats()[0].jid).unwrap().into_iter().map(|(m, _)| m).collect();
        assert_eq!(msgs, scripted_messages());
    }

    #[test]
    fn delivery_state_round_trips_and_never_downgrades() {
        // QC1614-A0-1: delivery is persisted synced state. A
        // Read message reloads as Read, a Sent-only message as
        // Sent, and a later rewrite carrying a lesser state
        // never lowers the archived one.
        let store = Store::open_in_memory().unwrap();
        let chat = &scripted_chats()[0];
        store.upsert_chat(chat).unwrap();
        let mut read_msg = LiveMessage::text("d1", chat.jid.clone(), None, "seen", 100, true);
        read_msg.delivery = Some(backend::DeliveryState::Read);
        store.upsert_message(&read_msg, 0).unwrap();
        let mut sent_msg = LiveMessage::text("d2", chat.jid.clone(), None, "sent", 200, true);
        sent_msg.delivery = Some(backend::DeliveryState::Sent);
        store.upsert_message(&sent_msg, 0).unwrap();
        let loaded = store.messages(&chat.jid).unwrap();
        assert_eq!(loaded[0].0.delivery, Some(backend::DeliveryState::Read));
        assert_eq!(loaded[1].0.delivery, Some(backend::DeliveryState::Sent));
        // A re-upsert with a lesser state keeps Read.
        let mut lesser = read_msg.clone();
        lesser.delivery = Some(backend::DeliveryState::Delivered);
        store.upsert_message(&lesser, 0).unwrap();
        // The receipt path advances monotonically too.
        assert!(store.set_message_delivery(&chat.jid, "d2", backend::DeliveryState::Read).unwrap());
        assert!(!store
            .set_message_delivery(&chat.jid, "d2", backend::DeliveryState::Sent)
            .unwrap());
        let loaded = store.messages(&chat.jid).unwrap();
        assert_eq!(loaded[0].0.delivery, Some(backend::DeliveryState::Read));
        assert_eq!(loaded[1].0.delivery, Some(backend::DeliveryState::Read));
    }

    #[test]
    fn starred_messages_span_all_chats() {
        // QC1614-A0-4: the Starred panel's source is the
        // persisted flags across chats, including chats that
        // were never opened.
        let store = Store::open_in_memory().unwrap();
        for chat in scripted_chats() {
            store.upsert_chat(&chat).unwrap();
        }
        let a = LiveMessage::text(
            "sa",
            scripted_chats()[0].jid.clone(),
            Some("A".into()),
            "one",
            100,
            false,
        );
        let b = LiveMessage::text(
            "sb",
            scripted_chats()[1].jid.clone(),
            Some("B".into()),
            "two",
            200,
            false,
        );
        store.upsert_message(&a, FLAG_STARRED).unwrap();
        store.upsert_message(&b, FLAG_STARRED).unwrap();
        let starred = store.starred_messages().unwrap();
        assert_eq!(starred.len(), 2);
        assert_eq!(starred[0].0.id, "sb", "newest first");
        assert_eq!(starred[1].0.id, "sa");
    }

    #[test]
    fn timed_mute_end_round_trips_through_the_archive() {
        // Schema v12: a timed mute's end persists with the mute
        // flag, so the mute survives a restart with its deadline
        // intact (section 24).
        let store = Store::open_in_memory().unwrap();
        let mut chat = scripted_chats()[0].clone();
        chat.muted = true;
        chat.mute_until = 1_700_028_800;
        store.upsert_chat(&chat).unwrap();
        let chats = store.chats().unwrap();
        let read = chats.iter().find(|c| c.jid == chat.jid).unwrap();
        assert!(read.muted);
        assert_eq!(read.mute_until, 1_700_028_800);
    }

    #[test]
    fn blocked_chats_round_trip_through_the_archive() {
        // The blocked set's persisted home is this encrypted
        // archive (privacy audit A6): a fresh archive has never
        // stored a set, a stored set reads back exactly, a
        // replacement replaces wholesale, and a stored empty set
        // stays distinguishable from "never stored".
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.blocked_chats().unwrap(), None, "nothing stored yet");
        store.set_blocked_chats(&[11, 22, 33]).unwrap();
        assert_eq!(store.blocked_chats().unwrap(), Some(vec![11, 22, 33]));
        store.set_blocked_chats(&[7]).unwrap();
        assert_eq!(store.blocked_chats().unwrap(), Some(vec![7]), "replacement is wholesale");
        store.set_blocked_chats(&[]).unwrap();
        assert_eq!(
            store.blocked_chats().unwrap(),
            Some(Vec::new()),
            "a stored empty set is a statement, not an absence"
        );
    }

    #[test]
    fn upsert_messages_writes_the_whole_batch() {
        let store = Store::open_in_memory().unwrap();
        let msgs = scripted_messages();
        let batch: Vec<(LiveMessage, i64)> =
            msgs.iter().cloned().map(|m| (m, FLAG_STARRED)).collect();
        store.upsert_messages(&batch).unwrap();
        let loaded = store.messages(&msgs[0].chat_jid).unwrap();
        assert_eq!(loaded.len(), msgs.len(), "every batched row lands");
        assert!(
            loaded.iter().all(|(_, flags)| *flags == FLAG_STARRED),
            "batched rows carry their flags"
        );
        assert_eq!(
            loaded.iter().map(|(m, _)| m).collect::<Vec<_>>(),
            msgs.iter().collect::<Vec<_>>(),
            "batched rows round-trip like single upserts"
        );
        // An empty batch is a no-op, not an error.
        store.upsert_messages(&[]).unwrap();
    }

    #[test]
    fn upsert_updates_in_place() {
        let store = Store::open_in_memory().unwrap();
        let mut chat = scripted_chats()[0].clone();
        store.upsert_chat(&chat).unwrap();
        chat.preview = "changed".to_owned();
        store.upsert_chat(&chat).unwrap();
        let chats = store.chats().unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].preview, "changed");

        let mut msg = scripted_messages()[0].clone();
        store.upsert_message(&msg, 0).unwrap();
        msg.body = "edited".to_owned();
        store.upsert_message(&msg, 0).unwrap();
        let msgs = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0.body, "edited");
    }

    #[test]
    fn set_message_flags_preserves_the_row() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.quoted_body = Some("original context".to_owned());
        store.upsert_message(&msg, 0).unwrap();
        assert!(store.set_message_flags(&msg.chat_jid, &msg.id, FLAG_STARRED).unwrap());
        let rows = store.messages(&msg.chat_jid).unwrap();
        let (loaded, flags) = &rows[0];
        assert_eq!(*flags, FLAG_STARRED);
        assert_eq!(loaded.timestamp, msg.timestamp, "timestamp must survive");
        assert_eq!(loaded.quoted_body.as_deref(), Some("original context"));
        assert_eq!(loaded.body, msg.body);
        // Missing rows report false instead of fabricating one.
        assert!(!store.set_message_flags(&msg.chat_jid, "nope", 0).unwrap());
    }

    /// Avatars round-trip: bytes with their picture id, the
    /// negative row (no bytes) that keeps a photo-less contact
    /// from being re-asked for a day, the full listing the app
    /// seeds its session from, and deletion on invalidation.
    #[test]
    fn avatars_round_trip_with_negatives_and_delete() {
        let store = Store::open_in_memory().unwrap();
        store
            .put_avatar("111@s.whatsapp.net", Some("pic-1"), Some(&[1, 2, 3]), 1_700_000_000)
            .unwrap();
        store.put_avatar("222@s.whatsapp.net", None, None, 1_700_000_100).unwrap();
        let (picture_id, bytes, fetched_at) = store.avatar("111@s.whatsapp.net").unwrap().unwrap();
        assert_eq!(picture_id.as_deref(), Some("pic-1"));
        assert_eq!(bytes, Some(vec![1, 2, 3]));
        assert_eq!(fetched_at, 1_700_000_000);
        let (picture_id, bytes, _) = store.avatar("222@s.whatsapp.net").unwrap().unwrap();
        assert!(picture_id.is_none() && bytes.is_none(), "the negative row carries no picture");
        assert!(store.avatar("333@s.whatsapp.net").unwrap().is_none());
        assert_eq!(store.avatars().unwrap().len(), 2);
        store.delete_avatar("111@s.whatsapp.net").unwrap();
        assert!(store.avatar("111@s.whatsapp.net").unwrap().is_none());
        // A fresh answer replaces the negative.
        store.put_avatar("222@s.whatsapp.net", Some("pic-2"), Some(&[9]), 1_700_000_200).unwrap();
        let (_, bytes, _) = store.avatar("222@s.whatsapp.net").unwrap().unwrap();
        assert_eq!(bytes, Some(vec![9]));
    }

    /// The media-retry recovery persists its refreshed direct
    /// path into the archived reference, so the next session
    /// downloads from the live location directly.
    #[test]
    fn media_direct_path_refresh_rewrites_the_archived_reference() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.photo_ref = Some(photo_reference());
        store.upsert_message(&msg, 0).unwrap();
        store.update_media_direct_path(&msg.chat_jid, &msg.id, "/v/t62.7118-24/fresh.enc").unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(
            rows[0].0.photo_ref.as_ref().map(|r| r.direct_path.as_str()),
            Some("/v/t62.7118-24/fresh.enc")
        );
        assert!(store.has_message(&msg.chat_jid, &msg.id).unwrap());
        assert!(!store.has_message(&msg.chat_jid, "nope").unwrap());
    }

    /// Mentions, the photo thumbnail and the sticker reference
    /// ride the message's data blob and come back on reload:
    /// that round-trip is what lets a restarted app render a
    /// history sticker as a sticker and a mention by name.
    #[test]
    fn mentions_thumbnail_and_sticker_ref_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.mentions = vec!["111@lid".to_owned(), "222@s.whatsapp.net".to_owned()];
        msg.photo_thumbnail = Some(vec![0xFF, 0xD8, 0xFF, 0xD9]);
        msg.sticker_ref = Some(backend::StickerRef {
            direct_path: "/v/t62.15575-24/sticker.enc".to_owned(),
            media_key: vec![1u8; 32],
            file_sha256: vec![2u8; 32],
            file_enc_sha256: vec![3u8; 32],
            file_length: 4321,
        });
        store.upsert_message(&msg, 0).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        let loaded = &rows[0].0;
        assert_eq!(loaded.mentions, msg.mentions);
        assert_eq!(loaded.photo_thumbnail, msg.photo_thumbnail);
        assert_eq!(loaded.sticker_ref, msg.sticker_ref);
    }

    fn photo_reference() -> backend::PhotoRef {
        backend::PhotoRef {
            direct_path: "/v/t62.7118-24/photo.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 123_456,
        }
    }

    /// A photo's re-download reference is archived with the
    /// message and comes back on reload: that round-trip is what
    /// keeps a received photo fetchable after a restart.
    #[test]
    fn photo_ref_round_trips_with_the_message() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.photo_ref = Some(photo_reference());
        store.upsert_message(&msg, 0).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows[0].0.photo_ref, Some(photo_reference()));
        // A message archived without a reference stays without one.
        let plain = scripted_messages()[0].clone();
        assert_eq!(plain.photo_ref, None);
    }

    fn voice_reference() -> backend::VoiceRef {
        backend::VoiceRef {
            direct_path: "/v/t62.7118-24/voice.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 45_678,
        }
    }

    /// A voice note's re-download reference round-trips with the
    /// message exactly like a photo's: that is what keeps a note
    /// received in an earlier session playable after a restart.
    #[test]
    fn voice_ref_round_trips_with_the_message() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.voice_ref = Some(voice_reference());
        store.upsert_message(&msg, 0).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows[0].0.voice_ref, Some(voice_reference()));
        // A message archived without a reference stays without one.
        let plain = scripted_messages()[0].clone();
        assert_eq!(plain.voice_ref, None);
    }

    /// A message author's JID round-trips with the message: it
    /// is the participant a group reaction must name, so an
    /// archived group message keeps its author across a restart
    /// instead of only while it is fresh in the session.
    #[test]
    fn sender_jid_round_trips_with_the_message() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.sender_jid = Some("6281234567890@s.whatsapp.net".to_owned());
        store.upsert_message(&msg, 0).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows[0].0.sender_jid.as_deref(), Some("6281234567890@s.whatsapp.net"));
        // A message archived without an author stays without one.
        let plain = scripted_messages()[0].clone();
        assert_eq!(plain.sender_jid, None);
    }

    /// Rows archived before the author JID was recorded carry a
    /// data payload without the field at all; they still load,
    /// with no author.
    #[test]
    fn message_data_without_sender_jid_loads_with_none() {
        let mut value = serde_json::to_value(MessageData::default()).unwrap();
        value.as_object_mut().unwrap().remove("sender_jid");
        let data: MessageData = serde_json::from_value(value).unwrap();
        assert_eq!(data.sender_jid, None);
    }

    fn video_reference() -> backend::VideoRef {
        backend::VideoRef {
            direct_path: "/v/t62.7118-24/video.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 1_234_567,
        }
    }

    fn video_thumbnail() -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 0xFF, 0xD9]
    }

    /// A video's re-download reference and its embedded thumbnail
    /// round-trip with the message: the reference keeps the video
    /// fetchable after a restart, the thumbnail keeps its bubble
    /// and viewer honest (the video's own frame, not a generated
    /// picture) without fetching its bytes.
    #[test]
    fn video_ref_and_thumbnail_round_trip_with_the_message() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.video_ref = Some(video_reference());
        msg.video_thumbnail = Some(video_thumbnail());
        store.upsert_message(&msg, 0).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows[0].0.video_ref, Some(video_reference()));
        assert_eq!(rows[0].0.video_thumbnail, Some(video_thumbnail()));
        // A message archived without either stays without either.
        let plain = scripted_messages()[0].clone();
        assert_eq!(plain.video_ref, None);
        assert_eq!(plain.video_thumbnail, None);
    }

    /// Clearing a video reference (the view-once spend) removes
    /// the reference and the thumbnail together: the row, its
    /// body and its kind all survive, and clearing a row that has
    /// neither (or does not exist) is a quiet no-op.
    #[test]
    fn clear_video_ref_drops_reference_and_thumbnail() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.video_ref = Some(video_reference());
        msg.video_thumbnail = Some(video_thumbnail());
        store.upsert_message(&msg, 0).unwrap();
        store.clear_video_ref(&msg.chat_jid, &msg.id).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.video_ref, None);
        assert_eq!(rows[0].0.video_thumbnail, None);
        assert_eq!(rows[0].0.body, msg.body);
        assert_eq!(rows[0].0.kind, msg.kind);
        store.clear_video_ref(&msg.chat_jid, &msg.id).unwrap();
        store.clear_video_ref(&msg.chat_jid, "no-such-id").unwrap();
    }

    /// A document's reference, name, page count and thumbnail
    /// round-trip with the message, as do the GIF flag, the
    /// sticker's animation state and frame, and a location's
    /// snapshot; the view-once spends drop exactly their own
    /// kind's reference.
    #[test]
    fn media_extras_round_trip_and_spends_drop_their_refs() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.document_ref = Some(backend::DocumentRef {
            direct_path: "/v/t62.7118-24/doc.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 54_321,
        });
        msg.document_name = Some("report.pdf".to_owned());
        msg.document_page_count = Some(12);
        msg.document_thumbnail = Some(video_thumbnail());
        msg.gif_playback = true;
        msg.sticker_animated = true;
        msg.sticker_thumbnail = Some(video_thumbnail());
        msg.location_thumbnail = Some(video_thumbnail());
        msg.voice_ref = Some(backend::VoiceRef {
            direct_path: "/v/t62.7118-24/voice.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 45_678,
        });
        msg.voice_waveform = Some(vec![1u8; 32]);
        msg.sticker_ref = Some(backend::StickerRef {
            direct_path: "/v/t62.7118-24/sticker.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 12_345,
        });
        store.upsert_message(&msg, 0).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        let row = &rows[0].0;
        assert_eq!(row.document_ref, msg.document_ref);
        assert_eq!(row.document_name.as_deref(), Some("report.pdf"));
        assert_eq!(row.document_page_count, Some(12));
        assert_eq!(row.document_thumbnail, Some(video_thumbnail()));
        assert!(row.gif_playback);
        assert!(row.sticker_animated);
        assert_eq!(row.sticker_thumbnail, Some(video_thumbnail()));
        assert_eq!(row.location_thumbnail, Some(video_thumbnail()));
        // The spends drop exactly their own reference.
        store.clear_voice_ref(&msg.chat_jid, &msg.id).unwrap();
        store.clear_sticker_ref(&msg.chat_jid, &msg.id).unwrap();
        store.clear_document_ref(&msg.chat_jid, &msg.id).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows[0].0.voice_ref, None);
        assert_eq!(rows[0].0.voice_waveform, None);
        assert_eq!(rows[0].0.sticker_ref, None);
        assert_eq!(rows[0].0.document_ref, None);
        // The rest of the row survives the spends.
        assert_eq!(rows[0].0.document_name.as_deref(), Some("report.pdf"));
        assert_eq!(rows[0].0.body, msg.body);
    }

    /// A message row whose rich payload no longer decodes:
    /// the bulk read skips exactly that row and reports it,
    /// and a targeted rewrite fails loudly instead of
    /// silently erasing the row's data.
    #[test]
    fn corrupt_payload_rows_degrade_per_row_and_fail_loudly_on_write() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.photo_ref = Some(backend::PhotoRef {
            direct_path: "/v/t62.7118-24/photo.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 12_345,
        });
        store.upsert_message(&msg, 0).unwrap();
        store
            .conn
            .execute(
                "UPDATE messages SET data = '{broken' WHERE chat_jid = ?1 AND id = ?2",
                params![msg.chat_jid, msg.id],
            )
            .unwrap();
        let (rows, skipped) = store.messages_reporting(&msg.chat_jid).unwrap();
        assert!(rows.is_empty(), "the corrupt row is skipped, the chat still loads");
        assert_eq!(skipped, vec![msg.id.clone()], "and the loss is reported by ID");
        let err = store.clear_photo_ref(&msg.chat_jid, &msg.id).unwrap_err();
        assert!(
            matches!(err, StoreError::CorruptData { .. }),
            "a rewrite over corrupt data fails loudly, got {err:?}"
        );
    }

    /// The view-once caption scrub refuses to stamp itself
    /// done over a row it cannot read: an undecodable payload
    /// might be a view-once row whose caption would survive.
    #[test]
    fn scrub_refuses_to_stamp_done_over_an_unreadable_row() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.view_once = true;
        msg.body = "a caption that must not survive".to_owned();
        store.upsert_message(&msg, 0).unwrap();
        store
            .conn
            .execute(
                "UPDATE messages SET data = '{broken' WHERE chat_jid = ?1 AND id = ?2",
                params![msg.chat_jid, msg.id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "DELETE FROM store_meta WHERE key = ?1",
                params![super::VIEW_ONCE_CAPTIONS_SCRUBBED_KEY],
            )
            .unwrap();
        let err = store.scrub_view_once_captions_once().unwrap_err();
        assert!(matches!(err, StoreError::CorruptData { .. }), "got {err:?}");
        let stamped: bool = store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM store_meta WHERE key = ?1)",
                params![super::VIEW_ONCE_CAPTIONS_SCRUBBED_KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stamped, "the scrub did not stamp itself done");
    }

    fn poster_reference() -> backend::PosterRef {
        backend::PosterRef {
            direct_path: "/v/t62.7118-24/poster.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![11u8; 32],
            file_enc_sha256: vec![12u8; 32],
        }
    }

    /// A video's poster reference and wire dimensions round-trip
    /// with the message, a fetched poster lands in the thumbnail
    /// slot, and the view-once spend drops the poster reference
    /// with the rest of the video's fetchable material.
    #[test]
    fn poster_ref_dimensions_and_fetched_poster_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.video_ref = Some(video_reference());
        msg.poster_ref = Some(poster_reference());
        msg.video_width = Some(1280);
        msg.video_height = Some(720);
        store.upsert_message(&msg, 0).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows[0].0.poster_ref, Some(poster_reference()));
        assert_eq!(rows[0].0.video_width, Some(1280));
        assert_eq!(rows[0].0.video_height, Some(720));
        assert_eq!(rows[0].0.video_thumbnail, None);
        // A fetched poster becomes the archived thumbnail.
        store.set_video_thumbnail(&msg.chat_jid, &msg.id, &video_thumbnail()).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows[0].0.video_thumbnail, Some(video_thumbnail()));
        // Storing a thumbnail for a missing row is a quiet no-op.
        store.set_video_thumbnail(&msg.chat_jid, "no-such-id", &video_thumbnail()).unwrap();
        // The spend drops the poster reference too.
        store.clear_video_ref(&msg.chat_jid, &msg.id).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows[0].0.poster_ref, None);
        assert_eq!(rows[0].0.video_thumbnail, None);
    }

    /// Clearing a photo reference (the view-once spend) removes
    /// exactly the reference: the row, its body and its kind all
    /// survive, and clearing a row that has none (or does not
    /// exist) is a quiet no-op.
    #[test]
    fn clear_photo_ref_drops_only_the_reference() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = scripted_messages()[0].clone();
        msg.photo_ref = Some(photo_reference());
        store.upsert_message(&msg, 0).unwrap();
        store.clear_photo_ref(&msg.chat_jid, &msg.id).unwrap();
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.photo_ref, None);
        assert_eq!(rows[0].0.body, msg.body);
        assert_eq!(rows[0].0.kind, msg.kind);
        store.clear_photo_ref(&msg.chat_jid, &msg.id).unwrap();
        store.clear_photo_ref(&msg.chat_jid, "no-such-id").unwrap();
    }

    /// The view-once opened state persists with the message's
    /// flags (privacy audit D13): set through the same targeted
    /// flags update the app uses, it comes back on reload, so a
    /// restart never presents an opened view-once message as
    /// unopened again. The bit is independent of the others.
    #[test]
    fn view_once_opened_flag_survives_a_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let msg = scripted_messages()[0].clone();
        store.upsert_message(&msg, 0).unwrap();
        assert!(store
            .set_message_flags(&msg.chat_jid, &msg.id, FLAG_STARRED | FLAG_VIEW_ONCE_OPENED)
            .unwrap());
        let rows = store.messages(&msg.chat_jid).unwrap();
        let flags = rows[0].1;
        assert_ne!(flags & FLAG_VIEW_ONCE_OPENED, 0, "the opened bit is stored");
        assert_ne!(flags & FLAG_STARRED, 0, "the other bits are untouched");
        // Clearing just the opened bit leaves the rest alone.
        assert!(store.set_message_flags(&msg.chat_jid, &msg.id, FLAG_STARRED).unwrap());
        let rows = store.messages(&msg.chat_jid).unwrap();
        assert_eq!(rows[0].1 & FLAG_VIEW_ONCE_OPENED, 0);
        assert_eq!(rows[0].1, FLAG_STARRED);
    }

    #[test]
    fn delete_chat_removes_the_chat_and_its_messages_only() {
        let store = Store::open_in_memory().unwrap();
        for chat in scripted_chats() {
            store.upsert_chat(&chat).unwrap();
        }
        for msg in scripted_messages() {
            store.upsert_message(&msg, 0).unwrap();
        }
        // A message in a second chat, to prove the deletion is
        // scoped to the one chat (the scripted messages all live
        // in the first chat).
        let kept = scripted_chats()[1].clone();
        let mut kept_msg = scripted_messages()[0].clone();
        kept_msg.chat_jid = kept.jid.clone();
        store.upsert_message(&kept_msg, 0).unwrap();
        let gone = scripted_chats()[0].clone();
        assert!(store.delete_chat(&gone.jid).unwrap(), "an existing chat reports true");
        assert!(store.chats().unwrap().iter().all(|c| c.jid != gone.jid), "the chat row is gone");
        assert!(store.messages(&gone.jid).unwrap().is_empty(), "its messages go with it");
        // The other chats and their messages are untouched
        // (six scripted chats since the unlinked group joined
        // the roster, minus the deleted one).
        assert_eq!(store.chats().unwrap().len(), 5);
        assert_eq!(store.messages(&kept.jid).unwrap().len(), 1);
        // Deleting a chat that is not there reports false.
        assert!(!store.delete_chat(&gone.jid).unwrap());
        assert!(!store.delete_chat("nobody@s.whatsapp.net").unwrap());
    }

    #[test]
    fn delete_chat_messages_keeps_the_chat_starred_and_newer_rows() {
        let store = Store::open_in_memory().unwrap();
        let chat = scripted_chats()[0].clone();
        store.upsert_chat(&chat).unwrap();
        // Three rows: an old plain one, an old starred one, and
        // one newer than the clear bound. Flags ride the upsert.
        let msg = |id: &str, ts: i64| {
            LiveMessage::text(id, &chat.jid, Some("Sender".to_owned()), id, ts, false)
        };
        store.upsert_message(&msg("old-plain", 100), 0).unwrap();
        store.upsert_message(&msg("old-starred", 200), FLAG_STARRED).unwrap();
        store.upsert_message(&msg("new-plain", 300), 0).unwrap();

        // Keep-starred clear bounded at 250: only the old plain
        // row is the clear's to remove.
        let deleted = store.delete_chat_messages(&chat.jid, true, Some(250)).unwrap();
        assert_eq!(deleted, 1);
        let remaining: Vec<String> =
            store.messages(&chat.jid).unwrap().into_iter().map(|(m, _)| m.id).collect();
        assert_eq!(remaining, ["old-starred", "new-plain"]);
        assert_eq!(store.chats().unwrap().len(), 1, "the chat row itself stays");

        // The same clear without the keep-starred choice takes
        // the starred row too; the newer row is still out of
        // the bound's reach.
        let deleted = store.delete_chat_messages(&chat.jid, false, Some(250)).unwrap();
        assert_eq!(deleted, 1);
        let remaining: Vec<String> =
            store.messages(&chat.jid).unwrap().into_iter().map(|(m, _)| m.id).collect();
        assert_eq!(remaining, ["new-plain"]);

        // Unbounded, the rest goes; a repeat clear is a no-op.
        assert_eq!(store.delete_chat_messages(&chat.jid, false, None).unwrap(), 1);
        assert!(store.messages(&chat.jid).unwrap().is_empty());
        assert_eq!(store.delete_chat_messages(&chat.jid, false, None).unwrap(), 0);
        assert_eq!(store.chats().unwrap().len(), 1, "the chat row still stays");
    }

    /// Whether `needle` appears in the bytes of the database
    /// file at `path` or of its WAL sidecar. Used on plaintext
    /// test databases, where a surviving body is directly
    /// visible in the file.
    fn db_files_contain(path: &Path, needle: &str) -> bool {
        let needle = needle.as_bytes();
        let hit = |p: &Path| {
            std::fs::read(p).unwrap_or_default().windows(needle.len()).any(|w| w == needle)
        };
        hit(path) || hit(Path::new(&format!("{}-wal", path.display())))
    }

    #[test]
    fn secure_delete_is_enabled_on_store_connections() {
        // The pragma is per-connection; init must switch it on
        // for the store's one connection, or every delete below
        // silently degrades to logical-only.
        let store = Store::open_in_memory().unwrap();
        let on: i64 = store.conn.query_row("PRAGMA secure_delete", [], |row| row.get(0)).unwrap();
        assert_eq!(on, 1, "store connections must run with secure_delete on");
    }

    #[test]
    fn deleted_message_bodies_do_not_survive_in_the_file() {
        // A plaintext file database (opened through the real
        // init path, just without a key) so a surviving body
        // would be directly visible in the raw bytes: this
        // proves the physical-deletion property, not merely
        // that a pragma was issued.
        let path = temp_db_path("physical-delete");
        let marker = "DELETION-MARKER-body-6f1a9c4e-never-meant-to-survive";
        let marker2 = "DELETION-MARKER-chat-8b2d05f1-never-meant-to-survive";
        let control = "CONTROL-body-41c77d2e-meant-to-survive";
        let chat_a = scripted_chats()[0].clone();
        let chat_b = scripted_chats()[1].clone();
        let store = Store::init(Connection::open(&path).unwrap()).unwrap();
        store.upsert_chat(&chat_a).unwrap();
        store.upsert_chat(&chat_b).unwrap();
        store
            .upsert_message(&LiveMessage::text("m-del", &chat_a.jid, None, marker, 100, false), 0)
            .unwrap();
        store
            .upsert_message(
                &LiveMessage::text("m-del-chat", &chat_b.jid, None, marker2, 100, false),
                0,
            )
            .unwrap();
        store
            .upsert_message(&LiveMessage::text("m-keep", &chat_a.jid, None, control, 101, false), 0)
            .unwrap();
        // Sanity: the scan can see the bodies while they live.
        assert!(db_files_contain(&path, marker), "setup: marker body must be in the file");
        assert!(db_files_contain(&path, control), "setup: control body must be in the file");

        // Single delete (the delete-for-me path): secure_delete
        // zeroes the page and the WAL truncation drops the
        // pre-delete frames, so the body is gone from the raw
        // bytes without any VACUUM.
        assert!(store.delete_message(&chat_a.jid, "m-del").unwrap());
        assert!(!db_files_contain(&path, marker), "deleted body must not survive in the file");
        assert!(db_files_contain(&path, control), "the kept message's body must still be there");

        // Whole-chat delete: the reclaim's VACUUM drops the
        // freed pages entirely.
        assert!(store.delete_chat(&chat_b.jid).unwrap());
        assert!(!db_files_contain(&path, marker2), "deleted chat's body must not survive");
        assert!(db_files_contain(&path, control), "the other chat must be untouched");
        drop(store);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn one_time_purge_clears_prefixed_remnants_and_runs_once() {
        let path = temp_db_path("purge-once");
        let old_marker = "OLD-REMNANT-body-02fe91aa-from-before-the-fix";
        let new_marker = "NEW-REMNANT-body-7d04b3c9-after-the-fix";
        let chat = scripted_chats()[0].clone();
        // Build a pre-fix archive: schema migrated, but the
        // connection deletes the way old builds did (default
        // secure_delete off, no reclaim, no purge flag).
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA secure_delete = OFF;").unwrap();
            let store = Store { conn };
            store.migrate().unwrap();
            store.upsert_chat(&chat).unwrap();
            store
                .upsert_message(
                    &LiveMessage::text("old-1", &chat.jid, None, old_marker, 100, false),
                    0,
                )
                .unwrap();
            drop(store);
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA secure_delete = OFF;").unwrap();
            conn.execute("DELETE FROM messages WHERE id = 'old-1'", []).unwrap();
            drop(conn);
        }
        assert!(
            db_files_contain(&path, old_marker),
            "setup: the logically deleted body must linger in free pages"
        );

        // First open under the fixed build: the purge runs and
        // the remnant is physically gone.
        {
            let store = Store::init(Connection::open(&path).unwrap()).unwrap();
            assert!(store.messages(&chat.jid).unwrap().is_empty());
        }
        assert!(!db_files_contain(&path, old_marker), "the purge must clear pre-fix remnants");
        let conn = Connection::open(&path).unwrap();
        let flag: Option<String> = conn
            .query_row(
                "SELECT value FROM store_meta WHERE key = ?1",
                params![FREELIST_PURGED_KEY],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(flag.as_deref(), Some("1"), "the purge records itself in store_meta");
        // Leave a fresh remnant the pre-fix way.
        conn.execute_batch("PRAGMA secure_delete = OFF;").unwrap();
        conn.execute(
            "INSERT INTO messages (chat_jid, id, sender, body, timestamp, from_me)
             VALUES (?1, 'new-1', NULL, ?2, 200, 0)",
            params![chat.jid, new_marker],
        )
        .unwrap();
        conn.execute("DELETE FROM messages WHERE id = 'new-1'", []).unwrap();
        drop(conn);
        assert!(db_files_contain(&path, new_marker), "setup: the new remnant must linger");

        // Second open: the purge must NOT run again (it is a
        // one-time migration, not a per-startup VACUUM), so the
        // remnant left behind above survives this open.
        {
            let _store = Store::init(Connection::open(&path).unwrap()).unwrap();
        }
        assert!(
            db_files_contain(&path, new_marker),
            "the one-time purge must not repeat on the second open"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn view_once_captions_are_not_archived() {
        // Privacy finding D2: a view-once message's caption is
        // part of its sealed payload. The archive keeps the row
        // (kind + view-once flag, so the placeholder renders
        // after a restart) but never the caption, in neither the
        // body column nor the kind payload.
        let store = Store::open_in_memory().unwrap();
        let chat = scripted_chats()[0].clone();
        store.upsert_chat(&chat).unwrap();
        let mut vo = LiveMessage::text("vo-1", &chat.jid, None, "secret caption", 100, false);
        vo.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Photo,
            caption: "secret caption".to_owned(),
            seconds: 0,
            size_bytes: 10,
        };
        vo.view_once = true;
        store.upsert_message(&vo, 0).unwrap();
        let mut photo = LiveMessage::text("ph-1", &chat.jid, None, "kept caption", 101, false);
        photo.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Photo,
            caption: "kept caption".to_owned(),
            seconds: 0,
            size_bytes: 10,
        };
        store.upsert_message(&photo, 0).unwrap();

        let msgs = store.messages(&chat.jid).unwrap();
        let vo_back = &msgs.iter().find(|(m, _)| m.id == "vo-1").unwrap().0;
        assert_eq!(vo_back.body, "", "the archived body must not be the caption");
        assert!(vo_back.view_once, "the view-once flag survives the round trip");
        match &vo_back.kind {
            backend::LiveMessageKind::Media { caption, .. } => {
                assert_eq!(caption, "", "the archived kind must not carry the caption");
            }
            other => panic!("expected the media kind to survive, got {other:?}"),
        }
        let photo_back = &msgs.iter().find(|(m, _)| m.id == "ph-1").unwrap().0;
        assert_eq!(photo_back.body, "kept caption", "a normal photo's caption round-trips");
        match &photo_back.kind {
            backend::LiveMessageKind::Media { caption, .. } => {
                assert_eq!(caption, "kept caption");
            }
            other => panic!("expected the media kind to survive, got {other:?}"),
        }
    }

    #[test]
    fn one_time_scrub_removes_archived_view_once_captions() {
        // Archives written before the D2 fix hold view-once
        // captions in the body column, the kind payload, and
        // the chat preview. The rows are identifiable (the
        // view-once flag is in the stored data), so the first
        // open under the fixed build scrubs exactly those rows
        // and recomputes the affected previews, exactly once.
        let path = temp_db_path("vo-scrub");
        let chat = scripted_chats()[0].clone();
        let caption = "VO-LEGACY-caption-9f31c";
        let late_caption = "VO-LATE-caption-51b8e";
        let media_data = |caption: &str, view_once: bool| {
            serde_json::to_string(&MessageData {
                kind: Some(backend::LiveMessageKind::Media {
                    media: backend::MediaKind::Photo,
                    caption: caption.to_owned(),
                    seconds: 0,
                    size_bytes: 10,
                }),
                view_once,
                ..Default::default()
            })
            .unwrap()
        };
        let insert_legacy = |conn: &Connection, id: &str, body: &str, data: &str, ts: i64| {
            conn.execute(
                "INSERT INTO messages (chat_jid, id, sender, body, timestamp, from_me, data, flags, expires_at)
                 VALUES (?1, ?2, NULL, ?3, ?4, 0, ?5, 0, 0)",
                params![chat.jid, id, body, ts, data],
            )
            .unwrap();
        };
        // Build the pre-fix archive by hand: schema migrated,
        // rows written the way the old builds wrote them.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
            let store = Store { conn };
            store.migrate().unwrap();
            let mut legacy_chat = chat.clone();
            legacy_chat.preview = caption.to_owned();
            store.upsert_chat(&legacy_chat).unwrap();
            insert_legacy(
                &store.conn,
                "photo-old",
                "a normal caption",
                &media_data("a normal caption", false),
                200,
            );
            insert_legacy(&store.conn, "vo-old", caption, &media_data(caption, true), 300);
            drop(store);
        }

        // First open under the fixed build: the scrub runs.
        {
            let store = Store::init(Connection::open(&path).unwrap()).unwrap();
            let msgs = store.messages(&chat.jid).unwrap();
            let vo_back = &msgs.iter().find(|(m, _)| m.id == "vo-old").unwrap().0;
            assert_eq!(vo_back.body, "", "the legacy caption leaves the body column");
            assert!(vo_back.view_once);
            match &vo_back.kind {
                backend::LiveMessageKind::Media { caption: stored, .. } => {
                    assert_eq!(stored, "", "the legacy caption leaves the kind payload");
                }
                other => panic!("expected the media kind to survive, got {other:?}"),
            }
            let photo_back = &msgs.iter().find(|(m, _)| m.id == "photo-old").unwrap().0;
            assert_eq!(photo_back.body, "a normal caption", "normal rows are untouched");
            let chats = store.chats().unwrap();
            let row = chats.iter().find(|c| c.jid == chat.jid).unwrap();
            assert_eq!(
                row.preview,
                core::model::VIEW_ONCE_PREVIEW,
                "the preview that was the caption becomes the placeholder"
            );
        }
        let conn = Connection::open(&path).unwrap();
        let flag: Option<String> = conn
            .query_row(
                "SELECT value FROM store_meta WHERE key = ?1",
                params![VIEW_ONCE_CAPTIONS_SCRUBBED_KEY],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(flag.as_deref(), Some("1"), "the scrub records itself in store_meta");
        // A legacy-style row written after the scrub (by hand,
        // standing in for any later writer) must survive the
        // next open: the scrub is one-time, not per-startup.
        insert_legacy(&conn, "vo-late", late_caption, &media_data(late_caption, true), 400);
        drop(conn);
        {
            let store = Store::init(Connection::open(&path).unwrap()).unwrap();
            let msgs = store.messages(&chat.jid).unwrap();
            let late = &msgs.iter().find(|(m, _)| m.id == "vo-late").unwrap().0;
            assert_eq!(late.body, late_caption, "the one-time scrub must not repeat");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn delete_messages_removes_the_batch_and_reports_the_count() {
        let store = Store::open_in_memory().unwrap();
        let chat_a = scripted_chats()[0].clone();
        let chat_b = scripted_chats()[1].clone();
        for (chat, id) in [(&chat_a, "a1"), (&chat_a, "a2"), (&chat_b, "b1")] {
            store
                .upsert_message(&LiveMessage::text(id, &chat.jid, None, id, 100, false), 0)
                .unwrap();
        }
        // The expiry pass's shape: one batch spanning chats,
        // including a key that does not exist.
        let deleted = store
            .delete_messages(&[
                (chat_a.jid.clone(), "a1".to_owned()),
                (chat_b.jid.clone(), "b1".to_owned()),
                (chat_a.jid.clone(), "missing".to_owned()),
            ])
            .unwrap();
        assert_eq!(deleted, 2);
        let remaining: Vec<String> =
            store.messages(&chat_a.jid).unwrap().into_iter().map(|(m, _)| m.id).collect();
        assert_eq!(remaining, ["a2"]);
        assert!(store.messages(&chat_b.jid).unwrap().is_empty());
        // An empty batch is a no-op, not an error.
        assert_eq!(store.delete_messages(&[]).unwrap(), 0);
    }

    #[test]
    fn file_backed_store_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("zaprust-store-test-{}", std::process::id()));
        let path = dir.join("archive.db");
        {
            let store = Store::open(&path, &[1u8; 32]).unwrap();
            store.upsert_chat(&scripted_chats()[0]).unwrap();
        }
        let store = Store::open(&path, &[1u8; 32]).unwrap();
        assert_eq!(store.chats().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A poll message as the backend delivers a creation: zero
    /// counts, no ballots, and the creation secret plus creator
    /// the translation layer extracted from the payload.
    fn poll_creation(chat_jid: &str, id: &str) -> LiveMessage {
        let mut msg = LiveMessage::text(
            id,
            chat_jid,
            Some("Sarah Miller".to_owned()),
            "Lunch?",
            1_700_000_000,
            false,
        );
        msg.kind = backend::LiveMessageKind::Poll {
            question: "Lunch?".to_owned(),
            options: vec![
                ("Deli".to_owned(), 0),
                ("Sushi".to_owned(), 0),
                ("Delivery".to_owned(), 0),
            ],
            multiple: false,
            my_votes: Vec::new(),
            correct: None,
        };
        msg.poll_secret = Some(vec![7u8; 32]);
        msg.poll_creator = Some("10000000001@s.whatsapp.net".to_owned());
        msg
    }

    /// Item 123: the creation secret, the creator and the ballots
    /// ride with the archived poll, so a later session can vote on
    /// it and tally its votes.
    #[test]
    fn poll_secret_creator_and_ballots_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let chat = "10000000001@s.whatsapp.net";
        store.upsert_message(&poll_creation(chat, "poll-1"), 0).unwrap();
        store
            .set_poll_tally(
                chat,
                "poll-1",
                &[1, 0, 0],
                &[],
                &[("10000000007@s.whatsapp.net".to_owned(), vec![0])],
            )
            .unwrap();
        let rows = store.messages(chat).unwrap();
        assert_eq!(rows.len(), 1);
        let (msg, _) = &rows[0];
        assert_eq!(msg.poll_secret.as_deref(), Some([7u8; 32].as_slice()));
        assert_eq!(msg.poll_creator.as_deref(), Some("10000000001@s.whatsapp.net"));
        assert_eq!(msg.poll_ballots, vec![("10000000007@s.whatsapp.net".to_owned(), vec![0])]);
        let backend::LiveMessageKind::Poll { options, .. } = &msg.kind else {
            panic!("the row reads back as a poll");
        };
        assert_eq!(
            options.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
            vec![1, 0, 0],
            "the tally write lands on the counts"
        );
    }

    fn event_creation(chat_jid: &str, id: &str) -> LiveMessage {
        let mut msg = LiveMessage::text(
            id,
            chat_jid,
            Some("Sarah Miller".to_owned()),
            "Rooftop dinner",
            1_700_000_000,
            false,
        );
        msg.kind = backend::LiveMessageKind::Event {
            name: "Rooftop dinner".to_owned(),
            description: String::new(),
            start_time: 1_700_086_400,
            end_time: 1_700_097_200,
            location: "Skyline Rooftop".to_owned(),
            join_link: String::new(),
            scheduled_call: false,
            extra_guests: true,
            canceled: false,
            my_rsvp: None,
            going: 0,
            maybe: 0,
            not_going: 0,
        };
        msg.event_secret = Some(vec![9u8; 32]);
        msg.event_creator = Some("10000000001@s.whatsapp.net".to_owned());
        msg
    }

    /// P7: the creation secret, the creator and the ballots
    /// ride with the archived event, so a later session can
    /// answer it and tally its responses; field edits and
    /// cancellations land without touching the tallies.
    #[test]
    fn event_secret_creator_ballots_and_fields_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let chat = "10000000001@s.whatsapp.net";
        store.upsert_message(&event_creation(chat, "event-1"), 0).unwrap();
        store
            .set_event_tally(
                chat,
                "event-1",
                2,
                1,
                0,
                Some(backend::EventRsvp::Going),
                &[
                    ("10000000001@s.whatsapp.net".to_owned(), backend::EventRsvp::Going),
                    ("10000000007@s.whatsapp.net".to_owned(), backend::EventRsvp::Maybe),
                ],
            )
            .unwrap();
        store
            .set_event_fields(
                chat,
                "event-1",
                "Rooftop dinner (moved)",
                "Bring a jacket",
                1_700_100_000,
                0,
                "Harbor Terrace",
                "",
                false,
                true,
                true,
            )
            .unwrap();
        let rows = store.messages(chat).unwrap();
        assert_eq!(rows.len(), 1);
        let (msg, _) = &rows[0];
        assert_eq!(msg.event_secret.as_deref(), Some([9u8; 32].as_slice()));
        assert_eq!(msg.event_creator.as_deref(), Some("10000000001@s.whatsapp.net"));
        assert_eq!(msg.event_ballots.len(), 2);
        let backend::LiveMessageKind::Event {
            name,
            start_time,
            location,
            canceled,
            my_rsvp,
            going,
            maybe,
            ..
        } = &msg.kind
        else {
            panic!("the row reads back as an event");
        };
        assert_eq!(name, "Rooftop dinner (moved)");
        assert_eq!(*start_time, 1_700_100_000);
        assert_eq!(location, "Harbor Terrace");
        assert!(canceled, "the cancellation lands");
        assert_eq!(*my_rsvp, Some(backend::EventRsvp::Going), "edits keep my answer");
        assert_eq!((*going, *maybe), (2, 1), "edits keep the tallies");
    }

    /// Item 123: tallies written by vote aggregation are durable
    /// across a restart (a file-backed reopen), together with the
    /// secret that makes the poll votable in the new session.
    #[test]
    fn poll_tally_survives_a_reopen() {
        let path = temp_db_path("polls");
        let _ = std::fs::remove_file(&path);
        let key = [3u8; 32];
        let chat = "10000000001@s.whatsapp.net";
        {
            let store = Store::open(&path, &key).unwrap();
            store.upsert_message(&poll_creation(chat, "poll-1"), 0).unwrap();
            store
                .set_poll_tally(
                    chat,
                    "poll-1",
                    &[2, 1, 0],
                    &[1],
                    &[
                        ("10000000007@s.whatsapp.net".to_owned(), vec![0]),
                        ("10000000008@s.whatsapp.net".to_owned(), vec![0]),
                    ],
                )
                .unwrap();
        }
        let store = Store::open(&path, &key).unwrap();
        let rows = store.messages(chat).unwrap();
        let (msg, _) = &rows[0];
        let backend::LiveMessageKind::Poll { options, my_votes, .. } = &msg.kind else {
            panic!("the row reads back as a poll");
        };
        assert_eq!(options.iter().map(|(_, v)| *v).collect::<Vec<_>>(), vec![2, 1, 0]);
        assert_eq!(my_votes, &vec![1]);
        assert_eq!(msg.poll_ballots.len(), 2);
        assert_eq!(msg.poll_secret.as_deref(), Some([7u8; 32].as_slice()));
        let _ = std::fs::remove_file(&path);
    }

    /// Item 123: a creation re-delivered after votes were tallied
    /// (history sync, a receipt-driven re-upsert) carries zero
    /// counts and no ballots; the write merge must keep the
    /// tallied state instead of resetting the poll.
    #[test]
    fn poll_creation_reupsert_keeps_the_tallied_state() {
        let store = Store::open_in_memory().unwrap();
        let chat = "10000000001@s.whatsapp.net";
        store.upsert_message(&poll_creation(chat, "poll-1"), 0).unwrap();
        store
            .set_poll_tally(
                chat,
                "poll-1",
                &[1, 0, 0],
                &[0],
                &[("10000000007@s.whatsapp.net".to_owned(), vec![0])],
            )
            .unwrap();
        // The same creation again, exactly as re-delivered.
        store.upsert_message(&poll_creation(chat, "poll-1"), 0).unwrap();
        let rows = store.messages(chat).unwrap();
        let (msg, _) = &rows[0];
        let backend::LiveMessageKind::Poll { options, my_votes, .. } = &msg.kind else {
            panic!("the row reads back as a poll");
        };
        assert_eq!(
            options.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
            vec![1, 0, 0],
            "the re-delivered creation does not reset the counts"
        );
        assert_eq!(my_votes, &vec![0], "nor my votes");
        assert_eq!(msg.poll_ballots.len(), 1, "nor the ballots");
        assert_eq!(msg.poll_secret.as_deref(), Some([7u8; 32].as_slice()));
    }

    /// A tally write for a missing or non-poll row is a quiet
    /// no-op, and a count list that does not match the poll's
    /// options is refused rather than half-applied.
    #[test]
    fn set_poll_tally_ignores_missing_rows_and_bad_shapes() {
        let store = Store::open_in_memory().unwrap();
        let chat = "10000000001@s.whatsapp.net";
        store.set_poll_tally(chat, "missing", &[1], &[], &[]).unwrap();
        let text = LiveMessage::text("t-1", chat, None, "hello", 1_700_000_000, false);
        store.upsert_message(&text, 0).unwrap();
        store.set_poll_tally(chat, "t-1", &[1], &[], &[]).unwrap();
        store.upsert_message(&poll_creation(chat, "poll-1"), 0).unwrap();
        store.set_poll_tally(chat, "poll-1", &[1, 2], &[], &[]).unwrap();
        let rows = store.messages(chat).unwrap();
        let (msg, _) = rows.iter().find(|(m, _)| m.id == "poll-1").unwrap();
        let backend::LiveMessageKind::Poll { options, .. } = &msg.kind else {
            panic!("the row reads back as a poll");
        };
        assert!(options.iter().all(|(_, v)| *v == 0), "a mismatched count list changes nothing");
    }

    fn titled_chat(jid: &str, title: &str) -> LiveChat {
        LiveChat {
            jid: jid.to_owned(),
            title: title.to_owned(),
            kind: backend::LiveChatKind::Group,
            preview: String::new(),
            preview_sender: None,
            timestamp: 1_700_000_000,
            unread: 0,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        }
    }

    /// Identity monotonicity at the archive layer: a raw
    /// identifier title may never overwrite a resolved name, a
    /// resolved name always overwrites a raw identifier, and the
    /// same rule holds for stored message sender labels.
    #[test]
    fn raw_identifiers_never_overwrite_resolved_names() {
        let store = Store::open_in_memory().unwrap();
        let group = "120363430176830545@g.us";
        store.upsert_chat(&titled_chat(group, "Weekend Hikers")).unwrap();
        store.upsert_chat(&titled_chat(group, "120363430176830545")).unwrap();
        let chats = store.chats().unwrap();
        assert_eq!(
            chats[0].title, "Weekend Hikers",
            "a live raw title cannot downgrade the subject"
        );

        let dm = "128815045062660@lid";
        store.upsert_chat(&titled_chat(dm, "128815045062660")).unwrap();
        store.upsert_chat(&titled_chat(dm, "Bima Prasetyo")).unwrap();
        let chats = store.chats().unwrap();
        let dm_row = chats.iter().find(|c| c.jid == dm).unwrap();
        assert_eq!(dm_row.title, "Bima Prasetyo", "a resolved name replaces the raw title");

        let mut msg = LiveMessage::text(
            "m1",
            group,
            Some("Bima Prasetyo".to_owned()),
            "halo",
            1_700_000_100,
            false,
        );
        msg.sender_jid = Some("57243458396358@lid".to_owned());
        store.upsert_message(&msg, 0).unwrap();
        let mut raw = msg.clone();
        raw.sender = Some("57243458396358".to_owned());
        store.upsert_message(&raw, 0).unwrap();
        let rows = store.messages(group).unwrap();
        assert_eq!(
            rows[0].0.sender.as_deref(),
            Some("Bima Prasetyo"),
            "a raw sender label cannot downgrade a resolved one"
        );
    }

    /// The rename paths heal exactly the raw rows: chat titles,
    /// message sender labels (matched through the archived sender
    /// JID), and previews, while resolved values and raw rename
    /// inputs are refused.
    #[test]
    fn renames_heal_only_raw_labels() {
        let store = Store::open_in_memory().unwrap();
        let group = "120363430176830545@g.us";
        store.upsert_chat(&titled_chat(group, "120363430176830545")).unwrap();
        assert!(store.rename_chat(group, "Weekend Hikers").unwrap());
        assert!(!store.rename_chat(group, "Other Name").unwrap(), "a resolved title is final");
        assert!(
            !store.rename_chat(group, "120363430176830545").unwrap(),
            "a raw rename is refused"
        );

        // A chat healed to its formatted phone number is not
        // resolved yet: the contact's name still upgrades it
        // through the heal path.
        let dm = "6281234567890@s.whatsapp.net";
        store.upsert_chat(&titled_chat(dm, "+6281234567890")).unwrap();
        assert!(store.rename_chat(dm, "Saved Sarah").unwrap());
        // An authoritative rename replaces even a resolved
        // title (a contact renamed on the phone, a group subject
        // change); empty and raw inputs stay refused.
        assert!(store.rename_chat_authoritative(dm, "Sarah Wijaya").unwrap());
        assert!(!store.rename_chat_authoritative(dm, "").unwrap());
        assert!(!store.rename_chat_authoritative(dm, "6281234567890").unwrap());
        let chats = store.chats().unwrap();
        let dm_row = chats.iter().find(|c| c.jid == dm).unwrap();
        assert_eq!(dm_row.title, "Sarah Wijaya");

        let mut msg = LiveMessage::text(
            "m1",
            group,
            Some("57243458396358".to_owned()),
            "halo",
            1_700_000_100,
            false,
        );
        msg.sender_jid = Some("57243458396358@lid".to_owned());
        store.upsert_message(&msg, 0).unwrap();
        let mut named = msg.clone();
        named.id = "m2".to_owned();
        named.sender = Some("99999999999999".to_owned());
        named.sender_jid = Some("99999999999999@lid".to_owned());
        store.upsert_message(&named, 0).unwrap();

        let changed = store.rename_sender("57243458396358@lid", "Bima Prasetyo").unwrap();
        assert_eq!(changed, 1);
        let rows = store.messages(group).unwrap();
        let healed = rows.iter().find(|(m, _)| m.id == "m1").unwrap();
        assert_eq!(healed.0.sender.as_deref(), Some("Bima Prasetyo"));
        let other = rows.iter().find(|(m, _)| m.id != "m1").unwrap();
        assert_eq!(
            other.0.sender.as_deref(),
            Some("99999999999999"),
            "another sender's raw label is untouched by this rename"
        );
        assert_eq!(store.rename_sender("57243458396358@lid", "57243458396358").unwrap(), 0);
    }

    fn search_text(id: &str, chat: &str, body: &str, ts: i64) -> LiveMessage {
        LiveMessage::text(id, chat, Some("Ana".to_owned()), body, ts, false)
    }

    /// Global search finds text across chats, newest first,
    /// case-insensitively, and reports the chat each hit lives
    /// in (design standard section 25).
    #[test]
    fn search_messages_matches_text_across_chats_newest_first() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_message(&search_text("a1", "111@s.whatsapp.net", "the Report is ready", 100), 0)
            .unwrap();
        store
            .upsert_message(&search_text("b1", "222@s.whatsapp.net", "no match here", 300), 0)
            .unwrap();
        store
            .upsert_message(
                &search_text("b2", "222@s.whatsapp.net", "another report draft", 200),
                0,
            )
            .unwrap();
        let hits = store.search_messages("report", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].message_id, "b2", "newest first");
        assert_eq!(hits[0].chat_jid, "222@s.whatsapp.net");
        assert_eq!(hits[1].message_id, "a1");
        assert_eq!(hits[1].sender, "Ana");
        // The limit is a hard cap.
        assert_eq!(store.search_messages("report", 1).unwrap().len(), 1);
        // An empty query searches nothing.
        assert!(store.search_messages("  ", 10).unwrap().is_empty());
    }

    /// Only visible text matches: a media caption counts (a
    /// document's file name is its caption), a captionless
    /// media row's kind label does not, and deleted or
    /// view-once rows never surface.
    #[test]
    fn search_messages_matches_only_visible_text() {
        let store = Store::open_in_memory().unwrap();
        let mut photo = search_text("p1", "111@s.whatsapp.net", "Photo", 100);
        photo.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Photo,
            caption: String::new(),
            seconds: 0,
            size_bytes: 10,
        };
        store.upsert_message(&photo, 0).unwrap();
        let mut doc = search_text("d1", "111@s.whatsapp.net", "photo-notes.pdf", 200);
        doc.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Document,
            caption: "photo-notes.pdf".to_owned(),
            seconds: 0,
            size_bytes: 10,
        };
        store.upsert_message(&doc, 0).unwrap();
        let gone = search_text("g1", "111@s.whatsapp.net", "photo of the keys", 300);
        store.upsert_message(&gone, FLAG_DELETED).unwrap();
        let mut sealed = search_text("v1", "111@s.whatsapp.net", "photo secret", 400);
        sealed.view_once = true;
        store.upsert_message(&sealed, 0).unwrap();
        let hits = store.search_messages("photo", 10).unwrap();
        assert_eq!(hits.len(), 1, "only the captioned document matches: {hits:?}");
        assert_eq!(hits[0].message_id, "d1");
    }

    /// LIKE metacharacters in the query are literal text, not
    /// pattern syntax.
    #[test]
    fn search_messages_escapes_like_metacharacters() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_message(&search_text("a1", "111@s.whatsapp.net", "100% done_ deal", 100), 0)
            .unwrap();
        store
            .upsert_message(&search_text("a2", "111@s.whatsapp.net", "1000 doneX deal", 200), 0)
            .unwrap();
        let hits = store.search_messages("100% done_", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "a1");
    }
}
