//! The Chats settings page: sending and history behavior.

use super::*;

impl App {
    pub(crate) fn page_chats(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.chats"));
        group_label(ui, t, &crate::i18n::t("chats_page.sending"));
        settings_card(ui, t, |ui| {
            let mut v = self.enter_sends;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "chats.enter_sends",
                t,
                &crate::i18n::t("chats_page.enter_sends"),
                &crate::i18n::t("chats_page.enter_sends_caption"),
                |ui| switch(ui, &mut v, t),
            );
            if v != self.enter_sends {
                self.enter_sends = v;
                self.persist_settings();
            }
        });
        ui.add_space(16.0);
        group_label(ui, t, &crate::i18n::t("chats_page.archiving"));
        settings_card(ui, t, |ui| {
            let mut v = self.keep_archived;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "chats.keep_archived",
                t,
                "Keep chats archived",
                "Archived chats stay archived when new messages arrive",
                |ui| switch(ui, &mut v, t),
            );
            if v != self.keep_archived {
                self.keep_archived = v;
                self.persist_settings();
            }
        });
    }
}
