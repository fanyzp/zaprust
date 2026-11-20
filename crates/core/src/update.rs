//! Self-update logic: version comparison, GitHub release parsing,
//! per-platform asset selection, checksum verification, install
//! location classification, and the binary swap.
//!
//! This module is pure logic plus small file operations. It knows
//! nothing about HTTP: the app crate downloads the release JSON and
//! the binary, and hands the bytes and paths to the functions here.
//! Everything is unit-tested, including a captured release payload.

use std::cmp::Ordering;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// A parsed semantic version, from a release tag like `v0.22.0` or a
/// plain `0.22.0`. Build metadata (`+...`) is accepted and ignored,
/// per SemVer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Major version number.
    pub major: u64,
    /// Minor version number.
    pub minor: u64,
    /// Patch version number.
    pub patch: u64,
    /// Dot-separated prerelease identifiers, empty for a release.
    pub pre: Vec<String>,
}

impl Version {
    /// Parses a version from a tag or plain string. A leading `v` is
    /// optional. Returns `None` for anything that is not SemVer.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        // Build metadata is accepted and ignored, per SemVer, but
        // a `+` followed by nothing (or by an empty identifier) is
        // malformed, not a release with empty metadata.
        let s = match s.split_once('+') {
            Some((head, build)) => {
                let bad_build = build.split('.').any(|id| {
                    id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                });
                if bad_build {
                    return None;
                }
                head
            }
            None => s,
        };
        // Likewise a `-` must introduce a non-empty prerelease:
        // `1.0.0-` is malformed, not the release `1.0.0` itself.
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => {
                if pre.is_empty() {
                    return None;
                }
                (core, pre)
            }
            None => (s, ""),
        };
        let mut parts = core.split('.');
        let major: u64 = parts.next()?.parse().ok()?;
        let minor: u64 = parts.next()?.parse().ok()?;
        let patch: u64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        let pre: Vec<String> = if pre.is_empty() {
            Vec::new()
        } else {
            let ids: Vec<String> = pre.split('.').map(str::to_owned).collect();
            if ids.iter().any(std::string::String::is_empty) {
                return None;
            }
            ids
        };
        Some(Self { major, minor, patch, pre })
    }

    /// True when `self` is strictly newer than `other`.
    #[must_use]
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Greater
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let core =
            (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        if core != Ordering::Equal {
            return core;
        }
        // A release is newer than any of its prereleases.
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => cmp_prerelease(&self.pre, &other.pre),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// SemVer prerelease comparison: numeric identifiers compare
/// numerically and rank below alphanumeric ones; a shorter list that
/// is a prefix of a longer one ranks lower.
fn cmp_prerelease(a: &[String], b: &[String]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(m), Ok(n)) => m.cmp(&n),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// One downloadable file attached to a GitHub release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    /// File name, exactly as published, e.g. `zaprust-linux-x86_64`.
    pub name: String,
    /// Direct download URL.
    pub url: String,
    /// Size in bytes.
    pub size: u64,
}

/// A parsed GitHub release (the `releases/latest` payload, reduced
/// to what the updater needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// The release tag, e.g. `v0.22.0`.
    pub tag: String,
    /// The parsed version from the tag.
    pub version: Version,
    /// Release title (may be empty).
    pub name: String,
    /// Release notes body, Markdown (may be empty).
    pub notes: String,
    /// All assets attached to the release.
    pub assets: Vec<ReleaseAsset>,
}

