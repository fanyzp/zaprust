//! Profile photos for the demo dataset's people.
//!
//! The demo ships with a small set of bundled avatar portraits
//! (generated with DiceBear, adventurer style) so the demo chats
//! read as lived-in conversations instead of initial discs. The
//! portraits are keyed by display name: full names for one-to-one
//! chats, first names for group senders, matching how each surface
//! refers to the person. Live chats never consult this table; a
//! name that is not a demo persona simply has no photo and the UI
//! falls back to the painted initials disc.

use crate::state::App;

/// The bundled portrait PNG for a demo persona, by full or first
/// name.
pub(crate) fn png_for(name: &str) -> Option<&'static [u8]> {
    let bytes: &[u8] = match name {
        "Sarah Miller" | "Sarah" => {
            include_bytes!("../assets/demo-avatars/sarah-miller.png")
        }
        "Mike Johnson" | "Mike" => {
            include_bytes!("../assets/demo-avatars/mike-johnson.png")
        }
        "Emma Wilson" | "Emma" => {
            include_bytes!("../assets/demo-avatars/emma-wilson.png")
        }
        "David Kim" | "David" => include_bytes!("../assets/demo-avatars/david-kim.png"),
        "Alex Carter" | "Alex" => {
            include_bytes!("../assets/demo-avatars/alex-carter.png")
        }
        "James Rodriguez" | "James" => {
            include_bytes!("../assets/demo-avatars/james-rodriguez.png")
        }
        "Lily Parker" | "Lily" => {
            include_bytes!("../assets/demo-avatars/lily-parker.png")
        }
        "Ava Thompson" | "Ava" => {
            include_bytes!("../assets/demo-avatars/ava-thompson.png")
        }
        "Noah" => include_bytes!("../assets/demo-avatars/noah.png"),
        "Mom" => include_bytes!("../assets/demo-avatars/mom.png"),
        _ => return None,
    };
    Some(bytes)
}

impl App {
    /// The decoded texture for a demo persona's portrait, decoded
    /// and cached on first use. `None` for anyone who is not a
    /// demo persona.
    pub(crate) fn demo_avatar(
        &mut self,
        ctx: &egui::Context,
        name: &str,
    ) -> Option<egui::TextureId> {
        if let Some(handle) = self.media.avatar_textures.get(name) {
            return Some(handle.id());
        }
        let bytes = png_for(name)?;
        let img = image::load_from_memory(bytes).ok()?;
        let rgba = img.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        let handle =
            ctx.load_texture(format!("demo-avatar-{name}"), color, egui::TextureOptions::LINEAR);
        let id = handle.id();
        self.media.avatar_textures.insert(name.to_owned(), handle);
        Some(id)
    }

    /// Portraits for every one-to-one demo chat currently in the
    /// list, keyed by chat id. Surfaces that render rows inside
    /// closures take this map up front instead of borrowing the
    /// app mid-draw.
    pub(crate) fn demo_photos(
        &mut self,
        ctx: &egui::Context,
    ) -> std::collections::HashMap<u64, egui::TextureId> {
        let names: Vec<(u64, String)> = self
            .chats
            .iter()
            .filter(|c| c.kind == core::model::ChatKind::Dm)
            .map(|c| (c.id, c.title.clone()))
            .collect();
        let mut out = std::collections::HashMap::new();
        for (id, name) in names {
            if let Some(tex) = self.demo_avatar(ctx, &name) {
                out.insert(id, tex);
            }
        }
        out
    }
}
