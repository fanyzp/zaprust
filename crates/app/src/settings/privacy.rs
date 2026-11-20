//! The Privacy & Security settings page: receipts, presence, account privacy.

use super::*;

impl App {
    pub(crate) fn page_privacy(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.privacy"), &crate::i18n::t("privacy.subtitle"));
        group_label(ui, t, &crate::i18n::t("privacy.receipts_group"));
        settings_card(ui, t, |ui| {
            let mut v = self.read_receipts;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "privacy.read_receipts",
                t,
                &crate::i18n::t("privacy.read_receipts"),
                &crate::i18n::t("privacy.read_receipts_caption"),
                |ui| switch(ui, &mut v, t),
            );
            if v != self.read_receipts {
                self.set_read_receipts(v);
            }
            divider(ui, t);
            let mut tp = self.typing_indicators;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "privacy.typing",
                t,
                &crate::i18n::t("privacy.typing"),
                &crate::i18n::t("privacy.typing_caption"),
                |ui| switch(ui, &mut tp, t),
            );
            if tp != self.typing_indicators {
                self.typing_indicators = tp;
                if !tp {
                    self.typing_sent_for = None;
                }
                self.persist_settings();
            }
        });
        ui.add_space(16.0);
        group_label(ui, t, &crate::i18n::t("privacy.default_timer"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("privacy.default_timer_caption"))
                        .size(11.5)
                        .color(rgb(t.text_secondary)),
                );
                ui.add_space(4.0);
                for secs in core::model::EPHEMERAL_OPTIONS {
                    let label = crate::extras::ephemeral_duration_label(secs);
                    if radio_row(ui, t, self.default_ephemeral_secs == secs, None, &label, "") {
                        self.default_ephemeral_secs = secs;
                        self.persist_settings();
                        if self.live_mode {
                            self.send_backend(
                                backend::BackendCommand::SetDefaultDisappearingTimer {
                                    duration_secs: secs,
                                },
                            );
                        }
                    }
                }
            });
        });
        paint_highlight(
            ui,
            &mut self.settings_highlight,
            self.motion,
            "privacy.default_timer",
            scoped.response.rect,
            t,
        );
        ui.add_space(16.0);
        group_label(ui, t, &crate::i18n::t("privacy.account"));
        if self.live_mode && !self.privacy_state.is_empty() {
            // These genuinely round-trip to WhatsApp through the
            // library's privacy API while linked.
            let state = self.privacy_state.clone();
            let mut cycle: Vec<&str> = Vec::new();
            settings_card(ui, t, |ui| {
                for (label, key) in [
                    ("Last seen", "last"),
                    ("Online", "online"),
                    ("Profile photo", "profile"),
                    ("About", "status"),
                    ("Group adds", "groupadd"),
                ] {
                    let current = state.get(key).cloned().unwrap_or_else(|| "-".to_owned());
                    row_with(ui, t, label, "Tap to cycle the audience", |ui| {
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::live::privacy_value_label(&current),
                            crate::widgets::Btn::Secondary,
                        )
                        .clicked()
                        {
                            cycle.push(key);
                        }
                    });
                    divider(ui, t);
                }
                ui.label(
                    RichText::new("Changes are sent to WhatsApp immediately (live mode).")
                        .size(11.5)
                        .color(rgb(t.text_faint)),
                );
            });
            for category in cycle {
                self.cycle_privacy(category);
            }
        } else {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(
                        "Last seen & online, profile photo, About, and group adds are \
                         managed on your phone in WhatsApp > Settings > Privacy. When \
                         Zaprust is linked, this page shows and edits them here for real.",
                    )
                    .size(12.5)
                    .color(rgb(t.text_secondary)),
                );
            });
        }
        ui.add_space(16.0);
        group_label(ui, t, "Security");
        settings_card(ui, t, |ui| {
            ui.label(
                RichText::new(
                    "Blocked contacts and two-step verification are managed on your \
                     phone in WhatsApp > Settings. Zaprust shows no fake controls \
                     for them.",
                )
                .size(12.5)
                .color(rgb(t.text_secondary)),
            );
        });
    }
}