impl ReleaseInfo {
    /// Parses the JSON body of the GitHub `releases/latest` API.
    ///
    /// # Errors
    /// Returns [`UpdateError::BadReleaseData`] when the payload is not
    /// valid JSON, has no usable tag, or misses required fields.
    pub fn parse(json: &str) -> Result<Self, UpdateError> {
        let raw: RawRelease =
            serde_json::from_str(json).map_err(|_| UpdateError::BadReleaseData)?;
        let version = Version::parse(&raw.tag_name).ok_or(UpdateError::BadReleaseData)?;
        let mut assets = Vec::with_capacity(raw.assets.len());
        for a in raw.assets {
            // SEC-04: never trust `browser_download_url` verbatim.
            // The updater downloads and executes whatever an asset
            // URL points at, so every URL must be HTTPS and hosted
            // by GitHub itself before it is stored at all.
            if !is_trusted_download_url(&a.browser_download_url) {
                return Err(UpdateError::UntrustedAssetUrl);
            }
            assets.push(ReleaseAsset { name: a.name, url: a.browser_download_url, size: a.size });
        }
        Ok(Self {
            tag: raw.tag_name,
            version,
            name: raw.name.unwrap_or_default(),
            notes: raw.body.unwrap_or_default(),
            assets,
        })
    }

    /// Looks up one asset by its exact published file name.
    #[must_use]
    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// The asset a one-click update should download on this platform,
    /// or `None` when no binary is published for it.
    #[must_use]
    pub fn asset_for(&self, os: &str, arch: &str) -> Option<&ReleaseAsset> {
        self.asset(asset_name_for(os, arch)?)
    }
}

/// True when a release asset download URL is safe for the updater
/// to fetch: HTTPS only, and the host is `github.com`,
/// `api.github.com`, or a subdomain of `githubusercontent.com`
/// (release assets redirect to hosts such as
/// `objects.githubusercontent.com`). Lookalike hosts such as
/// `github.com.evil.example` or `evilgithubusercontent.com` are
/// rejected: the suffix check requires a dot boundary.
#[must_use]
pub(crate) fn is_trusted_download_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    // Authority runs to the first path/query/fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Drop any userinfo, then any port.
    let host_port = authority.rsplit('@').next().unwrap_or("");
    if host_port.starts_with('[') {
        return false; // No IP-literal hosts for release assets.
    }
    let host = host_port.split(':').next().unwrap_or("").to_lowercase();
    host == "github.com"
        || host == "api.github.com"
        || host == "githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    name: Option<String>,
    body: Option<String>,
    assets: Vec<RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// The release asset file name for an OS and CPU architecture pair,
/// exactly as the Release workflow publishes it. Windows binaries
/// carry the `.exe` suffix; Linux and macOS binaries have none.
/// Returns `None` for platforms with no published binary (for
/// example macOS on Intel, which the project does not ship).
#[must_use]
pub(crate) fn asset_name_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("zaprust-linux-x86_64"),
        ("linux", "aarch64") => Some("zaprust-linux-arm64"),
        ("windows", "x86_64") => Some("zaprust-windows-x86_64.exe"),
        ("windows", "aarch64") => Some("zaprust-windows-arm64.exe"),
        ("macos", "aarch64") => Some("zaprust-macos-arm64"),
        _ => None,
    }
}

/// The name of the combined checksum asset on every release.
pub const SHA256SUMS_ASSET: &str = "SHA256SUMS";

/// The name of the checksum signature asset on every release: the
/// base64 Ed25519 signature over the exact bytes of `SHA256SUMS`,
/// made with the offline release key (SEC-F04). A release without
/// this asset, or with a signature that does not verify, is
/// refused by the updater: the checksums alone only prove the
/// download was not corrupted, not that the publisher made it.
pub const SHA256SUMS_SIG_ASSET: &str = "SHA256SUMS.sig";

/// The release signing public key (Ed25519), embedded so the
/// updater can verify `SHA256SUMS.sig` without trusting anything
/// fetched from the release origin. The matching private key is
/// held offline by the maintainer and never ships; see
/// `packaging/RELEASE-SIGNING.md`.
pub const RELEASE_SIGNING_PUBKEY: [u8; 32] = [
    7, 191, 230, 122, 70, 69, 118, 105, 183, 248, 176, 216, 119, 102, 114, 174, 72, 63, 43, 119,
    14, 148, 58, 182, 231, 163, 122, 13, 113, 249, 115, 25,
];

