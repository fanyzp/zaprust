//! The Chats settings page: sending and history behavior.

use super::*;

impl App {
    pub(crate) fn page_chats(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.chats"), &crate::i18n::t("chats_page.subtitle"));
        group_label(ui, t, "Behavior");
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
            self.enter_sends = v;
            divider(ui, t);
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
            self.keep_archived = v;
            divider(ui, t);
            let mut lp = self.link_previews;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "chats.link_previews",
                t,
                &crate::i18n::t("chats_page.link_previews"),
                &crate::i18n::t("chats_page.link_previews_caption"),
                |ui| switch(ui, &mut lp, t),
            );
            if lp != self.link_previews {
                self.link_previews = lp;
                self.persist_settings();
            }
            divider(ui, t);
            // The hint bar's re-show switch: dismissing the bar in
            // the composer sets this off, and this is the only way
            // back on, so the dismissed surface is never lost.
            let mut show = !self.composer.hint_dismissed;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "chats.composer_hint",
                t,
                &crate::i18n::t("chats_page.composer_hint"),
                &crate::i18n::t("chats_page.composer_hint_caption"),
                |ui| switch(ui, &mut show, t),
            );
            let new_dismissed = !show;
            if new_dismissed != self.composer.hint_dismissed {
                self.composer.hint_dismissed = new_dismissed;
                self.persist_settings();
            }
        });
    }
}
