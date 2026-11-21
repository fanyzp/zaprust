//! Demo media: the photo shown by image messages.
//!
//! The demo ships no binary assets for photos, so the sample photo is a
//! flat illustration painted pixel by pixel at startup (sky, sun, clouds,
//! mountains, sea) and uploaded once as an egui texture. When the connect
//! phase lands, real attachments replace this through the same texture
//! path.

use eframe::egui::{self, ColorImage};

const W: usize = 600;
const H: usize = 400;

fn in_circle(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> bool {
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn in_ellipse(x: f32, y: f32, cx: f32, cy: f32, rx: f32, ry: f32) -> bool {
    ((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2) <= 1.0
}

/// Signed area helper for the triangle test.
fn tri_sign(x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) -> f32 {
    (x1 - x3) * (y2 - y3) - (x2 - x3) * (y1 - y3)
}

fn in_triangle(x: f32, y: f32, a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let d1 = tri_sign(x, y, a.0, a.1, b.0, b.1);
    let d2 = tri_sign(x, y, b.0, b.1, c.0, c.1);
    let d3 = tri_sign(x, y, c.0, c.1, a.0, a.1);
    let has_neg = (d1 < 0.0) || (d2 < 0.0) || (d3 < 0.0);
    let has_pos = (d1 > 0.0) || (d2 > 0.0) || (d3 > 0.0);
    !(has_neg && has_pos)
}

fn px(x: usize, y: usize) -> [u8; 4] {
    let (fx, fy) = (x as f32, y as f32);
    // Sea and shore (bottom band).
    let mut color = if fy > 300.0 {
        [64u8, 145, 180, 255]
    } else if fy > 288.0 {
        [238, 225, 190, 255] // sand strip
    } else {
        // Sky in flat bands.
        match y {
            0..=95 => [116, 184, 226, 255],
            96..=190 => [146, 201, 233, 255],
            _ => [178, 218, 240, 255],
        }
    };
    // Sun.
    if in_circle(fx, fy, 470.0, 92.0, 40.0) {
        color = [255, 209, 102, 255];
    }
    // Clouds.
    for (cx, cy, s) in [(140.0, 74.0, 1.0f32), (330.0, 48.0, 0.7), (90.0, 140.0, 0.55)] {
        if in_ellipse(fx, fy, cx, cy, 62.0 * s, 20.0 * s)
            || in_ellipse(fx, fy, cx - 34.0 * s, cy + 6.0 * s, 40.0 * s, 15.0 * s)
            || in_ellipse(fx, fy, cx + 36.0 * s, cy + 6.0 * s, 42.0 * s, 16.0 * s)
        {
            color = [250, 252, 255, 255];
        }
    }
    // Far mountains.
    if in_triangle(fx, fy, (-20.0, 292.0), (190.0, 130.0), (400.0, 292.0))
        || in_triangle(fx, fy, (240.0, 292.0), (440.0, 160.0), (640.0, 292.0))
    {
        color = [110, 118, 148, 255];
    }
    // Sun glitter on the water.
    if fy > 306.0 && ((x as f32 - 470.0).abs() < 34.0 - (fy - 306.0) * 0.22) && (y % 9 < 3) {
        color = [255, 226, 130, 255];
    }
    // Near green hills.
    if in_ellipse(fx, fy, 90.0, 420.0, 300.0, 150.0)
        || in_ellipse(fx, fy, 560.0, 430.0, 320.0, 160.0)
    {
        color = [96, 165, 110, 255];
    }
    color
}

/// Builds the demo photo texture (600x400).
pub(crate) fn demo_photo(ctx: &egui::Context) -> egui::TextureHandle {
    let mut pixels = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) * 4;
            pixels[i..i + 4].copy_from_slice(&px(x, y));
        }
    }
    let image = ColorImage::from_rgba_unmultiplied([W, H], &pixels);
    ctx.load_texture("demo-photo", image, egui::TextureOptions::LINEAR)
}

/// Decodes a photo from disk into a small square-capped preview
/// texture for the staged bar and message bubbles. Returns None
/// when the file is unreadable or not a decodable image, and the
/// caller falls back to the demo photo. Textures bind to the live
/// context, so `ctx` must be the app's own.
pub(crate) fn load_thumb(
    ctx: &egui::Context,
    path: &std::path::Path,
) -> Option<egui::TextureHandle> {
    let img = image::open(path).ok()?;
    let img = img.resize(160, 160, image::imageops::FilterType::Triangle);
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let image = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Some(ctx.load_texture("staged-thumb", image, egui::TextureOptions::LINEAR))
}

/// What a photo message's tile may honestly show right now. The
/// decision is made once per message by `App::photo_tile` and
/// consumed by the bubble, the album grid and the photo viewer,
/// so the three surfaces can never disagree about whether a
/// photo is real, a demo stand-in, or still missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PhotoTile {
    /// The photo's own bytes are decoded and ready to paint.
    Real,
    /// The generated demo photo stands in. Only ever chosen in
    /// demo mode and the fake backend's test mode, whose photos
    /// are disclosed fixtures; a real live session never picks it
    /// for a received photo.
    Demo,
    /// A fetch for the real bytes is in flight.
    Loading,
    /// The fetch ran and failed; the photo is unavailable.
    Failed,
    /// No fetch has been requested yet.
    #[default]
    Idle,
}

/// Longest side, in pixels, of a decoded display photo. Bubbles
/// paint photos a few hundred points wide and the viewer caps at
/// a similar scale, so decoding a multi-megapixel original at
/// full size would only cost memory; this cap keeps the texture
/// comfortably above both surfaces.
const DISPLAY_PHOTO_CAP: u32 = 1024;

/// Decodes fetched photo bytes into a texture for display.
/// Returns None when the bytes are not a decodable image; the
/// caller treats that as a failed fetch, never as a reason to
/// substitute another image.
pub(crate) fn decode_photo(ctx: &egui::Context, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > DISPLAY_PHOTO_CAP || img.height() > DISPLAY_PHOTO_CAP {
        img.resize(DISPLAY_PHOTO_CAP, DISPLAY_PHOTO_CAP, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let image = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Some(ctx.load_texture("photo", image, egui::TextureOptions::LINEAR))
}
