//! Stickers, a native Zaprust feature.
//!
//! Provides the **Stickers** tab of the composer's media picker.
//! Two kinds of stickers:
//!
//! - a bundled set rendered procedurally at runtime (simple faces
//!   and shapes drawn pixel-by-pixel, no binary assets), and
//! - user-created **text stickers**: text rendered with a tiny 3×5
//!   bitmap font into a transparent PNG, stored under the app data
//!   dir, organised into **packs** the user can create and edit.
//!
//! Clicking a sticker in the picker sends it as an image message.
//! In demo mode the host appends a photo message; in live mode
//! media sending is not part of the backend yet, so the host shows
//! an honest notice instead (see the app integration).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::{Path, PathBuf};

/// The picker tab id this feature registers.
pub const TAB_ID: &str = "stickers";

/// Maximum characters in a text sticker. A sticker shows a few
/// words at most; anything longer is illegible at sticker size.
/// Enforced in [`add_text_sticker`] and again inside
/// [`render_text_sticker`], so no caller can drive an unbounded
/// render.
const MAX_TEXT_CHARS: usize = 120;

/// Maximum wrapped lines on one text sticker.
const MAX_LINES: usize = 16;

/// Maximum characters on one wrapped line of a text sticker.
///
/// Together with [`MAX_LINES`] this bounds the render canvas
/// before allocation: at 56 px per character column and 84 px
/// per line (see [`render_text_sticker`]) the canvas is at most
/// 1358 px per side, under 1.9 million pixels in total, no
/// matter what input arrives.
const MAX_LINE_CHARS: usize = 24;

/// One sticker pack on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pack {
    /// Display name.
    pub name: String,
    /// Directory holding the pack's PNG files.
    pub dir: PathBuf,
    /// Sticker image files, sorted by name.
    pub stickers: Vec<PathBuf>,
}

/// Slugifies a pack or sticker name for filesystem use.
#[must_use]
pub(crate) fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "pack".to_string()
    } else {
        trimmed
    }
}

/// Lists packs under `root` (each subdirectory is a pack).
///
/// # Errors
/// I/O errors other than a missing root (which reads as no packs).
pub fn list_packs(root: &Path) -> Result<Vec<Pack>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut packs = Vec::new();
    let entries = std::fs::read_dir(root).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = std::fs::read_to_string(dir.join("name.txt"))
            .unwrap_or_else(|_| entry.file_name().to_string_lossy().to_string())
            .trim()
            .to_string();
        let mut stickers: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "png"))
                    .collect()
            })
            .unwrap_or_default();
        stickers.sort();
        packs.push(Pack { name, dir, stickers });
    }
    packs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packs)
}

/// Creates a pack directory under `root`.
///
/// # Errors
/// On I/O failure or when the pack already exists.
pub fn create_pack(root: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = root.join(slugify(name));
    if dir.exists() {
        return Err(format!("pack {name:?} already exists"));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("name.txt"), name).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Deletes a pack and all its stickers.
///
/// # Errors
/// On I/O failure.
pub fn delete_pack(pack_dir: &Path) -> Result<(), String> {
    std::fs::remove_dir_all(pack_dir).map_err(|e| e.to_string())
}

/// Removes one sticker file.
///
/// # Errors
/// On I/O failure.
pub fn remove_sticker(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|e| e.to_string())
}

/// Wraps `upper` (already uppercased) into display lines of at
/// most two words each. Falls back to the whole text as one line
/// when it contains no words at all (an empty or all-whitespace
/// string).
fn wrap_lines(upper: &str) -> Vec<String> {
    let lines: Vec<String> =
        upper.split_whitespace().collect::<Vec<_>>().chunks(2).map(|pair| pair.join(" ")).collect();
    if lines.is_empty() {
        vec![upper.to_string()]
    } else {
        lines
    }
}

/// Renders `text` as a sticker PNG and stores it in `pack_dir`.
/// Returns the written file.
///
/// # Errors
/// On I/O failure, empty text, or text that cannot fit on a
/// sticker: more than [`MAX_TEXT_CHARS`] characters, or a wrapped
/// layout over [`MAX_LINES`] lines or [`MAX_LINE_CHARS`]
/// characters per line. Oversized text is rejected, never
/// truncated silently and never rendered into an unbounded
/// canvas.
pub fn add_text_sticker(pack_dir: &Path, text: &str) -> Result<PathBuf, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("sticker text is empty".to_string());
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!("sticker text is too long: at most {MAX_TEXT_CHARS} characters"));
    }
    let lines = wrap_lines(&text.to_uppercase());
    if lines.len() > MAX_LINES || lines.iter().any(|l| l.chars().count() > MAX_LINE_CHARS) {
        return Err(format!(
            "sticker text does not fit: at most {MAX_LINES} lines of {MAX_LINE_CHARS} characters"
        ));
    }
    let png = render_text_sticker(text);
    let base = slugify(&text.chars().take(24).collect::<String>());
    let mut path = pack_dir.join(format!("text-{base}.png"));
    let mut n = 2;
    while path.exists() {
        path = pack_dir.join(format!("text-{base}-{n}.png"));
        n += 1;
    }
    std::fs::write(&path, png).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Deterministic accent color for a sticker text (palette hash).
