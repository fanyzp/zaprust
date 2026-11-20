//! In-app version check and one-click self-update.
//!
//! The app asks GitHub for the latest Zaprust release, compares it
//! with its own version, and, only when the user clicks, downloads
//! the binary for this platform, verifies it against the release's
//! published checksums, and swaps it in place of the running binary.
//! All comparison, parsing, verification, and swap logic lives in
//! `core::update`; this module is the HTTP, threading, and UI glue.
//!
//! Copies installed by a system package manager (a `.deb` or `.rpm`
//! under a system prefix) are never overwritten: they show the
//! update and link to the release page instead, because replacing
//! them would need administrator rights the app does not have.
//!
//! Failures stay typed end to end: workers report an
//! [`UpdateFailure`] (a kind plus the human sentence), never a bare
//! string, so a checksum mismatch, a network failure, and "no
//! download for this platform" stay distinguishable in the UI state
//! and in any log line, instead of collapsing into one another.

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use core::update::{InstallKind, ReleaseInfo, UpdateError, Version};

use crate::state::{now_unix, App};

/// GitHub API endpoint for the newest stable release.
fn latest_release_url() -> String {
    format!("https://api.github.com/repos/{}/releases/latest", core::crash::GITHUB_REPO)
}

/// Human-facing release page for manual downloads.
pub(crate) fn release_page_url() -> String {
    format!("https://github.com/{}/releases/latest", core::crash::GITHUB_REPO)
}

/// The version this build reports for itself.
pub(crate) fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version {
        major: 0,
        minor: 0,
        patch: 0,
        pre: Vec::new(),
    })
}

/// Where this copy of the app lives, and whether the app may
/// replace it. An AppImage reports the image file through the
/// `APPIMAGE` variable (the running path itself is a read-only
/// mount); anything else uses the executable path.
///
/// SEC-16: the environment variable is only trusted when it names
/// a path that actually exists and is a file. A stale or hostile
/// `APPIMAGE` must not get to decide which file an update
/// overwrites; anything else falls back to the executable path.
pub(crate) fn install_location() -> (PathBuf, InstallKind) {
    let path = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty() && p.is_file())
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("zaprust"));
    let kind = core::update::classify_install_path(&path);
    (path, kind)
}

/// Removes the `.old` backup a previous successful swap left next
/// to the binary. Called once at startup; failure is silent because
/// the backup doubles as a manual rollback copy.
pub(crate) fn cleanup_stale_backup() {
    let (path, _) = install_location();
    let backup = core::update::backup_path_for(&path);
    if backup.exists() {
        let _ = std::fs::remove_file(backup);
    }
}

/// Which kind of update failure happened. The UI renders the human
/// sentence from the failure; the kind is what code (and logs) can
/// still tell apart afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateErrorKind {
    /// The update server could not be reached or answered badly.
    Network,
    /// The release payload could not be understood.
    BadReleaseData,
    /// A release asset URL was not a trusted GitHub HTTPS address.
    UntrustedAssetUrl,
    /// The downloaded file's hash differs from the published one.
    ChecksumMismatch,
    /// The checksum file was missing, unusable, or incomplete.
    BadChecksumFile,
    /// The checksum file's signature did not verify against the
    /// release key, or the release carries no signature at all
    /// (SEC-F04): the update is refused before anything downloads.
    SignatureInvalid,
    /// The release publishes no binary for this platform.
    NoAssetForPlatform,
    /// The download's size differs from the published size.
    SizeMismatch,
    /// This copy is owned by the system package manager.
    SystemPackage,
    /// A local file operation failed.
    Io,
    /// The worker stopped without reporting a result.
    WorkerStopped,
}

/// A typed update failure: the kind, kept for code and logs, plus
/// the sentence shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateFailure {
    /// What kind of failure this was.
    pub kind: UpdateErrorKind,
    /// The user-readable sentence for this failure.
    pub message: String,
}