/// Verifies a `SHA256SUMS.sig` payload (base64 Ed25519 signature)
/// over the exact bytes of the release's `SHA256SUMS` against the
/// embedded release key.
#[must_use]
pub fn verify_sums_signature(sums: &[u8], signature_text: &str) -> bool {
    use base64::Engine;
    let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(signature_text.trim()) else {
        return false;
    };
    let key =
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, RELEASE_SIGNING_PUBKEY);
    key.verify(sums, &sig).is_ok()
}

/// Parses a `SHA256SUMS` file into `(hash, file name)` pairs. Lines
/// follow the `sha256sum` format: hash, whitespace, then the name
/// (a leading `*` binary marker is stripped).
#[must_use]
pub fn parse_sha256sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.len() < 66 {
                return None;
            }
            // `get`, not `split_at`: byte 64 can fall inside a
            // multi-byte character, where `split_at` would panic.
            // A malformed line is skipped, never a crash.
            let hash = line.get(..64)?;
            let rest = line.get(64..)?;
            if !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let name = rest.trim().trim_start_matches('*').trim();
            if name.is_empty() {
                return None;
            }
            Some((hash.to_lowercase(), name.to_owned()))
        })
        .collect()
}

/// The expected SHA-256 hash for one file from parsed sums.
#[must_use]
pub fn expected_hash<'a>(sums: &'a [(String, String)], file_name: &str) -> Option<&'a str> {
    sums.iter().find(|(_, name)| name == file_name).map(|(hash, _)| hash.as_str())
}

/// Lowercase hex encoding of raw bytes (the digest display form).
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Computes the SHA-256 of a byte slice, lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}

/// Computes the SHA-256 of a file, lowercase hex, reading in chunks
/// so large binaries never sit in memory whole.
///
/// # Errors
/// Returns [`UpdateError::Io`] when the file cannot be read.
pub fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(hex_lower(&digest))
}

/// True when `bytes` hash to the expected lowercase-or-uppercase hex
/// digest (surrounding whitespace is ignored).
#[must_use]
pub fn verify_sha256(bytes: &[u8], expected_hex: &str) -> bool {
    sha256_hex(bytes) == expected_hex.trim().to_lowercase()
}

/// How the running copy was installed, which decides whether the app
/// may replace its own binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// A portable binary or an AppImage living in user space: the
    /// app can download and swap in a new version by itself.
    Portable,
    /// Installed by a system package manager (`.deb`, `.rpm`, a
    /// package under a system prefix): the app must not try to
    /// overwrite it, it points at the release page instead.
    SystemPackage,
}

/// Classifies an executable path as a portable or system install,
/// from the path alone. System prefixes are the read-only-for-users
/// locations package managers install into: `/usr`, `/bin`, `/sbin`,
/// `/opt`, `/Applications`, and the Windows `Program Files` folders.
/// Everything else (home folders, local bins, extracted AppImages)
/// counts as portable.
#[must_use]
pub fn classify_install_path(path: &Path) -> InstallKind {
    const SYSTEM_PREFIXES: [&str; 5] = ["/usr/", "/bin/", "/sbin/", "/opt/", "/applications/"];
    let normalized = path.to_string_lossy().replace('\\', "/").to_lowercase();
    if SYSTEM_PREFIXES.iter().any(|prefix| normalized.starts_with(prefix))
        || normalized.contains("/program files/")
        || normalized.contains("/program files (x86)/")
    {
        InstallKind::SystemPackage
    } else {
        InstallKind::Portable
    }
}

/// The sibling path a replaced binary is kept at during a swap
/// (`zaprust` becomes `zaprust.old`, `zaprust.exe` becomes
/// `zaprust.exe.old`).
#[must_use]
pub fn backup_path_for(current: &Path) -> PathBuf {
    let mut name = current
        .file_name()
        .map_or_else(|| "zaprust".to_owned(), |n| n.to_string_lossy().into_owned());
    name.push_str(".old");
    current.with_file_name(name)
}

