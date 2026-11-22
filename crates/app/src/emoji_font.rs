//! Discovery of the device's own color emoji font.
//!
//! Emoji are content, and every platform already ships a color
//! emoji typeface the user expects to see: Noto Color Emoji on
//! Linux, Segoe UI Emoji on Windows, Apple Color Emoji on macOS.
//! At startup the app looks that font up on THIS device and
//! registers it with egui as a fallback in the proportional
//! family (see `theme::install_fonts`), so the picker, messages
//! and chat rows render the device's real color emoji instead of
//! the bundled monochrome glyphs. Nothing is bundled and no path
//! is assumed to exist: when no color emoji font is found (or it
//! cannot be read), the app quietly keeps the monochrome fallback
//! and starts exactly as before.
//!
//! One class of font is deliberately rejected: bitmap-only
//! color fonts (the common Linux Noto Color Emoji is CBDT/CBLC
//! strikes with no outline table at all). egui's rasterizer
//! draws glyph coverage only; it cannot decode bitmap strikes,
//! so every glyph of such a font comes out as a solid block in
//! the text color. On a device whose only emoji font is one of
//! those, registering it turns the whole emoji picker into a
//! grid of black squares (the QA capture defect), while the
//! bundled monochrome fallback renders correctly. Only fonts
//! with real outline glyphs (`glyf` or `CFF `) are registered.

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

/// The offset of the sfnt table directory: 0 for a plain
/// TrueType/OpenType face, or the first face's offset inside a
/// TrueType collection. `None` for anything else.
fn font_dir_offset(bytes: &[u8]) -> Option<usize> {
    match bytes.get(0..4)? {
        [0x00, 0x01, 0x00, 0x00] | b"true" | b"OTTO" => Some(0),
        b"ttcf" => {
            let raw = bytes.get(12..16)?;
            Some(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize)
        }
        _ => None,
    }
}

/// Whether the font carries outline glyphs (`glyf` or `CFF `)
/// that egui's coverage rasterizer can draw. A bitmap-only
/// color font (CBDT/CBLC strikes, no outlines) fails this, by
/// design: registering one renders every emoji as a solid
/// block (see the module docs).
fn has_outline_glyphs(bytes: &[u8]) -> bool {
    let Some(dir) = font_dir_offset(bytes) else { return false };
    let Some(raw) = bytes.get(dir + 4..dir + 6) else { return false };
    let count = usize::from(u16::from_be_bytes([raw[0], raw[1]]));
    (0..count).any(|i| {
        let at = dir + 12 + i * 16;
        matches!(bytes.get(at..at + 4), Some(b"glyf" | b"CFF "))
    })
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
            Ok(bytes) if looks_like_font(&bytes) && has_outline_glyphs(&bytes) => {
                return Some(bytes);
            }
            Ok(_) => {
                eprintln!(
                    "zaprust: {} exists but is not a usable outline font, ignoring it",
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

#[cfg(test)]
mod tests {
    use super::{has_outline_glyphs, looks_like_font};

    /// Builds a minimal synthetic sfnt directory carrying the
    /// given table tags, enough for the directory scan (the
    /// records' checksums and offsets are zero: the scan reads
    /// tags only).
    fn fake_font(tags: &[&str]) -> Vec<u8> {
        let mut bytes = vec![0x00, 0x01, 0x00, 0x00];
        bytes.extend_from_slice(&(tags.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&[0; 6]);
        for tag in tags {
            bytes.extend_from_slice(tag.as_bytes());
            bytes.extend_from_slice(&[0; 12]);
        }
        bytes
    }

    #[test]
    fn outline_fonts_pass_and_bitmap_only_fonts_do_not() {
        // Outline color fonts (the Windows and modern COLR
        // stacks) carry real glyph outlines: registered.
        assert!(has_outline_glyphs(&fake_font(&["cmap", "glyf", "COLR", "CPAL"])));
        assert!(has_outline_glyphs(&fake_font(&["cmap", "CFF "])));
        // The bitmap-only layout of the common Linux color
        // emoji font: CBDT/CBLC strikes and no outline table.
        // Registering it renders solid blocks, so it is refused
        // and the monochrome fallback stays in charge.
        assert!(!has_outline_glyphs(&fake_font(&["CBDT", "CBLC", "cmap", "GSUB"])));
        // Truncated and foreign data never passes.
        assert!(!has_outline_glyphs(&[0x00, 0x01]));
        assert!(!has_outline_glyphs(b"not a font at all"));
    }

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
