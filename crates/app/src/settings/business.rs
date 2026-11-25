//! The Business settings page (parity P10, rows 132 and
//! 133): the synced quick replies and the signed-in
//! account's own business profile. Both are account state on
//! the server, so the page exists only inside a live
//! session; demo mode has neither to show.

use super::*;

impl App {
    pub(crate) fn page_business(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("business.title"));
        if self.live.is_none() {
            ui.label(
                RichText::new(crate::i18n::t("business.live_only"))
                    .size(15.0)
                    .color(rgb(t.text_secondary)),
            );
            return;
        }
        self.section_quick_replies(ui, t);
        ui.add_space(SP_4);
        self.section_own_profile(ui, t);
    }

    /// The quick replies: one row per reply with its actions
    /// behind the row's menu, and the editor form in place of
    /// the list while one is open.
    fn section_quick_replies(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        group_label(ui, t, &crate::i18n::t("business.quick_title"));
        if self.business.quick_editor.is_some() {
            self.quick_editor_form(ui, t);
            return;
        }
        let replies = self.business.quick_replies.clone();
        if replies.is_empty() {
            // The empty state sits in the same card as the
            // populated list, so the group reads the same
            // whether or not replies exist yet.
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("business.quick_empty"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
            });
            ui.add_space(SP_2);
        } else {
            settings_card(ui, t, |ui| {
                let mut edit: Option<backend::LiveQuickReply> = None;
                let mut delete: Option<backend::LiveQuickReply> = None;
                let n = replies.len();
                for (i, reply) in replies.iter().enumerate() {
                    let resp = ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(format!("/{}", reply.shortcut))
                                    .font(crate::theme::semibold(15.0)),
                            );
                            ui.label(
                                RichText::new(ellipsize_chars(&reply.message, 60))
                                    .size(12.0)
                                    .color(rgb(t.text_secondary)),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let anchor = crate::widgets::icon_button(
                                ui,
                                t,
                                crate::icons::Icon::MoreVert,
                                28.0,
                                &crate::i18n::t("common.actions"),
                                None,
                            );
                            anchor.context_menu(|ui| {
                                ui.set_min_width(160.0);
                                if crate::widgets::menu_item_icon(
                                    ui,
                                    t,
                                    Some(crate::icons::Icon::Edit),
                                    &crate::i18n::t("common.edit"),
                                    None,
                                )
                                .clicked()
                                {
                                    edit = Some(reply.clone());
                                    ui.close();
                                }
                                if crate::widgets::menu_item_danger(
                                    ui,
                                    t,
                                    crate::icons::Icon::Delete,
                                    &crate::i18n::t("common.delete"),
                                )
                                .clicked()
                                {
                                    delete = Some(reply.clone());
                                    ui.close();
                                }
                            });
                        });
                    });
                    let _ = resp;
                    if i + 1 < n {
                        divider(ui, t);
                    }
                }
                if let Some(reply) = edit {
                    self.business.quick_editor = Some(crate::state::QuickReplyForm {
                        id: Some(reply.id.clone()),
                        shortcut: reply.shortcut.clone(),
                        message: reply.message.clone(),
                        keywords: reply.keywords.join(", "),
                    });
                    self.business.quick_editor_error = None;
                }
                if let Some(reply) = delete {
                    self.request_delete_quick_reply(&reply.id);
                }
            });
        }
        if crate::widgets::button(
            ui,
            t,
            &crate::i18n::t("business.quick_new"),
            crate::widgets::Btn::Secondary,
        )
        .clicked()
        {
            self.business.quick_editor = Some(crate::state::QuickReplyForm {
                id: None,
                shortcut: String::new(),
                message: String::new(),
                keywords: String::new(),
            });
            self.business.quick_editor_error = None;
        }
    }

    /// The quick reply editor: shortcut, message, keywords,
    /// the inline error, and Save/Cancel.
    fn quick_editor_form(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        let mut save = false;
        let mut cancel = false;
        if let Some(form) = &mut self.business.quick_editor {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("business.quick_shortcut"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                let _ = crate::widgets::text_field(
                    ui,
                    t,
                    &mut form.shortcut,
                    &crate::i18n::t("business.quick_shortcut"),
                    ui.available_width(),
                );
                ui.add_space(SP_2);
                ui.label(
                    RichText::new(crate::i18n::t("business.quick_message"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                let _ = crate::widgets::text_field(
                    ui,
                    t,
                    &mut form.message,
                    &crate::i18n::t("business.quick_message"),
                    ui.available_width(),
                );
                ui.add_space(SP_2);
                ui.label(
                    RichText::new(crate::i18n::t("business.quick_keywords"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                let _ = crate::widgets::text_field(
                    ui,
                    t,
                    &mut form.keywords,
                    &crate::i18n::t("business.quick_keywords"),
                    ui.available_width(),
                );
            });
            if let Some(error) = &self.business.quick_editor_error {
                ui.add_space(SP_1);
                ui.label(RichText::new(error).size(12.0).color(rgb(t.danger)));
            }
            ui.add_space(SP_2);
            ui.horizontal(|ui| {
                if crate::widgets::button(
                    ui,
                    t,
                    &crate::i18n::t("common.save"),
                    crate::widgets::Btn::Primary,
                )
                .clicked()
                {
                    save = true;
                }
                if crate::widgets::button(
                    ui,
                    t,
                    &crate::i18n::t("common.cancel"),
                    crate::widgets::Btn::Secondary,
                )
                .clicked()
                {
                    cancel = true;
                }
            });
        }
        if save {
            self.submit_quick_editor();
        } else if cancel {
            self.business.quick_editor = None;
            self.business.quick_editor_error = None;
        }
    }

    /// The own business profile: the fetched profile as a
    /// summary with an Edit action, or the editor form while
    /// it is open. An account with no business profile says
    /// so plainly (the server creates profiles from the
    /// phone's business app, not from a form here).
    fn section_own_profile(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        group_label(ui, t, &crate::i18n::t("business.own_title"));
        self.ensure_own_business_profile();
        if self.business.own_form.is_some() {
            self.own_profile_form(ui, t);
            return;
        }
        match &self.business.own_profile {
            None => {
                let key = if self.business.own_profile_failed.is_some() {
                    "business.own_failed"
                } else {
                    "business.own_loading"
                };
                ui.label(RichText::new(crate::i18n::t(key)).size(12.0).color(rgb(t.text_faint)));
            }
            Some(None) => {
                ui.label(
                    RichText::new(crate::i18n::t("business.own_not_business"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
            }
            Some(Some(profile)) => {
                let profile = profile.clone();
                settings_card(ui, t, |ui| {
                    if !profile.description.is_empty() {
                        ui.label(RichText::new(&profile.description).size(15.0));
                    }
                    if let Some(email) = &profile.email {
                        ui.label(RichText::new(email).size(12.0).color(rgb(t.text_secondary)));
                    }
                    if let Some(address) = &profile.address {
                        ui.label(RichText::new(address).size(12.0).color(rgb(t.text_secondary)));
                    }
                    if !profile.websites.is_empty() {
                        ui.label(
                            RichText::new(profile.websites.join(", "))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                    }
                });
                ui.add_space(SP_2);
                if crate::widgets::button(
                    ui,
                    t,
                    &crate::i18n::t("business.own_edit"),
                    crate::widgets::Btn::Secondary,
                )
                .clicked()
                {
                    self.open_own_business_form();
                }
            }
        }
    }

    /// The own-profile editor. Categories are keep/remove
    /// checkboxes over the profile's current categories: the
    /// library exposes no categories directory to pick new
    /// ones from, and the form says so (row 133's bound).
    /// Coordinates and a cover photo are not offered: the
    /// read profile never returns coordinates to edit from,
    /// and the cover upload is not implemented in the
    /// library.
    fn own_profile_form(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        let mut save = false;
        let mut cancel = false;
        // The current categories' names, for the keep/remove
        // checkboxes (the form itself stores only kept ids).
        let categories: Vec<(String, String)> = match &self.business.own_profile {
            Some(Some(profile)) => {
                profile.categories.iter().map(|c| (c.id.clone(), c.name.clone())).collect()
            }
            _ => Vec::new(),
        };
        if let Some(form) = &mut self.business.own_form {
            settings_card(ui, t, |ui| {
                field_label(ui, t, "business.description");
                let _ = crate::widgets::text_field(
                    ui,
                    t,
                    &mut form.description,
                    "",
                    ui.available_width(),
                );
                ui.add_space(SP_2);
                field_label(ui, t, "business.email");
                let _ =
                    crate::widgets::text_field(ui, t, &mut form.email, "", ui.available_width());
                ui.add_space(SP_2);
                field_label(ui, t, "business.address");
                let _ =
                    crate::widgets::text_field(ui, t, &mut form.address, "", ui.available_width());
                ui.add_space(SP_2);
                field_label(ui, t, "business.websites");
                let _ =
                    crate::widgets::text_field(ui, t, &mut form.websites, "", ui.available_width());
                ui.add_space(SP_2);
                if !categories.is_empty() {
                    field_label(ui, t, "business.categories");
                    for (id, name) in &categories {
                        let keep = form.kept_categories.iter().any(|k| k == id);
                        if crate::widgets::checkbox_row(ui, t, keep, name.as_str()) {
                            if keep {
                                form.kept_categories.retain(|k| k != id);
                            } else {
                                form.kept_categories.push(id.clone());
                            }
                        }
                    }
                    ui.label(
                        RichText::new(crate::i18n::t("business.categories_bound"))
                            .size(12.0)
                            .color(rgb(t.text_faint)),
                    );
                    ui.add_space(SP_2);
                }
                field_label(ui, t, "business.timezone");
                let _ =
                    crate::widgets::text_field(ui, t, &mut form.timezone, "", ui.available_width());
                ui.add_space(SP_2);
                field_label(ui, t, "business.hours");
                const MODES: [(&str, &str); 4] = [
                    ("closed", "business.closed"),
                    ("open_24h", "business.open_24h"),
                    ("specific_hours", "business.day_specific"),
                    ("appointment_only", "business.appointment_only"),
                ];
                for (i, day) in form.days.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(crate::i18n::t(day_name_key(day.day)))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                        let current = MODES
                            .iter()
                            .find(|(value, _)| *value == day.mode)
                            .map(|(_, key)| crate::i18n::t(key))
                            .unwrap_or_default();
                        let combo = egui::ComboBox::from_id_salt(("own-biz-day", i))
                            .selected_text(current)
                            .width(150.0)
                            .show_ui(ui, |ui| {
                                for (value, key) in MODES {
                                    if ui
                                        .selectable_label(day.mode == value, crate::i18n::t(key))
                                        .clicked()
                                    {
                                        day.mode = value.to_string();
                                    }
                                }
                            });
                        crate::anim::stock_popup_enter(
                            ui.ctx(),
                            combo.response.id.with("popup"),
                            combo.response.rect.left_top(),
                        );
                        if day.mode == "specific_hours" {
                            ui.add(egui::TextEdit::singleline(&mut day.open).desired_width(52.0));
                            ui.label("-");
                            ui.add(egui::TextEdit::singleline(&mut day.close).desired_width(52.0));
                        }
                    });
                }
            });
            if let Some(error) = &self.business.own_error {
                ui.add_space(SP_1);
                ui.label(RichText::new(error).size(12.0).color(rgb(t.danger)));
            }
            ui.add_space(SP_2);
            ui.horizontal(|ui| {
                if crate::widgets::button_enabled(
                    ui,
                    t,
                    &crate::i18n::t("common.save"),
                    crate::widgets::Btn::Primary,
                    !self.business.own_saving,
                )
                .clicked()
                {
                    save = true;
                }
                if crate::widgets::button(
                    ui,
                    t,
                    &crate::i18n::t("common.cancel"),
                    crate::widgets::Btn::Secondary,
                )
                .clicked()
                {
                    cancel = true;
                }
            });
        }
        if save {
            self.submit_own_business_form();
        } else if cancel {
            self.business.own_form = None;
            self.business.own_error = None;
        }
    }
}

/// A small field caption inside the business forms.
fn field_label(ui: &mut egui::Ui, t: core::theme::Tokens, key: &str) {
    ui.label(RichText::new(crate::i18n::t(key)).size(12.0).color(rgb(t.text_secondary)));
}

/// Truncates to at most `max` characters with an ellipsis,
/// for the one-line previews on this page.
fn ellipsize_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        crate::widgets::ellipsize_fit(text, |p| p.chars().count() <= max)
    }
}

/// The locale key for a form day's wire spelling.
fn day_name_key(day: &str) -> &'static str {
    match day {
        "mon" => "business.day_mon",
        "tue" => "business.day_tue",
        "wed" => "business.day_wed",
        "thu" => "business.day_thu",
        "fri" => "business.day_fri",
        "sat" => "business.day_sat",
        _ => "business.day_sun",
    }
}
