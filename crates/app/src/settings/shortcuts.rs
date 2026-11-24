//! The Shortcuts settings page: the read-only reference for
//! the app's keyboard shortcuts. The rows render from the
//! same table the shortcut handler answers
//! (crate::shortcuts::SHORTCUTS), so the page cannot promise
//! a chord the app does not honor, or hide one it does.

use super::*;

impl App {
    pub(crate) fn page_shortcuts(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, &crate::i18n::t("settings.shortcuts"));
        settings_card(ui, t, |ui| {
            let rows = crate::shortcuts::SHORTCUTS;
            for (idx, shortcut) in rows.iter().enumerate() {
                if idx > 0 {
                    divider(ui, t);
                }
                let chord = shortcut.chord;
                row_hl(
                    ui,
                    &mut self.settings_highlight,
                    self.motion,
                    shortcut.id,
                    t,
                    &crate::i18n::t(shortcut.label_key),
                    "",
                    |ui| {
                        ui.label(RichText::new(chord).size(13.0).color(rgb(t.text_secondary)));
                    },
                );
            }
        });
        ui.add_space(SP_2);
        ui.label(
            RichText::new(crate::i18n::t("shortcuts.note")).size(12.0).color(rgb(t.text_faint)),
        );
    }
}
