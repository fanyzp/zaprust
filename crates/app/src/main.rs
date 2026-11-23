//! Zaprust, a fast, native WhatsApp client in Rust and egui.
//!
//! The binary is organised by domain, one module per area:
//!
//! - [`state`], CLI arguments, screens, root app state.
//! - [`shell`], root frame, navigation rail, collapsed strip.
//! - [`chat_list`], search, filters, chat rows.
//! - [`conversation`], header, message stream, composer.
//! - [`bubble`], message bubble rendering.
//! - [`connect`], the link flow (QR / pairing code) + account modal.
//! - [`live`], live mode: backend events → UI data, archive
//!   write-through.
//! - [`crash_ui`], the "crashed last time" banner and report flow.
//! - [`settings`], the full settings page.
//! - [`widgets`] / [`theme`], shared drawing helpers and visuals.
//!
//! All logic (models, themes, filtering) lives in `core` and
//! is tested there; feature logic lives in `crates/features`. The protocol boundary is the
//! `backend` crate; the chat archive is the `store` crate.
//!
//! Modes: live (the default, the real client: link an account or
//! reconnect the stored session) and demo (`--demo`, deterministic
//! dataset, no network). `--fake-backend` runs live mode against
//! the scripted fake backend.
//!
//! Flags: `--theme dark|light|auto`,
//! `--screen chats|channels|groups|settings|connect`,
//! `--collapsed`, `--live`, `--demo`,
//! `--fake-backend`, `--fake-linked`.
#![forbid(unsafe_code)]
// Drawing code works in small pixel spaces by construction, so the
// pedantic cast/name lints are noise in this crate. Everything else,
// including clippy's default warnings, denied in CI, stays on.
#![allow(clippy::cast_precision_loss, clippy::many_single_char_names, clippy::similar_names)]

mod anim;
mod attach;
mod bubble;
mod call_ui;
mod calls;
mod chat_list;
mod community;
mod connect;
mod conversation;
mod countries;
mod crash_ui;
mod debug_ui;
mod demo_avatars;
mod desktop_install;
mod emoji_font;
mod extras;
mod i18n;
mod icons;
mod live;
mod markdown;
mod media;
mod motion;
mod notify;
mod player;
mod portrait;
mod profile;
mod recorder;
mod rich_ui;
mod settings;
mod shell;
mod starred;
mod state;
mod theme;
mod update;
mod updates;
mod video;
mod widgets;

use state::{App, Args};

/// Wraps a debug-log writer so every line passes through the
/// crash-report redaction pipeline before it reaches disk (the
/// user's home prefix included, when the environment names
/// one). Debug logs are the artifact users are asked to share
/// when reporting bugs, so backend and library lines must not
/// persist raw.
fn scrub_log_writer<W: std::io::Write>(inner: W) -> core::debug::ScrubWriter<W> {
    match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        Some(home) if !home.is_empty() => {
            core::debug::ScrubWriter::with_home(inner, std::path::PathBuf::from(home))
        }
        _ => core::debug::ScrubWriter::new(inner),
    }
}

/// Initializes structured tracing to rotating daily log files
/// under the app data dir's debug/ folder (only with --debug).
/// The writer guard is leaked on purpose: logging lives as long
/// as the process. Level filter: ZAPRUST_LOG (e.g. "debug").
fn init_debug_tracing() {
    let dir = backend::data_dir().join("debug");
    let _ = std::fs::create_dir_all(&dir);
    // Prune: keep the 7 most recent log files.
    if let Ok(read) = std::fs::read_dir(&dir) {
        let mut files: Vec<std::path::PathBuf> = read
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "log"))
            .collect();
        files.sort();
        while files.len() > 7 {
            let _ = std::fs::remove_file(files.remove(0));
        }
    }
    let appender = tracing_appender::rolling::daily(&dir, "zaprust-debug.log");
    // The appender creates today's file eagerly, after the
    // startup hardening pass has already run, so tighten the
    // debug dir again now that the fresh file exists (privacy
    // audit C4). A file created by a midnight rotation during a
    // long session still lands with default permissions inside
    // the 0700 data dir and is tightened at the next start.
    #[cfg(unix)]
    tighten_tree(&dir);
    let (writer, guard) = tracing_appender::non_blocking(appender);
    std::mem::forget(guard);
    let writer = scrub_log_writer(writer);
    let filter = std::env::var("ZAPRUST_LOG").unwrap_or_else(|_| "info".to_owned());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .try_init();
}

