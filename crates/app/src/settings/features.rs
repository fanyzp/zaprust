//! The Features settings page: the native feature switchboard and feature options.

use super::*;

/// Display label for a stored GIPHY content rating value. The
/// stored values stay the raw GIPHY codes (`giphy::fetch_params`
/// validates and the API expects them); only the rendering is
/// human. Unknown values fall back to the least permissive
/// label, matching the stored default.
pub(super) fn rating_label(stored: &str) -> &'static str {
    match stored {
        "pg" => "PG",
        "pg-13" => "PG-13",
        "r" => "R",
        _ => "General",
    }
}

impl App {
    /// The Features category: every former plugin is a native
    /// feature of the app now. The switches are the whole control
    /// surface; each feature's own options sit under its switch.
    pub(crate) fn page_features(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.features"));
        group_label(ui, t, "Feature switches");
        let mut changed = false;
        settings_card(ui, t, |ui| {
            for (i, (id, name, _search_desc)) in FEATURE_ROWS.iter().enumerate() {
                let mut on = self.feature_flags.get(*id).copied().unwrap_or(false);
                row_hl(ui, &mut self.settings_highlight, self.motion, id, t, name, "", |ui| {
                    switch(ui, &mut on, t)
                });
                if on != self.feature_flags.get(*id).copied().unwrap_or(false) {
                    self.feature_flags.insert((*id).to_owned(), on);
                    changed = true;
                }
                if i + 1 < FEATURE_ROWS.len() {
                    divider(ui, t);
                }
            }
        });
        if changed {
            self.persist_settings();
        }
        self.section_gif(ui, t);
        self.section_credentials(ui, t);
    }

    /// GIF search preferences (content rating, results per
    /// search), stored as the raw map `giphy::fetch_params`
    /// validates.
    fn section_gif(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        ui.add_space(16.0);
        group_label(ui, t, "GIF search");
        let (limit, rating) = giphy::fetch_params(&self.giphy_prefs);
        let mut new_limit = limit;
        let mut new_rating = rating.clone();
        settings_card(ui, t, |ui| {
            form_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "features.gif_rating",
                t,
                "Content rating",
                None,
                |ui| {
                    select(ui, "giphy_rating", FORM_CONTROL_W, rating_label(&new_rating), |ui| {
                        for r in ["g", "pg", "pg-13", "r"] {
                            ui.selectable_value(&mut new_rating, r.to_owned(), rating_label(r));
                        }
                    });
                },
            );
            form_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "features.gif_limit",
                t,
                &crate::i18n::t("features.results_per_search"),
                None,
                |ui| {
                    let mut v = f64::from(new_limit);
                    if number_field(ui, t, &mut v, 1.0..=50.0, 96.0, 1.0).changed() {
                        new_limit = v as u32;
                    }
                },
            );
        });
        if new_limit != limit || new_rating != rating {
            self.giphy_prefs.insert("result_limit".to_owned(), serde_json::json!(new_limit));
            self.giphy_prefs.insert("rating".to_owned(), serde_json::json!(new_rating));
            self.persist_settings();
        }
    }

    /// Features declare the slots they need; values live in the
    /// OS keyring (or the 0600 fallback file), never in per-feature
    /// config files.
    pub(crate) fn section_credentials(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        ui.add_space(16.0);
        group_label(ui, t, "API keys");
        let backend_name = self.credentials.backend_name();
        // A backend whose at-rest protection is weaker than its
        // name might suggest discloses it in the same breath
        // (privacy audit A10: the local secrets file holds the
        // keys unencrypted under its owner-only permissions, and
        // the user should not have to guess that).
        let storage_note = match self.credentials.storage_note() {
            Some(note) => format!(" {note}"),
            None => String::new(),
        };
        let key_hint = format!("Stored in: {backend_name}");
        let slots: Vec<(String, core::credentials::CredentialSlot)> =
            self.credentials.declared_slots().to_vec();
        settings_card(ui, t, |ui| {
            ui.label(
                RichText::new(format!("Stored in: {backend_name}.{storage_note}"))
                    .size(12.0)
                    .color(rgb(t.text_secondary)),
            );
            if slots.is_empty() {
                ui.label(RichText::new(crate::i18n::t("features.no_key_needed")).size(12.0));
            }
            for (owner, slot) in &slots {
                divider(ui, t);
                ui.label(RichText::new(format!("{} · {}", slot.label, owner)).size(12.0).strong());
                let keys = self.credentials.keys_for(&slot.id);
                if keys.is_empty() {
                    ui.label(RichText::new(crate::i18n::t("features.no_keys_yet")).size(12.0).color(rgb(t.text_faint)));
                }
                let mut remove: Option<String> = None;
                for key in &keys {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(core::credentials::mask(key)).size(12.0).monospace(),
                        );
                        // A key row's only action is removal:
                        // a quiet danger-tinted icon, not a
                        // filled red pill on every row.
                        if crate::widgets::icon_button(
                            ui,
                            t,
                            crate::icons::Icon::Delete,
                            28.0,
                            "Remove key",
                            Some(t.danger),
                        )
                        .clicked()
                        {
                            remove = Some(key.clone());
                        }
                    });
                }
                if let Some(key) = remove {
                    let _ = self.credentials.remove_key(&slot.id, &key);
                }
                form_row_hl(
                    ui,
                    &mut self.settings_highlight,
                    self.motion,
                    "features.api_keys",
                    t,
                    "API key",
                    Some(&key_hint),
                    |ui| {
                        let draft = self.credential_drafts.entry(slot.id.clone()).or_default();
                        text_field_password(ui, t, draft, "Paste an API key", FORM_CONTROL_W);
                        if crate::widgets::button(ui, t, "Add key", crate::widgets::Btn::Primary)
                            .clicked()
                        {
                            let value = self
                                .credential_drafts
                                .get(&slot.id)
                                .cloned()
                                .unwrap_or_default()
                                .trim()
                                .to_owned();
                            if !value.is_empty() {
                                match self.credentials.add_key(&slot.id, value) {
                                    Ok(()) => {
                                        self.credential_drafts.remove(&slot.id);
                                    }
                                    Err(e) => {
                                        self.notice =
                                            Some(Notice::error(format!("Credential store: {e}")));
                                    }
                                }
                            }
                        }
                    },
                );
            }
        });
    }
}
