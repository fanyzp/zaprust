//! Crash reports, the privacy-first half of crash safety.
//!
//! When the app panics, the panic hook (installed in the app's
//! `main`) builds a [`CrashReport`] and writes it as JSON into
//! `<data dir>/crashes/`. On the next start the app offers to show
//! and report it.
//!
//! **What a report contains:** app version, OS/arch, timestamp, the
//! panic message, the panic location, and a backtrace.
//! **What it never contains:** message bodies, phone numbers, chat
//! data, session material, or credentials, the format has no field
//! that could hold them, and every string is passed through
//! [`redact_home`] before it is written, so the user's home path
//! shows up as `~`, never verbatim.
//!
//! The workspace deliberately keeps the default `panic = "unwind"`
//! strategy: containment (supervisor + `catch_unwind` guards) only
//! works with unwinding. `panic = "abort"` is never set.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// GitHub repository slug (`owner/repo`) used for the
/// "Report on GitHub" flow.
///
/// This is the project's official public repository
/// (github.com/fanyzp/zaprust). It stays a single constant so a
/// future move or rename touches exactly one place.
pub const GITHUB_REPO: &str = "fanyzp/zaprust";

/// Issue template used for crash reports (`.github/ISSUE_TEMPLATE/`).
pub const CRASH_TEMPLATE: &str = "crash-report.md";
/// Issue template used for manual bug reports.
pub const BUG_TEMPLATE: &str = "bug-report.md";

/// One crash report. Serialized as JSON under `crashes/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrashReport {
    /// App version that crashed.
    pub app_version: String,
    /// Operating system (`std::env::consts::OS`).
    pub os: String,
    /// CPU architecture (`std::env::consts::ARCH`).
    pub arch: String,
    /// Unix timestamp (seconds) of the crash.
    pub timestamp_unix: i64,
    /// The panic message (redacted).
    pub panic_message: String,
    /// `file:line:column` of the panic, when known (redacted).
    pub location: Option<String>,
    /// Captured backtrace (redacted; may be a note when unavailable).
    pub backtrace: String,
}

impl CrashReport {
    /// Builds a report from panic parts. Secret-shaped values are
    /// scrubbed HERE, at capture (SEC-15), not only inside
    /// [`CrashReport::write_to`]: the panic hook holds the original
    /// report and prints `report.summary()` to stderr, so the
    /// in-memory report must already be redacted or stderr would
    /// leak what the crash file correctly scrubbed. Home-path
    /// redaction still happens in `write_to` / [`CrashReport::redact`],
    /// which know the home directory.
    #[must_use]
    pub fn capture(panic_message: String, location: Option<String>, backtrace: String) -> Self {
        Self {
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            timestamp_unix: unix_now(),
            panic_message: scrub_secrets(&panic_message),
            location: location.as_deref().map(scrub_secrets),
            backtrace: scrub_secrets(&backtrace),
        }
    }

    /// One-line summary for banners: message + location.
    ///
    /// The returned text is redacted (secret shapes, phone-like
    /// runs, and the current user's home prefix when it can be
    /// determined from the environment), because the panic hook
    /// prints exactly this string to stderr (SEC-15): stderr and
    /// the crash file must carry the same redacted text.
    #[must_use]
    pub fn summary(&self) -> String {
        let raw = match &self.location {
            Some(loc) => format!("{} (at {loc})", self.panic_message),
            None => self.panic_message.clone(),
        };
        scrub_for_display(&raw)
    }

    /// The full human-readable report text (View details / issue body
    /// base). Fields are scrubbed again here so the invariant holds
    /// even for a report that was constructed or loaded without
    /// going through [`CrashReport::capture`] / `write_to`.
    #[must_use]
    pub fn full_text(&self) -> String {
        scrub_for_display(&format!(
            "Zaprust {version} crash report\nOS: {os} ({arch})\nTime (unix): {ts}\nPanic: {msg}\nLocation: {loc}\n\nBacktrace:\n{bt}",
            version = self.app_version,
            os = self.os,
            arch = self.arch,
            ts = self.timestamp_unix,
            msg = self.panic_message,
            loc = self.location.as_deref().unwrap_or("unknown"),
            bt = self.backtrace,
        ))
    }

