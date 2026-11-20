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
//! first, then the file, and a fresh key is generated only when
//! neither has one, so the two sources can never diverge.

use std::path::Path;

use crate::StoreError;

const KEYRING_SERVICE: &str = "zaprust";
const KEYRING_ACCOUNT: &str = "database-key";
const KEY_FILE_NAME: &str = "db.key";

/// Loads the database key, creating and storing one on first use.
///
/// # Errors
/// [`StoreError::Io`] when a fresh key could be generated but
/// neither the keyring nor the key file could store it.
pub fn load_or_create_key(data_dir: &Path) -> Result<[u8; 32], StoreError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).ok();
    if let Some(entry) = &entry {
        if let Ok(saved) = entry.get_password() {
            if let Some(key) = decode_key(&saved) {
                return Ok(key);
            }
        }
    }
    let file = data_dir.join(KEY_FILE_NAME);
    if let Ok(saved) = std::fs::read_to_string(&file) {
        if let Some(key) = decode_key(&saved) {
            return Ok(key);
        }
    }
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
}
