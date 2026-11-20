//! Conversation menus and dialogs: the message context menu, the reaction picker, message info, the rich-message menu, and the create-group dialog.

use super::*;

impl App {
    pub(crate) fn draw_context_menu(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        // While the menu is open, snapshot its message into temp
        // data: the exit animation keeps drawing the menu for a
        // few frames after `bubble_menu` clears, and by then the
        // id alone no longer says what the menu was for.
        if let Some(msg_id) = self.bubble_menu {
            if let Some(msg) = self.conversation.iter().find(|m| m.id == msg_id).cloned() {
                ctx.data_mut(|d| d.insert_temp(egui::Id::unique("bubble_menu_snap"), msg));
            } else {
                self.bubble_menu = None;
            }
        }
        let Some(menu_p) =
            crate::anim::popup_phase(ctx, "bubble_menu", self.bubble_menu.is_some(), self.motion)
        else {
            return;
        };
        let exiting = self.bubble_menu.is_none();
        let Some(msg) = ctx.data(|d| d.get_temp::<Message>(egui::Id::unique("bubble_menu_snap")))
        else {
            return;
        };
        let mine = msg.sender.is_none();
        let is_text = msg.kind == MessageKind::Text;
        let mut close = false;
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        // Fade and scale on the house curve while the menu's
        // layer grows from the screen center it is anchored to;
        // the exit is the same motion, shorter and reversed.
        egui::Area::new(egui::Id::unique("bubble_menu"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if menu_p < 0.999 {
                    ui.set_opacity(menu_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.set_width(220.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Reply),
                            "Reply",
                            None,
                        )
                        .clicked()
                        {
                            self.composer.reply_to = Some((
                                msg.id,
                                msg.sender.clone().unwrap_or_else(|| "Me".to_owned()),
                                msg.body.chars().take(80).collect(),
                            ));
                            close = true;
                        }
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::React),
                            "React\u{2026}",
                            None,
                        )
                        .clicked()
                        {
                            self.reaction_picker_id = Some(msg.id);
                            close = true;
                        }
                        if is_text
                            && crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Copy),
                                "Copy text",
                                None,
                            )
                            .clicked()
                        {
                            ui.ctx().copy_text(msg.body.clone());
                            close = true;
                        }
                        if mine
                            && is_text
                            && crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Edit),
                                "Edit",
                                None,
                            )
                            .clicked()
                        {
                            self.start_edit(msg.id);
                            close = true;
                        }
                        if mine
                            && crate::widgets::menu_item_danger(
                                ui,
                                t,
                                crate::icons::Icon::Delete,
                                "Delete for everyone",
                            )
                            .clicked()
                        {
                            self.delete_for_everyone(msg.id);
                            close = true;
                        }
                        if mine
                            && crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Info),
                                "Message info",
                                None,
                            )
                            .clicked()
                        {
                            self.message_info_id = Some(msg.id);
                            close = true;
                        }
                        let is_media = matches!(
                            msg.kind,
                            MessageKind::Image { .. }
                                | MessageKind::Video { .. }
                                | MessageKind::Voice { .. }
                                | MessageKind::Document { .. }
                        );
                        // View-once media gets neither Forward nor
                        // Save: it is viewed once, in place, and the
                        // client never offers a way to keep or relay
                        // it (the actions refuse as a backstop).
                        let view_once = msg.is_view_once();
                        if !view_once
                            && crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Forward),
                                "Forward\u{2026}",
                                None,
                            )
                            .clicked()
                        {
                            self.forward_from = Some(msg.id);
                            close = true;
                        }
                        if msg.starred {
                            if crate::widgets::menu_item_icon_tinted(
                                ui,
                                t,
                                crate::icons::Icon::Star,
                                t.accent,
                                "Unstar",
                            )
                            .clicked()
                            {
                                self.toggle_star_message(msg.id);
                                close = true;
                            }
                        } else if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Star),
                            "Star",
                            None,
                        )
                        .clicked()
                        {
                            self.toggle_star_message(msg.id);
                            close = true;
                        }
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Pin),
                            if msg.pinned { "Unpin message" } else { "Pin message" },
                            None,
                        )
                        .clicked()
                        {
                            self.toggle_pin_message(msg.id);
                            close = true;
                        }
                        if is_media
                            && !view_once
                            && self.live.is_some()
                            && crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Download),
                                "Save to device",
                                None,
                            )
                            .clicked()
                        {
                            self.download_media(msg.id);
                            close = true;
                        }
                        ui.separator();
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::X),
                            "Close",
                            Some("Esc"),
                        )
                        .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("bubble_menu"),
            ctx.content_rect().center(),
            menu_p,
        );
        if close {
            self.bubble_menu = None;
        }
    }

    pub(crate) fn draw_reaction_picker(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        // Snapshot the target id while open, like the context
        // menu: the exit animation outlives the state.
        if let Some(id) = self.reaction_picker_id {
            ctx.data_mut(|d| d.insert_temp(egui::Id::unique("reaction_snap"), id));
        }
        let Some(pick_p) = crate::anim::popup_phase(
            ctx,
            "reaction_picker",
            self.reaction_picker_id.is_some(),
            self.motion,
        ) else {
            return;
        };
        let exiting = self.reaction_picker_id.is_none();
        let Some(msg_id) = ctx.data(|d| d.get_temp::<u64>(egui::Id::unique("reaction_snap")))
        else {
            return;
        };
        let mut picked: Option<String> = None;
        let mut close = false;
        egui::Area::new(egui::Id::unique("reaction_picker"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if pick_p < 0.999 {
                    ui.set_opacity(pick_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.add_enabled_ui(!exiting, |ui| {
                        ui.horizontal(|ui| {
                            for emoji in QUICK_REACTIONS {
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new(emoji).size(20.0))
                                            .frame(false),
                                    )
                                    .clicked()
                                {
                                    picked = Some(emoji.to_owned());
                                }
                            }
                            if crate::widgets::button(ui, t, "✕", crate::widgets::Btn::Secondary)
                                .clicked()
                            {
                                close = true;
                            }
                        });
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("reaction_picker"),
            ctx.content_rect().center(),
            pick_p,
        );
        if let Some(emoji) = picked {
            self.apply_reaction(msg_id, &emoji);
        } else if close {
            self.reaction_picker_id = None;
        }
    }

    pub(crate) fn draw_message_info(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.message_info_id {
            ctx.data_mut(|d| d.insert_temp(egui::Id::unique("info_snap"), id));
        }
        let Some(info_p) = crate::anim::popup_phase(
            ctx,
            "message_info",
            self.message_info_id.is_some(),
            self.motion,
        ) else {
            return;
        };
        let exiting = self.message_info_id.is_none();
        let Some(msg_id) = ctx.data(|d| d.get_temp::<u64>(egui::Id::unique("info_snap"))) else {
            return;
        };
        let mut close = false;
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        let t = self.tokens;
        let delivery = self.conversation.iter().find(|m| m.id == msg_id).and_then(|m| m.delivery);
        let entries = self.receipt_log.get(&msg_id).cloned().unwrap_or_default();
        egui::Area::new(egui::Id::unique("message_info"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if info_p < 0.999 {
                    ui.set_opacity(info_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.set_width(240.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        ui.label(RichText::new("Message info").size(14.0).strong());
                        ui.separator();
                        if entries.is_empty() {
                            let state = match delivery {
                                Some(core::model::Delivery::Failed) => "Failed to send",
                                Some(core::model::Delivery::Sending) => "Sending…",
                                Some(core::model::Delivery::Sent) => "Sent ✓",
                                Some(core::model::Delivery::Delivered) => "Delivered ✓✓",
                                Some(core::model::Delivery::Read) => "Read ✓✓ (blue)",
                                None => "-",
                            };
                            ui.label(RichText::new(state).size(12.5).color(rgb(t.text_secondary)));
                            ui.label(
                                RichText::new("Detailed receipt events appear here in live mode.")
                                    .size(11.5)
                                    .color(rgb(t.text_faint)),
                            );
                        } else {
                            for (kind, at) in &entries {
                                let label = match kind {
                                    backend::ReceiptKind::Sent => "Sent ✓",
                                    backend::ReceiptKind::Delivered => "Delivered ✓✓",
                                    backend::ReceiptKind::Read => "Read ✓✓ (blue)",
                                    backend::ReceiptKind::Played => "Played ▶",
                                };
                                ui.label(RichText::new(format!("{label} · {at}")).size(12.5));
                            }
                        }
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::X),
                            "Close",
                            Some("Esc"),
                        )
                        .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("message_info"),
            ctx.content_rect().center(),
            info_p,
        );
        if close {
            self.message_info_id = None;
        }
    }

    pub(crate) fn draw_rich_menu(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        let Some(menu_p) =
            crate::anim::popup_phase(ctx, "rich_menu", self.composer.rich_menu_open, self.motion)
        else {
            return;
        };
        let exiting = !self.composer.rich_menu_open;
        let screen = ctx.content_rect();
        let mut close = false;
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        // Fake-frost underlay behind the menu, from the rect the
        // menu occupied last frame (its height varies with the
        // feature rows, so the first frame of an entrance simply
        // has none yet). It fades with the menu and pads the rect
        // a little on every side, like the picker's.
        if let Some(last) =
            ctx.data(|d| d.get_temp::<egui::Rect>(egui::Id::unique("rich_menu_rect")))
        {
            let under = crate::theme::glass_underlay(t);
            let faded = egui::Color32::from_rgba_unmultiplied(
                under.r(),
                under.g(),
                under.b(),
                (f32::from(under.a()) * menu_p) as u8,
            );
            ctx.layer_painter(egui::LayerId::background()).rect_filled(
                last.expand(10.0),
                CornerRadius::same(24),
                faded,
            );
        }
        // Fade + scale from the menu's own top-left, the corner
        // nearest the composer button that opens it; the exit is
        // the same motion, shorter and reversed. The menu sits
        // tall enough for the four WhatsApp attach items plus the
        // feature extras.
        let anchor = egui::Pos2::new(screen.left() + 430.0, screen.bottom() - 320.0);
        let area = egui::Area::new(egui::Id::unique("rich_menu"))
            .fixed_pos(anchor)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if menu_p < 0.999 {
                    ui.set_opacity(menu_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.set_width(230.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Image),
                            &crate::i18n::t("attach.photo_video"),
                            None,
                        )
                        .clicked()
                        {
                            self.pick_photos(ctx);
                            close = true;
                        }
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Document),
                            &crate::i18n::t("attach.document"),
                            None,
                        )
                        .clicked()
                        {
                            self.pick_document(ctx);
                            close = true;
                        }
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::LocationOn),
                            &crate::i18n::t("attach.location"),
                            None,
                        )
                        .clicked()
                        {
                            self.composer.location_draft = Some(crate::state::LocationDraft {
                                name: String::new(),
                                latitude: "-6.200000".to_owned(),
                                longitude: "106.816666".to_owned(),
                            });
                            close = true;
                        }
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Poll),
                            &crate::i18n::t("attach.poll"),
                            None,
                        )
                        .clicked()
                        {
                            self.composer.poll_draft = Some(crate::state::PollDraft {
                                question: String::new(),
                                options: vec![String::new(), String::new()],
                                multiple: false,
                            });
                            close = true;
                        }
                        ui.separator();
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::X),
                            &crate::i18n::t("common.close"),
                            Some("Esc"),
                        )
                        .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });
        ctx.data_mut(|d| {
            d.insert_temp(egui::Id::unique("rich_menu_rect"), area.response.rect);
        });
        crate::anim::popup_transform(ctx, egui::Id::unique("rich_menu"), anchor, menu_p);
        if close {
            self.composer.rich_menu_open = false;
        }
    }

    /// The "New group" dialog: name + comma-separated participants.
    /// Live mode creates the group through the backend; demo mode
    /// creates a local group entry. One group at a time, created by
    /// the user in the UI (no bulk creation path exists).
    pub(crate) fn draw_create_group_dialog(&mut self, ctx: &egui::Context) {
        let Some(grp_p) =
            crate::anim::popup_phase(ctx, "create_group", self.groups.create_open, self.motion)
        else {
            return;
        };
        let exiting = !self.groups.create_open;
        let t = self.tokens;
        let mut create = false;
        let mut close = false;
        egui::Area::new(egui::Id::unique("create_group"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if grp_p < 0.999 {
                    ui.set_opacity(grp_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.set_width(300.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("groups.new_title")).size(15.0).strong(),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(crate::i18n::t("groups.name_label"))
                                .size(11.5)
                                .color(rgb(t.text_secondary)),
                        );
                        crate::widgets::text_field(ui, t, &mut self.groups.create_name, "", 280.0);
                        ui.label(
                            RichText::new(crate::i18n::t("groups.participants_label"))
                                .size(11.5)
                                .color(rgb(t.text_secondary)),
                        );
                        crate::widgets::text_field(
                            ui,
                            t,
                            &mut self.groups.create_members,
                            "",
                            280.0,
                        );
                        if !self.live_mode {
                            ui.label(
                                RichText::new(crate::i18n::t("groups.demo_note"))
                                    .size(11.0)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                        if let Some(error) = &self.groups.create_error {
                            ui.add_space(4.0);
                            ui.label(RichText::new(error.clone()).size(11.5).color(rgb(t.danger)));
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("common.cancel"),
                                crate::widgets::Btn::Secondary,
                            )
                            .clicked()
                            {
                                close = true;
                            }
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("groups.create"),
                                crate::widgets::Btn::Primary,
                            )
                            .clicked()
                            {
                                create = true;
                            }
                        });
                        ui.add_space(6.0);
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::i18n::t("groups.join_with_link"),
                            crate::widgets::Btn::Secondary,
                        )
                        .clicked()
                        {
                            close = true;
                            self.groups.join_link_open = true;
                        }
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("create_group"),
            ctx.content_rect().center(),
            grp_p,
        );
        if create && self.do_create_group() {
            close = true;
        }
        if close {
            self.groups.create_open = false;
            self.groups.create_error = None;
        }
    }

    /// Sends the join-by-link command. Returns true when the
    /// dialog can close (the joined group arrives as a chat
    /// upsert; failures surface as backend error notices).
    pub(crate) fn do_join_group(&mut self) -> bool {
        let link = self.groups.join_link_input.trim().to_owned();
        if link.is_empty() {
            self.groups.join_link_error = Some(crate::i18n::t("groups.join_empty"));
            return false;
        }
        self.send_backend(backend::BackendCommand::JoinGroupByLink { link });
        self.groups.join_link_error = None;
        self.groups.join_link_input.clear();
        true
    }

    /// The "Join group" dialog: one invite link field. Joining
    /// needs a live session, so demo mode shows the honest note
    /// instead of a Join button.
    pub(crate) fn draw_join_group_dialog(&mut self, ctx: &egui::Context) {
        let Some(grp_p) =
            crate::anim::popup_phase(ctx, "join_group", self.groups.join_link_open, self.motion)
        else {
            return;
        };
        let exiting = !self.groups.join_link_open;
        let t = self.tokens;
        let mut join = false;
        let mut close = false;
        egui::Area::new(egui::Id::unique("join_group"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if grp_p < 0.999 {
                    ui.set_opacity(grp_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.set_width(300.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("groups.join_title")).size(15.0).strong(),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(crate::i18n::t("groups.join_label"))
                                .size(11.5)
                                .color(rgb(t.text_secondary)),
                        );
                        crate::widgets::text_field(
                            ui,
                            t,
                            &mut self.groups.join_link_input,
                            "https://chat.whatsapp.com/...",
                            280.0,
                        );
                        if !self.live_mode {
                            ui.label(
                                RichText::new(crate::i18n::t("groups.join_live_only"))
                                    .size(11.0)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                        if let Some(error) = &self.groups.join_link_error {
                            ui.add_space(4.0);
                            ui.label(RichText::new(error.clone()).size(11.5).color(rgb(t.danger)));
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("common.cancel"),
                                crate::widgets::Btn::Secondary,
                            )
                            .clicked()
                            {
                                close = true;
                            }
                            if self.live_mode
                                && crate::widgets::button(
                                    ui,
                                    t,
                                    &crate::i18n::t("groups.join"),
                                    crate::widgets::Btn::Primary,
                                )
                                .clicked()
                            {
                                join = true;
                            }
                        });
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("join_group"),
            ctx.content_rect().center(),
            grp_p,
        );
        if join && self.do_join_group() {
            close = true;
        }
        if close {
            self.groups.join_link_open = false;
            self.groups.join_link_error = None;
        }
    }
}