    /// Redacts the home directory prefix in every free-text field.
    pub fn redact(&mut self, home: &Path) {
        self.panic_message = scrub(&self.panic_message, home);
        self.location = self.location.as_deref().map(|l| scrub(l, home));
        self.backtrace = scrub(&self.backtrace, home);
    }

    /// Writes the report as `crash-<timestamp>-<pid>-<counter>.json`
    /// inside `dir` (created if needed), after redacting `home` if
    /// given, and prunes the directory to the newest
    /// [`MAX_KEPT_REPORTS`] reports.
    ///
    /// # Errors
    /// Returns I/O and serialization errors; the panic hook treats a
    /// failed write as non-fatal (stderr still gets the panic).
    pub fn write_to(mut self, dir: &Path, home: Option<&Path>) -> std::io::Result<PathBuf> {
        // The secret scrub always runs, even when the home dir is
        // unknown: a report must never be written with raw key or
        // token shapes in its free-text fields.
        if let Some(home) = home {
            self.redact(home);
        } else {
            self.panic_message = scrub_secrets(&self.panic_message);
            self.location = self.location.as_deref().map(scrub_secrets);
            self.backtrace = scrub_secrets(&self.backtrace);
        }
        std::fs::create_dir_all(dir)?;
        let path = unique_report_path(dir, self.timestamp_unix);
        let json = serde_json::to_string_pretty(&self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, json)?;
        // Owner-only from the start (0600 on Unix): a report is
        // exactly the kind of file a user is asked to share, and
        // the startup hardening pass should not be its only
        // protection (privacy audit A4/A5).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        prune_reports(dir);
        Ok(path)
    }

    /// Loads a report from a JSON file.
    ///
    /// The fields are re-redacted on load (SEC-15): a crash file is
    /// not trusted as already-redacted just because it is on disk,
    /// it may come from an older build or have been placed in
    /// `crashes/` by hand, and it is displayed and offered for
    /// one-click reporting as-is.
    ///
    /// # Errors
    /// Returns I/O and parse errors.
    pub(crate) fn read_from(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut report: Self = serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        report.panic_message = scrub_secrets(&report.panic_message);
        report.location = report.location.as_deref().map(scrub_secrets);
        report.backtrace = scrub_secrets(&report.backtrace);
        Ok(report)
    }
}

/// Crash reports kept on disk: writing a new one prunes the
/// oldest beyond this count, so a crash loop (start, panic,
/// restart) cannot grow the `crashes/` directory without bound.
const MAX_KEPT_REPORTS: usize = 10;

/// A crash report path that no existing report uses. The name
/// keeps the unix-seconds prefix (so lexical order stays
/// chronological) and adds pid + a process counter, the naming
/// precedent the pasted-image temp files use: two crashes in the
/// same second, or from two app instances, get distinct files
/// instead of the second silently overwriting the first.
fn unique_report_path(dir: &Path, timestamp_unix: i64) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("crash-{timestamp_unix}-{}-{n}.json", std::process::id()));
        if !path.exists() {
            return path;
        }
    }
}

