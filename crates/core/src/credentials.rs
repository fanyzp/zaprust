//! Feature credentials: one central store, owned by the host.
//!
//! Native features declare the credential *slots* they need; they
//! never ask the user for a key and never store one themselves. The
//! host keeps the values and hands a feature a key from the slot's
//! pool on demand.
//!
//! A slot can hold several keys. The pool hands them out
//! round-robin, cools a key down after an HTTP 429, and marks a key
//! bad (skipped until replaced) after a 401/403. All clock input is
//! injected (`now`, seconds) so behavior is deterministic in tests.
//!
//! Storage backends: the OS keyring when available (feature
//! `keyring-store`), otherwise a JSON file under the app data dir
//! with `0600` permissions on Unix. Environment variables
//! (`ZAPRUST_CRED_<SLOT>`, comma-separated) override both for
//! development. Values are never written to the archive database,
//! never logged, and never part of the repository.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What kind of credential a slot holds. Only API keys exist today;
/// the kind exists so the UI can label fields honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// An API key / bearer token string.
    ApiKey,
}

impl CredentialKind {
    /// Parses the manifest spelling (`api_key`).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "api_key" => Some(Self::ApiKey),
            _ => None,
        }
    }

    /// The manifest spelling.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
        }
    }

    /// Human label for settings UIs.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::ApiKey => "API key",
        }
    }
}

/// One credential slot a native feature declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialSlot {
    /// Slot id, unique per feature by convention (e.g. `example_api_key`).
    pub id: String,
    /// Human label shown in Settings (e.g. "AI provider API key").
    pub label: String,
    /// Credential kind.
    pub kind: CredentialKind,
}

/// Health of one key inside a pool.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyHealth {
    /// Usable.
    Ready,
    /// Cooling down until this timestamp (seconds) after a 429.
    CooldownUntil(u64),
    /// Rejected by the provider (401/403); skipped until replaced.
    Bad,
}

/// A pool of keys for one slot, with rotation and health tracking.
///
/// Manual `Debug` (SEC-20): a pool holds raw secret values, so its
/// debug form shows only how many keys it holds, never a value.
#[derive(Clone, Default)]
pub struct KeyPool {
    keys: Vec<String>,
    health: Vec<KeyHealth>,
    cursor: usize,
}