#[must_use]
pub(crate) fn sticker_color(text: &str) -> (u8, u8, u8) {
    const PALETTE: [(u8, u8, u8); 6] = [
        (0x25, 0xD3, 0x66),
        (0x53, 0xB7, 0xDE),
        (0xFF, 0xC0, 0x40),
        (0xE8, 0x6B, 0x9C),
        (0x9B, 0x7E, 0xDE),
        (0xFF, 0x7A, 0x59),
    ];
    let hash = text.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(u32::from(b)));
    PALETTE[(hash as usize) % PALETTE.len()]
}

/// A 3×5 bitmap glyph: five rows of three bits (bit 2 = left).
fn glyph(c: char) -> Option<[u8; 5]> {
    let g = match c.to_ascii_uppercase() {
        'A' => [0b111, 0b101, 0b111, 0b101, 0b101],
        'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
        'C' => [0b011, 0b100, 0b100, 0b100, 0b011],
        'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
        'E' => [0b111, 0b100, 0b110, 0b100, 0b111],
        'F' => [0b111, 0b100, 0b110, 0b100, 0b100],
        'G' => [0b111, 0b100, 0b101, 0b101, 0b011],
        'H' => [0b101, 0b101, 0b111, 0b101, 0b101],
        'I' => [0b111, 0b010, 0b010, 0b010, 0b111],
        'J' => [0b001, 0b001, 0b001, 0b101, 0b010],
        'K' => [0b101, 0b101, 0b110, 0b101, 0b101],
        'L' => [0b100, 0b100, 0b100, 0b100, 0b111],
        'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        'N' => [0b101, 0b111, 0b111, 0b101, 0b101],
        'O' => [0b010, 0b101, 0b101, 0b101, 0b010],
        'P' => [0b110, 0b101, 0b110, 0b100, 0b100],
        'Q' => [0b010, 0b101, 0b101, 0b110, 0b011],
        'R' => [0b110, 0b101, 0b110, 0b101, 0b101],
        'S' => [0b011, 0b100, 0b010, 0b001, 0b110],
        'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        'V' => [0b101, 0b101, 0b101, 0b101, 0b010],
        'W' => [0b101, 0b101, 0b111, 0b111, 0b101],
        'X' => [0b101, 0b101, 0b010, 0b101, 0b101],
        'Y' => [0b101, 0b101, 0b010, 0b010, 0b010],
        'Z' => [0b111, 0b001, 0b010, 0b100, 0b111],
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b110, 0b001, 0b010, 0b100, 0b111],
        '3' => [0b110, 0b001, 0b010, 0b001, 0b110],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b110, 0b001, 0b110],
        '6' => [0b011, 0b100, 0b110, 0b101, 0b010],
        '7' => [0b111, 0b001, 0b010, 0b010, 0b010],
        '8' => [0b010, 0b101, 0b010, 0b101, 0b010],
        '9' => [0b010, 0b101, 0b011, 0b001, 0b110],
        ' ' => [0, 0, 0, 0, 0],
        '!' => [0b010, 0b010, 0b010, 0b000, 0b010],
        '?' => [0b110, 0b001, 0b010, 0b000, 0b010],
        '.' => [0, 0, 0, 0, 0b010],
        ',' => [0, 0, 0, 0b010, 0b100],
        '\'' => [0b010, 0b010, 0, 0, 0],
        '-' => [0, 0, 0b111, 0, 0],
        ':' => [0, 0b010, 0, 0b010, 0],
        '+' => [0, 0b010, 0b111, 0b010, 0],
        _ => return None,
    };
    Some(g)
}

