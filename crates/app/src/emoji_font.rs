//! Discovery of the device's own color emoji font.
//!
//! Emoji are content, and every platform already ships a color
//! emoji typeface the user expects to see: Noto Color Emoji on
//! Linux, Segoe UI Emoji on Windows, Apple Color Emoji on macOS.
//! The app looks that font up on THIS device and registers it
//! with egui as a fallback in the proportional family (see
//! `theme::font_definitions`), so the picker, messages and chat
//! rows render the device's real color emoji instead of the
//! bundled monochrome glyphs. Nothing is bundled and no path
//! is assumed to exist: when no color emoji font is found (or it
//! cannot be read), the app quietly keeps the monochrome fallback
//! and starts exactly as before.
//!
//! The lookup is LAZY (qc1636 Step 7): the font file runs to
//! 10-24 MB on a typical install, and reading it (plus the
//! fontconfig lookup) used to sit on the startup path before
//! the first frame. Now `theme::install_fonts` starts without
//! it, and [`poll_lazy_install`] runs the read on a worker
//! thread once the first frames are painting, registering the
//! face when the bytes land. Emoji painted before that point
//! use the monochrome fallback for those frames only.

use std::path::PathBuf;

/// The egui font name the discovered font is registered under.
pub(crate) const FONT_NAME: &str = "system-color-emoji";

/// Well-known install locations of the platform color emoji font,
/// in the order they should be tried. Linux packaging varies the
/// most, so it gets the distro layouts plus the per-user font dir.
fn candidate_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let mut paths: Vec<PathBuf> = [
            "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
            "/usr/share/fonts/google-noto-emoji/NotoColorEmoji.ttf",
            "/usr/share/fonts/google-noto-color-emoji/NotoColorEmoji.ttf",
            "/usr/share/fonts/noto-emoji/NotoColorEmoji.ttf",
            "/usr/share/fonts/NotoColorEmoji.ttf",
            "/usr/local/share/fonts/NotoColorEmoji.ttf",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        if let Some(home) = std::env::var_os("HOME") {
            paths.push(PathBuf::from(home).join(".local/share/fonts/NotoColorEmoji.ttf"));
        }
        paths
    }
    #[cfg(target_os = "windows")]
    {
        vec![PathBuf::from(r"C:\Windows\Fonts\seguiemj.ttf")]
    }
    #[cfg(target_os = "macos")]
    {
        vec![PathBuf::from("/System/Library/Fonts/Apple Color Emoji.ttc")]
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// Asks fontconfig which file backs the generic `emoji` family
/// (Linux only). This honours the user's own fontconfig setup, so
/// a device whose emoji font lives somewhere unusual (Twemoji,
/// a user-installed Noto) is still found. `None` when fontconfig
/// is absent or answers with something unusable.
#[cfg(target_os = "linux")]
fn fc_match_emoji() -> Option<PathBuf> {
    let output =
        std::process::Command::new("fc-match").args(["-f", "%{file}", "emoji"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_fc_match(&String::from_utf8_lossy(&output.stdout))
}

/// The pure half of [`fc_match_emoji`]: fontconfig prints the
/// resolved file path with no decoration under `-f %{file}`;
/// anything empty or not path-like is rejected.
#[cfg(target_os = "linux")]
fn parse_fc_match(text: &str) -> Option<PathBuf> {
    let path = text.trim();
    if path.is_empty() || !path.starts_with('/') {
        return None;
    }
    Some(PathBuf::from(path))
}

/// True when the bytes carry a font container signature egui's
/// rasterizer (skrifa) can open: a TrueType/OpenType face or a
/// TrueType collection. A file that fails this is not registered
/// at all, so a corrupt or unrelated file at a candidate path can
/// never poison the font set.
fn looks_like_font(bytes: &[u8]) -> bool {
    matches!(bytes.get(0..4), Some([0x00, 0x01, 0x00, 0x00] | b"true" | b"OTTO" | b"ttcf"))
}

/// Loads the device's color emoji font bytes, or `None` when the
/// device has none we can use. The known install locations are
/// tried first (they are unambiguous color fonts), then
/// fontconfig's own answer on Linux. A candidate that exists but
/// cannot be read or parsed is logged and skipped, never fatal:
/// starting the app matters more than coloring its emoji.
pub(crate) fn load_system_color_emoji() -> Option<Vec<u8>> {
    let mut candidates = candidate_paths();
    #[cfg(target_os = "linux")]
    if let Some(path) = fc_match_emoji() {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) if looks_like_font(&bytes) => return Some(bytes),
            Ok(_) => {
                eprintln!(
                    "zaprust: {} exists but is not a readable font, ignoring it",
                    path.display()
                );
            }
            Err(e) => {
                eprintln!("zaprust: could not read {}: {e}", path.display());
            }
        }
    }
    None
}

/// The lazy install's state: the device read runs at most
/// once per process, on its own thread, and the registration
/// happens at most once, on the UI thread, when the bytes land.
enum LazyEmoji {
    NotStarted,
    Loading(std::sync::mpsc::Receiver<Option<Vec<u8>>>),
    Done,
}

static LAZY_EMOJI: std::sync::Mutex<LazyEmoji> = std::sync::Mutex::new(LazyEmoji::NotStarted);

/// Drives the lazy color-emoji install. Called every frame
/// from the shell, and by the emoji picker when it opens (the
/// first-need surface: if the read has somehow not started by
/// then, opening the picker starts it). The first call spawns
/// the read on a worker thread; a later call that finds the
/// bytes registers them through the same font definitions the
/// startup install used, so color emoji render from that frame
/// on with no second load. A device with no usable font (or a
/// worker that could not start) settles the state with nothing
/// installed and is never retried: the monochrome fallback is
/// the designed outcome there, not an error.
pub(crate) fn poll_lazy_install(ctx: &egui::Context) {
    let mut state = LAZY_EMOJI.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match &mut *state {
        LazyEmoji::Done => {}
        LazyEmoji::NotStarted => {
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("zaprust-emoji-font".to_owned()).spawn(
                move || {
                    let _ = tx.send(load_system_color_emoji());
                },
            );
            *state = match spawned {
                Ok(_) => LazyEmoji::Loading(rx),
                Err(_) => LazyEmoji::Done,
            };
        }
        LazyEmoji::Loading(rx) => match rx.try_recv() {
            Ok(Some(bytes)) => {
                ctx.set_fonts(crate::theme::font_definitions(Some(bytes)));
                *state = LazyEmoji::Done;
            }
            Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                *state = LazyEmoji::Done;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        },
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_font;

    #[test]
    fn font_magic_is_recognized() {
        assert!(looks_like_font(&[0x00, 0x01, 0x00, 0x00, 1, 2]));
        assert!(looks_like_font(b"ttcf...."));
        assert!(looks_like_font(b"OTTO...."));
        assert!(!looks_like_font(b"not a font"));
        assert!(!looks_like_font(&[0x00, 0x01]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fc_match_output_parsing() {
        use super::parse_fc_match;
        assert_eq!(
            parse_fc_match("/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf\n"),
            Some(std::path::PathBuf::from("/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf"))
        );
        assert_eq!(parse_fc_match(""), None);
        assert_eq!(parse_fc_match("NotoColorEmoji.ttf"), None);
    }
}