impl std::fmt::Debug for KeyPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPool")
            .field("key_count", &self.keys.len())
            .field("keys", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl KeyPool {
    /// The raw keys in this pool (for building a sibling pool over
    /// the same keys; health state is intentionally not shared).
    #[must_use]
    pub fn keys(&self) -> &[String] {
        &self.keys
    }
}

/// Default cooldown after a rate-limit response, in seconds.
pub const RATE_LIMIT_COOLDOWN_SECS: u64 = 60;

impl KeyPool {
    /// Builds a pool from stored key values (order preserved).
    #[must_use]
    pub fn new(keys: Vec<String>) -> Self {
        let health = vec![KeyHealth::Ready; keys.len()];
        Self { keys, health, cursor: 0 }
    }

    /// Number of keys in the pool.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the pool holds no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// How many keys are currently usable at `now`.
    #[must_use]
    pub fn usable_count(&self, now: u64) -> usize {
        self.health
            .iter()
            .filter(|h| {
                matches!(h, KeyHealth::Ready)
                    || matches!(h, KeyHealth::CooldownUntil(t) if *t <= now)
            })
            .count()
    }

    /// The stored values (host-side only; the UI masks them).
    #[must_use]
    pub fn values(&self) -> &[String] {
        &self.keys
    }

    /// Next usable key, round-robin. `None` when every key is bad or
    /// cooling down. The cursor advances past the returned key so
    /// successive calls rotate.
    pub fn acquire(&mut self, now: u64) -> Option<&str> {
        if self.keys.is_empty() {
            return None;
        }
        for step in 0..self.keys.len() {
            let idx = (self.cursor + step) % self.keys.len();
            let usable = match self.health[idx] {
                KeyHealth::Ready => true,
                KeyHealth::CooldownUntil(t) => t <= now,
                KeyHealth::Bad => false,
            };
            if usable {
                self.cursor = (idx + 1) % self.keys.len();
                return Some(&self.keys[idx]);
            }
        }
        None
    }

    /// Records the outcome of a request made with `key`:
    /// 2xx clears any cooldown, 429 starts a cooldown, 401/403 marks
    /// the key bad. Other statuses are ignored.
    pub fn report_http_status(&mut self, key: &str, status: u16, now: u64) {
        let Some(idx) = self.keys.iter().position(|k| k == key) else {
            return;
        };
        match status {
            200..=299 => self.health[idx] = KeyHealth::Ready,
            429 => self.health[idx] = KeyHealth::CooldownUntil(now + RATE_LIMIT_COOLDOWN_SECS),
            401 | 403 => self.health[idx] = KeyHealth::Bad,
            _ => {}
        }
    }

    /// Whether `key` is marked bad.
    #[must_use]
    pub fn is_bad(&self, key: &str) -> bool {
        self.keys.iter().position(|k| k == key).is_some_and(|i| self.health[i] == KeyHealth::Bad)
    }
}

/// Masks a secret for display: first 3 + last 2 characters survive.
#[must_use]
pub fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 6 {
        return "••••••".to_string();
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars.iter().skip(chars.len() - 2).collect();
    format!("{head}••••{tail}")
}

/// Errors from credential storage backends.
#[derive(Debug)]
pub enum CredentialError {
    /// Filesystem failure (message only; never includes a value).
    Io(String),
    /// Stored data could not be parsed.
    Corrupt(String),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "credential store I/O error: {m}"),
            Self::Corrupt(m) => write!(f, "credential store is corrupt: {m}"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// A credential storage backend. One implementation is active at a
/// time (keyring preferred, file as fallback; environment variables
/// are layered on top by [`CredentialStore::keys_for`]).
pub trait CredentialBackend {
    /// All stored slots and their key values.
    fn load_all(&self) -> Result<BTreeMap<String, Vec<String>>, CredentialError>;
    /// Persists the full slot map.
    fn save_all(&self, slots: &BTreeMap<String, Vec<String>>) -> Result<(), CredentialError>;
    /// Short backend name for the UI ("OS keyring", "Local file").
    fn name(&self) -> &'static str;
}

/// In-memory backend (tests, ephemeral sessions).
///
/// Manual `Debug` (SEC-20): the map holds raw secret values, so its
/// debug form shows only slot names and per-slot key counts.
#[derive(Default)]
pub struct MemoryBackend {
    slots: std::sync::Mutex<BTreeMap<String, Vec<String>>>,
}

impl std::fmt::Debug for MemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.slots.lock() {
            Ok(slots) => {
                let counts: BTreeMap<&str, usize> =
                    slots.iter().map(|(slot, keys)| (slot.as_str(), keys.len())).collect();
                f.debug_struct("MemoryBackend")
                    .field("slot_key_counts", &counts)
                    .field("values", &"<redacted>")
                    .finish()
            }
            Err(_) => f.debug_struct("MemoryBackend").field("values", &"<locked>").finish(),
        }
    }
}

impl CredentialBackend for MemoryBackend {
    fn load_all(&self) -> Result<BTreeMap<String, Vec<String>>, CredentialError> {
        Ok(self.slots.lock().expect("memory backend mutex").clone())
    }

    fn save_all(&self, slots: &BTreeMap<String, Vec<String>>) -> Result<(), CredentialError> {
        *self.slots.lock().expect("memory backend mutex") = slots.clone();
        Ok(())
    }

    fn name(&self) -> &'static str {
        "In-memory (not persisted)"
    }
}

/// File backend: one JSON document under the app data dir. On Unix
/// the file is created with `0600` permissions, it holds secrets.
#[derive(Debug, Clone)]
pub struct FileBackend {
    path: PathBuf,
}

impl FileBackend {
    /// A file backend rooted at `path` (e.g. `<data dir>/secrets.json`).
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The file this backend writes to.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl CredentialBackend for FileBackend {
    fn load_all(&self) -> Result<BTreeMap<String, Vec<String>>, CredentialError> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let text =
            std::fs::read_to_string(&self.path).map_err(|e| CredentialError::Io(e.to_string()))?;
        serde_json::from_str(&text).map_err(|e| CredentialError::Corrupt(e.to_string()))
    }