/// Deletes all but the newest [`MAX_KEPT_REPORTS`] crash reports
/// in `dir`. Best effort: a report that cannot be listed or
/// removed is left for the next write to retry.
fn prune_reports(dir: &Path) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("crash-") && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    if files.len() > MAX_KEPT_REPORTS {
        for old in &files[..files.len() - MAX_KEPT_REPORTS] {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// Home redaction plus the secret scrubber: panic payloads are
/// free text and could interpolate a secret, so crash reports get
/// the same treatment as log lines before they are written or
/// shared. Also applied to every debug log line by
/// [`crate::debug::ScrubWriter`].
pub(crate) fn scrub(text: &str, home: &Path) -> String {
    scrub_secrets(&redact_home(text, home))
}

/// Secret scrub for crash text: the shared debug scrubber plus the
/// shapes it does not cover (SEC-14, crash-side layer).
///
/// `crate::debug::redact` requires a literal `=` for labelled
/// assignments, so JSON-shaped secrets survive it:
/// `"api_key": "..."`, `"apiKey": "..."`, `"apikey": "..."`.
/// This layer masks those quoted-key values, `apiKey=`/`secret=`/
/// `client_secret=` spellings, and `Authorization: Basic ...`
/// values (the debug scrubber already handles `Bearer`).
pub(crate) fn scrub_secrets(text: &str) -> String {
    let s = redact_json_secrets(text);
    let s = redact_extended_assignments(&s);
    let s = redact_basic_tokens(&s);
    let s = crate::debug::redact(&s);
    // A second JSON pass: cheap, and guarantees no quoted secret
    // value survives whatever the earlier passes rewrote.
    redact_json_secrets(&s)
}

/// Scrub for text that is about to be DISPLAYED or printed (the
/// panic hook's stderr line, the crash banner, the details view,
/// and the backend supervisor's `BackendEvent::Crashed` summary,
/// which the app renders verbatim in the connection status):
/// secret shapes and phone-like numbers are masked, plus the
/// home prefix when the environment names one (`HOME`, or
/// `USERPROFILE` on Windows). It does not mask names or message
/// text; it is a secrets-and-identifiers scrub, not a content
/// filter.
#[must_use]
pub fn scrub_for_display(text: &str) -> String {
    let mut out = text.to_owned();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        if !home.is_empty() {
            out = redact_home(&out, Path::new(&home));
        }
    }
    scrub_secrets(&out)
}

/// Index of the closing quote for a `"` opening at `start`
/// (backslash escapes are skipped), or `None` if unterminated.
fn closing_quote(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Masks one JSON string value: a leading `Bearer `/`Basic ` scheme
/// is kept (so logs still show the auth kind), the credential
/// itself never survives.
fn mask_json_value(value: &str) -> String {
    let lower = value.to_lowercase();
    for scheme in ["bearer ", "basic "] {
        if lower.starts_with(scheme) {
            return format!("{}***", &value[..scheme.len() - 1]);
        }
    }
    "***".to_owned()
}

/// Masks values of JSON members whose quoted key names a secret:
/// `"api_key": "..."`, `"apiKey": "..."`, `"x-api-key": "..."`,
/// `"authorization": "Basic ..."`, and unquoted variants.
fn redact_json_secrets(text: &str) -> String {
    const SECRET_KEYS: &[&str] = &[
        "api_key",
        "api-key",
        "apikey",
        "x-api-key",
        "token",
        "access_token",
        "refresh_token",
        "password",
        "secret",
        "client_secret",
        "authorization",
        "auth",
        "pair_code",
    ];
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0_usize;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if let Some(end) = closing_quote(bytes, i) {
                let key = text[i + 1..end].to_lowercase();
                if SECRET_KEYS.contains(&key.as_str()) {
                    let mut j = end + 1;
                    while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b':' {
                        // Copy `"key":` verbatim, then the spacing.
                        out.push_str(&text[i..=j]);
                        i = j + 1;
                        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                            out.push(bytes[i] as char);
                            i += 1;
                        }
                        if i < bytes.len() && bytes[i] == b'"' {
                            if let Some(vend) = closing_quote(bytes, i) {
                                out.push('"');
                                out.push_str(&mask_json_value(&text[i + 1..vend]));
                                out.push('"');
                                i = vend + 1;
                                continue;
                            }
                        } else {
                            // Unquoted value: up to a delimiter.
                            let start = i;
                            while i < bytes.len()
                                && !matches!(bytes[i], b',' | b'}' | b']' | b'\n')
                                && !(bytes[i] as char).is_whitespace()
                            {
                                i += 1;
                            }
                            if i > start {
                                out.push_str("***");
                                continue;
                            }
                        }
                        continue;
                    }
                }
                out.push_str(&text[i..=end]);
                i = end + 1;
                continue;
            }
        }
        let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Masks `key=value` / `key: value` assignments for spellings the
/// debug scrubber's list does not include (`apiKey`, `api-key`,
/// `secret`, `client_secret`, `auth`, `authorization`).
fn redact_extended_assignments(text: &str) -> String {
    let mut masked = text.to_owned();
    for key in [
        "apikey",
        "api_key",
        "api-key",
        "x-api-key",
        "client_secret",
        "secret",
        "auth",
        "authorization",
    ] {
        let mut search_from = 0;
        while search_from < masked.len() {
            // ASCII case-insensitive search over the ORIGINAL
            // string: offsets computed on a `to_lowercase()` copy
            // shift whenever a character's lowercase form has a
            // different byte length (e.g. `İ`), landing the mask
            // on the wrong span or panicking inside the panic hook.
            let Some(key_start) = crate::rich::find_ci(&masked, key, search_from) else {
                break;
            };
            let key_end = key_start + key.len();
            // The key must stand alone (not be part of a longer
            // word) and be followed by `=` or `:`.
            let before_ok = key_start == 0
                || !masked.as_bytes()[key_start - 1].is_ascii_alphanumeric()
                    && !matches!(masked.as_bytes()[key_start - 1], b'_' | b'-');
            let bytes = masked.as_bytes();
            let mut j = key_end;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
                j += 1;
            }
            if !before_ok || j >= bytes.len() || !matches!(bytes[j], b'=' | b':') {
                search_from = key_end;
                continue;
            }
            j += 1;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b'"' | b'\'') {
                j += 1;
            }
            // For an Authorization value, keep a Bearer/Basic
            // scheme word and mask only the credential after it.
            // Compared on the original string, ASCII-insensitively,
            // for the same offset-safety reason as the key search
            // above (`j` only ever advances over ASCII bytes here,
            // so it stays on a char boundary).
            if key == "authorization" {
                for scheme in ["bearer ", "basic "] {
                    let head = masked[j..].get(..scheme.len());
                    if head.is_some_and(|h| h.eq_ignore_ascii_case(scheme)) {
                        j += scheme.len();
                        break;
                    }
                }
            }
            let value_start = j;
            let mut value_end = value_start;
            let b = masked.as_bytes();
            while value_end < b.len()
                && (b[value_end].is_ascii_alphanumeric()
                    || matches!(b[value_end], b'-' | b'_' | b'.' | b'+' | b'/' | b'=' | b'~'))
            {
                value_end += 1;
            }
            if value_end == value_start {
                search_from = key_end;
                continue;
            }
            masked.replace_range(value_start..value_end, "***");
            search_from = value_start + 3;
        }
    }
    masked
}

