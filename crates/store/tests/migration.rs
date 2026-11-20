//! Store integration tests against real on-disk SQLite files:
//! fresh migration, the v1 upgrade path with data intact,
//! round-trips, and the v7 cleanup of the retired agent tables.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use backend::{LiveChat, LiveChatKind, LiveMessage};
use store::{Store, StoreError};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_db(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("zaprust-store-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn chat(jid: &str, title: &str) -> LiveChat {
    LiveChat {
        jid: jid.to_owned(),
        title: title.to_owned(),
        kind: LiveChatKind::Dm,
        preview: String::new(),
        preview_sender: None,
        timestamp: 100,
        unread: 0,
        archived: false,
        pinned: false,
        ephemeral_secs: 0,
        muted: false,
    }
}

fn message(chat_jid: &str, id: &str, body: &str) -> LiveMessage {
    LiveMessage::text(id, chat_jid, Some("Ana".to_owned()), body, 100, false)
}

fn table_exists(path: &PathBuf, table: &str) -> bool {
    let conn = rusqlite::Connection::open(path).unwrap();
    // The store under test encrypts its files (SEC-F01); every
    // fixture here is opened with the same key, so raw reads in
    // this file must apply it before querying.
    let hex = "02".repeat(32);
    conn.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .unwrap();
    count > 0
}

#[test]
fn fresh_database_migrates_to_current_version() {
    let path = temp_db("fresh.db");
    let store = Store::open(&path, &[2u8; 32]).unwrap();
    assert_eq!(store.schema_version().unwrap(), store::SCHEMA_VERSION);
}

#[test]
fn v1_database_upgrades_with_data_intact() {
    let path = temp_db("v1.db");
    // Build a genuine v1 database with the v1 DDL only.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE chats (
                jid TEXT PRIMARY KEY, title TEXT NOT NULL, kind TEXT NOT NULL,
                preview TEXT NOT NULL DEFAULT '', preview_sender TEXT,
                timestamp INTEGER NOT NULL DEFAULT 0, unread INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE messages (
                chat_jid TEXT NOT NULL, id TEXT NOT NULL, sender TEXT,
                body TEXT NOT NULL, timestamp INTEGER NOT NULL,
                from_me INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (chat_jid, id)
            );
            CREATE INDEX idx_messages_chat_time ON messages (chat_jid, timestamp);
            PRAGMA user_version = 1;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chats VALUES ('123@s.whatsapp.net', 'Ana', 'dm', '', NULL, 100, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages VALUES ('123@s.whatsapp.net', 'm1', 'Ana', 'hello from v1', 100, 0)",
            [],
        )
        .unwrap();
    }
    let store = Store::open(&path, &[2u8; 32]).unwrap();
    assert_eq!(store.schema_version().unwrap(), store::SCHEMA_VERSION);
    let chats = store.chats().unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].title, "Ana");
    let msgs = store.messages("123@s.whatsapp.net").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].0.body, "hello from v1");
    // The retired agent tables are never (re)created on the way up.
    // (Raw file inspection happens after the store is dropped: a
    // second connection cannot open the encrypted WAL database
    // while the store's connection holds it.)
    drop(store);
    assert!(!table_exists(&path, "memory_chunks"));
    assert!(!table_exists(&path, "reminders"));
}

#[test]
fn chat_and_message_round_trip_on_disk() {
    let path = temp_db("roundtrip.db");
    {
        let store = Store::open(&path, &[2u8; 32]).unwrap();
        store.upsert_chat(&chat("123@s.whatsapp.net", "Ana")).unwrap();
        store.upsert_message(&message("123@s.whatsapp.net", "m1", "persisted"), 1).unwrap();
    }
    // Reopen from disk: everything survives.
    let store = Store::open(&path, &[2u8; 32]).unwrap();
    let msgs = store.messages("123@s.whatsapp.net").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].0.body, "persisted");
    assert_eq!(msgs[0].1, 1, "flags bitfield round-trips");
    let json = store.export_json().unwrap();
    assert!(json.contains("persisted"));
}

