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
pub const SCHEMA_VERSION: i64 = 7;

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

impl Store {
    /// Opens (or creates) the encrypted archive at `path` and runs
    /// migrations. A plaintext archive left by an older build is
    /// converted in place first (see [`encrypt_file_in_place`]).
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
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// The schema version stored in the database file.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Applies migrations until the schema reaches
    /// [`SCHEMA_VERSION`]. Idempotent.
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
                    tx.execute_batch(
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
                        CREATE INDEX idx_messages_chat_time
                            ON messages (chat_jid, timestamp);",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 1;")?;
                }
                1 => {
                    // v2: rich payloads (kind/reactions/quote/preview
                    // as JSON) + a flags bitfield (AI-generated,
                    // deleted-by-sender, deleted).
                    tx.execute_batch(
                        "ALTER TABLE messages ADD COLUMN data TEXT;
                         ALTER TABLE messages ADD COLUMN flags INTEGER NOT NULL DEFAULT 0;",
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
                    tx.execute_batch(
                        "ALTER TABLE chats ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
                         ALTER TABLE chats ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 4;")?;
                }
                4 => {
                    // v5: disappearing messages. The per-chat
                    // timer lives on the chat row; each message
                    // carries the unix second at which it expires
                    // (0 = never), computed by the backend from
                    // the wire expiration.
                    tx.execute_batch(
                        "ALTER TABLE chats ADD COLUMN ephemeral_secs INTEGER NOT NULL DEFAULT 0;
                         ALTER TABLE messages ADD COLUMN expires_at INTEGER NOT NULL DEFAULT 0;",
                    )?;
                    tx.execute_batch("PRAGMA user_version = 5;")?;
                }
                5 => {
                    // v6: chat mute joins archive and pin in the
                    // archive, so a muted chat stays muted across
                    // restarts (it previously lived only in the
                    // running app's memory).
                    tx.execute_batch(
                        "ALTER TABLE chats ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;",
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
        self.conn.execute(
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
    /// existed. The disappearing-messages expiry pass uses this
    /// so an expired message leaves the archive together with
    /// the conversation.
    ///
    /// # Errors
    /// [`StoreError`] on SQLite failures.
    pub fn delete_message(&self, chat_jid: &str, id: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM messages WHERE chat_jid = ?1 AND id = ?2",
            params![chat_jid, id],
        )?;
        Ok(changed > 0)
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