/// Renders `text` (uppercased, wrapped two words per line) into
/// PNG bytes: transparent background, bold colored glyphs with a
/// soft shadow.
///
/// Defense in depth behind [`add_text_sticker`]'s validation:
/// the input is capped at [`MAX_TEXT_CHARS`] characters before
/// any layout work, and the wrapped lines are truncated to
/// [`MAX_LINES`] lines of [`MAX_LINE_CHARS`] characters before
/// the canvas size is computed, so the allocation below is
/// bounded (at most 1358 px per side) for any input whatsoever.
#[must_use]
pub(crate) fn render_text_sticker(text: &str) -> Vec<u8> {
    let bounded: String = text.chars().take(MAX_TEXT_CHARS).collect();
    let upper = bounded.to_uppercase();
    let mut lines = wrap_lines(&upper);
    for line in &mut lines {
        if line.chars().count() > MAX_LINE_CHARS {
            *line = line.chars().take(MAX_LINE_CHARS).collect();
        }
    }
    lines.truncate(MAX_LINES);
    let scale = 14u32;
    let char_w = 4 * scale; // 3 glyph cols + 1 spacing
    let line_h = 6 * scale; // 5 glyph rows + 1 spacing
    let max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1) as u32;
    let width = (max_chars * char_w + scale).max(64);
    let height = (lines.len() as u32 * line_h + scale).max(64);
    let (r, g, b) = sticker_color(text);
    let mut img = image::RgbaImage::new(width, height);
    for (li, line) in lines.iter().enumerate() {
        for (ci, ch) in line.chars().enumerate() {
            let Some(bits) = glyph(ch) else { continue };
            let ox = scale / 2 + ci as u32 * char_w;
            let oy = scale / 2 + li as u32 * line_h;
            for (row, bits_row) in bits.iter().enumerate() {
                for col in 0..3u32 {
                    if bits_row & (1 << (2 - col)) == 0 {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let x = ox + col * scale + dx;
                            let y = oy + row as u32 * scale + dy;
                            // Shadow pass first (offset), then face.
                            if x + 3 < width && y + 3 < height {
                                img.put_pixel(x + 3, y + 3, image::Rgba([0, 0, 0, 90]));
                            }
                            img.put_pixel(x, y, image::Rgba([r, g, b, 255]));
                        }
                    }
                }
            }
        }
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("PNG encoding of an in-memory image cannot fail");
    out.into_inner()
}

/// Renders one bundled procedural sticker face by index (0..=5):
/// a filled circle with two eyes and a smile/frown/neutral mouth.
#[must_use]
pub(crate) fn render_bundled_sticker(index: usize) -> Vec<u8> {
    const FACES: [(u8, u8, u8); 6] = [
        (0xFF, 0xD9, 0x3D),
        (0xFF, 0x9F, 0x43),
        (0x54, 0xA0, 0xFF),
        (0x5F, 0x27, 0xCD),
        (0x1D, 0xD1, 0xA1),
        (0xFF, 0x6B, 0x81),
    ];
    let (r, g, b) = FACES[index % FACES.len()];
    let size = 240u32;
    let mut img = image::RgbaImage::new(size, size);
    let center = size as f32 / 2.0;
    let radius = center - 8.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            if dx * dx + dy * dy <= radius * radius {
                img.put_pixel(x, y, image::Rgba([r, g, b, 255]));
            }
        }
    }
    let dark = image::Rgba([30, 30, 40, 255]);
    let mut blob = |cx: u32, cy: u32, rad: u32| {
        for y in cy.saturating_sub(rad)..(cy + rad).min(size) {
            for x in cx.saturating_sub(rad)..(cx + rad).min(size) {
                let dx = i64::from(x) - i64::from(cx);
                let dy = i64::from(y) - i64::from(cy);
                if dx * dx + dy * dy <= i64::from(rad) * i64::from(rad) {
                    img.put_pixel(x, y, dark);
                }
            }
        }
    };
    blob(85, 95, 13);
    blob(155, 95, 13);
    // Mouth: smile arc (indices 0-2), flat (3), frown (4-5).
    let smile = index % 3 != 2;
    for x in 70..170u32 {
        let t = (x as f32 - 70.0) / 100.0;
        let arc = (t - 0.5) * (t - 0.5) * 4.0;
        let y = if index == 3 {
            160
        } else if smile {
            (150.0 + arc * 26.0) as u32
        } else {
            (176.0 - arc * 26.0) as u32
        };
        for dy in 0..6u32 {
            if y + dy < size {
                img.put_pixel(x, y + dy, dark);
            }
        }
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("PNG encoding of an in-memory image cannot fail");
    out.into_inner()
}