/// Tightens data-directory permissions on Unix (security audit
/// SEC-06): the directory itself and every directory under it
/// become owner-only (0700), and every file under it becomes
/// 0600. That covers the archive and its sidecars, the secrets
/// and key files, the per-account session databases under
/// `accounts/<id>/` (an earlier version of this pass looked for
/// them one level too shallow and never reached them, and never
/// touched `crashes/` either; privacy audit A4), and every other
/// artifact the app keeps there: settings,
/// feature state, stickers, downloads, exports, pasted temps,
/// crash reports and debug output (A5). Symlinks are not
/// followed. Runs at startup; files created later in the
/// session are tightened on the next start, and the writers of
/// the most sensitive files (secrets, key, downloads, exports,
/// crash reports, JSON state) also set 0600 at creation.
#[cfg(unix)]
fn harden_data_dir() {
    let dir = backend::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    tighten_tree(&dir);
}

/// The recursive tightening behind [`harden_data_dir`]:
/// `dir` to 0700, then the same for everything below it,
/// files to 0600. Best-effort throughout: a file that cannot
/// be tightened is left as it is, never a startup failure.
#[cfg(unix)]
fn tighten_tree(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_symlink() {
            continue;
        } else if file_type.is_dir() {
            tighten_tree(&path);
        } else if file_type.is_file() {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn main() -> eframe::Result {
    install_panic_hook();
    #[cfg(unix)]
    harden_data_dir();
    // Desktop integration (Wayland icon resolution) is
    // user-level state the app maintains itself; see
    // desktop_install for why a bare binary must.
    #[cfg(target_os = "linux")]
    desktop_install::ensure_desktop_integration();
    // Pasted-image temp files from earlier sessions are
    // orphans by construction (the stage is in-memory only);
    // clear them before anything can stage a new paste.
    crate::attach::sweep_stale_pasted_images();
    if std::env::args().any(|a| a == "--debug") {
        init_debug_tracing();
    }
    let args = Args::parse();
    let mut viewport = eframe::egui::ViewportBuilder::default()
        // The default window is one of the standard capture
        // ratios (design standard section 12) and wide enough
        // that the two-pane shell opens with a conversation
        // column at full bubble width: 1440 minus the rail and
        // the list panel leaves about 1000 points for it.
        .with_inner_size([1440.0, 900.0])
        // Phone-like windows are supported (the shell goes
        // single-pane below its breakpoint), so the minimum
        // sits under the narrowest of those sizes instead of
        // forbidding them outright.
        .with_min_inner_size([320.0, 480.0])
        .with_title("Zaprust")
        // The app id must match the desktop file basename
        // (packaging/zaprust.desktop): on Wayland the shell
        // resolves the running window's icon and grouping
        // through that file, not through the window icon, so
        // without it the dock shows a generic placeholder.
        .with_app_id("zaprust");
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "Zaprust",
        options,
        Box::new(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            Ok(Box::new(App::new(args, &cc.egui_ctx)))
        }),
    )
}

/// Installs the global panic hook (crash safety): every panic is
/// captured into a structured, redacted crash report under
/// `<data dir>/crashes/` and echoed to stderr. The hook never
/// re-panics; worker-thread panics are additionally contained by the
/// backend supervisor and the feature guard, so in practice this hook
/// is the last resort for the UI thread itself.
///
/// Maintained invariant (privacy audit C3): the crash scrubber
/// masks secret shapes, phone-like numbers, and the home prefix,
/// but it is NOT a content filter: it does not mask names or
/// message text. Panic messages must therefore never interpolate
/// chat content (message bodies, contact or group names, chat
/// titles). No production panic site does so today (every
/// interpolating `panic!`/`expect`/`assert!` in the workspace is
/// in test code); keep it that way.
/// The window icon: the app's official green chat-bubble mark,
/// the same artwork the desktop file declares, decoded from the
/// bundled PNG. Without it the window wears the toolkit's
/// default icon, which is not Zaprust's mark. A decode failure
/// is not fatal: the app starts without a custom icon rather
/// than not starting at all.
fn window_icon() -> Option<std::sync::Arc<eframe::egui::IconData>> {
    const BYTES: &[u8] = include_bytes!("../../../packaging/icons/zaprust-256.png");
    let img = image::load_from_memory(BYTES).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(std::sync::Arc::new(eframe::egui::IconData { rgba: img.into_raw(), width, height }))
}

