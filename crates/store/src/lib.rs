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
//!   databases under one key held in the OS keyring (see
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
pub const SCHEMA_VERSION: i64 = 8;

/// `store_meta` key recording that the one-time freelist purge
/// (see [`Store::open`]) has run for this archive. The purge
/// rewrites the whole file, so it must happen exactly once per
/// archive, not on every startup; the flag is written only
/// after the purge completes, so a crash mid-purge simply
/// retries on the next open.
const FREELIST_PURGED_KEY: &str = "freelist_purged";

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

/// The rich payload of one message, stored as JSON in the
/// `messages.data` column (schema v2). Absence (NULL) means a plain
/// text message with no extras.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct MessageData {
    #[serde(default)]
    kind: Option<backend::LiveMessageKind>,
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
            std::fs::create_dir_all(parent).map_err(|e| {
                StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })?;
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
        Ok(store)
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
    pub fn upsert_chat(&self, chat: &LiveChat) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO chats (jid, title, kind, preview, preview_sender, timestamp, unread, archived, pinned, ephemeral_secs, muted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(jid) DO UPDATE SET
                title = excluded.title,
                kind = excluded.kind,
                preview = excluded.preview,
                preview_sender = excluded.preview_sender,
                timestamp = excluded.timestamp,
                unread = excluded.unread,
                archived = excluded.archived,
                pinned = excluded.pinned,
                ephemeral_secs = excluded.ephemeral_secs,
                muted = excluded.muted",
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
            ],
        )?;
        Ok(())
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

    /// The single-row write behind [`Store::upsert_message`] and
    /// [`Store::upsert_messages`], against any connection handle
    /// (the store's own, or a transaction borrowing it).
    fn write_message(conn: &Connection, msg: &LiveMessage, flags: i64) -> Result<(), StoreError> {
        let data = MessageData {
            kind: match &msg.kind {
                backend::LiveMessageKind::Text => None,
                other => Some(other.clone()),
            },
            reactions: msg.reactions.clone(),
            my_reaction: msg.my_reaction.clone(),
            quoted_sender: msg.quoted_sender.clone(),
            quoted_body: msg.quoted_body.clone(),
            link_preview: msg.link_preview.clone(),
            edited: msg.edited,
            forwarded: msg.forwarded,
            view_once: msg.view_once,
        };
        let data_json = serde_json::to_string(&data).map_err(|e| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        conn.execute(
            "INSERT INTO messages (chat_jid, id, sender, body, timestamp, from_me, data, flags, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(chat_jid, id) DO UPDATE SET
                sender = excluded.sender,
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
                msg.body,
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

    /// All archived chats, most recent first.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn chats(&self) -> Result<Vec<LiveChat>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT jid, title, kind, preview, preview_sender, timestamp, unread, archived, pinned, ephemeral_secs, muted
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
        let mut stmt = self.conn.prepare(
            "SELECT chat_jid, id, sender, body, timestamp, from_me, data, flags, expires_at
             FROM messages WHERE chat_jid = ?1 ORDER BY timestamp ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![chat_jid], |row| {
            let data_text: Option<String> = row.get(6)?;
            let data: MessageData =
                data_text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            let mut msg = LiveMessage::text(
                row.get::<_, String>(1)?,
                row.get::<_, String>(0)?,
                row.get(2)?,
                row.get::<_, String>(3)?,
                row.get(4)?,
                row.get::<_, i64>(5)? != 0,
            );
            msg.kind = data.kind.unwrap_or(backend::LiveMessageKind::Text);
            msg.reactions = data.reactions;
            msg.my_reaction = data.my_reaction;
            msg.quoted_sender = data.quoted_sender;
            msg.quoted_body = data.quoted_body;
            msg.link_preview = data.link_preview;
            msg.edited = data.edited;
            msg.forwarded = data.forwarded;
            msg.view_once = data.view_once;
            msg.expires_at = row.get(8)?;
            let flags: i64 = row.get(7)?;
            Ok((msg, flags))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
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
}

fn kind_str(kind: LiveChatKind) -> &'static str {
    match kind {
        LiveChatKind::Dm => "dm",
        LiveChatKind::Group => "group",
        LiveChatKind::Channel => "channel",
    }
}

fn kind_of(s: &str) -> LiveChatKind {
    match s {
        "group" => LiveChatKind::Group,
        "channel" => LiveChatKind::Channel,
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
        assert_eq!(chats.len(), 3);
        assert_eq!(chats[0].jid, scripted_chats()[0].jid, "most recent first");
        let msgs: Vec<LiveMessage> =
            store.messages(&scripted_chats()[0].jid).unwrap().into_iter().map(|(m, _)| m).collect();
        assert_eq!(msgs, scripted_messages());
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
        // The other chats and their messages are untouched.
        assert_eq!(store.chats().unwrap().len(), 2);
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
}
