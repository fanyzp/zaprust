//! The Chats settings page: sending and history behavior.

use super::*;

impl App {
    pub(crate) fn page_chats(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.chats"));
        group_label(ui, t, &crate::i18n::t("chats_page.sending"));
        settings_card(ui, t, |ui| {
            let mut v = self.enter_sends;
            switch_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "chats.enter_sends",
                t,
                &crate::i18n::t("chats_page.enter_sends"),
                &crate::i18n::t("chats_page.enter_sends_caption"),
                &mut v,
            );
            if v != self.enter_sends {
                self.enter_sends = v;
                self.persist_settings();
            }
        });
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("chats_page.writing"));
        settings_card(ui, t, |ui| {
            let mut v = self.spell_check;
            switch_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "chats.spell_check",
                t,
                &crate::i18n::t("chats_page.spell_check"),
                &crate::i18n::t("chats_page.spell_check_caption"),
                &mut v,
            );
            if v != self.spell_check {
                self.spell_check = v;
                self.persist_settings();
            }
            if self.spell_check {
                divider(ui, t);
                ui.label(
                    egui::RichText::new(crate::i18n::t("chats_page.spell_personal"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                let response = crate::widgets::text_field(
                    ui,
                    t,
                    &mut self.spell_personal_text,
                    &crate::i18n::t("chats_page.spell_personal_hint"),
                    ui.available_width(),
                );
                if response.changed() {
                    // The personal dictionary is a word list:
                    // comma or whitespace separated, stored
                    // sorted and deduplicated, and the live
                    // checker follows at once.
                    let mut words: Vec<String> = self
                        .spell_personal_text
                        .split(|c: char| c == ',' || c.is_whitespace())
                        .map(|w| w.trim().to_owned())
                        .filter(|w| !w.is_empty())
                        .collect();
                    words.sort();
                    words.dedup();
                    self.spell_personal = words;
                    if let Some(checker) = &mut self.spell_checker {
                        checker.set_personal(&self.spell_personal);
                    }
                    self.persist_settings();
                }
            }
        });
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("chats_page.archiving"));
        settings_card(ui, t, |ui| {
            let mut v = self.keep_archived;
            switch_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "chats.keep_archived",
                t,
                "Keep chats archived",
                "Archived chats stay archived when new messages arrive",
                &mut v,
            );
            if v != self.keep_archived {
                self.keep_archived = v;
                self.persist_settings();
            }
        });
    }
}
