//! Profile photos for the demo dataset's people.
//!
//! The demo ships with a small set of bundled avatar portraits
//! (generated with DiceBear, adventurer style) so the demo chats
//! read as lived-in conversations instead of initial discs. The
//! portraits are keyed by display name: full names for one-to-one
//! chats, first names for group senders, matching how each surface
//! refers to the person.
//!
//! The table is a demo fixture, and the gate is enforced in
//! [`App::demo_avatar`], the only way in: it resolves a portrait
//! only where demo fixtures may stand in as content (see
//! `App::demo_fixtures_allowed`). A real live session gets `None`
//! for every name, including names that collide with a demo
//! persona ("Mom", "Noah", a group sender called "Sarah"), so
//! every surface falls back to the painted initials disc. Live
//! profile photos are not fetched today; nothing paints in their
//! place.

use crate::state::App;

/// The bundled portrait PNG for a demo persona, by full or first
/// name. Only reached through [`App::demo_avatar`], which gates
/// the lookup on the session mode; this table on its own knows
/// nothing about modes.
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
    /// demo persona, and `None` for everyone in a real live
    /// session: the gate runs before the cache lookup, so a
    /// portrait decoded in another mode can never be served to a
    /// live surface either.
    pub(crate) fn demo_avatar(
        &mut self,
        ctx: &egui::Context,
        name: &str,
    ) -> Option<egui::TextureId> {
        if !self.demo_fixtures_allowed() {
            return None;
        }
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
    /// app mid-draw. Empty in a real live session, because every
    /// entry resolves through the gated [`App::demo_avatar`].
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

#[cfg(test)]
mod tests {
    //! Item 127: the demo avatar table answers to names ("Mom",
    //! "Noah", first names like "Sarah") that real people in a
    //! live session can equally carry. These tests pin the gate
    //! in `App::demo_avatar`, the funnel every surface resolves
    //! portraits through: a real live session must get nothing
    //! for any name, while demo mode and the fake backend's test
    //! mode keep their disclosed fixtures.

    use super::*;
    use crate::live::{LinkStatus, LiveSession};
    use crate::state::Args;
    use std::collections::HashMap;

    fn app() -> (App, egui::Context) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        // App::new may have attempted a real live start against
        // this machine's data dir; the gate tests own the state
        // from here.
        app.live = None;
        app.live_mode = false;
        (app, ctx)
    }

    fn session(fake: bool) -> LiveSession {
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<backend::BackendCommand>();
        let (_evt_tx, evt_rx) = std::sync::mpsc::channel::<backend::BackendEvent>();
        LiveSession {
            channels: backend::BackendChannels { commands: cmd_tx, events: evt_rx },
            status: LinkStatus::Linked,
            qr: None,
            qr_expires_at: None,
            pair_code: None,
            pair_code_at: None,
            account: String::new(),
            connected_at: None,
            syncing: false,
            sync_last_activity: None,
            synced_once: false,
            store: None,
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake,
            logout_requested: false,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            photo_refs: HashMap::new(),
            voice_refs: HashMap::new(),
            video_refs: HashMap::new(),
            video_thumbs: HashMap::new(),
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
        }
    }

    /// Every name the table answers to: full names (one-to-one
    /// surfaces) and first names (group sender surfaces).
    const PERSONAS: [&str; 18] = [
        "Sarah Miller",
        "Sarah",
        "Mike Johnson",
        "Mike",
        "Emma Wilson",
        "Emma",
        "David Kim",
        "David",
        "Alex Carter",
        "Alex",
        "James Rodriguez",
        "James",
        "Lily Parker",
        "Lily",
        "Ava Thompson",
        "Ava",
        "Noah",
        "Mom",
    ];

    #[test]
    fn live_session_never_resolves_a_demo_portrait() {
        let (mut app, ctx) = app();
        app.live = Some(session(false));
        app.live_mode = true;
        for name in PERSONAS {
            assert!(
                app.demo_avatar(&ctx, name).is_none(),
                "a live session must not paint the demo portrait for {name}"
            );
        }
        for name in PERSONAS {
            assert!(
                app.media.avatar_textures.get(name).is_none(),
                "the gate runs before any portrait is decoded or cached"
            );
        }
        // The chat-list map is empty too, even with a colliding
        // demo persona sitting in the list.
        app.chats = core::model::demo_chats();
        assert!(app.demo_photos(&ctx).is_empty());
    }

    #[test]
    fn cache_from_demo_mode_is_not_served_to_a_live_session() {
        let (mut app, ctx) = app();
        assert!(app.demo_avatar(&ctx, "Mom").is_some(), "demo mode resolves the fixture");
        app.live = Some(session(false));
        app.live_mode = true;
        assert!(
            app.demo_avatar(&ctx, "Mom").is_none(),
            "a cached demo portrait must not leak into a live session"
        );
    }

    #[test]
    fn demo_mode_still_resolves_the_fixtures() {
        let (mut app, ctx) = app();
        for name in PERSONAS {
            assert!(app.demo_avatar(&ctx, name).is_some(), "demo fixture for {name}");
        }
        assert!(app.demo_avatar(&ctx, "Not A Persona").is_none());
        app.chats = core::model::demo_chats();
        let photos = app.demo_photos(&ctx);
        let mom = app.chats.iter().find(|c| c.title == "Mom").expect("the demo set has Mom");
        assert!(photos.contains_key(&mom.id), "the demo chat list paints Mom's portrait");
    }

    #[test]
    fn fake_backend_test_mode_still_resolves_the_fixtures() {
        let (mut app, ctx) = app();
        app.live = Some(session(true));
        app.live_mode = true;
        assert!(
            app.demo_avatar(&ctx, "Mom").is_some(),
            "test mode data is scripted fixtures, labelled as such"
        );
        assert!(app.demo_avatar(&ctx, "Noah").is_some());
    }
}