/// Replaces the binary at `current` with the file at `staged`.
///
/// The old binary is first renamed to the backup path, then the new
/// file is moved in (copied when the two live on different file
/// systems). When the move fails, the old binary is restored before
/// the error is returned, so `current` is never left missing. On
/// Unix the new binary gets the executable bit. Returns the backup
/// path so the caller can clean it up on a later start; keeping it
/// is also the manual rollback.
///
/// # Errors
/// Returns [`UpdateError::Io`] on any file operation failure.
pub fn swap_binary(current: &Path, staged: &Path) -> Result<PathBuf, UpdateError> {
    let backup = backup_path_for(current);
    if backup.exists() {
        std::fs::remove_file(&backup)?;
    }
    std::fs::rename(current, &backup)?;
    match move_file(staged, current) {
        Ok(()) => {
            set_executable(current);
            Ok(backup)
        }
        Err(e) => {
            // Restore the old binary before reporting the failure.
            let _ = std::fs::rename(&backup, current);
            Err(e)
        }
    }
}

/// Renames a file, falling back to copy plus delete across file
/// systems (the download cache and the install dir often differ).
fn move_file(from: &Path, to: &Path) -> Result<(), UpdateError> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)?;
    Ok(())
}

/// Sets the executable bit on Unix; a no-op on Windows.
#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// Sets the executable bit on Unix; a no-op on Windows.
#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

/// Something went wrong while preparing or performing an update.
#[derive(Debug)]
pub enum UpdateError {
    /// The release payload could not be understood.
    BadReleaseData,
    /// A release asset URL was not HTTPS or not hosted by GitHub
    /// (SEC-04), so the whole release was refused.
    UntrustedAssetUrl,
    /// The downloaded file's SHA-256 did not match the hash the
    /// release published for it. Kept distinct from every other
    /// failure: a mismatch can mean tampering, not just breakage.
    ChecksumMismatch {
        /// Hash from the release's `SHA256SUMS`.
        expected: String,
        /// Hash of the file that was actually downloaded.
        actual: String,
    },
    /// The release's `SHA256SUMS` was missing, unreadable, or did
    /// not list the asset being downloaded.
    BadChecksumFile,
    /// The release publishes no binary for this OS/architecture.
    NoAssetForPlatform,
    /// The download's byte count did not match the size the
    /// release declared for the asset (SEC-05).
    SizeMismatch {
        /// Declared size in bytes.
        expected: u64,
        /// Bytes actually received.
        actual: u64,
    },
    /// A file operation failed.
    Io(std::io::Error),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadReleaseData => write!(f, "the release data could not be read"),
            Self::UntrustedAssetUrl => {
                write!(f, "a release download address was not a GitHub HTTPS address")
            }
            Self::ChecksumMismatch { expected, actual } => {
                write!(f, "checksum mismatch: expected {expected}, downloaded file is {actual}")
            }
            Self::BadChecksumFile => write!(f, "the release checksum file could not be used"),
            Self::NoAssetForPlatform => {
                write!(f, "the release has no download for this kind of computer")
            }
            Self::SizeMismatch { expected, actual } => {
                write!(f, "download size mismatch: expected {expected} bytes, got {actual}")
            }
            Self::Io(e) => write!(f, "a file operation failed: {e}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<std::io::Error> for UpdateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed but shape-faithful capture of the GitHub
    /// `releases/latest` payload, including the Linux packages and
    /// the checksum file as distractors for asset selection.
    const RELEASE_FIXTURE: &str = r###"{
        "tag_name": "v0.22.0",
        "name": "Zaprust v0.22.0",
        "body": "## feat:\n- One-click updates from inside the app.\n",
        "assets": [
            { "name": "zaprust-linux-x86_64", "browser_download_url": "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/zaprust-linux-x86_64", "size": 45952768 },
            { "name": "zaprust-linux-arm64", "browser_download_url": "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/zaprust-linux-arm64", "size": 44201088 },
            { "name": "zaprust-windows-x86_64.exe", "browser_download_url": "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/zaprust-windows-x86_64.exe", "size": 39752704 },
            { "name": "zaprust-windows-arm64.exe", "browser_download_url": "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/zaprust-windows-arm64.exe", "size": 34813440 },
            { "name": "zaprust-macos-arm64", "browser_download_url": "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/zaprust-macos-arm64", "size": 36900272 },
            { "name": "zaprust-linux-x86_64.deb", "browser_download_url": "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/zaprust-linux-x86_64.deb", "size": 21000000 },
            { "name": "zaprust-linux-x86_64.rpm", "browser_download_url": "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/zaprust-linux-x86_64.rpm", "size": 21000001 },
            { "name": "zaprust-linux-x86_64.AppImage", "browser_download_url": "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/zaprust-linux-x86_64.AppImage", "size": 51000000 },
            { "name": "SHA256SUMS", "browser_download_url": "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/SHA256SUMS", "size": 700 }
        ]
    }"###;