    fn save_all(&self, slots: &BTreeMap<String, Vec<String>>) -> Result<(), CredentialError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CredentialError::Io(e.to_string()))?;
        }
        let text = serde_json::to_string_pretty(slots)
            .map_err(|e| CredentialError::Corrupt(e.to_string()))?;
        write_secret_file(&self.path, &text)
    }

    fn name(&self) -> &'static str {
        "Local secrets file"
    }
}

fn write_secret_file(path: &std::path::Path, text: &str) -> Result<(), CredentialError> {
    // SEC-19: never truncate the live secrets file in place. Write
    // the full contents to a fresh temp file in the SAME directory
    // (0600 from creation on Unix, before any secret bytes land in
    // it), fsync it, then rename over the target. The rename is
    // atomic on the same filesystem, a crash mid-write leaves the
    // old file intact, and a planted symlink at the target path is
    // replaced by the rename rather than followed.
    use std::io::Write;
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("secrets.json");
    let counter = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_path = parent.join(format!(".{file_name}.tmp-{}-{counter}", std::process::id()));

    let write_temp = |temp_path: &std::path::Path| -> Result<(), CredentialError> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(temp_path).map_err(|e| CredentialError::Io(e.to_string()))?;
        #[cfg(unix)]
        {
            // Belt and braces: the mode above applies because the
            // file is created new, but assert 0600 explicitly before
            // writing any secret bytes.
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| CredentialError::Io(e.to_string()))?;
        }
        file.write_all(text.as_bytes()).map_err(|e| CredentialError::Io(e.to_string()))?;
        file.sync_all().map_err(|e| CredentialError::Io(e.to_string()))?;
        Ok(())
    };

    if let Err(e) = write_temp(&temp_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(CredentialError::Io(e.to_string()));
    }
    // Fsync the directory so the rename itself is durable.
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Keyring service the credential entries live under.
#[cfg(feature = "keyring-store")]
const KEYRING_SERVICE: &str = "zaprust-credentials";

/// Keyring service from before the rename (the app shipped as
/// Zaplite until 1.0.0). Entries saved by the old build are adopted
/// on first read so saved API keys are not orphaned by the rename.
#[cfg(feature = "keyring-store")]
const LEGACY_KEYRING_SERVICE: &str = "zaplite-credentials";

/// OS keyring backend (feature `keyring-store`): one entry per slot,
/// values joined as a JSON array under the `zaprust-credentials`
/// service.
#[cfg(feature = "keyring-store")]
pub struct KeyringBackend;

#[cfg(feature = "keyring-store")]
impl CredentialBackend for KeyringBackend {
    fn load_all(&self) -> Result<BTreeMap<String, Vec<String>>, CredentialError> {
        // The keyring API has no enumeration, so an index entry
        // (`__slots__`, a JSON list of slot ids) tracks what exists.
        // The index read falls back to the pre-rename service, and
        // adopted slots are written through under the new service
        // (see LEGACY_KEYRING_SERVICE).
        let Some(ids) =
            Self::get_slot("__slots__").or_else(|| Self::adopt_legacy_slot("__slots__"))
        else {
            return Ok(BTreeMap::new());
        };
        let mut out = BTreeMap::new();
        for id in ids {
            if id != "__slots__" {
                let keys = Self::get_slot(&id).or_else(|| Self::adopt_legacy_slot(&id));
                if let Some(keys) = keys {
                    out.insert(id, keys);
                }
            }
        }
        Ok(out)
    }

    fn save_all(&self, slots: &BTreeMap<String, Vec<String>>) -> Result<(), CredentialError> {
        // SEC-08: mirror the file backend, whose whole-map rewrite
        // makes removed keys disappear. The keyring has no such
        // rewrite, so slots that were indexed before but are gone
        // now (or have been emptied, e.g. by `remove_key` dropping
        // the last key of a slot) must be DELETED explicitly, or
        // their secret values would stay in the OS keyring forever
        // after the user removed them.
        let previous = Self::get_slot("__slots__").unwrap_or_default();
        for old in &previous {
            if old != "__slots__" && slots.get(old).is_none_or(std::vec::Vec::is_empty) {
                Self::delete_slot(old)?;
            }
        }
        let mut ids: Vec<String> = Vec::new();
        for (slot, keys) in slots {
            if keys.is_empty() {
                // An emptied slot stores nothing (also covers a
                // slot that was never in the old index).
                Self::delete_slot(slot)?;
            } else {
                Self::set_slot(slot, keys)?;
                ids.push(slot.clone());
            }
        }
        Self::set_slot("__slots__", &ids)
    }

