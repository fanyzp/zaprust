//! The Appearance settings page: theme, interface, motion, language.

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
                // One list, one active token source: the scheme
                // rows activate the built-in scheme (a fixed
                // palette from the canonical seed constant), the
                // preset rows activate that preset's token
                // table. Preset proper names stay untranslated.
                let row_count = ThemeId::ALL.len() + core::theme::THEME_PRESETS.len();
                for (i, id) in ThemeId::ALL.iter().enumerate() {
                    let tokens = core::theme::Tokens::dynamic(
                        core::theme::DEFAULT_SEED,
                        *id != ThemeId::Light,
                    );
                    let selected = self.theme_preset.is_none() && self.theme_choice == *id;
                    if radio_row(
                        ui,
                        t,
                        selected,
                        Some((rgb(tokens.bg_app), rgb(tokens.bg_panel))),
                        id.label(),
                        "",
                    ) {
                        self.theme_preset = None;
                        self.theme_choice = *id;
                        self.apply_theme_choice(ui.ctx());
                        self.persist_settings();
                    }
                    if i + 1 < row_count {
                        divider_inset(ui, t, 66.0);
                    }
                }
                for (j, preset) in core::theme::THEME_PRESETS.iter().enumerate() {
                    let tokens = preset.resolve();
                    let selected = self.theme_preset.as_deref() == Some(preset.id());
                    if radio_row(
                        ui,
                        t,
                        selected,
                        Some((rgb(tokens.bg_app), rgb(tokens.bg_panel))),
                        preset.name(),
                        "",
                    ) {
                        self.theme_preset = Some(preset.id().to_owned());
                        // Every shipped preset is a dark scheme;
                        // the scheme field follows it so a later
                        // return to the built-in schemes starts dark.
                        self.theme_choice = ThemeId::Dark;
                        self.apply_theme_choice(ui.ctx());
                        self.persist_settings();
                    }
                    if ThemeId::ALL.len() + j + 1 < row_count {
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
        let adv_hov =
            crate::anim::hover_factor(ui, "appearance.advanced_header", header_resp.hovered());
        let adv_press = crate::anim::press_factor(
            ui,
            "appearance.advanced_header",
            header_resp.is_pointer_button_down_on(),
        );
        if adv_hov > 0.001 {
            ui.painter().rect_filled(
                header_rect,
                crate::theme::RADIUS_MICRO,
                crate::theme::glass_wash(t, t.bg_panel_alt, (adv_hov * 115.0) as u8),
            );
        }
        if adv_press > 0.001 {
            ui.painter().rect_filled(
                header_rect,
                crate::theme::RADIUS_MICRO,
                crate::theme::glass_wash(t, t.bg_panel_alt, (adv_press * 170.0) as u8),
            );
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
            crate::i18n::t("appearance.advanced"),
            crate::theme::medium(crate::theme::TEXT_LABEL),
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
                    changed |= slider_row(
                        ui,
                        t,
                        &crate::i18n::t("appearance.saturation"),
                        &mut self.saturation,
                        0.0..=1.5,
                    );
                    divider(ui, t);
                    changed |= slider_row(
                        ui,
                        t,
                        &crate::i18n::t("appearance.contrast"),
                        &mut self.contrast,
                        0.8..=1.2,
                    );
                    divider(ui, t);
                    // A fixed-height row: a bare with_layout in
                    // the card's vertical flow would claim the
                    // card's whole remaining height and centre
                    // the button in the void.
                    let width = ui.available_width();
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 30.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
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
                        },
                    );
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
        // Font size, motion and language are one Interface
        // group: three choice rows in a single card, each row
        // the label column on the left and its segmented toggle
        // on the shared right edge, hairlines between. They used
        // to be three groups whose control sat alone at the top
        // of an otherwise empty card, which read as loose words
        // rather than settings.
        group_label(ui, t, &crate::i18n::t("appearance.interface"));
        settings_card(ui, t, |ui| {
            let percents = [85_u32, 100, 115];
            let labels: Vec<String> =
                ["appearance.font_small", "appearance.font_default", "appearance.font_large"]
                    .iter()
                    .map(|key| crate::i18n::t(key))
                    .collect();
            let options: Vec<&str> = labels.iter().map(String::as_str).collect();
            let selected = percents.iter().position(|p| *p == self.zoom_percent).unwrap_or(1);
            let (clicked, rect) = choice_row(
                ui,
                t,
                &crate::i18n::t("appearance.font_size"),
                "",
                "appearance.font_size",
                &options,
                selected,
            );
            paint_highlight(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "appearance.font_size",
                rect,
                t,
            );
            if let Some(i) = clicked {
                // Applies from the next frame (the shell
                // composes the preference with the layout
                // mode's density factor) and persists like
                // every other Appearance choice.
                self.zoom_percent = percents[i];
                self.persist_settings();
            }
            divider(ui, t);
            let settings = [
                crate::motion::MotionSetting::Full,
                crate::motion::MotionSetting::Reduced,
                crate::motion::MotionSetting::Off,
            ];
            let labels: Vec<String> =
                ["appearance.motion_full", "appearance.motion_reduced", "appearance.motion_off"]
                    .iter()
                    .map(|key| crate::i18n::t(key))
                    .collect();
            let options: Vec<&str> = labels.iter().map(String::as_str).collect();
            let selected = settings.iter().position(|s| *s == self.motion).unwrap_or(0);
            let (clicked, rect) = choice_row(
                ui,
                t,
                &crate::i18n::t("appearance.motion"),
                &crate::i18n::t("appearance.motion_caption"),
                "appearance.motion",
                &options,
                selected,
            );
            paint_highlight(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "appearance.motion",
                rect,
                t,
            );
            if let Some(i) = clicked {
                self.motion = settings[i];
                self.persist_settings();
            }
            divider(ui, t);
            let locales: Vec<(&str, &str)> = crate::i18n::LOCALES.to_vec();
            let options: Vec<&str> = locales.iter().map(|(_, name)| *name).collect();
            let selected = locales.iter().position(|(id, _)| self.language == *id).unwrap_or(0);
            let (clicked, rect) = choice_row(
                ui,
                t,
                &crate::i18n::t("appearance.language"),
                &crate::i18n::t("appearance.language_caption"),
                "appearance.language",
                &options,
                selected,
            );
            paint_highlight(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "appearance.language",
                rect,
                t,
            );
            if let Some(i) = clicked {
                let (id, _) = locales[i];
                self.language = id.to_owned();
                crate::i18n::set_locale(id);
                self.persist_settings();
            }
        });
    }
}

/// One calibration slider row in the settings grammar: the
/// label at body size on the left, the slider filling the
/// rest of the row with its value at its right end. The
/// sliders used to sit in ad-hoc horizontal rows with 12 px
/// labels, which read as debug controls instead of settings.
/// Returns whether the value changed.
fn slider_row(
    ui: &mut egui::Ui,
    _t: core::theme::Tokens,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
) -> bool {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 44.0), egui::Sense::hover());
    let label_rect =
        egui::Rect::from_min_max(rect.min, egui::Pos2::new(rect.left() + 120.0, rect.bottom()));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(label_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            ui.label(RichText::new(label).size(15.0));
        },
    );
    let field_rect =
        egui::Rect::from_min_max(egui::Pos2::new(rect.left() + 120.0, rect.top()), rect.max);
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(field_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            // The stock slider's own width is a fixed spacing
            // token; widen the rail so the control fills its
            // row, leaving room for the value at its end.
            ui.spacing_mut().slider_width = (field_rect.width() - 72.0).max(80.0);
            ui.add(
                egui::Slider::new(value, range)
                    .show_value(true)
                    .custom_formatter(|v, _| format!("{v:.2}")),
            )
            .changed()
        },
    )
    .inner
}
