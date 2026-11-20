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
mod crash_ui;
mod debug_ui;
mod demo_avatars;
mod emoji_font;
mod extras;
mod i18n;
mod icons;
mod live;
mod markdown;
mod media;
mod motion;
mod notify;
mod profile;
mod recorder;
mod rich_ui;
mod settings;
mod shell;
mod starred;
mod state;
mod theme;
mod update;
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
/// SEC-06): the directory itself becomes owner-only (0700), and the
/// files that hold chat history, session state and credentials
/// (archive, secrets, per-account session databases, crash
/// reports) become 0600. Runs at startup; files created later in
/// the session are tightened on the next start, and the archive is
/// tightened again when the store opens.
#[cfg(unix)]
fn harden_data_dir() {
    use std::os::unix::fs::PermissionsExt;
    let dir = backend::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let sensitive = |p: &std::path::Path| {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    };
    for name in ["archive.db", "archive.db-wal", "archive.db-shm", "secrets.json"] {
        let p = dir.join(name);
        if p.exists() {
            sensitive(&p);
        }
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                for name in ["session.db", "session.db-wal", "session.db-shm"] {
                    let f = p.join(name);
                    if f.exists() {
                        sensitive(&f);
                    }
                }
            }
        }
    }
}

fn main() -> eframe::Result {
    install_panic_hook();
    #[cfg(unix)]
    harden_data_dir();
    // Pasted-image temp files from earlier sessions are
    // orphans by construction (the stage is in-memory only);
    // clear them before anything can stage a new paste.
    crate::attach::sweep_stale_pasted_images();
    if std::env::args().any(|a| a == "--debug") {
        init_debug_tracing();
    }
    let args = Args::parse();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([860.0, 560.0])
            .with_title("Zaprust"),
        ..Default::default()
    };
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
        let report = core::crash::CrashReport::capture(message, location, backtrace);
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