impl UpdateFailure {
    fn new(kind: UpdateErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    fn network(e: &ureq::Error) -> Self {
        Self::new(UpdateErrorKind::Network, friendly_net_error(e))
    }

    /// The sentence the user sees for this failure.
    pub(crate) fn user_message(&self) -> String {
        self.message.clone()
    }
}

impl fmt::Display for UpdateFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl From<UpdateError> for UpdateFailure {
    fn from(e: UpdateError) -> Self {
        match e {
            UpdateError::BadReleaseData => Self::new(
                UpdateErrorKind::BadReleaseData,
                "The update answer could not be read. Please try again.",
            ),
            UpdateError::UntrustedAssetUrl => Self::new(
                UpdateErrorKind::UntrustedAssetUrl,
                "That release points at a download address Zaprust does not trust, \
                 so nothing was downloaded.",
            ),
            UpdateError::ChecksumMismatch { .. } => Self::new(
                UpdateErrorKind::ChecksumMismatch,
                "The downloaded file did not match the published checksum, so it was \
                 deleted and nothing was changed.",
            ),
            UpdateError::BadChecksumFile => Self::new(
                UpdateErrorKind::BadChecksumFile,
                "The checksum file could not be read, so nothing was changed.",
            ),
            UpdateError::NoAssetForPlatform => Self::new(
                UpdateErrorKind::NoAssetForPlatform,
                "There is no download for this kind of computer on the release page.",
            ),
            UpdateError::SizeMismatch { .. } => Self::new(
                UpdateErrorKind::SizeMismatch,
                "The download did not match the size published for it, so nothing \
                 was changed.",
            ),
            UpdateError::Io(_) => {
                Self::new(UpdateErrorKind::Io, "A file operation failed, so nothing was changed.")
            }
        }
    }
}

/// What the updater is doing right now, mirrored in Settings > About.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdatePhase {
    /// Nothing checked yet in this session.
    Idle,
    /// A check is running on a worker thread.
    Checking,
    /// The newest release is the one already running.
    UpToDate,
    /// A newer release exists and can be offered.
    Available,
    /// Downloading and verifying the new binary.
    Downloading,
    /// The new binary is in place; a restart finishes the update.
    Ready,
    /// The last step failed; the failure keeps its kind and its
    /// user-readable message.
    Failed(UpdateFailure),
}

/// Messages from the update worker threads.
pub(crate) enum UpdateMsg {
    /// A version check finished.
    Checked(Result<ReleaseInfo, UpdateFailure>),
    /// A download, verification, and swap finished; the payload is
    /// the path to restart into.
    Downloaded(Result<PathBuf, UpdateFailure>),
}

/// Turns a transport failure into a sentence a person can act on.
fn friendly_net_error(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => {
            "The update server took too long to answer. Check your connection and try again."
                .to_owned()
        }
        ureq::Error::StatusCode(code) => {
            format!(
                "GitHub could not complete that request (error {code}). Please try again later."
            )
        }
        _ => "Could not reach GitHub. Check your internet connection and try again.".to_owned(),
    }
}