    fn name(&self) -> &'static str {
        "OS keyring"
    }
}

#[cfg(feature = "keyring-store")]
impl KeyringBackend {
    /// Reads one slot from the keyring (`None` = absent/unavailable).
    #[must_use]
    pub(crate) fn get_slot(slot: &str) -> Option<Vec<String>> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, slot).ok()?;
        let raw = entry.get_password().ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Reads one slot from the pre-rename keyring service and, when
    /// found, writes it through under the current service so the
    /// adoption happens once. `None` = absent/unavailable.
    fn adopt_legacy_slot(slot: &str) -> Option<Vec<String>> {
        let entry = keyring::Entry::new(LEGACY_KEYRING_SERVICE, slot).ok()?;
        let raw = entry.get_password().ok()?;
        let keys: Vec<String> = serde_json::from_str(&raw).ok()?;
        let _ = Self::set_slot(slot, &keys);
        Some(keys)
    }

    /// Writes one slot to the keyring.
    pub(crate) fn set_slot(slot: &str, keys: &[String]) -> Result<(), CredentialError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, slot)
            .map_err(|e| CredentialError::Io(e.to_string()))?;
        let raw =
            serde_json::to_string(keys).map_err(|e| CredentialError::Corrupt(e.to_string()))?;
        entry.set_password(&raw).map_err(|e| CredentialError::Io(e.to_string()))
    }

    /// Deletes one slot from the keyring (SEC-08). The vendored
    /// keyring 3.x exposes deletion as `Entry::delete_credential`;
    /// an already-absent entry is success, not an error.
    pub(crate) fn delete_slot(slot: &str) -> Result<(), CredentialError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, slot)
            .map_err(|e| CredentialError::Io(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CredentialError::Io(e.to_string())),
        }
    }

    /// Whether the keyring answers at all on this machine.
    #[must_use]
    pub fn available() -> bool {
        keyring::Entry::new(KEYRING_SERVICE, "__probe__").is_ok()
    }
}

/// The host-side credential store: declaration registry + pools +
/// persistence, with environment overrides for development.
pub struct CredentialStore {
    backend: Box<dyn CredentialBackend + Send>,
    declared: Vec<(String, CredentialSlot)>,
    pools: BTreeMap<String, KeyPool>,
    overrides: BTreeMap<String, String>,
}

impl CredentialStore {
    /// Opens a store over `backend`, loading any persisted keys.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError`] when the backend cannot be read.
    pub fn open(backend: Box<dyn CredentialBackend + Send>) -> Result<Self, CredentialError> {
        let stored = backend.load_all()?;
        let pools = stored.into_iter().map(|(slot, keys)| (slot, KeyPool::new(keys))).collect();
        Ok(Self { backend, declared: Vec::new(), pools, overrides: BTreeMap::new() })
    }

    /// Sets `.env.local`-style overrides (variable name → value).
    /// Real environment variables take precedence over these.
    pub fn set_overrides(&mut self, overrides: BTreeMap<String, String>) {
        self.overrides = overrides;
    }

    /// The active backend's display name.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Registers a feature's declared slots (idempotent per feature).
    pub fn declare(&mut self, owner: &str, slots: &[CredentialSlot]) {
        for slot in slots {
            let entry = (owner.to_string(), slot.clone());
            if !self.declared.contains(&entry) {
                self.declared.push(entry);
            }
            self.pools.entry(slot.id.clone()).or_default();
        }
    }

    /// Every declared slot with the feature that declared it.
    #[must_use]
    pub fn declared_slots(&self) -> &[(String, CredentialSlot)] {
        &self.declared
    }

    /// The pool for a slot, if the slot is known.
    #[must_use]
    pub fn pool(&self, slot: &str) -> Option<&KeyPool> {
        self.pools.get(slot)
    }

    /// Keys for a slot: environment override first
    /// (`ZAPRUST_CRED_<SLOT>` comma-separated, plus values from a
    /// `.env.local` the host loaded into the environment), then the
    /// persisted pool.
    #[must_use]
    pub fn keys_for(&self, slot: &str) -> Vec<String> {
        let name = env_var_name(slot);
        // The pre-rename variable name (ZAPLITE_CRED_<SLOT>) is read
        // as a fallback so existing shell setups and .env.local
        // files keep working after the rename.
        let from_env = std::env::var(&name)
            .ok()
            .or_else(|| std::env::var(legacy_env_var_name(slot)).ok())
            .or_else(|| self.overrides.get(&name).cloned());
        if let Some(raw) = from_env {
            let keys: Vec<String> =
                raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if !keys.is_empty() {
                return keys;
            }
        }
        self.pools.get(slot).map(|p| p.values().to_vec()).unwrap_or_default()
    }