    #[test]
    fn version_parse_and_compare() {
        let v021 = Version::parse("0.21.0").unwrap();
        let v022 = Version::parse("v0.22.0").unwrap();
        assert!(v022.is_newer_than(&v021));
        assert!(!v021.is_newer_than(&v022));
        assert!(!v022.is_newer_than(&v022));
        assert_eq!(Version::parse("1.0.0+build5"), Version::parse("1.0.0"));
        // Prereleases rank below the release itself, above older ones.
        let alpha = Version::parse("1.0.0-alpha").unwrap();
        let rel = Version::parse("1.0.0").unwrap();
        assert!(rel.is_newer_than(&alpha));
        assert!(alpha.is_newer_than(&v022));
        assert_eq!(
            Version::parse("1.0.0-alpha.1").unwrap().cmp(&Version::parse("1.0.0-alpha").unwrap()),
            Ordering::Greater
        );
        assert_eq!(
            Version::parse("1.0.0-alpha.beta")
                .unwrap()
                .cmp(&Version::parse("1.0.0-alpha.1").unwrap()),
            Ordering::Greater
        );
        // Junk is rejected, never guessed.
        assert!(Version::parse("").is_none());
        assert!(Version::parse("1.0").is_none());
        assert!(Version::parse("v1.0.x").is_none());
        assert!(Version::parse("1.0.0.1").is_none());
        assert_eq!(v022.to_string(), "0.22.0");
    }

    #[test]
    fn version_parse_rejects_empty_prerelease_and_build() {
        // A trailing separator with nothing after it used to parse
        // as the plain release: `v1.0.0-` compared equal to
        // `1.0.0`. Malformed tags are rejected outright.
        assert!(Version::parse("1.0.0-").is_none());
        assert!(Version::parse("v1.0.0-").is_none());
        assert!(Version::parse("1.0.0+").is_none());
        assert!(Version::parse("1.0.0+a..b").is_none());
        assert!(Version::parse("1.0.0-alpha..1").is_none());
        // The forms the project actually ships still parse: plain
        // and `v` releases, and the nightly workflow's
        // `<base>-nightly.<date>+<short sha>` tags.
        assert!(Version::parse("v1.0.0").is_some());
        assert_eq!(
            Version::parse("0.20.0-nightly.20261114+abc1234"),
            Version::parse("0.20.0-nightly.20261114")
        );
    }