/// Masks a standalone `Basic <token>` credential (HTTP Basic auth
/// outside an explicit `Authorization:` label): the token must be
/// at least 8 base64-shaped characters, so ordinary prose like
/// "basic usage" is untouched.
fn redact_basic_tokens(text: &str) -> String {
    let mut masked = text.to_owned();
    let mut search_from = 0;
    while search_from < masked.len() {
        // Same ASCII-insensitive search as the assignment
        // scrubber: no offsets from a length-changing copy.
        let Some(pos) = crate::rich::find_ci(&masked, "basic ", search_from) else {
            break;
        };
        let token_start = pos + "basic ".len();
        let b = masked.as_bytes();
        let mut token_end = token_start;
        while token_end < b.len()
            && (b[token_end].is_ascii_alphanumeric() || matches!(b[token_end], b'+' | b'/' | b'='))
        {
            token_end += 1;
        }
        if token_end - token_start >= 8 {
            masked.replace_range(token_start..token_end, "***");
            search_from = token_start + 3;
        } else {
            search_from = token_start;
        }
    }
    masked
}

/// Replaces every occurrence of the `home` path prefix in `text`
/// with `~`.
#[must_use]
pub(crate) fn redact_home(text: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    if home.is_empty() || home == "/" {
        return text.to_owned();
    }
    text.replace(home.as_ref(), "~")
}

/// The most recent crash report in `dir`, if any parses.
#[must_use]
pub(crate) fn latest_report(dir: &Path) -> Option<CrashReport> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("crash-")))
        .collect();
    files.sort();
    files.iter().rev().find_map(|p| CrashReport::read_from(p).ok())
}

