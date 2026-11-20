//! The Data & Storage settings page: real on-device sizes and caches.

use super::*;

impl App {
    pub(crate) fn page_storage(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.storage"), &crate::i18n::t("storage.subtitle"));
        // Real sizes, measured from the app data dir right now.
        let data_dir = backend::data_dir();
        let archive = std::fs::metadata(data_dir.join("archive.db")).map_or(0, |m| m.len());
        let session = dir_size(&data_dir.join("accounts"));
        let total = dir_size(&data_dir);
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
                     on this device, with the key kept in your operating \
                     system keyring. Backups you export are not encrypted.",
                )
                .size(12.0)
                .color(rgb(t.text_secondary)),
            );
            ui.add_space(8.0);
            if self.cache_cleared {
                ui.label(
                    RichText::new(
                        "In-memory caches cleared, link previews and GIF \
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
            ui.add_space(4.0);
        });
        if clear {
            self.og_cache.clear();
            self.picker.giphy_textures.clear();
            // The sticker texture cache was missed by this button
            // until v0.25 (perf review).
            self.picker.sticker_textures.clear();
            self.cache_cleared = true;
        }
    }
}
