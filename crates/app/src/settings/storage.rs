//! The Data & Storage settings page: real on-device sizes and caches.

use super::*;

impl App {
    pub(crate) fn page_storage(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.storage"));
        // Real sizes, measured from the app data dir when the
        // page opened and then cached: this draw runs on every
        // repaint, and a recursive walk of the whole data dir is
        // thousands of syscalls per frame on a large install
        // (audit finding 13). draw_settings drops the cache
        // whenever another page is shown, so reopening the page
        // measures again.
        let (archive, session, total) = match self.storage_sizes {
            Some(sizes) => sizes,
            None => {
                let sizes = measure_storage(&backend::data_dir());
                self.storage_sizes = Some(sizes);
                sizes
            }
        };
        let other = total.saturating_sub(archive + session);
        group_label(ui, t, &format!("Storage used, {}", fmt_bytes(total)));
        let mut clear = false;
        settings_card(ui, t, |ui| {
            // Segments must read as three distinct blocks in
            // every theme: accent, a lightened accent step, and
            // the "free/other" tone (the panel-alt step). Deriving
            // from accent avoids the collision where accent and
            // the badge green are the same color (Dark, Monokai).
            let accent_light = {
                let a = t.accent;
                core::theme::Rgb(
                    a.0.saturating_add((255 - a.0) / 3),
                    a.1.saturating_add((255 - a.1) / 3),
                    a.2.saturating_add((255 - a.2) / 3),
                )
            };
            let parts = [
                (archive as f32, rgb(t.accent)),
                (session as f32, rgb(accent_light)),
                (other as f32, rgb(t.bg_panel_alt)),
            ];
            let total_f: f32 = parts.iter().map(|p| p.0).sum::<f32>().max(1.0);
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 12.0), Sense::hover());
            let mut x = rect.left();
            for (v, c) in parts {
                let w = rect.width() * v / total_f;
                if w > 1.0 {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            Pos2::new(x, rect.top()),
                            Pos2::new(x + w - 2.0, rect.bottom()),
                        ),
                        CornerRadius::same(3),
                        c,
                    );
                }
                x += w;
            }
            ui.add_space(6.0);
            for (label, v, swatch) in [
                ("Archive database & messages", fmt_bytes(archive), rgb(t.accent)),
                ("Linked account data", fmt_bytes(session), rgb(accent_light)),
                ("Feature data, settings & logs", fmt_bytes(other), rgb(t.bg_panel_alt)),
            ] {
                let hid = match label {
                    "Archive database & messages" => "storage.archive",
                    "Linked account data" => "storage.session",
                    _ => "storage.other",
                };
                row_hl(ui, &mut self.settings_highlight, self.motion, hid, t, label, "", |ui| {
                    ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().rect_filled(dot, CornerRadius::same(3), swatch);
                        ui.label(RichText::new(v).color(rgb(t.text_secondary)));
                    });
                });
            }
            divider(ui, t);
            ui.add_space(4.0);
            // Honest storage posture (security audit SEC-07): say
            // plainly how the local data is protected.
            ui.label(
                RichText::new(
                    "Your chat archive and linked-account data are encrypted \
                     on this device, with the key kept in an owner-only \
                     key file in the app data folder and mirrored to your \
                     operating system keyring. Backups you export are not encrypted.",
                )
                .size(12.0)
                .color(rgb(t.text_secondary)),
            );
            ui.add_space(4.0);
        });
        ui.add_space(16.0);
        group_label(ui, t, &crate::i18n::t("storage.caches"));
        settings_card(ui, t, |ui| {
            if self.cache_cleared {
                ui.label(
                    RichText::new(
                        "In-memory caches cleared, images and \
                         thumbnails will be refetched on demand.",
                    )
                    .size(12.0)
                    .color(rgb(t.accent)),
                );
            } else if crate::widgets::button(
                ui,
                t,
                "Clear in-memory caches",
                crate::widgets::Btn::Secondary,
            )
            .clicked()
            {
                clear = true;
            }
        });
        if clear {
            self.clear_in_memory_caches();
        }
    }

    /// Clears every in-memory cache the app keeps, no
    /// exceptions: an earlier version of this action missed the
    /// sticker textures, and the per-message media and avatar
    /// textures were never cleared at all (audit finding 10).
    pub(crate) fn clear_in_memory_caches(&mut self) {
        self.media.textures.clear();
        self.media.avatar_textures.clear();
        self.picker.giphy_textures.clear();
        self.picker.sticker_textures.clear();
        self.cache_cleared = true;
    }
}