#[test]
fn v7_drops_the_retired_agent_tables_and_keeps_the_archive() {
    let path = temp_db("v7-drop.db");
    // Build a v6-era archive that still carries the v3 agent
    // tables, with rows in them and a chat that must survive.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE chats (
                jid TEXT PRIMARY KEY, title TEXT NOT NULL, kind TEXT NOT NULL,
                preview TEXT NOT NULL DEFAULT '', preview_sender TEXT,
                timestamp INTEGER NOT NULL DEFAULT 0, unread INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0,
                ephemeral_secs INTEGER NOT NULL DEFAULT 0, muted INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE memory_chunks (
                chat_key TEXT NOT NULL, chunk_id TEXT NOT NULL, text TEXT NOT NULL,
                vector TEXT NOT NULL, created INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (chat_key, chunk_id)
            );
            CREATE TABLE reminders (
                id INTEGER PRIMARY KEY, chat_key TEXT NOT NULL, text TEXT NOT NULL,
                due_unix INTEGER NOT NULL, created_unix INTEGER NOT NULL DEFAULT 0,
                idem_key TEXT UNIQUE
            );
            INSERT INTO chats (jid, title, kind) VALUES ('123@s.whatsapp.net', 'Ana', 'dm');
            INSERT INTO memory_chunks VALUES ('123@s.whatsapp.net', 'm1', 'likes tea', '[0.1]', 100);
            INSERT INTO reminders VALUES (1, '123@s.whatsapp.net', 'call back', 200, 100, 'k1');
            PRAGMA user_version = 6;",
        )
        .unwrap();
    }
    let store = Store::open(&path, &[2u8; 32]).unwrap();
    assert_eq!(store.schema_version().unwrap(), store::SCHEMA_VERSION);
    // The archive itself opens healthy and its rows are untouched.
    let chats = store.chats().unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].title, "Ana");
    drop(store);
    assert!(!table_exists(&path, "memory_chunks"), "memory table dropped");
    assert!(!table_exists(&path, "reminders"), "reminder table dropped");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn v3_database_upgrades_keeping_rows_and_new_columns_default_off() {
    let path = temp_db("v3.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE chats (
            jid TEXT PRIMARY KEY, title TEXT NOT NULL, kind TEXT NOT NULL,
            preview TEXT NOT NULL DEFAULT '', preview_sender TEXT,
            timestamp INTEGER NOT NULL DEFAULT 0, unread INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE messages (
            chat_jid TEXT NOT NULL, id TEXT NOT NULL, sender TEXT,
            body TEXT NOT NULL, timestamp INTEGER NOT NULL,
            from_me INTEGER NOT NULL DEFAULT 0, data TEXT,
            flags INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (chat_jid, id));
         INSERT INTO chats (jid, title, kind) VALUES ('1@s.whatsapp.net', 'Ana', 'dm');
         PRAGMA user_version = 3;",
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &[2u8; 32]).unwrap();
    assert_eq!(store.schema_version().unwrap(), store::SCHEMA_VERSION);
    let chats = store.chats().unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].title, "Ana");
    assert!(!chats[0].archived && !chats[0].pinned);
    // The new columns are writable through the normal upsert path.
    let mut updated = chats[0].clone();
    updated.archived = true;
    store.upsert_chat(&updated).unwrap();
    assert!(store.chats().unwrap()[0].archived);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn v5_ephemeral_state_round_trips_and_messages_delete() {
    let path = temp_db("v5-ephemeral.db");
    let store = Store::open(&path, &[2u8; 32]).unwrap();
    assert_eq!(store.schema_version().unwrap(), store::SCHEMA_VERSION);
    // A chat with a disappearing timer and one expiring message.
    let mut c = chat("123@s.whatsapp.net", "Ana");
    c.ephemeral_secs = 604_800;
    store.upsert_chat(&c).unwrap();
    let mut m = message("123@s.whatsapp.net", "m1", "gone in a week");
    m.expires_at = 1_700_604_800;
    store.upsert_message(&m, 0).unwrap();
    store.upsert_message(&message("123@s.whatsapp.net", "m2", "stays"), 0).unwrap();

    let chats = store.chats().unwrap();
    assert_eq!(chats[0].ephemeral_secs, 604_800);
    let msgs = store.messages("123@s.whatsapp.net").unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].0.expires_at, 1_700_604_800);
    assert_eq!(msgs[1].0.expires_at, 0);

    // The expiry pass deletes by id; a second delete is a no-op.
    assert!(store.delete_message("123@s.whatsapp.net", "m1").unwrap());
    assert!(!store.delete_message("123@s.whatsapp.net", "m1").unwrap());
    let msgs = store.messages("123@s.whatsapp.net").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].0.id, "m2");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn v6_mute_state_round_trips_and_defaults_off() {
    let path = temp_db("v6-mute.db");
    let store = Store::open(&path, &[2u8; 32]).unwrap();
    assert_eq!(store.schema_version().unwrap(), store::SCHEMA_VERSION);
    let mut c = chat("123@s.whatsapp.net", "Ana");
    c.muted = true;
    store.upsert_chat(&c).unwrap();
    store.upsert_chat(&chat("456@s.whatsapp.net", "Ben")).unwrap();
    let chats = store.chats().unwrap();
    let ana = chats.iter().find(|c| c.jid == "123@s.whatsapp.net").unwrap();
    let ben = chats.iter().find(|c| c.jid == "456@s.whatsapp.net").unwrap();
    assert!(ana.muted, "a muted chat reloads muted");
    assert!(!ben.muted, "mute defaults off");
    // Unmuting persists too.
    let mut un = ana.clone();
    un.muted = false;
    store.upsert_chat(&un).unwrap();
    assert!(!store.chats().unwrap().iter().find(|c| c.jid == ana.jid).unwrap().muted);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn negative_schema_version_is_a_typed_error_not_a_panic() {
    let path = temp_db("negative-version");
    let conn = rusqlite::Connection::open(&path).expect("create db");
    conn.pragma_update(None, "user_version", -1).expect("set version");
    drop(conn);
    let Err(err) = Store::open(&path, &[2u8; 32]) else {
        panic!("a negative version must be rejected")
    };
    assert!(
        matches!(err, StoreError::BadSchemaVersion(-1)),
        "typed BadSchemaVersion expected, got {err}"
    );
}

#[test]
fn newer_schema_version_is_refused() {
    let path = temp_db("future-version");
    let conn = rusqlite::Connection::open(&path).expect("create db");
    conn.pragma_update(None, "user_version", 99).expect("set version");
    drop(conn);
    let Err(err) = Store::open(&path, &[2u8; 32]) else {
        panic!("a future version must be refused")
    };
    assert!(matches!(err, StoreError::SchemaTooNew { found: 99, .. }), "got {err}");
}
