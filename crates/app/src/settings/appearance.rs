//! The Appearance settings page: theme, dynamic color, interface, motion, language.

use super::*;

impl App {
    pub(crate) fn page_appearance(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.appearance"));
        group_label(ui, t, &crate::i18n::t("appearance.theme"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                for (i, id) in ThemeId::ALL.iter().enumerate() {
                    let tokens = core::theme::Tokens::dynamic(self.seed, *id != ThemeId::Light);
                    let selected = self.theme_choice == *id;
                    if radio_row(ui, t, selected, Some(rgb(tokens.bg_app)), id.label(), "") {
                        self.theme_choice = *id;
                        self.apply_theme_choice(ui.ctx());
                    }
                    if i + 1 < ThemeId::ALL.len() {
                        divider_inset(ui, t, 60.0);
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
        ui.add_space(16.0);
        group_label(ui, t, &crate::i18n::t("appearance.dynamic_color"));
        let mut picked_seed: Option<core::theme::Rgb> = None;
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
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
                                15.0,
                                rgb(core::theme::on_color(seed)),
                                rgb(seed),
                            );
                        }
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.on_hover_text(name).clicked() {
                            picked_seed = Some(seed);
                        }
                    }
                });
                ui.add_space(6.0);
                let current = core::theme::SEED_PRESETS
                    .iter()
                    .find(|(_, s)| *s == self.seed)
                    .map_or("Custom", |(n, _)| *n);
                ui.label(RichText::new(current).size(12.5).color(rgb(t.text_secondary)));
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
        ui.add_space(16.0);
        group_label(ui, t, &crate::i18n::t("appearance.color_finetuning"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                let mut changed = false;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::i18n::t("appearance.saturation")).size(13.0));
                    let resp = ui.add(
                        egui::Slider::new(&mut self.saturation, 0.0..=1.5)
                            .show_value(true)
                            .custom_formatter(|v, _| format!("{v:.2}")),
                    );
                    changed |= resp.changed();
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::i18n::t("appearance.contrast")).size(13.0));
                    let resp = ui.add(
                        egui::Slider::new(&mut self.contrast, 0.8..=1.2)
                            .show_value(true)
                            .custom_formatter(|v, _| format!("{v:.2}")),
                    );
                    changed |= resp.changed();
                });
                ui.add_space(4.0);
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
        ui.add_space(16.0);
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
        ui.add_space(16.0);
        group_label(ui, t, &crate::i18n::t("appearance.interface"));
        settings_card(ui, t, |ui| {
            let mut v = self.custom_title_bar;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "appearance.title_bar",
                t,
                "Custom title bar",
                "",
                |ui| {
                    switch(ui, &mut v, t);
                },
            );
            self.custom_title_bar = v;
            divider(ui, t);
            let mut glow = self.glow;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "appearance.glow",
                t,
                &crate::i18n::t("appearance.glow"),
                "",
                |ui| {
                    switch(ui, &mut glow, t);
                },
            );
            if glow != self.glow {
                self.glow = glow;
                self.apply_theme_choice(ui.ctx());
                self.persist_settings();
            }
        });
        ui.add_space(16.0);
        group_label(ui, t, &crate::i18n::t("appearance.motion"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("appearance.motion_caption"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                ui.add_space(6.0);
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
        ui.add_space(16.0);
        group_label(ui, t, &crate::i18n::t("appearance.language"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("appearance.language_caption"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                ui.add_space(6.0);
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
