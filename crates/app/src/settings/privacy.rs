//! The Privacy & Security settings page: receipts, presence, account privacy.

use super::*;

impl App {
    pub(crate) fn page_privacy(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.privacy"));
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
                "",
                |ui| switch(ui, &mut tp, t),
            );
            if tp != self.typing_indicators {
                self.set_typing_indicators(tp);
            }
        });
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("privacy.default_timer"));
        let scoped = ui.scope(|ui| {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("privacy.default_timer_caption"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                ui.add_space(SP_1);
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
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("privacy.account"));
        if self.live_mode && !self.privacy_state.is_empty() {
            // These genuinely round-trip to WhatsApp through the
            // library's privacy API while linked.
            let state = self.privacy_state.clone();
            let mut cycle: Vec<(&str, &str)> = Vec::new();
            settings_card(ui, t, |ui| {
                for (label, key) in [
                    ("Last seen", "last"),
                    ("Online", "online"),
                    ("Profile photo", "profile"),
                    ("About", "status"),
                    ("Group adds", "groupadd"),
                ] {
                    let current = state.get(key).cloned().unwrap_or_else(|| "-".to_owned());
                    row_with(ui, t, label, "", |ui| {
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::live::privacy_value_label(&current),
                            crate::widgets::Btn::Secondary,
                        )
                        .clicked()
                        {
                            cycle.push((key, label));
                        }
                    });
                    divider(ui, t);
                }
                ui.label(
                    RichText::new(crate::i18n::t("privacy.live_note"))
                        .size(12.0)
                        .color(rgb(t.text_faint)),
                );
            });
            for (category, label) in cycle {
                self.request_cycle_privacy(category, label);
            }
        } else {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(
                        "Last seen & online, profile photo, About, and group adds are \
                         managed on your phone in WhatsApp > Settings > Privacy. When \
                         Zaprust is linked, this page shows and edits them here for real.",
                    )
                    .size(12.0)
                    .color(rgb(t.text_secondary)),
                );
            });
        }
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("privacy.blocked"));
        settings_card(ui, t, |ui| {
            let count = self.blocked_chats.len();
            let caption = if count == 0 {
                crate::i18n::t("privacy.blocked_none")
            } else {
                crate::i18n::tf("privacy.blocked_count", "count", &count.to_string())
            };
            let mut open = false;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "privacy.blocked",
                t,
                &crate::i18n::t("privacy.blocked"),
                &caption,
                |ui| {
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("common.open"),
                        crate::widgets::Btn::Secondary,
                    )
                    .clicked()
                    {
                        open = true;
                    }
                },
            );
            if open {
                self.settings_blocked_open = true;
            }
        });
        ui.add_space(SP_4);
        self.lock_code_section(ui, t);
        ui.add_space(SP_4);
        group_label(ui, t, "Security");
        settings_card(ui, t, |ui| {
            ui.label(
                RichText::new(crate::i18n::t("privacy.two_step_note"))
                    .size(12.0)
                    .color(rgb(t.text_secondary)),
            );
        });
    }

    /// The lock code section of Privacy & Security: create,
    /// change or remove the local lock code, the app-lock
    /// switch it enables, and Lock now. Changing or removing
    /// asks for the current code inline; the outcome is
    /// stated inline on failure and as a notice on success.
    /// The caption states the code's bounds (see
    /// core::lockcode): a window gate, not encryption.
    fn lock_code_section(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        group_label(ui, t, &crate::i18n::t("privacy.lock_group"));
        let has_code = self.lock_code.is_some();
        let mut action: Option<&'static str> = None;
        settings_card(ui, t, |ui| {
            ui.label(
                RichText::new(crate::i18n::t("privacy.lock_caption"))
                    .size(12.0)
                    .color(rgb(t.text_secondary)),
            );
            ui.add_space(SP_2);
            if has_code {
                crate::widgets::text_field_password(
                    ui,
                    t,
                    &mut self.lock_current_entry,
                    &crate::i18n::t("locked.current_hint"),
                    240.0,
                );
                ui.add_space(SP_1);
            }
            crate::widgets::text_field_password(
                ui,
                t,
                &mut self.lock_new_entry,
                &crate::i18n::t("locked.new_code_hint"),
                240.0,
            );
            if let Some(key) = self.lock_form_error {
                ui.add_space(SP_1);
                ui.label(
                    RichText::new(crate::i18n::t(key)).size(12.0).color(rgb(t.danger)),
                );
            }
            ui.add_space(SP_2);
            ui.horizontal(|ui| {
                if has_code {
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("privacy.change_code"),
                        crate::widgets::Btn::Secondary,
                    )
                    .clicked()
                    {
                        action = Some("change");
                    }
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("privacy.remove_code"),
                        crate::widgets::Btn::Danger,
                    )
                    .clicked()
                    {
                        action = Some("remove");
                    }
                } else if crate::widgets::button(
                    ui,
                    t,
                    &crate::i18n::t("locked.create_code"),
                    crate::widgets::Btn::Primary,
                )
                .clicked()
                {
                    action = Some("create");
                }
            });
            if has_code {
                divider(ui, t);
                let mut v = self.app_lock_enabled;
                row_hl(
                    ui,
                    &mut self.settings_highlight,
                    self.motion,
                    "privacy.app_lock",
                    t,
                    &crate::i18n::t("privacy.app_lock"),
                    &crate::i18n::t("privacy.app_lock_caption"),
                    |ui| switch(ui, &mut v, t),
                );
                if v != self.app_lock_enabled {
                    self.app_lock_enabled = v;
                    self.persist_settings();
                }
                divider(ui, t);
                row_with(ui, t, &crate::i18n::t("privacy.lock_now"), "", |ui| {
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("privacy.lock_now"),
                        crate::widgets::Btn::Secondary,
                    )
                    .clicked()
                    {
                        action = Some("lock_now");
                    }
                });
            }
        });
        match action {
            Some("create") => {
                if self.lock_new_entry.chars().count() < core::lockcode::MIN_CODE_LEN {
                    self.lock_form_error = Some("locked.too_short");
                } else {
                    self.lock_code =
                        Some(core::lockcode::LockCode::new(&self.lock_new_entry.clone()));
                    self.persist_settings();
                    self.lock_new_entry.clear();
                    self.lock_form_error = None;
                    self.notice = Some(Notice::info(crate::i18n::t("privacy.lock_set")));
                }
            }
            Some("change") => {
                if !self
                    .lock_code
                    .as_ref()
                    .is_some_and(|c| c.verify(&self.lock_current_entry))
                {
                    self.lock_form_error = Some("locked.wrong_code");
                } else if self.lock_new_entry.chars().count() < core::lockcode::MIN_CODE_LEN {
                    self.lock_form_error = Some("locked.too_short");
                } else {
                    self.lock_code =
                        Some(core::lockcode::LockCode::new(&self.lock_new_entry.clone()));
                    self.persist_settings();
                    self.lock_current_entry.clear();
                    self.lock_new_entry.clear();
                    self.lock_form_error = None;
                    self.notice = Some(Notice::info(crate::i18n::t("privacy.lock_changed")));
                }
            }
            Some("remove") => {
                if !self
                    .lock_code
                    .as_ref()
                    .is_some_and(|c| c.verify(&self.lock_current_entry))
                {
                    self.lock_form_error = Some("locked.wrong_code");
                } else {
                    self.lock_code = None;
                    self.app_lock_enabled = false;
                    self.persist_settings();
                    self.lock_current_entry.clear();
                    self.lock_new_entry.clear();
                    self.lock_form_error = None;
                    self.notice = Some(Notice::info(crate::i18n::t("privacy.lock_removed")));
                }
            }
            Some("lock_now") => {
                self.app_locked_now = true;
                self.code_entry.clear();
                self.code_error = None;
            }
            _ => {}
        }
    }
}