/// Ensures the default pack exists with the bundled stickers in it
/// (first run). Returns the pack dir.
pub fn ensure_default_pack(root: &Path) -> Result<PathBuf, String> {
    let packs = list_packs(root)?;
    if let Some(existing) = packs.iter().find(|p| p.name == "Zaprust Faces") {
        return Ok(existing.dir.clone());
    }
    let dir = create_pack(root, "Zaprust Faces")?;
    for i in 0..6 {
        std::fs::write(dir.join(format!("face-{i}.png")), render_bundled_sticker(i))
            .map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zaprust-stickers-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn pack_crud_round_trip() {
        let root = temp_root("crud");
        let dir = create_pack(&root, "My Pack").expect("create");
        assert!(create_pack(&root, "My Pack").is_err(), "duplicate rejected");
        let sticker = add_text_sticker(&dir, "Mantap!").expect("sticker");
        assert!(sticker.exists());
        let packs = list_packs(&root).expect("list");
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].name, "My Pack");
        assert_eq!(packs[0].stickers.len(), 1);
        remove_sticker(&sticker).expect("remove");
        assert_eq!(list_packs(&root).expect("list")[0].stickers.len(), 0);
        delete_pack(&dir).expect("delete");
        assert!(list_packs(&root).expect("list").is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rendered_stickers_are_real_pngs() {
        let text_png = render_text_sticker("HALO");
        assert_eq!(&text_png[..8], b"\x89PNG\r\n\x1a\n");
        let img = image::load_from_memory(&text_png).expect("decodable");
        assert!(img.width() >= 64 && img.height() >= 64);
        let face_png = render_bundled_sticker(0);
        assert_eq!(&face_png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn default_pack_is_created_once_with_faces() {
        let root = temp_root("default");
        let dir = ensure_default_pack(&root).expect("ensure");
        let again = ensure_default_pack(&root).expect("ensure again");
        assert_eq!(dir, again);
        let packs = list_packs(&root).expect("list");
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].stickers.len(), 6);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn normal_text_renders_at_its_previous_size() {
        // One word on one line: width is 4 columns of 56 px plus
        // one scale of padding, height one 84 px line plus padding.
        let img = image::load_from_memory(&render_text_sticker("HALO")).expect("decodable");
        assert_eq!((img.width(), img.height()), (238, 98));
        // Two words share a line: 13 characters wide, still one line.
        let img =
            image::load_from_memory(&render_text_sticker("Mantap Sekali")).expect("decodable");
        assert_eq!((img.width(), img.height()), (742, 98));
    }

    #[test]
    fn oversized_render_stays_within_the_canvas_bound() {
        // 100k characters, as one word and as many words: the
        // render must not attempt a proportional allocation.
        for text in ["a".repeat(100_000), "word ".repeat(20_000)] {
            let img = image::load_from_memory(&render_text_sticker(&text)).expect("decodable");
            assert!(img.width() <= 1358, "width {} over the bound", img.width());
            assert!(img.height() <= 1358, "height {} over the bound", img.height());
            assert!(u64::from(img.width()) * u64::from(img.height()) <= 1358 * 1358);
        }
    }

    #[test]
    fn oversized_text_is_rejected_not_rendered() {
        let root = temp_root("toolong");
        let dir = create_pack(&root, "Long").expect("create");
        let huge = "a".repeat(100_000);
        assert!(add_text_sticker(&dir, &huge).is_err(), "100k chars rejected");
        // Under the character cap but over the line cap: 40
        // one-letter words wrap to 20 lines.
        let many_words = vec!["a"; 40].join(" ");
        assert!(add_text_sticker(&dir, &many_words).is_err(), "20 lines rejected");
        // One word longer than a line can hold.
        assert!(add_text_sticker(&dir, &"a".repeat(30)).is_err(), "30 char word rejected");
        assert!(list_packs(&root).expect("list")[0].stickers.is_empty(), "nothing was written");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn full_size_valid_text_is_accepted() {
        let root = temp_root("fullsize");
        let dir = create_pack(&root, "Full").expect("create");
        // 24 four-letter words: 119 characters, 12 wrapped lines.
        let text = vec!["jaya"; 24].join(" ");
        let sticker = add_text_sticker(&dir, &text).expect("sticker at the caps");
        let img = image::load_from_memory(&std::fs::read(&sticker).expect("read")).expect("decode");
        assert_eq!(img.height(), 12 * 84 + 14);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn slugify_is_filesystem_safe() {
        assert_eq!(slugify("Halo Dunia!"), "halo-dunia");
        assert_eq!(slugify("  --  "), "pack");
        assert_eq!(slugify("Café"), "caf");
    }
}
