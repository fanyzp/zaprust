//! The blocked-contacts page (parity row 101), reached from
//! Privacy & Security and drawn inside the settings panel:
//! the secondary-surface rule applies here too, so the page
//! is a list in the panel, never a full screen.
//!
//! One row per blocked contact this client can name (the
//! chat roster, then the live session's address book: the
//! same sources the profile panel reasons from). Every
//! toggle, in both directions, rides the profile panel's
//! block path: the set flips at once, the confirming
//! Blocklist report speaks the success, and a BlockFailed
//! restores the set exactly. A "Block a contact" picker over
//! the roster's direct chats not yet blocked feeds the same
//! path.

use super::*;

impl App {
    pub(crate) fn page_blocked(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        // Header: a back chevron to Privacy & Security and
        // the page's own title, the filtered-list pattern.
        let mut back = false;
        ui.horizontal(|ui| {
            if crate::widgets::icon_button(
                ui,
                t,
                crate::icons::Icon::ChevronLeft,
                28.0,
                &crate::i18n::t("common.back"),
                None,
            )
            .clicked()
            {
                back = true;
            }
            ui.label(
                RichText::new(crate::i18n::t("privacy.blocked"))
                    .font(crate::theme::semibold(crate::theme::TEXT_TITLE)),
            );
        });
        if back {
            self.settings_blocked_open = false;
            self.settings_tab = "Privacy & Security";
            return;
        }
        ui.add_space(SP_4);
        // The named blocked contacts, in title order. An id
        // the roster and the address book cannot name does
        // not render: a row without a name would be a dead
        // surface, and the count on the Privacy page still
        // reports the set's full size.
        let mut entries: Vec<(u64, String, String)> = Vec::new();
        for id in self.blocked_chats.iter().copied().collect::<Vec<u64>>() {
            if let Some(chat) = self.chats.iter().find(|c| c.id == id) {
                entries.push((id, chat.title.clone(), chat.subtitle.clone()));
            } else if let Some(session) = &self.live {
                if let Some(jid) = session.jids.get(&id) {
                    let name = session
                        .names
                        .get(jid)
                        .filter(|n| !n.is_empty())
                        .cloned()
                        .unwrap_or_else(|| jid.clone());
                    entries.push((id, name, jid.clone()));
                }
            }
        }
        entries.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        let mut toggle: Option<u64> = None;
        if entries.is_empty() {
            settings_card(ui, t, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("privacy.blocked_none"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
            });
        } else {
            settings_card(ui, t, |ui| {
                for (idx, (id, title, subtitle)) in entries.iter().enumerate() {
                    if idx > 0 {
                        divider(ui, t);
                    }
                    let chat = self.chats.iter().find(|c| c.id == *id);
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                        match chat {
                            Some(chat) => {
                                crate::widgets::paint_chat_avatar(
                                    ui.painter(),
                                    rect.center(),
                                    20.0,
                                    chat,
                                );
                            }
                            None => {
                                crate::widgets::paint_initials_avatar(
                                    ui.painter(),
                                    rect.center(),
                                    20.0,
                                    title,
                                    (*id % 251) as u8,
                                );
                            }
                        }
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(title.clone())
                                    .font(crate::theme::medium(14.0))
                                    .color(rgb(t.text_primary)),
                            );
                            if !subtitle.is_empty() {
                                ui.label(
                                    RichText::new(subtitle.clone())
                                        .size(12.0)
                                        .color(rgb(t.text_secondary)),
                                );
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let kebab = crate::widgets::icon_button(
                                ui,
                                t,
                                crate::icons::Icon::MoreVert,
                                28.0,
                                &crate::i18n::t("conversation.more_options"),
                                None,
                            );
                            egui::Popup::menu(&kebab)
                                .frame(crate::theme::glass_panel_frame(
                                    t,
                                    crate::theme::RADIUS_PANEL,
                                ))
                                .show(|ui| {
                                    if crate::widgets::menu_item_icon(
                                        ui,
                                        t,
                                        None,
                                        &crate::i18n::t("profile.unblock"),
                                        None,
                                    )
                                    .clicked()
                                    {
                                        toggle = Some(*id);
                                        ui.close();
                                    }
                                });
                        });
                    });
                }
            });
        }
        ui.add_space(SP_4);
        // Block a contact: the picker lists the roster's
        // direct chats not yet blocked; picking one blocks
        // through the same path and closes the picker.
        if crate::widgets::button(
            ui,
            t,
            &crate::i18n::t("privacy.block_contact"),
            crate::widgets::Btn::Secondary,
        )
        .clicked()
        {
            self.block_picker_open = !self.block_picker_open;
        }
        if self.block_picker_open {
            ui.add_space(SP_2);
            let candidates: Vec<(u64, String)> = self
                .chats
                .iter()
                .filter(|c| {
                    c.kind == core::model::ChatKind::Dm && !self.blocked_chats.contains(&c.id)
                })
                .map(|c| (c.id, c.title.clone()))
                .collect();
            settings_card(ui, t, |ui| {
                if candidates.is_empty() {
                    ui.label(
                        RichText::new(crate::i18n::t("privacy.block_picker_none"))
                            .size(12.0)
                            .color(rgb(t.text_secondary)),
                    );
                }
                for (idx, (id, title)) in candidates.iter().enumerate() {
                    if idx > 0 {
                        divider(ui, t);
                    }
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(title.clone())
                                .font(crate::theme::medium(14.0))
                                .color(rgb(t.text_primary)),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("profile.block"),
                                crate::widgets::Btn::Danger,
                            )
                            .clicked()
                            {
                                toggle = Some(*id);
                                self.block_picker_open = false;
                            }
                        });
                    });
                }
            });
        }
        if let Some(id) = toggle {
            self.toggle_block_contact(id);
        }
    }
}
