//! The database encryption key (SEC-F01).
//!
//! One random 32-byte key encrypts both local databases (this
//! archive and the protocol session store) through SQLCipher. The
//! key itself lives in the OS keyring (Secret Service, Keychain,
//! or Credential Manager, via the `keyring` crate), so copying the
//! database files on their own yields nothing readable.
//!
//! When no keyring is available (a headless session, a locked
//! keychain), the key falls back to an owner-only file `db.key`
//! inside the data directory. That fallback is weaker by design
//! and says so: it still protects a database file copied on its
//! own, but not a copy of the whole data directory. Which source
//! provided the key is not recorded; lookup order is keyring
//! first, then the file. A fresh key is generated only when
//! neither source has one AND no database exists yet: if a
//! database is already on disk, its key existed once and is now
//! lost, and minting a replacement would strand the data while
//! looking like a fresh install, so the load fails instead.

use std::path::{Path, PathBuf};

use crate::StoreError;

const KEYRING_SERVICE: &str = "zaprust";
const KEYRING_ACCOUNT: &str = "database-key";
const KEY_FILE_NAME: &str = "db.key";

/// Loads the database key, creating and storing one on first use.
///
/// A fresh key is created only when both sources affirmatively
/// have none and no database exists yet. A source that exists but
/// cannot be read (a locked or unavailable keyring, an unreadable
/// or malformed key file) is an error, never a reason to invent a
/// replacement: a new key would leave every database encrypted
/// under the stored one unreadable. The same applies when both
/// sources are empty but a database is already on disk: its key
/// was lost (the keyring entry removed or reset, the key file
/// deleted), and a replacement would strand the data.
///
/// # Errors
/// [`StoreError::Io`] when a stored key could not be read, when a
/// database exists but its key is missing from both sources, or
/// when a fresh key could be generated but neither the keyring
/// nor the key file could store it.
pub fn load_or_create_key(data_dir: &Path) -> Result<[u8; 32], StoreError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).ok();
    if let Some(entry) = &entry {
        match entry.get_password() {
            Ok(saved) => {
                if let Some(key) = decode_key(&saved) {
                    return Ok(key);
                }
                // An entry exists but does not hold a key this
                // build wrote; the key file is the second source,
                // so fall through to it before concluding anything.
            }
            // The keyring affirmatively holds no entry: the key,
            // if there is one, is in the file.
            Err(keyring::Error::NoEntry) => {}
            Err(e) => {
                return Err(StoreError::Io(std::io::Error::other(format!(
                    "the database key could not be read from the system keyring: {e}"
                ))));
            }
        }
    }
    let file = data_dir.join(KEY_FILE_NAME);
    if let Some(key) = read_key_file(&file)? {
        return Ok(key);
    }
    refuse_to_replace_a_missing_key(data_dir)?;
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
    let encoded = encode_key(&key);
    let in_keyring = entry.as_ref().and_then(|e| e.set_password(&encoded).ok()).is_some();
    if !in_keyring {
        std::fs::create_dir_all(data_dir).map_err(StoreError::Io)?;
        std::fs::write(&file, &encoded).map_err(StoreError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(key)
}

/// Reads the key stored in the key file.
///
/// `Ok(None)` means no key file exists. A file that exists but
/// cannot be read, or whose contents are not a key, is an error:
/// a key was stored there once, and replacing it with a fresh one
/// would strand the databases it encrypts.
fn read_key_file(file: &Path) -> Result<Option<[u8; 32]>, StoreError> {
    match std::fs::read_to_string(file) {
        Ok(saved) => match decode_key(&saved) {
            Some(key) => Ok(Some(key)),
            None => Err(StoreError::Io(std::io::Error::other(format!(
                "the database key file {} exists but does not contain a valid key",
                file.display()
            )))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Io(e)),
    }
}

/// Fails when a database the key protects already exists in
/// `data_dir`.
///
/// Both key sources have affirmatively reported no key. On a
/// first install that is normal, and the caller goes on to create
/// one. But when a database file is already on disk, a key
/// existed once and is now lost: minting a replacement would
/// render the existing data unreadable while looking like a
/// fresh install, so the load fails closed instead.
fn refuse_to_replace_a_missing_key(data_dir: &Path) -> Result<(), StoreError> {
    if let Some(db) = existing_database(data_dir) {
        return Err(StoreError::Io(std::io::Error::other(format!(
            "saved data exists on this device ({}), but its unlock key is missing: \
             it is not in the system keyring and no key file remains, so the data \
             cannot be opened. No new key was created and nothing was changed",
            db.display()
        ))));
    }
    Ok(())
}

/// The first existing database file the key from this module
/// protects: the chat archive in `data_dir`, or any account's
/// session store under `data_dir/accounts/`. Both locations
/// derive from `data_dir` by the workspace's fixed layout, so
/// every caller of [`load_or_create_key`] is covered without
/// passing paths around.
fn existing_database(data_dir: &Path) -> Option<PathBuf> {
    let archive = data_dir.join("archive.db");
    if database_file_present(&archive) {
        return Some(archive);
    }
    let accounts = data_dir.join("accounts");
    match std::fs::read_dir(&accounts) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("session.db"))
            .find(|path| database_file_present(path)),
        // A missing accounts directory means no session store was
        // ever created. Any other failure leaves the question
        // open, so report the directory itself: failing closed
        // beats minting a key over data that may exist.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(accounts),
    }
}

