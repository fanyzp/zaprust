//! Conversation menus and dialogs: the message context menu, the reaction picker, message info, the rich-message menu, and the create-group dialog.

use super::*;

/// One message-info state row: the receipt's glyph from the
/// shared icon font beside its word, never a text checkmark.
fn receipt_state_row(
    ui: &mut egui::Ui,
    t: core::theme::Tokens,
    icon: crate::icons::Icon,
    color: core::theme::Rgb,
    text: &str,
) {
    ui.horizontal(|ui| {
        let (rect, _) = ui
            .allocate_exact_size(egui::Vec2::splat(crate::icons::SIZE_BADGE), egui::Sense::hover());
        crate::icons::draw(
            ui.painter(),
            icon,
            rect.center(),
            crate::icons::SIZE_BADGE,
            rgb(color),
            rgb(t.bg_panel),
        );
        ui.label(RichText::new(text).size(12.0).color(rgb(t.text_secondary)));
    });
}

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
        // A channel update's menu is the subset the channel
        // contract backs: react, copy, and (for the channel's
        // own posts) edit and delete for everyone, plus media
        // save. Reply, forward, star, pin, report and
        // delete-for-me have no channel command behind them,
        // so they are not offered here instead of failing
        // after the click.
        let in_channel = self
            .chats
            .iter()
            .find(|c| c.id == self.selected_chat)
            .is_some_and(|c| c.kind == core::model::ChatKind::Channel);
        // Admin delete (design standard section 24): in a group
        // where my account is an admin, another member's message
        // can be deleted for everyone. The role comes from the
        // backend's own computation over the group metadata;
        // unknown (metadata not fetched yet) never offers it.
        let admin_can_delete = !mine
            && self.live_jid(self.selected_chat).is_some_and(|j| j.ends_with("@g.us"))
            && self.groups.own_admin.get(&self.selected_chat) == Some(&true);
        // A disappearing message (it carries an expiry) can be
        // kept in the chat; a kept one can be released again.
        let keepable = msg.expires_at != 0;
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
                        if !in_channel
                            && crate::widgets::menu_item_icon(
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
                        if (mine || admin_can_delete)
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
                        if !in_channel
                            && crate::widgets::menu_item_danger(
                                ui,
                                t,
                                crate::icons::Icon::Delete,
                                "Delete for me",
                            )
                            .clicked()
                        {
                            self.delete_for_me(msg.id);
                            close = true;
                        }
                        if mine
                            && !in_channel
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
                            && !in_channel
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
                        if !in_channel && msg.starred {
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
                        } else if !in_channel
                            && crate::widgets::menu_item_icon(
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
                        if keepable
                            && crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Bookmark),
                                if msg.kept { "Stop keeping" } else { "Keep in chat" },
                                None,
                            )
                            .clicked()
                        {
                            self.toggle_keep_message(msg.id);
                            close = true;
                        }
                        if !in_channel && msg.pinned {
                            if crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Pin),
                                "Unpin message",
                                None,
                            )
                            .clicked()
                            {
                                self.toggle_pin_message(msg.id, backend::PinDuration::Days7);
                                close = true;
                            }
                        } else if !in_channel {
                            // The pin duration is part of the pin
                            // itself (section 24): the three
                            // durations the wire offers, chosen
                            // at pin time like WhatsApp presents
                            // them.
                            for (label, duration) in [
                                ("Pin for 24 hours", backend::PinDuration::Hours24),
                                ("Pin for 7 days", backend::PinDuration::Days7),
                                ("Pin for 30 days", backend::PinDuration::Days30),
                            ] {
                                if crate::widgets::menu_item_icon(
                                    ui,
                                    t,
                                    Some(crate::icons::Icon::Pin),
                                    label,
                                    None,
                                )
                                .clicked()
                                {
                                    self.toggle_pin_message(msg.id, duration);
                                    close = true;
                                }
                            }
                        }
                        if !mine
                            && !in_channel
                            && self.live.is_some()
                            && crate::widgets::menu_item_danger(
                                ui,
                                t,
                                crate::icons::Icon::Shield,
                                "Report",
                            )
                            .clicked()
                        {
                            self.ui_confirm = Some(crate::state::UiConfirm::ReportSpam {
                                chat_id: self.selected_chat,
                                msg_id: msg.id,
                                name: msg
                                    .sender
                                    .clone()
                                    .unwrap_or_else(|| "this contact".to_owned()),
                            });
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
                        crate::widgets::soft_separator(ui, t);
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
                                        egui::Button::new(RichText::new(emoji).size(19.0))
                                            .frame(false),
                                    )
                                    .clicked()
                                {
                                    picked = Some(emoji.to_owned());
                                }
                            }
                            if crate::widgets::icon_button(
                                ui,
                                t,
                                crate::icons::Icon::X,
                                26.0,
                                "Close",
                                None,
                            )
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
                        ui.label(
                            RichText::new(crate::i18n::t("menu.message_info")).size(15.0).strong(),
                        );
                        ui.add_space(SP_1_5);
                        if entries.is_empty() {
                            // The state glyph comes from the icon
                            // font, like the ticks in the bubble
                            // meta row; the word beside it is
                            // plain text, never a text checkmark.
                            let state = match delivery {
                                Some(core::model::Delivery::Failed) => {
                                    Some((crate::icons::Icon::X, t.danger, "Failed to send"))
                                }
                                Some(core::model::Delivery::Sending) => {
                                    Some((crate::icons::Icon::Schedule, t.text_faint, "Sending…"))
                                }
                                Some(core::model::Delivery::Sent) => {
                                    Some((crate::icons::Icon::Done, t.text_faint, "Sent"))
                                }
                                Some(core::model::Delivery::Delivered) => {
                                    Some((crate::icons::Icon::DoneAll, t.text_faint, "Delivered"))
                                }
                                Some(core::model::Delivery::Read) => {
                                    Some((crate::icons::Icon::DoneAll, t.tick_read, "Read"))
                                }
                                None => None,
                            };
                            match state {
                                Some((icon, color, word)) => {
                                    receipt_state_row(ui, t, icon, color, word);
                                }
                                None => {
                                    ui.label(
                                        RichText::new("-").size(12.0).color(rgb(t.text_secondary)),
                                    );
                                }
                            }
                            ui.label(
                                RichText::new(crate::i18n::t("menu.message_info_live_only"))
                                    .size(12.0)
                                    .color(rgb(t.text_faint)),
                            );
                        } else {
                            for (kind, at) in &entries {
                                let (icon, color, word) = match kind {
                                    backend::ReceiptKind::Sent => {
                                        (crate::icons::Icon::Done, t.text_faint, "Sent")
                                    }
                                    backend::ReceiptKind::Delivered => {
                                        (crate::icons::Icon::DoneAll, t.text_faint, "Delivered")
                                    }
                                    backend::ReceiptKind::Read => {
                                        (crate::icons::Icon::DoneAll, t.tick_read, "Read")
                                    }
                                    backend::ReceiptKind::Played => {
                                        (crate::icons::Icon::Play, t.text_faint, "Played")
                                    }
                                };
                                receipt_state_row(ui, t, icon, color, &format!("{word} · {at}"));
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
                        // In a channel, the attach menu keeps
                        // the items the channel contract backs
                        // (media, documents, polls): location
                        // and contact posts have no channel
                        // command behind them.
                        let in_channel = self
                            .chats
                            .iter()
                            .find(|c| c.id == self.selected_chat)
                            .is_some_and(|c| c.kind == core::model::ChatKind::Channel);
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
                        if !in_channel
                            && crate::widgets::menu_item_icon(
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
                        if !in_channel
                            && crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Person),
                                &crate::i18n::t("attach.contact"),
                                None,
                            )
                            .clicked()
                        {
                            self.contact_picker_open = true;
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
                            if in_channel {
                                // A channel poll posts through
                                // the channel contract, with
                                // its own composer.
                                self.channel_poll_for = Some(self.selected_chat);
                                self.channel_poll_question.clear();
                                self.channel_poll_options = vec![String::new(), String::new()];
                                self.channel_poll_multiple = false;
                            } else {
                                self.composer.poll_draft = Some(crate::state::PollDraft {
                                    question: String::new(),
                                    options: vec![String::new(), String::new()],
                                    multiple: false,
                                });
                            }
                            close = true;
                        }
                        crate::widgets::soft_separator(ui, t);
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
                        ui.add_space(SP_1_5);
                        ui.label(
                            RichText::new(crate::i18n::t("groups.name_label"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                        crate::widgets::text_field(ui, t, &mut self.groups.create_name, "", 280.0);
                        ui.label(
                            RichText::new(crate::i18n::t("groups.participants_label"))
                                .size(12.0)
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
                                    .size(10.0)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                        if let Some(error) = &self.groups.create_error {
                            ui.add_space(SP_1);
                            ui.label(RichText::new(error.clone()).size(12.0).color(rgb(t.danger)));
                        }
                        ui.add_space(SP_2);
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
                        ui.add_space(SP_1_5);
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
                        ui.add_space(SP_1_5);
                        ui.label(
                            RichText::new(crate::i18n::t("groups.join_label"))
                                .size(12.0)
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
                                    .size(10.0)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                        if let Some(error) = &self.groups.join_link_error {
                            ui.add_space(SP_1);
                            ui.label(RichText::new(error.clone()).size(12.0).color(rgb(t.danger)));
                        }
                        ui.add_space(SP_2);
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

    /// The "New chat" dialog: one phone number field (design
    /// standard section 23). Checking asks the backend whether
    /// the number is on WhatsApp; a resolved person opens
    /// their conversation (the ContactResolved handler), a
    /// number that is not on WhatsApp gets a plain inline
    /// answer (not an error), and a failed lookup gets an
    /// inline error (never reported as not found). Looking up
    /// needs a session, so demo mode shows the honest note
    /// instead of a Check button, like the join dialog.
    pub(crate) fn draw_new_chat_dialog(&mut self, ctx: &egui::Context) {
        let Some(grp_p) =
            crate::anim::popup_phase(ctx, "new_chat", self.new_chat.open, self.motion)
        else {
            return;
        };
        let exiting = !self.new_chat.open;
        let t = self.tokens;
        let mut check = false;
        let mut close = false;
        egui::Area::new(egui::Id::unique("new_chat"))
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
                            RichText::new(crate::i18n::t("chats.new_chat_title"))
                                .size(15.0)
                                .strong(),
                        );
                        ui.add_space(SP_1_5);
                        ui.label(
                            RichText::new(crate::i18n::t("chats.phone_label"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                        ui.add_enabled_ui(self.new_chat.checking.is_none(), |ui| {
                            crate::widgets::text_field(
                                ui,
                                t,
                                &mut self.new_chat.phone,
                                "+62 812 3456 7890",
                                280.0,
                            );
                        });
                        if !self.live_mode {
                            ui.label(
                                RichText::new(crate::i18n::t("chats.new_chat_live_only"))
                                    .size(10.0)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                        if self.new_chat.checking.is_some() {
                            ui.add_space(SP_1);
                            crate::widgets::loading_row(
                                ui,
                                &crate::i18n::t("chats.checking_number"),
                                14.0,
                                t.text_secondary,
                            );
                        }
                        if let Some(phone) = &self.new_chat.not_found {
                            ui.add_space(SP_1);
                            ui.label(
                                RichText::new(crate::i18n::tf(
                                    "chats.not_on_whatsapp",
                                    "phone",
                                    phone,
                                ))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                            );
                        }
                        if let Some(error) = &self.new_chat.error {
                            ui.add_space(SP_1);
                            ui.label(RichText::new(error.clone()).size(12.0).color(rgb(t.danger)));
                        }
                        ui.add_space(SP_2);
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
                                && self.new_chat.checking.is_none()
                                && crate::widgets::button(
                                    ui,
                                    t,
                                    &crate::i18n::t("chats.start_chat"),
                                    crate::widgets::Btn::Primary,
                                )
                                .clicked()
                            {
                                check = true;
                            }
                        });
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("new_chat"),
            ctx.content_rect().center(),
            grp_p,
        );
        if check {
            self.start_new_chat_lookup();
        }
        if close {
            // Closing abandons any in-flight check: its
            // outcome events key off the tracked input, which
            // is cleared here, so a late answer changes
            // nothing.
            self.new_chat.open = false;
            self.new_chat.checking = None;
            self.new_chat.not_found = None;
            self.new_chat.error = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Args;

    /// A demo-mode app with the popup motion off, so the
    /// context menu paints at its final geometry on the
    /// first frame.
    fn menu_app(ctx: &egui::Context) -> App {
        crate::theme::install_fonts(ctx);
        let mut app = App::new(Args::parse(), ctx);
        // App::new may have attempted a real live start against
        // this machine's data dir; the test owns the state from
        // here and wants the plain demo shell.
        app.live = None;
        app.live_mode = false;
        app.motion = crate::motion::MotionSetting::Off;
        app
    }

    fn run_menu(
        app: &mut App,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        time: f64,
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            time: Some(time),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_pass(input, |ctx| app.draw_context_menu(ctx));
        // No renderer will ever apply this frame's texture
        // deltas; clear them so dropping the output is quiet.
        out.textures_delta.clear();
        out
    }

    /// The centre of the painted label `label` in a frame's
    /// output shapes: where a user's pointer would land.
    fn label_centre(out: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
        out.shapes.iter().find_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) if text.galley.job.text == label => {
                let size = text.galley.size();
                Some(egui::pos2(text.pos.x + size.x / 2.0, text.pos.y + size.y / 2.0))
            }
            _ => None,
        })
    }

    fn copied_texts(out: &egui::FullOutput) -> Vec<&str> {
        out.platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The Copy text row of the message context menu, clicked
    /// for real: the frame paints the menu, the test reads
    /// the row's painted position back out of the frame,
    /// presses and releases the pointer there, and the
    /// clipboard boundary must receive the message's exact
    /// body as a copy command.
    #[test]
    fn copy_text_menu_item_copies_the_exact_message_body() {
        let ctx = egui::Context::default();
        let mut app = menu_app(&ctx);
        let body = "kopi tubruk, no sugar, 100% exact";
        app.conversation = vec![Message::theirs(4242, "Alice", body, "09:41")];
        app.bubble_menu = Some(4242);

        // The first frame only sizes the popup area (its shapes
        // are culled until the area has a stored rect), so the
        // menu becomes readable on the second frame.
        let _ = run_menu(&mut app, &ctx, Vec::new(), 0.0);
        let painted = run_menu(&mut app, &ctx, Vec::new(), 0.05);
        let at = label_centre(&painted, "Copy text")
            .expect("a text message's menu paints a Copy text row");

        let press = vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        run_menu(&mut app, &ctx, press, 0.10);
        let release = vec![egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }];
        let clicked = run_menu(&mut app, &ctx, release, 0.15);

        assert_eq!(copied_texts(&clicked), [body], "the click copies the body verbatim");
        assert_eq!(app.bubble_menu, None, "the action closes the menu");
        assert!(app.composer.reply_to.is_none(), "the click did not land on Reply");
        assert_eq!(app.reaction_picker_id, None, "the click did not land on React");
        assert_eq!(app.forward_from, None, "the click did not land on Forward");
    }

    /// Copy text is a text-message action: a media message's
    /// menu paints its other rows but no Copy text row.
    #[test]
    fn media_message_menu_paints_no_copy_text_row() {
        let ctx = egui::Context::default();
        let mut app = menu_app(&ctx);
        let mut msg = Message::theirs(4243, "Alice", "", "09:42");
        msg.kind = MessageKind::Image { caption: String::new(), view_once: false };
        app.conversation = vec![msg];
        app.bubble_menu = Some(4243);

        let _ = run_menu(&mut app, &ctx, Vec::new(), 0.0);
        let painted = run_menu(&mut app, &ctx, Vec::new(), 0.05);
        assert!(label_centre(&painted, "Star").is_some(), "the menu itself painted");
        assert!(label_centre(&painted, "Copy text").is_none(), "no copy row for media");
    }
}