/// Builds the crash report for a panic, stamped with THIS
/// crate's version: the report must name the application that
/// crashed (the app binary), not the core crate's package
/// version, which only coincides with it while the workspace
/// version stays unified.
fn capture_report(
    message: String,
    location: Option<String>,
    backtrace: String,
) -> core::crash::CrashReport {
    core::crash::CrashReport::capture(env!("CARGO_PKG_VERSION"), message, location, backtrace)
}

fn install_panic_hook() {
    let crash_dir = backend::data_dir().join("crashes");
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_owned()
        };
        let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let report = capture_report(message, location, backtrace);
        let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
        match report.clone().write_to(&crash_dir, home.as_deref()) {
            Ok(path) => {
                eprintln!("zaprust: panic captured, crash report written to {}", path.display());
            }
            Err(e) => {
                eprintln!("zaprust: panic captured, could not write crash report: {e}");
            }
        }
        eprintln!("zaprust: panic: {}", report.summary());
    }));
}

#[cfg(test)]
mod version_tests {
    /// The panic hook's reports carry the app binary's own
    /// version (this crate's), pinned so a future refactor
    /// cannot quietly restamp them with another crate's.
    #[test]
    fn crash_reports_carry_the_app_version() {
        let report = super::capture_report("boom".to_owned(), None, "bt".to_owned());
        assert_eq!(report.app_version, env!("CARGO_PKG_VERSION"));
    }

    /// The window icon is the bundled 256px mark, decoded:
    /// the window must never fall back to the toolkit default
    /// because the artwork failed to load.
    #[test]
    fn window_icon_is_the_bundled_mark_at_full_size() {
        let icon = super::window_icon().expect("the bundled icon decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::tighten_tree;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    /// The hardening pass (privacy audit A4/A5) reaches the
    /// files the old hand-listed version never did: a session
    /// database nested two levels down under `accounts/<id>/`,
    /// a crash report, and an ordinary artifact, all starting
    /// at permissive default modes.
    #[test]
    fn hardening_reaches_nested_session_db_and_crash_reports() {
        let root = std::env::temp_dir().join(format!("zaprust-harden-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let account = root.join("accounts").join("default");
        let crashes = root.join("crashes");
        std::fs::create_dir_all(&account).expect("account dir");
        std::fs::create_dir_all(&crashes).expect("crashes dir");
        let session = account.join("session.db");
        let report = crashes.join("crash-1.json");
        let settings = root.join("settings.json");
        for f in [&session, &report, &settings] {
            std::fs::write(f, b"x").expect("seed file");
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644)).expect("loosen");
        }
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
            .expect("loosen root");
        tighten_tree(&root);
        assert_eq!(mode_of(&root), 0o700, "the data dir itself is owner-only");
        assert_eq!(mode_of(&account), 0o700, "account dirs are owner-only");
        assert_eq!(mode_of(&session), 0o600, "the nested session database is tightened");
        assert_eq!(mode_of(&report), 0o600, "crash reports are tightened");
        assert_eq!(mode_of(&settings), 0o600, "plain artifacts are tightened");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Privacy audit C4: the rolling appender creates today's
    /// log file when it is built, which is after the startup
    /// hardening pass, so the tightening call in
    /// `init_debug_tracing` is what closes the gap. This mirrors
    /// that sequence against a real appender: the fresh file
    /// exists immediately, and tightening makes it owner-only.
    #[test]
    fn fresh_debug_log_is_tightened_after_appender_creation() {
        let dir =
            std::env::temp_dir().join(format!("zaprust-debuglog-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("debug dir");
        let _appender = tracing_appender::rolling::daily(&dir, "zaprust-debug.log");
        let logs: Vec<std::path::PathBuf> =
            std::fs::read_dir(&dir).expect("read dir").flatten().map(|e| e.path()).collect();
        assert_eq!(logs.len(), 1, "the appender creates today's file eagerly: {logs:?}");
        tighten_tree(&dir);
        assert_eq!(mode_of(&logs[0]), 0o600, "the fresh debug log is owner-only after tightening");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