    /// A ready-to-use pool for a slot (env override applied).
    #[must_use]
    pub fn pool_for(&self, slot: &str) -> KeyPool {
        KeyPool::new(self.keys_for(slot))
    }

    /// Replaces a slot's keys and persists them.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError`] when persistence fails.
    pub(crate) fn set_keys(
        &mut self,
        slot: &str,
        keys: Vec<String>,
    ) -> Result<(), CredentialError> {
        self.pools.insert(slot.to_string(), KeyPool::new(keys));
        self.persist()
    }

    /// Adds one key to a slot and persists.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError`] when persistence fails.
    pub fn add_key(&mut self, slot: &str, key: String) -> Result<(), CredentialError> {
        let mut keys = self.keys_for(slot);
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
        self.set_keys(slot, keys)
    }

    /// Removes one key (by value) from a slot and persists.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError`] when persistence fails.
    pub fn remove_key(&mut self, slot: &str, key: &str) -> Result<(), CredentialError> {
        let keys: Vec<String> = self.keys_for(slot).into_iter().filter(|k| k != key).collect();
        self.set_keys(slot, keys)
    }

    fn persist(&self) -> Result<(), CredentialError> {
        let map: BTreeMap<String, Vec<String>> =
            self.pools.iter().map(|(slot, pool)| (slot.clone(), pool.values().to_vec())).collect();
        self.backend.save_all(&map)
    }
}

/// The environment variable that overrides a slot, e.g.
/// `example_api_key` → `ZAPRUST_CRED_EXAMPLE_API_KEY`.
#[must_use]
pub(crate) fn env_var_name(slot: &str) -> String {
    let upper: String = slot
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    format!("ZAPRUST_CRED_{upper}")
}

/// The pre-rename form of [`env_var_name`] (the app shipped as
/// Zaplite until 1.0.0), kept as a read fallback in
/// [`CredentialStore::keys_for`].
#[must_use]
pub(crate) fn legacy_env_var_name(slot: &str) -> String {
    env_var_name(slot).replacen("ZAPRUST", "ZAPLITE", 1)
}