/// The latest report the user has not yet dismissed, if any.
/// Dismissal is recorded in a `seen` file holding the timestamp of
/// the newest dismissed report.
#[must_use]
pub fn unseen_report(dir: &Path) -> Option<CrashReport> {
    let report = latest_report(dir)?;
    let seen = std::fs::read_to_string(dir.join("seen"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if report.timestamp_unix > seen {
        Some(report)
    } else {
        None
    }
}

/// Records dismissal of every report up to `timestamp_unix`.
///
/// # Errors
/// Returns I/O errors.
pub fn mark_seen(dir: &Path, timestamp_unix: i64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("seen"), timestamp_unix.to_string())
}

/// Builds the prefilled GitHub "new issue" URL for a crash report.
///
/// The body carries version, OS, the redacted summary and backtrace,
/// plus a redaction note. It contains nothing the report itself does
/// not contain, and the report contains no user data by design.
#[must_use]
pub fn crash_issue_url(report: &CrashReport) -> String {
    let title = format!("Crash: {}", report.summary());
    let body = format!(
        "Zaprust crashed. Details from the local crash report below.\n\n\
         - App version: {version}\n- OS: {os} ({arch})\n- Panic: {msg}\n- Location: {loc}\n\n\
         <details><summary>Backtrace</summary>\n\n```\n{bt}\n```\n\n</details>\n\n\
         Privacy note: this report was generated locally and redacted automatically \
         (home paths replaced with ~). It contains no message contents, phone numbers, \
         or session data. Please check it once more before submitting.",
        version = report.app_version,
        os = report.os,
        arch = report.arch,
        msg = scrub_secrets(&report.panic_message),
        loc = report.location.as_deref().unwrap_or("unknown"),
        bt = scrub_secrets(&report.backtrace),
    );
    issue_url(CRASH_TEMPLATE, &title, &body)
}

/// Builds the prefilled GitHub "new issue" URL for a manual bug
/// report (no crash data, version/OS context only).
#[must_use]
pub fn bug_issue_url() -> String {
    let body = format!(
        "What happened:\n\n\nSteps to reproduce:\n1. \n\nExpected:\n\n\n\
         - App version: {version}\n- OS: {os} ({arch})\n",
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
    );
    issue_url(BUG_TEMPLATE, "Bug report", &body)
}

fn issue_url(template: &str, title: &str, body: &str) -> String {
    format!(
        "https://github.com/{GITHUB_REPO}/issues/new?template={template}&title={}&body={}",
        percent_encode(title),
        percent_encode(body),
    )
}

/// Percent-encodes a URL query component (unreserved characters pass
/// through, everything else becomes `%XX`, space is `%20`).
#[must_use]
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl fmt::Display for CrashReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zaprust-crash-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn report_round_trips_and_redacts_home() {
        let dir = temp_dir("roundtrip");
        let home = Path::new("/home/someone");
        let report = CrashReport::capture(
            "index out of bounds in /home/someone/app".to_owned(),
            Some("/home/someone/src/main.rs:10:5".to_owned()),
            "frame at /home/someone/lib.rs".to_owned(),
        );
        let ts = report.timestamp_unix;
        let path = report.write_to(&dir, Some(home)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("/home/someone"), "home path must be scrubbed: {text}");
        assert!(text.contains("~/app"), "redaction replaces the prefix with ~");
        let back = CrashReport::read_from(&path).unwrap();
        assert_eq!(back.timestamp_unix, ts);
        assert!(back.panic_message.contains("~/app"));
        assert!(back.location.unwrap().starts_with("~/src"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unseen_report_respects_dismissal() {
        let dir = temp_dir("seen");
        let report = CrashReport::capture("boom".to_owned(), None, "bt".to_owned());
        let ts = report.timestamp_unix;
        report.write_to(&dir, None).unwrap();
        assert!(unseen_report(&dir).is_some());
        mark_seen(&dir, ts).unwrap();
        assert!(unseen_report(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn crash_issue_url_is_prefilled_and_clean() {
        let report = CrashReport::capture(
            "boom".to_owned(),
            Some("src/x.rs:1:1".to_owned()),
            "bt".to_owned(),
        );
        let url = crash_issue_url(&report);
        assert!(url.starts_with("https://github.com/fanyzp/zaprust/issues/new?"));
        assert!(url.contains("template=crash-report.md"));
        assert!(url.contains("Crash%3A%20boom"), "title is encoded: {url}");
        assert!(!url.contains(' '), "no raw spaces in a URL");
    }

    #[test]
    fn capture_redacts_json_shaped_and_basic_auth_secrets() {
        // SEC-14: JSON spellings and Basic auth must not survive,
        // in the in-memory report, its summary, or the file.
        let report = CrashReport::capture(
            r#"failed with {"api_key": "jsonsecret123", "apiKey": "camelSecret456", "apikey": "lowerSecret789"} and Authorization: Basic dXNlcjpwYXNzd29yZA=="#.to_owned(),
            None,
            "bt".to_owned(),
        );
        for text in [report.panic_message.clone(), report.summary(), report.full_text()] {
            assert!(!text.contains("jsonsecret123"), "{text}");
            assert!(!text.contains("camelSecret456"), "{text}");
            assert!(!text.contains("lowerSecret789"), "{text}");
            assert!(!text.contains("dXNlcjpwYXNzd29yZA=="), "{text}");
        }
        // Existing shapes stay covered (Bearer via the debug scrubber).
        let report = CrashReport::capture(
            "Authorization: Bearer abcDEF123456789".to_owned(),
            None,
            "bt".to_owned(),
        );
        assert!(!report.summary().contains("abcDEF123456789"), "{}", report.summary());
        assert!(report.summary().contains("Bearer ***"), "{}", report.summary());
    }

    #[test]
    fn read_from_reredacts_a_hand_placed_report() {
        // SEC-15: a crash file that was NOT produced by the current
        // write_to (older build / hand-placed) is scrubbed on load.
        let dir = temp_dir("reredact");
        let path = dir.join("crash-1.json");
        let raw = serde_json::json!({
            "app_version": "0.0.0",
            "os": "linux",
            "arch": "x86_64",
            "timestamp_unix": 1,
            "panic_message": "boom {\"api_key\": \"plantedsecret42\"}",
            "location": null,
            "backtrace": "bt",
        });
        std::fs::write(&path, raw.to_string()).unwrap();
        let loaded = CrashReport::read_from(&path).unwrap();
        assert!(!loaded.panic_message.contains("plantedsecret42"), "{}", loaded.panic_message);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn percent_encoding_keeps_unreserved() {
        assert_eq!(percent_encode("a b&c~d"), "a%20b%26c~d");
    }

    #[test]
    fn scrub_survives_length_changing_unicode_before_a_secret() {
        // `İ` lowercases to a longer byte string. Offsets computed
        // on a lowercased copy used to shift every later span: the
        // mask landed on the wrong bytes (secret survives) or
        // `replace_range` panicked, inside the panic hook itself.
        let out = scrub_secrets("İstanbul apiKey=supersecretvalue99");
        assert!(!out.contains("supersecretvalue99"), "{out}");
        assert!(out.contains("apiKey=***"), "{out}");
        let out = scrub_secrets("İ Basic dXNlcjpwYXNzd29yZA==");
        assert!(!out.contains("dXNlcjpwYXNzd29yZA=="), "{out}");
        // Degenerate inputs pass through untouched.
        assert_eq!(scrub_secrets(""), "");
        assert_eq!(scrub_secrets("İıß"), "İıß");
    }

    fn report_at(timestamp_unix: i64) -> CrashReport {
        CrashReport {
            app_version: "0.0.0".to_owned(),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            timestamp_unix,
            panic_message: "boom".to_owned(),
            location: None,
            backtrace: "bt".to_owned(),
        }
    }

    #[test]
    fn same_second_reports_do_not_overwrite_each_other() {
        let dir = temp_dir("same-second");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = report_at(1_700_000_000).write_to(&dir, None).unwrap();
        let second = report_at(1_700_000_000).write_to(&dir, None).unwrap();
        assert_ne!(first, second);
        assert!(first.exists() && second.exists(), "both reports survive");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_are_pruned_to_the_newest_few() {
        let dir = temp_dir("prune");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..MAX_KEPT_REPORTS as i64 + 3 {
            report_at(1_700_000_000 + i).write_to(&dir, None).unwrap();
        }
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("crash-"))
            .collect();
        assert_eq!(names.len(), MAX_KEPT_REPORTS, "{names:?}");
        let newest = format!("crash-{}-", 1_700_000_000 + MAX_KEPT_REPORTS as i64 + 2);
        assert!(names.iter().any(|n| n.starts_with(&newest)), "newest kept: {names:?}");
        assert!(
            !names.iter().any(|n| n.starts_with("crash-1700000000-")),
            "oldest pruned: {names:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