/// A short-lived HTTP agent with update-friendly timeouts.
/// SEC-04: HTTPS only, so a plaintext `http://` asset URL (or a
/// redirect down to one) is refused by the transport itself, on
/// top of the URL validation in `core::update`.
fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .https_only(true)
        .user_agent(concat!("zaprust/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

/// Largest `SHA256SUMS` body the updater will read (SEC-05). The
/// real file lists a handful of assets and is about a kilobyte;
/// 64 KiB is generous headroom, and anything larger is refused
/// instead of being buffered whole.
const MAX_SHA256SUMS_BYTES: u64 = 64 * 1024;

impl App {
    /// True when the Settings list should show the update dot on
    /// the About row.
    pub(crate) fn update_available(&self) -> bool {
        matches!(self.update.phase, UpdatePhase::Available | UpdatePhase::Ready)
    }

    /// Records a finished version check (F17): the daily-check
    /// timestamp is stamped when a check completes, in either
    /// outcome, never when it starts, so a failed silent check does
    /// not suppress retries for the next 24 hours by itself and a
    /// crashed worker still counts as "checked".
    fn note_update_check_completed(&mut self) {
        self.update.last_check = now_unix();
        self.persist_settings();
    }

    /// Starts a version check on a worker thread. `manual` checks
    /// surface failures in About; silent startup failures just reset
    /// to idle, nobody needs an error for a check they never asked
    /// for.
    pub(crate) fn start_update_check(&mut self, ctx: &egui::Context, manual: bool) {
        if self.update.rx.is_some() {
            return;
        }
        self.update.phase = UpdatePhase::Checking;
        self.update.manual = manual;
        let (tx, rx) = std::sync::mpsc::channel::<UpdateMsg>();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("zaprust-update-check".to_owned())
            .spawn(move || {
                let agent = http_agent(Duration::from_secs(30));
                let result = agent
                    .get(&latest_release_url())
                    .call()
                    .map_err(|e| UpdateFailure::network(&e))
                    .and_then(|mut resp| {
                        resp.body_mut()
                            .read_to_string()
                            .map_err(|_| UpdateFailure::from(UpdateError::BadReleaseData))
                    })
                    .and_then(|text| ReleaseInfo::parse(&text).map_err(UpdateFailure::from));
                let _ = tx.send(UpdateMsg::Checked(result));
                ctx.request_repaint();
            })
            .ok();
        self.update.rx = Some(rx);
    }

    /// Downloads the release binary for this platform, verifies it
    /// against the release checksums, and swaps it into place. Only
    /// ever runs after the user clicks Update now on a portable
    /// install; the thread re-checks the install kind itself so a
    /// system package can never be overwritten by accident.
    pub(crate) fn start_update_download(&mut self, ctx: &egui::Context) {
        if self.update.rx.is_some() {
            return;
        }
        let Some(release) = self.update.release.clone() else {
            return;
        };
        let Some(asset) = release.asset_for(std::env::consts::OS, std::env::consts::ARCH).cloned()
        else {
            self.update.phase =
                UpdatePhase::Failed(UpdateFailure::from(UpdateError::NoAssetForPlatform));
            return;
        };
        let Some(sums_asset) = release.asset(core::update::SHA256SUMS_ASSET).cloned() else {
            self.update.phase = UpdatePhase::Failed(UpdateFailure::new(
                UpdateErrorKind::BadChecksumFile,
                "That release has no checksum file, so the download cannot be verified.",
            ));
            return;
        };
        let Some(sig_asset) = release.asset(core::update::SHA256SUMS_SIG_ASSET).cloned() else {
            self.update.phase = UpdatePhase::Failed(UpdateFailure::new(
                UpdateErrorKind::SignatureInvalid,
                "That release is not signed, so the update cannot be trusted.",
            ));
            return;
        };
        self.update.phase = UpdatePhase::Downloading;
        let (tx, rx) = std::sync::mpsc::channel::<UpdateMsg>();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("zaprust-update-download".to_owned())
            .spawn(move || {
                let result = download_verify_swap(&asset, &sums_asset, &sig_asset);
                let _ = tx.send(UpdateMsg::Downloaded(result));
                ctx.request_repaint();
            })
            .ok();
        self.update.rx = Some(rx);
    }

    /// Per-frame updater housekeeping: the once-a-day silent check
    /// on startup, and draining worker results.
    pub(crate) fn poll_update(&mut self, ctx: &egui::Context) {
        if !self.update.startup_done {
            self.update.startup_done = true;
            if now_unix().saturating_sub(self.update.last_check) >= 86_400 {
                self.start_update_check(ctx, false);
            }
        }
        let Some(rx) = &self.update.rx else {
            return;
        };
        let msg = match rx.try_recv() {
            Ok(msg) => msg,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // F17: a worker that died without reporting (spawn
                // failure, panic) must never strand the UI. Any
                // non-terminal phase becomes a failure the user can
                // retry, except a silent check, which just resets.
                self.update.rx = None;
                match self.update.phase {
                    UpdatePhase::Checking => {
                        self.note_update_check_completed();
                        if self.update.manual {
                            self.update.phase = UpdatePhase::Failed(UpdateFailure::new(
                                UpdateErrorKind::WorkerStopped,
                                "The update check stopped unexpectedly. Please try again.",
                            ));
                        } else {
                            self.update.phase = UpdatePhase::Idle;
                        }
                    }
                    UpdatePhase::Downloading => {
                        self.update.phase = UpdatePhase::Failed(UpdateFailure::new(
                            UpdateErrorKind::WorkerStopped,
                            "The update download stopped unexpectedly. Please try again.",
                        ));
                    }
                    _ => {}
                }
                return;
            }
        };
        self.update.rx = None;
        match msg {
            UpdateMsg::Checked(result) => {
                // F17: stamp on completion, success or failure.
                self.note_update_check_completed();
                match result {
                    Ok(release) => {
                        if release.version.is_newer_than(&current_version()) {
                            self.update.release = Some(release);
                            self.update.phase = UpdatePhase::Available;
                        } else {
                            self.update.release = None;
                            self.update.phase = UpdatePhase::UpToDate;
                        }
                    }
                    Err(e) => {
                        if self.update.manual {
                            self.update.phase = UpdatePhase::Failed(e);
                        } else {
                            self.update.phase = UpdatePhase::Idle;
                        }
                    }
                }
            }
            UpdateMsg::Downloaded(Ok(path)) => {
                self.update.target = Some(path);
                self.update.phase = UpdatePhase::Ready;
            }
            UpdateMsg::Downloaded(Err(e)) => {
                self.update.phase = UpdatePhase::Failed(e);
            }
        }
    }

    /// Restarts into the freshly swapped binary and exits. When the
    /// new copy cannot be started, the app keeps running and says
    /// so; the update itself is already in place for the next
    /// manual start.
    pub(crate) fn restart_into_update(&mut self) {
        let Some(target) = self.update.target.clone() else {
            return;
        };
        match std::process::Command::new(&target).spawn() {
            Ok(_) => std::process::exit(0),
            Err(_) => {
                self.notice = Some(
                    "The update is installed, but the new copy could not be started. \
                     Please reopen Zaprust yourself."
                        .to_owned(),
                );
            }
        }
    }
}

/// Fetches the release's `SHA256SUMS`, reading at most
/// [`MAX_SHA256SUMS_BYTES`] so a hostile or broken endpoint cannot
/// make the updater buffer an unbounded body (SEC-05).
fn fetch_sha256sums(
    agent: &ureq::Agent,
    sums_asset: &core::update::ReleaseAsset,
) -> Result<String, UpdateFailure> {
    let mut resp = agent.get(&sums_asset.url).call().map_err(|e| UpdateFailure::network(&e))?;
    let mut bytes = Vec::new();
    resp.body_mut()
        .as_reader()
        .take(MAX_SHA256SUMS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UpdateFailure::from(UpdateError::BadChecksumFile))?;
    if bytes.len() as u64 > MAX_SHA256SUMS_BYTES {
        return Err(UpdateFailure::new(
            UpdateErrorKind::BadChecksumFile,
            "The checksum file was unexpectedly large, so nothing was changed.",
        ));
    }
    String::from_utf8(bytes).map_err(|_| UpdateFailure::from(UpdateError::BadChecksumFile))
}

/// Streams the binary asset to `part_path`, enforcing the size the
/// release declared for it (SEC-05): the copy aborts the moment
/// more bytes arrive than declared, and afterwards the total must
/// match the declared size exactly.
fn download_asset(
    agent: &ureq::Agent,
    asset: &core::update::ReleaseAsset,
    part_path: &std::path::Path,
) -> Result<(), UpdateFailure> {
    if asset.size == 0 {
        return Err(UpdateFailure::new(
            UpdateErrorKind::SizeMismatch,
            "The release does not say how large this download should be, so it \
             cannot be checked safely. Nothing was changed.",
        ));
    }
    let mut resp = agent.get(&asset.url).call().map_err(|e| UpdateFailure::network(&e))?;
    let mut file = std::fs::File::create(part_path).map_err(|_| {
        UpdateFailure::new(
            UpdateErrorKind::Io,
            "The download could not be saved, so nothing was changed.",
        )
    })?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|_| {
            UpdateFailure::new(
                UpdateErrorKind::Network,
                "The download was interrupted, so nothing was changed.",
            )
        })?;
        if n == 0 {
            break;
        }
        received = received.saturating_add(n as u64);
        if received > asset.size {
            return Err(UpdateFailure::from(UpdateError::SizeMismatch {
                expected: asset.size,
                actual: received,
            }));
        }
        file.write_all(&buf[..n]).map_err(|_| {
            UpdateFailure::new(
                UpdateErrorKind::Io,
                "The download could not be saved, so nothing was changed.",
            )
        })?;
    }
    if received != asset.size {
        return Err(UpdateFailure::from(UpdateError::SizeMismatch {
            expected: asset.size,
            actual: received,
        }));
    }
    Ok(())
}

