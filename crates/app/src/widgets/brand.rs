//! The shared adaptive brand texture.
//!
//! The brand mark (the chat bubble of `packaging/icon.svg`)
//! ships in two bare rasters: black for light grounds and the
//! exact-inverse white twin for dark grounds. Every in-app
//! brand placement must pick between them by the active tokens,
//! because a black bubble on a dark surface is effectively
//! invisible; this helper is the one place that choice is made,
//! so placements can never drift apart. A decode failure yields
//! no mark rather than a substitute glyph: the brand is never
//! improvised from the icon font.

/// Which bare raster a set of tokens calls for: the black mark
/// on light tokens, the white mark on dark tokens.
fn brand_bytes(tokens: core::theme::Tokens) -> &'static [u8] {
    if tokens.is_light() {
        include_bytes!("../../../../packaging/icons/zaprust-256.png")
    } else {
        include_bytes!("../../../../packaging/icons/zaprust-white-256.png")
    }
}

/// Loads one variant as a GPU texture, decoded once and kept
/// for the life of the app.
fn decode(
    ctx: &egui::Context,
    name: &'static str,
    bytes: &'static [u8],
) -> Option<egui::TextureHandle> {
    image::load_from_memory(bytes).ok().map(|img| {
        let rgba = img.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        ctx.load_texture(name, color, egui::TextureOptions::LINEAR)
    })
}

/// The brand mark texture for the active tokens: black on
/// light, white on dark. Both variants are cached after their
/// first decode, so a theme switch swaps textures without
/// decoding again.
pub(crate) fn brand_texture(
    ctx: &egui::Context,
    tokens: core::theme::Tokens,
) -> Option<egui::TextureId> {
    thread_local! {
        static BRAND: std::cell::RefCell<[Option<Option<egui::TextureHandle>>; 2]> =
            const { std::cell::RefCell::new([None, None]) };
    }
    BRAND.with(|cell| {
        let mut slots = cell.borrow_mut();
        let idx = usize::from(!tokens.is_light());
        if slots[idx].is_none() {
            let (name, bytes) = if tokens.is_light() {
                ("zaprust-brand", brand_bytes(tokens))
            } else {
                ("zaprust-brand-white", brand_bytes(tokens))
            };
            slots[idx] = Some(decode(ctx, name, bytes));
        }
        slots[idx].as_ref().and_then(|inner| inner.as_ref().map(|h| h.id()))
    })
}
