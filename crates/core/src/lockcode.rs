//! The local lock code: the code that gates the locked-chats
//! folder and the app lock screen.
//!
//! The code itself is never stored. What persists (in the
//! app's settings document) is a random salt and an iterated
//! SHA-256 digest of salt + code; verification recomputes
//! the digest. The iteration count makes a casual offline
//! guess against the settings file slow without making an
//! interactive unlock measurably wait.
//!
//! Honesty bounds, stated wherever the code is offered: this
//! is a window gate, not encryption. It stops a person with
//! casual access to the open window from reading locked
//! chats or the app behind it. It does not encrypt the chat
//! archive (the archive carries its own key), it does not
//! sync to the phone or to other devices (the chat-lock
//! protocol state syncs; no key ever leaves the phone), and
//! anyone who can delete the settings file from the data
//! directory can remove the gate along with it.

use sha2::{Digest, Sha256};

/// The shortest code the app accepts when one is created.
pub const MIN_CODE_LEN: usize = 4;

/// Digest iterations for a stored code. Twenty thousand
/// SHA-256 rounds over a short input cost a few milliseconds
/// interactively and roughly that per guess offline.
const ITERATIONS: u32 = 20_000;

/// Salt length in bytes (stored hex-encoded, 32 chars).
const SALT_BYTES: usize = 16;

/// A stored lock code: the salt and the iterated digest,
/// both hex strings, exactly as the settings document
/// carries them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LockCode {
    /// Hex-encoded random salt, unique per code.
    salt: String,
    /// Hex-encoded iterated SHA-256 digest of the code.
    digest: String,
}

/// Computes the iterated digest of `code` under `salt`.
fn digest_of(salt: &[u8], code: &str) -> [u8; 32] {
    let mut state = Sha256::new();
    state.update(salt);
    state.update(code.as_bytes());
    let mut out: [u8; 32] = state.finalize().into();
    // The remaining rounds chain the digest alone: cheap to
    // compute in bulk only one guess at a time, which is the
    // point.
    for _ in 1..ITERATIONS {
        let mut next = Sha256::new();
        next.update(out);
        next.update(salt);
        out = next.finalize().into();
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

impl LockCode {
    /// Creates a stored code for `code` with a fresh random
    /// salt.
    ///
    /// # Panics
    /// Only if the OS randomness source fails, which the
    /// store's key generation already treats as fatal at
    /// first run; a machine that cannot produce randomness
    /// cannot run the archive either.
    #[must_use]
    pub fn new(code: &str) -> Self {
        let mut salt = [0u8; SALT_BYTES];
        getrandom::fill(&mut salt).expect("OS randomness is required to salt a lock code");
        Self::with_salt(code, &salt)
    }

    /// Creates a stored code under a caller-supplied salt
    /// (the deterministic half, split out for tests).
    #[must_use]
    pub fn with_salt(code: &str, salt: &[u8]) -> Self {
        Self { salt: hex(salt), digest: hex(&digest_of(salt, code)) }
    }

    /// Rebuilds a stored code from its persisted parts.
    /// Returns `None` on malformed input (a hand-edited
    /// settings file then simply has no code, the state the
    /// app treats as "no code set" everywhere).
    #[must_use]
    pub fn from_parts(salt: &str, digest: &str) -> Option<Self> {
        let salt_bytes = unhex(salt)?;
        let digest_bytes = unhex(digest)?;
        if salt_bytes.is_empty() || digest_bytes.len() != 32 {
            return None;
        }
        Some(Self { salt: salt.to_owned(), digest: digest.to_owned() })
    }

    /// The persisted salt (hex), for the settings document.
    #[must_use]
    pub fn salt(&self) -> &str {
        &self.salt
    }

    /// The persisted digest (hex), for the settings document.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Whether `code` is the code this stored value was made
    /// from. An empty candidate never verifies: an unset code
    /// field must not open the gate by accident.
    #[must_use]
    pub fn verify(&self, code: &str) -> bool {
        if code.is_empty() {
            return false;
        }
        let Some(salt) = unhex(&self.salt) else {
            return false;
        };
        // Constant-time over the digest bytes: both sides are
        // always 32 bytes here, and a length difference is a
        // malformed stored value, not a timing signal.
        let candidate = digest_of(&salt, code);
        let Some(stored) = unhex(&self.digest) else {
            return false;
        };
        if stored.len() != candidate.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in candidate.iter().zip(stored.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_verifies_and_a_wrong_one_does_not() {
        let stored = LockCode::with_salt("481516", &[7u8; SALT_BYTES]);
        assert!(stored.verify("481516"));
        assert!(!stored.verify("481517"));
        assert!(!stored.verify(""));
        assert!(!stored.verify("4815162342"));
    }

    #[test]
    fn the_salt_changes_the_digest() {
        let a = LockCode::with_salt("1234", &[1u8; SALT_BYTES]);
        let b = LockCode::with_salt("1234", &[2u8; SALT_BYTES]);
        assert_ne!(a.digest(), b.digest(), "same code, different salt, different digest");
        assert!(a.verify("1234") && b.verify("1234"));
    }

    #[test]
    fn parts_round_trip_and_garbage_is_no_code() {
        let stored = LockCode::new("9999");
        let rebuilt = LockCode::from_parts(stored.salt(), stored.digest()).unwrap();
        assert_eq!(rebuilt, stored);
        assert!(rebuilt.verify("9999"));
        assert!(LockCode::from_parts("zz", stored.digest()).is_none());
        assert!(LockCode::from_parts(stored.salt(), "abcd").is_none());
        assert!(LockCode::from_parts("", "").is_none());
    }

    #[test]
    fn fresh_codes_get_fresh_salts() {
        let a = LockCode::new("1234");
        let b = LockCode::new("1234");
        assert_ne!(a.salt(), b.salt());
        assert_ne!(a.digest(), b.digest());
    }
}