/// The worker body: download the checksums, download the binary,
/// verify, swap. Every failure returns a typed [`UpdateFailure`]
/// and leaves the running binary untouched.
fn download_verify_swap(
    asset: &core::update::ReleaseAsset,
    sums_asset: &core::update::ReleaseAsset,
    sig_asset: &core::update::ReleaseAsset,
) -> Result<PathBuf, UpdateFailure> {
    let (target, kind) = install_location();
    if kind == InstallKind::SystemPackage {
        return Err(UpdateFailure::new(
            UpdateErrorKind::SystemPackage,
            "This copy was installed by your system, so it has to be updated from the release page.",
        ));
    }
    let agent = http_agent(Duration::from_secs(600));
    // 1. The checksums for this exact release, and their signature.
    // The signature is verified against the embedded release key
    // before the checksums are trusted with anything (SEC-F04):
    // checksums fetched from the same origin as the binary only
    // prove the download was not corrupted, never who published it.
    let sums_text = fetch_sha256sums(&agent, sums_asset)?;
    let sig_text = fetch_sha256sums(&agent, sig_asset)?;
    if !core::update::verify_sums_signature(sums_text.as_bytes(), &sig_text) {
        return Err(UpdateFailure::new(
            UpdateErrorKind::SignatureInvalid,
            "The checksum file's signature does not match the release key, so the update was refused.",
        ));
    }
    let sums = core::update::parse_sha256sums(&sums_text);
    let expected = core::update::expected_hash(&sums, &asset.name)
        .ok_or_else(|| {
            UpdateFailure::new(
                UpdateErrorKind::BadChecksumFile,
                "The checksum file does not list this download, so nothing was changed.",
            )
        })?
        .to_owned();
    // 2. The binary itself, into the updates folder in the data dir.
    let dir = backend::data_dir().join("updates");
    std::fs::create_dir_all(&dir).map_err(|_| {
        UpdateFailure::new(
            UpdateErrorKind::Io,
            "Zaprust could not create its updates folder, so nothing was changed.",
        )
    })?;
    let part_path = dir.join(format!("{}.part", asset.name));
    let staged_path = dir.join(&asset.name);
    if let Err(e) = download_asset(&agent, asset, &part_path) {
        let _ = std::fs::remove_file(&part_path);
        return Err(e);
    }
    std::fs::rename(&part_path, &staged_path).map_err(|_| {
        UpdateFailure::new(
            UpdateErrorKind::Io,
            "The download could not be saved, so nothing was changed.",
        )
    })?;
    // 3. Verify before anything near the running binary happens.
    // A mismatch keeps its own typed kind (possible tampering),
    // distinct from a read failure (broken local file).
    let actual = core::update::sha256_file(&staged_path).map_err(UpdateFailure::from)?;
    if actual != expected {
        let _ = std::fs::remove_file(&staged_path);
        return Err(UpdateFailure::from(UpdateError::ChecksumMismatch { expected, actual }));
    }
    // 4. Swap. The core swap restores the old binary if it fails.
    core::update::swap_binary(&target, &staged_path).map_err(|e| match e {
        UpdateError::Io(_) => UpdateFailure::new(
            UpdateErrorKind::Io,
            "The new version could not be put in place. Your current version is unchanged.",
        ),
        other => UpdateFailure::from(other),
    })?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_failures_keep_their_kinds() {
        // The whole point of the typed path: these three failures
        // must stay distinguishable after crossing the worker
        // channel, where they used to collapse into bare strings.
        let checksum = UpdateFailure::from(UpdateError::ChecksumMismatch {
            expected: "a".repeat(64),
            actual: "b".repeat(64),
        });
        assert_eq!(checksum.kind, UpdateErrorKind::ChecksumMismatch);
        assert!(checksum.user_message().contains("did not match the published checksum"));

        let no_asset = UpdateFailure::from(UpdateError::NoAssetForPlatform);
        assert_eq!(no_asset.kind, UpdateErrorKind::NoAssetForPlatform);

        let untrusted = UpdateFailure::from(UpdateError::UntrustedAssetUrl);
        assert_eq!(untrusted.kind, UpdateErrorKind::UntrustedAssetUrl);

        let size = UpdateFailure::from(UpdateError::SizeMismatch { expected: 10, actual: 11 });
        assert_eq!(size.kind, UpdateErrorKind::SizeMismatch);

        assert_ne!(checksum.kind, no_asset.kind);
        assert_ne!(checksum.kind, size.kind);
    }

    #[test]
    fn failure_display_is_the_user_message() {
        let f = UpdateFailure::new(UpdateErrorKind::WorkerStopped, "try again");
        assert_eq!(f.to_string(), "try again");
        assert_eq!(f.user_message(), "try again");
    }
}
