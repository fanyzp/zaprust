//! The Notifications settings page: in-app alert switches.

use super::*;

impl App {
    pub(crate) fn page_notifications(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.notifications"));
        group_label(ui, t, &crate::i18n::t("notifications.alerts"));
        settings_card(ui, t, |ui| {
            let mut v = self.notify_messages;
            switch_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "notifications.messages",
                t,
                "Message notifications",
                "",
                &mut v,
            );
            self.notify_messages = v;
            divider(ui, t);
            let mut v = self.notify_groups;
            switch_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "notifications.groups",
                t,
                "Group notifications",
                "Including @mentions and replies",
                &mut v,
            );
            self.notify_groups = v;
        });
        ui.add_space(SP_4);
        group_label(ui, t, &crate::i18n::t("notifications.sound_preview"));
        settings_card(ui, t, |ui| {
            let mut v = self.notify_sound;
            switch_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "notifications.sound",
                t,
                "Sound",
                "",
                &mut v,
            );
            self.notify_sound = v;
            divider(ui, t);
            let mut v = self.notify_preview;
            switch_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "notifications.preview",
                t,
                "Show preview",
                "Message text inside the notification (less private)",
                &mut v,
            );
            self.notify_preview = v;
        });
        ui.add_space(SP_4);
        settings_card(ui, t, |ui| {
            let mut v = self.tray_residence;
            switch_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "notifications.tray",
                t,
                &crate::i18n::t("notifications.tray"),
                &crate::i18n::t("notifications.tray_caption"),
                &mut v,
            );
            if v != self.tray_residence {
                self.tray_residence = v;
                self.persist_settings();
            }
        });
        // (Persistence: toggles write through in the card above
        // via persist-on-change, see the snapshot below.)
        if self.notify_snapshot
            != Some((
                self.notify_messages,
                self.notify_groups,
                self.notify_sound,
                self.notify_preview,
            ))
        {
            self.notify_snapshot = Some((
                self.notify_messages,
                self.notify_groups,
                self.notify_sound,
                self.notify_preview,
            ));
            self.persist_settings();
        }
    }
}
