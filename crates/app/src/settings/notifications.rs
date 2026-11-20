//! The Notifications settings page: in-app alert switches.

use super::*;

impl App {
    pub(crate) fn page_notifications(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(
            ui,
            &crate::i18n::t("settings.notifications"),
            &crate::i18n::t("notifications.subtitle"),
        );
        ui.label(
            RichText::new(crate::i18n::t("notifications.caption"))
                .size(12.0)
                .color(rgb(t.text_secondary)),
        );
        ui.add_space(6.0);
        group_label(ui, t, "Alerts");
        settings_card(ui, t, |ui| {
            let mut v = self.notify_messages;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "notifications.messages",
                t,
                "Message notifications",
                "Show a desktop notification for new messages",
                |ui| switch(ui, &mut v, t),
            );
            self.notify_messages = v;
            divider(ui, t);
            let mut v = self.notify_groups;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "notifications.groups",
                t,
                "Group notifications",
                "Including @mentions and replies",
                |ui| {
                    switch(ui, &mut v, t);
                },
            );
            self.notify_groups = v;
            divider(ui, t);
            let mut v = self.notify_sound;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "notifications.sound",
                t,
                "Sound",
                "Play a sound for incoming messages",
                |ui| {
                    switch(ui, &mut v, t);
                },
            );
            self.notify_sound = v;
            divider(ui, t);
            let mut v = self.notify_preview;
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "notifications.preview",
                t,
                "Show preview",
                "Message text inside the notification (less private)",
                |ui| switch(ui, &mut v, t),
            );
            self.notify_preview = v;
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
