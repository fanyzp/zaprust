//! The Appearance settings page: theme, dynamic color, interface, motion, language.

use super::*;

/// Whether a settings row lives under the Appearance page's
/// Advanced disclosure (the calibration sliders). A search
/// jump to one of these rows opens the disclosure, or the
/// highlighted row would not exist to highlight.
pub(super) fn advanced_contains(id: &str) -> bool {
    matches!(id, "appearance.saturation" | "appearance.contrast")
}

impl App {
    pub(crate) fn page_appearance(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.appearance"));
        group_label(ui, t, &crate::i18n::t("appearance.theme"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                for (i, id) in ThemeId::ALL.iter().enumerate() {
                    let tokens = core::theme::Tokens::dynamic(self.seed, *id != ThemeId::Light);
                    let selected = self.theme_choice == *id;
                    if radio_row(
                        ui,
                        t,
                        selected,
                        Some((rgb(tokens.bg_app), rgb(tokens.bg_panel))),
                        id.label(),
                        "",
                    ) {
                        self.theme_choice = *id;
                        self.apply_theme_choice(ui.ctx());
                        self.persist_settings();
                    }
                    if i + 1 < ThemeId::ALL.len() {
                        divider_inset(ui, t, 66.0);
                    }
                }
            })
        });
        paint_highlight(
            ui,
            &mut self.settings_highlight,
            self.motion,
            "appearance.theme",
            scoped.response.rect,
            t,
        );
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("appearance.dynamic_color"));
        let mut picked_seed: Option<core::theme::Rgb> = None;
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                // The name under the dots is the card's one label:
                // it names the dot under the pointer while one is
                // hovered, and the selected color otherwise. A
                // per-dot hover tooltip used to do the naming
                // instead, and it painted outside the card over
                // the label, so two names showed at once with the
                // tooltip pill hanging off the card's edge
                // (VRF-02).
                let mut hovered_name: Option<&str> = None;
                ui.horizontal(|ui| {
                    for (name, seed) in core::theme::SEED_PRESETS {
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::click());
                        let center = rect.center();
                        ui.painter().circle_filled(center, 13.0, rgb(seed));
                        if self.seed == seed {
                            ui.painter().circle_stroke(
                                center,
                                15.5,
                                egui::Stroke::new(2.0_f32, rgb(t.accent)),
                            );
                            crate::icons::draw(
                                ui.painter(),
                                crate::icons::Icon::Check,
                                center,
                                crate::icons::SIZE_ROW,
                                rgb(core::theme::on_color(seed)),
                                rgb(seed),
                            );
                        }
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            hovered_name = Some(name);
                        }
                        if resp.clicked() {
                            picked_seed = Some(seed);
                        }
                    }
                });
                ui.add_space(SP_1_5);
                let current = core::theme::SEED_PRESETS
                    .iter()
                    .find(|(_, s)| *s == self.seed)
                    .map_or("Custom", |(n, _)| *n);
                ui.label(
                    RichText::new(hovered_name.unwrap_or(current))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
            })
        });
        paint_highlight(
            ui,
            &mut self.settings_highlight,
            self.motion,
            "appearance.dynamic_color",
            scoped.response.rect,
            t,
        );
        if let Some(seed) = picked_seed {
            self.seed = seed;
            self.apply_theme_choice(ui.ctx());
            self.persist_settings();
        }
        ui.add_space(SP_4);
        // The saturation and contrast sliders are calibration
        // controls, not everyday settings: they live under an
        // Advanced disclosure, collapsed by default. Their
        // values, ranges and persistence are unchanged. A
        // search jump to either slider opens the disclosure,
        // or the highlighted row would not exist to highlight.
        if self.settings_highlight.as_ref().is_some_and(|h| advanced_contains(h.id)) {
            self.appearance_advanced_open = true;
        }
        let (header_rect, header_resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::click());
        if header_resp.hovered() {
            ui.painter().rect_filled(header_rect, egui::CornerRadius::same(8), rgb(t.bg_panel_alt));
        }
        crate::icons::draw(
            ui.painter(),
            if self.appearance_advanced_open {
                crate::icons::Icon::ChevronDown
            } else {
                crate::icons::Icon::ChevronRight
            },
            egui::Pos2::new(header_rect.left() + 9.0, header_rect.center().y),
            crate::icons::SIZE_ROW,
            rgb(t.text_secondary),
            rgb(t.bg_app),
        );
        ui.painter().text(
            egui::Pos2::new(header_rect.left() + 26.0, header_rect.center().y),
            egui::Align2::LEFT_CENTER,
            crate::i18n::t("appearance.advanced").to_uppercase(),
            crate::theme::semibold(10.0),
            rgb(t.text_secondary),
        );
        if header_resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.appearance_advanced_open = !self.appearance_advanced_open;
        }
        if self.appearance_advanced_open {
            ui.add_space(SP_1_5);
            let scoped = ui.scope(|ui| {
                settings_card(ui, t, |ui| {
                    let mut changed = false;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::i18n::t("appearance.saturation")).size(12.0));
                        let resp = ui.add(
                            egui::Slider::new(&mut self.saturation, 0.0..=1.5)
                                .show_value(true)
                                .custom_formatter(|v, _| format!("{v:.2}")),
                        );
                        changed |= resp.changed();
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::i18n::t("appearance.contrast")).size(12.0));
                        let resp = ui.add(
                            egui::Slider::new(&mut self.contrast, 0.8..=1.2)
                                .show_value(true)
                                .custom_formatter(|v, _| format!("{v:.2}")),
                        );
                        changed |= resp.changed();
                    });
                    ui.add_space(SP_1);
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("appearance.reset"),
                        crate::widgets::Btn::Secondary,
                    )
                    .clicked()
                    {
                        self.saturation = 1.0;
                        self.contrast = 1.0;
                        changed = true;
                    }
                    if changed {
                        self.apply_theme_choice(ui.ctx());
                        self.persist_settings();
                    }
                })
            });
            paint_highlight(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "appearance.saturation",
                scoped.response.rect,
                t,
            );
            paint_highlight(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "appearance.contrast",
                scoped.response.rect,
                t,
            );
        }
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("appearance.font_size"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                ui.horizontal(|ui| {
                    for (percent, key) in [
                        (85_u32, "appearance.font_small"),
                        (100, "appearance.font_default"),
                        (115, "appearance.font_large"),
                    ] {
                        if chip(ui, &crate::i18n::t(key), self.zoom_percent == percent, t).clicked()
                        {
                            // Applies from the next frame (the
                            // shell composes the preference with
                            // the layout mode's density factor)
                            // and persists like every other
                            // Appearance choice.
                            self.zoom_percent = percent;
                            self.persist_settings();
                        }
                    }
                });
            })
        });
        paint_highlight(
            ui,
            &mut self.settings_highlight,
            self.motion,
            "appearance.font_size",
            scoped.response.rect,
            t,
        );
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("appearance.motion"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("appearance.motion_caption"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                ui.add_space(SP_1_5);
                ui.horizontal(|ui| {
                    for (setting, key) in [
                        (crate::motion::MotionSetting::Full, "appearance.motion_full"),
                        (crate::motion::MotionSetting::Reduced, "appearance.motion_reduced"),
                        (crate::motion::MotionSetting::Off, "appearance.motion_off"),
                    ] {
                        if chip(ui, &crate::i18n::t(key), self.motion == setting, t).clicked() {
                            self.motion = setting;
                            self.persist_settings();
                        }
                    }
                });
            })
        });
        paint_highlight(
            ui,
            &mut self.settings_highlight,
            self.motion,
            "appearance.motion",
            scoped.response.rect,
            t,
        );
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("appearance.language"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("appearance.language_caption"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                ui.add_space(SP_1_5);
                ui.horizontal(|ui| {
                    for (id, name) in crate::i18n::LOCALES {
                        if chip(ui, name, self.language == *id, t).clicked() {
                            self.language = (*id).to_owned();
                            crate::i18n::set_locale(id);
                            self.persist_settings();
                        }
                    }
                });
            })
        });
        paint_highlight(
            ui,
            &mut self.settings_highlight,
            self.motion,
            "appearance.language",
            scoped.response.rect,
            t,
        );
    }
}