/// Whether `path` is an existing file holding bytes. A zero-length
/// leftover of an interrupted first run holds no data a new key
/// could strand, so it does not count. A metadata failure other
/// than "not there" counts as present: when the answer cannot be
/// proven to be "no database", no key may be minted.
fn database_file_present(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.len() > 0,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// The SQLCipher raw-key pragma value for a key: `x'<hex>'`.
pub(crate) fn key_pragma(key: &[u8; 32]) -> String {
    format!("x'{}'", encode_key(key))
}

/// Lowercase hex, the storage form of the key everywhere it is
/// written (keyring, key file, SQL pragmas).
fn encode_key(key: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for b in key {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Parses a stored key back; anything malformed is ignored, never
/// an error, so a corrupt entry falls through to the next source.
fn decode_key(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zaprust-dbkey-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_file_missing_is_no_key_not_an_error() {
        let dir = scratch_dir("missing");
        assert_eq!(read_key_file(&dir.join(KEY_FILE_NAME)).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_file_roundtrips_a_stored_key() {
        let dir = scratch_dir("stored");
        let key: [u8; 32] = std::array::from_fn(|i| (i * 3) as u8);
        std::fs::write(dir.join(KEY_FILE_NAME), encode_key(&key)).unwrap();
        assert_eq!(read_key_file(&dir.join(KEY_FILE_NAME)).unwrap(), Some(key));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_file_that_is_not_a_key_fails_closed() {
        // A file exists, so a key was stored here; inventing a
        // replacement would strand the databases it encrypts.
        let dir = scratch_dir("corrupt");
        std::fs::write(dir.join(KEY_FILE_NAME), "not a key").unwrap();
        assert!(read_key_file(&dir.join(KEY_FILE_NAME)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_file_that_cannot_be_read_fails_closed() {
        // A directory at the key path errors on read with
        // something other than NotFound.
        let dir = scratch_dir("unreadable");
        std::fs::create_dir(dir.join(KEY_FILE_NAME)).unwrap();
        assert!(read_key_file(&dir.join(KEY_FILE_NAME)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_codec_roundtrips() {
        let key: [u8; 32] = std::array::from_fn(|i| i as u8);
        let encoded = encode_key(&key);
        assert_eq!(encoded.len(), 64);
        assert_eq!(decode_key(&encoded), Some(key));
        assert_eq!(decode_key("  \n"), None);
        assert_eq!(decode_key(&encoded[..63]), None);
        assert_eq!(key_pragma(&key), format!("x'{encoded}'"));
    }

    #[test]
    fn fresh_key_guard_allows_a_dir_without_databases() {
        // First install: no key anywhere and nothing on disk, so
        // creating a key must proceed.
        let dir = scratch_dir("fresh");
        assert!(refuse_to_replace_a_missing_key(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_key_guard_refuses_over_an_existing_archive() {
        let dir = scratch_dir("archive-exists");
        std::fs::write(dir.join("archive.db"), b"encrypted archive bytes").unwrap();
        let err = refuse_to_replace_a_missing_key(&dir).unwrap_err();
        assert!(err.to_string().contains("unlock key is missing"), "error explains itself: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_key_guard_refuses_over_an_existing_session_db() {
        let dir = scratch_dir("session-exists");
        let account = dir.join("accounts").join("default");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("session.db"), b"encrypted session bytes").unwrap();
        assert!(refuse_to_replace_a_missing_key(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_key_guard_ignores_an_empty_database_file() {
        // A zero-length leftover of an interrupted first run holds
        // no data a new key could strand.
        let dir = scratch_dir("empty-db");
        std::fs::write(dir.join("archive.db"), b"").unwrap();
        assert!(refuse_to_replace_a_missing_key(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_key_with_existing_archive_fails_closed() {
        // End to end through the loader (the keyring holds no
        // entry for this service on a test machine): the load
        // errors and creates nothing, no key file, no key.
        let dir = scratch_dir("stranded");
        std::fs::write(dir.join("archive.db"), b"encrypted archive bytes").unwrap();
        assert!(load_or_create_key(&dir).is_err());
        assert!(!dir.join(KEY_FILE_NAME).exists(), "no replacement key file may appear");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stored_key_file_still_loads_when_databases_exist() {
        // A key in a source is found before the guard is ever
        // consulted: the load returns it and leaves it untouched.
        let dir = scratch_dir("key-and-data");
        let key: [u8; 32] = std::array::from_fn(|i| (i * 7) as u8);
        std::fs::write(dir.join(KEY_FILE_NAME), encode_key(&key)).unwrap();
        std::fs::write(dir.join("archive.db"), b"encrypted archive bytes").unwrap();
        assert_eq!(load_or_create_key(&dir).unwrap(), key);
        assert_eq!(
            std::fs::read_to_string(dir.join(KEY_FILE_NAME)).unwrap(),
            encode_key(&key),
            "the stored key file is not rewritten"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