/// Parses `KEY=VALUE` lines from a `.env.local`-style file into a
/// map. The app passes the result to
/// [`CredentialStore::set_overrides`]; real environment variables
/// still win over file values (the host checks env first).
#[must_use]
pub fn load_dotenv_local(path: &std::path::Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        out.insert(key.trim().to_string(), value.trim().trim_matches('"').to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_of(values: &[&str]) -> KeyPool {
        KeyPool::new(values.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn acquire_rotates_round_robin() {
        let mut pool = pool_of(&["k1", "k2", "k3"]);
        assert_eq!(pool.acquire(0), Some("k1"));
        assert_eq!(pool.acquire(0), Some("k2"));
        assert_eq!(pool.acquire(0), Some("k3"));
        assert_eq!(pool.acquire(0), Some("k1"));
    }

    #[test]
    fn rate_limited_key_cools_down_and_is_skipped() {
        let mut pool = pool_of(&["k1", "k2"]);
        pool.report_http_status("k1", 429, 1_000);
        assert_eq!(pool.acquire(1_000), Some("k2"));
        assert_eq!(pool.acquire(1_000), Some("k2"), "k1 still cooling");
        // After the cooldown the key rejoins the rotation.
        assert_eq!(pool.acquire(1_000 + RATE_LIMIT_COOLDOWN_SECS), Some("k1"));
    }

    #[test]
    fn unauthorized_key_is_marked_bad_and_never_served() {
        let mut pool = pool_of(&["k1", "k2"]);
        pool.report_http_status("k1", 401, 0);
        assert!(pool.is_bad("k1"));
        for _ in 0..4 {
            assert_eq!(pool.acquire(9_999), Some("k2"));
        }
        pool.report_http_status("k2", 403, 0);
        assert_eq!(pool.acquire(9_999), None, "all keys bad → None");
    }

    #[test]
    fn success_clears_a_cooldown() {
        let mut pool = pool_of(&["k1"]);
        pool.report_http_status("k1", 429, 0);
        assert_eq!(pool.acquire(1), None);
        pool.report_http_status("k1", 200, 1);
        assert_eq!(pool.acquire(1), Some("k1"));
    }

    #[test]
    fn empty_pool_acquires_nothing() {
        let mut pool = KeyPool::default();
        assert_eq!(pool.acquire(0), None);
        assert_eq!(pool.usable_count(0), 0);
    }

    #[test]
    fn mask_never_reveals_the_middle() {
        assert_eq!(mask("sk-1234567890"), "sk-••••90");
        assert_eq!(mask("short"), "••••••");
        assert!(!mask("sk-1234567890").contains("12345"));
    }

    #[test]
    fn env_var_name_is_stable() {
        assert_eq!(env_var_name("example_api_key"), "ZAPRUST_CRED_EXAMPLE_API_KEY");
        assert_eq!(env_var_name("giphy_api_key"), "ZAPRUST_CRED_GIPHY_API_KEY");
    }

    #[test]
    fn file_backend_round_trips_and_stays_private() {
        let dir = std::env::temp_dir().join(format!("zaprust-cred-test-{}", std::process::id()));
        let path = dir.join("secrets.json");
        let backend = FileBackend::new(path.clone());
        let mut store = CredentialStore::open(Box::new(backend)).expect("open");
        store.declare(
            "community.example",
            &[CredentialSlot {
                id: "example_api_key".into(),
                label: "Example key".into(),
                kind: CredentialKind::ApiKey,
            }],
        );
        store.add_key("example_api_key", "secret-one".into()).expect("add");
        store.add_key("example_api_key", "secret-two".into()).expect("add");
        assert_eq!(store.pool_for("example_api_key").len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "secrets file must be 0600");
        }
        // Reload from disk: values survive.
        let backend = FileBackend::new(path.clone());
        let reopened = CredentialStore::open(Box::new(backend)).expect("reopen");
        assert_eq!(reopened.keys_for("example_api_key").len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn debug_impls_redact_secret_values() {
        // SEC-20: formatting a pool or the memory backend must show
        // names/counts only, never a key value.
        let pool = pool_of(&["sk-super-secret-value", "another-secret-value"]);
        let pool_dbg = format!("{pool:?}");
        assert!(!pool_dbg.contains("super-secret"), "{pool_dbg}");
        assert!(!pool_dbg.contains("another-secret"), "{pool_dbg}");
        assert!(pool_dbg.contains('2'), "debug shows the key count: {pool_dbg}");

        let backend = MemoryBackend::default();
        backend
            .save_all(&BTreeMap::from([(
                "example_api_key".to_string(),
                vec!["sk-hidden-value".to_string()],
            )]))
            .expect("save");
        let backend_dbg = format!("{backend:?}");
        assert!(!backend_dbg.contains("sk-hidden-value"), "{backend_dbg}");
        assert!(backend_dbg.contains("example_api_key"), "debug shows slot names: {backend_dbg}");
    }

    #[test]
    fn file_backend_rewrite_is_atomic_and_leaves_no_temp() {
        // SEC-19: a second save replaces the file (rename, not
        // truncate-in-place) and leaves no temp file behind.
        let dir = std::env::temp_dir().join(format!("zaprust-cred-atomic-{}", std::process::id()));
        let path = dir.join("secrets.json");
        let backend = FileBackend::new(path.clone());
        backend
            .save_all(&BTreeMap::from([("s".to_string(), vec!["one".to_string()])]))
            .expect("first save");
        backend
            .save_all(&BTreeMap::from([("s".to_string(), vec!["two".to_string()])]))
            .expect("second save");
        let loaded = backend.load_all().expect("load");
        assert_eq!(loaded["s"], vec!["two".to_string()]);
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "replaced secrets file must stay 0600");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_removes_keys() {
        let mut store = CredentialStore::open(Box::new(MemoryBackend::default())).expect("open");
        store.add_key("s", "a".into()).expect("add");
        store.add_key("s", "b".into()).expect("add");
        store.remove_key("s", "a").expect("remove");
        assert_eq!(store.keys_for("s"), vec!["b".to_string()]);
    }
}