    #[test]
    fn sums_parser_skips_lines_that_would_split_mid_character() {
        // 63 ASCII bytes followed by a 2-byte character: byte 64
        // falls inside the character, where `split_at` panicked.
        // The line is malformed and skipped, never a crash.
        let mid_char = format!("{}é rest of the line, padding", "a".repeat(63));
        assert!(mid_char.len() >= 66);
        assert!(parse_sha256sums(&mid_char).is_empty());
        // Short and garbage lines are skipped; a valid line parses.
        let good = format!("{}  zaprust-linux-x86_64", "b".repeat(64));
        let parsed = parse_sha256sums(&format!("short\n{mid_char}\n{good}\n"));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "b".repeat(64));
        assert_eq!(parsed[0].1, "zaprust-linux-x86_64");
    }

    #[test]
    fn release_fixture_parses_and_selects_assets() {
        let rel = ReleaseInfo::parse(RELEASE_FIXTURE).unwrap();
        assert_eq!(rel.tag, "v0.22.0");
        assert_eq!(rel.version, Version::parse("0.22.0").unwrap());
        assert!(rel.notes.contains("One-click updates"));
        // Exact names per platform, Windows with .exe, and never a
        // package file where a raw binary is wanted.
        assert_eq!(
            rel.asset_for("linux", "x86_64").map(|a| a.name.as_str()),
            Some("zaprust-linux-x86_64")
        );
        assert_eq!(
            rel.asset_for("linux", "aarch64").map(|a| a.name.as_str()),
            Some("zaprust-linux-arm64")
        );
        assert_eq!(
            rel.asset_for("windows", "x86_64").map(|a| a.name.as_str()),
            Some("zaprust-windows-x86_64.exe")
        );
        assert_eq!(
            rel.asset_for("windows", "aarch64").map(|a| a.name.as_str()),
            Some("zaprust-windows-arm64.exe")
        );
        assert_eq!(
            rel.asset_for("macos", "aarch64").map(|a| a.name.as_str()),
            Some("zaprust-macos-arm64")
        );
        assert!(rel.asset_for("macos", "x86_64").is_none());
        assert!(rel.asset(SHA256SUMS_ASSET).is_some());
        // Malformed payloads are errors, not panics.
        assert!(ReleaseInfo::parse("not json").is_err());
        assert!(ReleaseInfo::parse("{}").is_err());
    }

    #[test]
    fn asset_url_trust_rules() {
        // The hosts GitHub actually serves releases and assets from.
        assert!(is_trusted_download_url("https://github.com/fanyzp/zaprust/releases"));
        assert!(is_trusted_download_url("https://api.github.com/repos/fanyzp/zaprust"));
        assert!(is_trusted_download_url("https://objects.githubusercontent.com/x"));
        assert!(is_trusted_download_url("https://release-assets.githubusercontent.com/x"));
        assert!(is_trusted_download_url("https://github.com:443/fanyzp/zaprust"));
        // Plain HTTP, foreign hosts, and lookalikes are all refused.
        assert!(!is_trusted_download_url("http://github.com/fanyzp/zaprust"));
        assert!(!is_trusted_download_url("https://example.com/zaprust"));
        assert!(!is_trusted_download_url("https://github.com.evil.example/zaprust"));
        assert!(!is_trusted_download_url("https://evilgithubusercontent.com/x"));
        assert!(!is_trusted_download_url("https://githubusercontent.com.evil.example/x"));
        assert!(!is_trusted_download_url("https://user@evil.example/x"));
        assert!(!is_trusted_download_url("https://[127.0.0.1]/x"));
        assert!(!is_trusted_download_url("github.com/no-scheme"));
        assert!(!is_trusted_download_url(""));
    }

    #[test]
    fn release_with_untrusted_asset_url_is_rejected() {
        let poisoned = RELEASE_FIXTURE.replace(
            "https://github.com/fanyzp/zaprust/releases/download/v0.22.0/SHA256SUMS",
            "http://attacker.example/SHA256SUMS",
        );
        assert!(matches!(ReleaseInfo::parse(&poisoned), Err(UpdateError::UntrustedAssetUrl)));
    }

    #[test]
    fn sums_signature_verifies_against_the_release_key() {
        // Signed with the release key over these exact bytes (the
        // maintainer's signing flow, packaging/RELEASE-SIGNING.md).
        const SUMS: &[u8] =
            b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  zaprust-linux-x86_64\n";
        const SIG: &str =
            "Yvx39rPIPe0LtZiMUTnR5p/jHptGQXsvIN9WfXUebec9Ot6pjPfd3Dy8pWRxL7PGAHOkhLh2x4upYN1/taMMBg==";
        assert!(verify_sums_signature(SUMS, SIG));
        // Any change to the signed bytes, or a malformed signature,
        // fails closed.
        assert!(!verify_sums_signature(b"e3b0  zaprust-linux-x86_64\n", SIG));
        assert!(!verify_sums_signature(SUMS, "not base64"));
        assert!(!verify_sums_signature(SUMS, ""));
    }

    #[test]
    fn sha256sums_parse_and_verify() {
        // The well-known SHA-256 of "abc".
        const ABC_HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_hex(b"abc"), ABC_HASH);
        assert!(verify_sha256(b"abc", ABC_HASH));
        assert!(verify_sha256(b"abc", &ABC_HASH.to_uppercase()));
        assert!(!verify_sha256(b"abd", ABC_HASH));
        let sums = format!(
            "{ABC_HASH}  zaprust-linux-x86_64\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 *zaprust-windows-x86_64.exe\n\
             garbage line\n"
        );
        let parsed = parse_sha256sums(&sums);
        assert_eq!(parsed.len(), 2);
        assert_eq!(expected_hash(&parsed, "zaprust-linux-x86_64"), Some(ABC_HASH));
        assert_eq!(
            expected_hash(&parsed, "zaprust-windows-x86_64.exe"),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert!(expected_hash(&parsed, "zaprust-macos-arm64").is_none());
    }

    #[test]
    fn classify_install_paths() {
        // System package locations: the app must not swap these.
        assert_eq!(
            classify_install_path(Path::new("/usr/bin/zaprust")),
            InstallKind::SystemPackage
        );
        assert_eq!(
            classify_install_path(Path::new("/usr/local/bin/zaprust")),
            InstallKind::SystemPackage
        );
        assert_eq!(
            classify_install_path(Path::new("/opt/zaprust/zaprust")),
            InstallKind::SystemPackage
        );
        assert_eq!(
            classify_install_path(Path::new("/Applications/Zaprust.app/Contents/MacOS/zaprust")),
            InstallKind::SystemPackage
        );
        assert_eq!(
            classify_install_path(Path::new(r"C:\Program Files\Zaprust\zaprust.exe")),
            InstallKind::SystemPackage
        );
        // User space: portable copies and AppImages keep one-click.
        assert_eq!(classify_install_path(Path::new("/home/user/zaprust")), InstallKind::Portable);
        assert_eq!(
            classify_install_path(Path::new("/home/user/.local/bin/zaprust")),
            InstallKind::Portable
        );
        assert_eq!(
            classify_install_path(Path::new("/home/user/Apps/Zaprust.AppImage")),
            InstallKind::Portable
        );
        assert_eq!(
            classify_install_path(Path::new(r"C:\Users\user\Downloads\zaprust.exe")),
            InstallKind::Portable
        );
    }

    #[test]
    fn swap_binary_in_temp_dir() {
        let dir = std::env::temp_dir().join(format!(
            "zaprust-update-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let current = dir.join("zaprust");
        let staged = dir.join("download").join("zaprust-linux-x86_64");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&current, b"old binary").unwrap();
        std::fs::write(&staged, b"new binary").unwrap();

        let backup = swap_binary(&current, &staged).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new binary");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old binary");
        assert!(!staged.exists(), "the staged file was moved, not copied");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&current).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "the new binary is executable");
        }
        // sha256_file agrees with the in-memory helper.
        assert_eq!(sha256_file(&current).unwrap(), sha256_hex(b"new binary"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
