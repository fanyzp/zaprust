//! Channels surface (parity P5): the dedicated left panel
//! behind the rail's Channels entry, the channel
//! conversation's follower bar, the channel info dialog, and
//! the channel poll composer.
//!
//! The panel follows the owner's left-panel rule: channels
//! are a list surface, and the conversation stays on the
//! right. Rows are the roster's channel chats rendered with
//! the shared chat row; everything the roster cannot say
//! (role, follower count, verified flag, invite code) comes
//! from the backend's channel metadata in the live session.
//! Discovery (find by invite link) and creation live at the
//! panel's head, in live mode only: the pure demo dataset
//! has no backend to ask, so it shows the roster alone
//! instead of controls that would do nothing.
//!
//! A channel conversation is one-way for followers: the
//! composer is replaced by a bar that says so (with the
//! follow action for a channel the user has not joined),
//! while admins and owners keep the normal composer.

use crate::state::{App, Notice, Screen, UiConfirm};
use backend::BackendCommand;
use crate::theme::{rgb, SP_1, SP_2, SP_3, SP_4};
use core::model::{Chat, ChatKind};
use core::theme::Tokens;
use eframe::egui::{self, Align2, Margin, RichText, Vec2};

/// The follower count in words: the locale's singular form
/// for exactly one, the counted form otherwise, with the
/// house count formatting (12.5K style) above the raw range.
pub(crate) fn followers_text(count: u64) -> String {
    if count == 1 {
        crate::i18n::t("channels.followers_one")
    } else {
        crate::i18n::tf(
            "channels.followers",
            "count",
            &core::filter::format_count(count),
        )
    }
}

impl App {
    /// This chat's channel metadata, when the live session
    /// has it (the roster alone cannot say the role).
    pub(crate) fn channel_meta_for(&self, chat_id: u64) -> Option<backend::LiveChannel> {
        let session = self.live.as_ref()?;
        let jid = session.jids.get(&chat_id)?;
        session.channel_meta.get(jid).cloned()
    }

    /// Sends the unfollow for a channel chat (the confirmed
    /// destructive action): the backend's event pair settles
    /// it, and its failure is a banner, never silence.
    pub(crate) fn unfollow_channel(&mut self, chat_id: u64) {
        if let Some(jid) = self.live_jid(chat_id) {
            self.send_backend(BackendCommand::UnfollowChannel { jid });
        }
    }

    /// Opens the channel info surface for a chat: the dialog
    /// opens at once with what is on hand, and the backend's
    /// info fetch fills the rest (admin count, follower
    /// sample) when it lands.
    pub(crate) fn open_channel_info(&mut self, chat_id: u64) {
        self.channel_info_for = Some(chat_id);
        if let Some(jid) = self.live_jid(chat_id) {
            self.send_backend(BackendCommand::FetchChannelInfo { jid });
        }
    }

    /// The Channels left panel: header with the panel's
    /// actions, the find and create forms, the found
    /// channel's preview card, the panel notice, and the
    /// channel rows.
    pub(crate) fn draw_channels_panel(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        // First show in live mode asks for the subscribed
        // list once; the answer fills the metadata the rows
        // and the conversation read.
        if self.live_mode {
            let ask = self
                .live
                .as_mut()
                .is_some_and(|s| !std::mem::replace(&mut s.channels_list_requested, true));
            if ask {
                self.send_backend(BackendCommand::FetchChannels);
            }
        }
        let live = self.live_mode;
        self.list_panel(root)
            .frame(
                egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .inner_margin(Margin::symmetric(crate::theme::LIST_PANEL_INSET_X, 8)),
            )
            .show(root, |ui| {
                // Header: back to the plain Chats list, the
                // destination's title, and (live only) the
                // find and create actions.
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
                        RichText::new(crate::i18n::t("nav.channels"))
                            .font(crate::theme::medium(15.0))
                            .color(rgb(t.text_primary)),
                    );
                    if live {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if crate::widgets::icon_button(
                                ui,
                                t,
                                crate::icons::Icon::Plus,
                                28.0,
                                &crate::i18n::t("channels.create"),
                                None,
                            )
                            .clicked()
                            {
                                self.channel_create_open = !self.channel_create_open;
                                self.channel_find_open = false;
                            }
                            if crate::widgets::icon_button(
                                ui,
                                t,
                                crate::icons::Icon::Search,
                                28.0,
                                &crate::i18n::t("channels.find_by_link"),
                                None,
                            )
                            .clicked()
                            {
                                self.channel_find_open = !self.channel_find_open;
                                self.channel_create_open = false;
                            }
                        });
                    }
                });
                if back {
                    self.screen = Screen::Chats;
                    self.filter = core::filter::ChatFilter::All;
                }
                if live && self.channel_find_open {
                    ui.add_space(SP_2);
                    self.draw_channel_find_form(ui, t);
                }
                if live && self.channel_create_open {
                    ui.add_space(SP_2);
                    self.draw_channel_create_form(ui, t);
                }
                if let Some(notice) = self.channel_panel_notice.clone() {
                    ui.add_space(SP_2);
                    ui.label(RichText::new(notice).size(12.0).color(rgb(t.danger)));
                }
                if live {
                    if let Some(found) = self.channel_found.clone() {
                        ui.add_space(SP_2);
                        self.draw_channel_found_card(ui, t, &found);
                    }
                }
                ui.add_space(SP_2);
                crate::widgets::soft_separator(ui, t);
                // Rows: the roster's channel chats, newest
                // first like the chat list. The row is the
                // shared chat row; its context menu carries
                // the channel actions.
                let rows: Vec<Chat> = self
                    .chats
                    .iter()
                    .filter(|c| c.kind == ChatKind::Channel && !c.archived)
                    .cloned()
                    .collect();
                if rows.is_empty() {
                    ui.add_space(SP_3);
                    ui.label(
                        RichText::new(crate::i18n::t("channels.empty"))
                            .size(12.0)
                            .color(rgb(t.text_secondary)),
                    );
                }
                let mut clicked: Option<u64> = None;
                let mut row_pick: Option<(u64, ChannelPick)> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for chat in &rows {
                            let state = self.row_state_for(chat);
                            let resp = crate::chat_list::chat_row(
                                ui,
                                chat,
                                chat.id == self.selected_chat,
                                false,
                                t,
                                state,
                                None,
                                None,
                            );
                            if resp.clicked() {
                                clicked = Some(chat.id);
                            }
                            let meta = self.channel_meta_for(chat.id);
                            resp.context_menu(|ui| {
                                ui.set_min_width(200.0);
                                if crate::widgets::menu_item_icon(
                                    ui,
                                    t,
                                    Some(crate::icons::Icon::Chat),
                                    &crate::i18n::t("channels.open"),
                                    None,
                                )
                                .clicked()
                                {
                                    row_pick = Some((chat.id, ChannelPick::Open));
                                    ui.close();
                                }
                                if crate::widgets::menu_item_icon(
                                    ui,
                                    t,
                                    Some(crate::icons::Icon::Info),
                                    &crate::i18n::t("channels.info"),
                                    None,
                                )
                                .clicked()
                                {
                                    row_pick = Some((chat.id, ChannelPick::Info));
                                    ui.close();
                                }
                                if live {
                                    let muted = meta.as_ref().is_some_and(|m| m.muted);
                                    let mute_label = if muted {
                                        crate::i18n::t("channels.unmute")
                                    } else {
                                        crate::i18n::t("channels.mute")
                                    };
                                    if crate::widgets::menu_item_icon(
                                        ui,
                                        t,
                                        Some(if muted {
                                            crate::icons::Icon::Bell
                                        } else {
                                            crate::icons::Icon::BellOff
                                        }),
                                        &mute_label,
                                        None,
                                    )
                                    .clicked()
                                    {
                                        row_pick = Some((chat.id, ChannelPick::Mute(!muted)));
                                        ui.close();
                                    }
                                    // The owner cannot
                                    // unfollow their own
                                    // channel; everyone
                                    // else confirms first
                                    // (section 24).
                                    let role = meta.as_ref().map(|m| m.role);
                                    if role != Some(backend::LiveChannelRole::Owner)
                                        && role.is_some()
                                        && crate::widgets::menu_item_danger(
                                            ui,
                                            t,
                                            crate::icons::Icon::Delete,
                                            &crate::i18n::t("channels.unfollow"),
                                        )
                                        .clicked()
                                    {
                                        row_pick = Some((chat.id, ChannelPick::Unfollow));
                                        ui.close();
                                    }
                                }
                            });
                        }
                    });
                if let Some(id) = clicked {
                    self.select_chat_now(id);
                }
                match row_pick {
                    Some((id, ChannelPick::Open)) => self.select_chat_now(id),
                    Some((id, ChannelPick::Info)) => self.open_channel_info(id),
                    Some((id, ChannelPick::Mute(muted))) => {
                        if let Some(jid) = self.live_jid(id) {
                            self.send_backend(BackendCommand::SetChannelMuted { jid, muted });
                        }
                    }
                    Some((id, ChannelPick::Unfollow)) => {
                        let name = self
                            .chats
                            .iter()
                            .find(|c| c.id == id)
                            .map(|c| c.title.clone())
                            .unwrap_or_default();
                        self.ui_confirm = Some(UiConfirm::UnfollowChannel { chat_id: id, name });
                    }
                    None => {}
                }
            });
    }

    /// The find-by-invite form: one field for the link or
    /// code, one action. The backend's ChannelFound or
    /// ChannelLookupFailed answers it in place.
    fn draw_channel_find_form(&mut self, ui: &mut egui::Ui, t: Tokens) {
        ui.label(
            RichText::new(crate::i18n::t("channels.find_by_link"))
                .size(12.0)
                .color(rgb(t.text_secondary)),
        );
        ui.add_space(SP_1);
        let mut find = false;
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 92.0).max(80.0);
            let resp = crate::widgets::text_field(
                ui,
                t,
                &mut self.channel_find_text,
                &crate::i18n::t("channels.find_hint"),
                w,
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                find = true;
            }
            if crate::widgets::button(
                ui,
                t,
                &crate::i18n::t("channels.find_action"),
                crate::widgets::Btn::Primary,
            )
            .clicked()
            {
                find = true;
            }
        });
        if find {
            let code = self.channel_find_text.trim().to_owned();
            if !code.is_empty() {
                self.channel_panel_notice = None;
                self.send_backend(BackendCommand::LookupChannel { code });
            }
        }
    }

    /// The create form: name, optional description, the
    /// inline error the backend's failure event fills, and
    /// the create action. A name is required; the form says
    /// so itself instead of sending a command that must
    /// fail.
    fn draw_channel_create_form(&mut self, ui: &mut egui::Ui, t: Tokens) {
        ui.label(
            RichText::new(crate::i18n::t("channels.create_title"))
                .size(12.0)
                .color(rgb(t.text_secondary)),
        );
        ui.add_space(SP_1);
        let w = ui.available_width();
        crate::widgets::text_field(
            ui,
            t,
            &mut self.channel_create_name,
            &crate::i18n::t("channels.name_placeholder"),
            w,
        );
        ui.add_space(SP_1);
        let w = ui.available_width();
        crate::widgets::text_field(
            ui,
            t,
            &mut self.channel_create_desc,
            &crate::i18n::t("channels.desc_placeholder"),
            w,
        );
        if let Some(error) = &self.channel_create_error {
            ui.add_space(SP_1);
            ui.label(RichText::new(error.clone()).size(12.0).color(rgb(t.danger)));
        }
        ui.add_space(SP_2);
        let mut create = false;
        ui.horizontal(|ui| {
            if crate::widgets::button(
                ui,
                t,
                &crate::i18n::t("common.cancel"),
                crate::widgets::Btn::Secondary,
            )
            .clicked()
            {
                self.channel_create_open = false;
                self.channel_create_error = None;
            }
            if crate::widgets::button(
                ui,
                t,
                &crate::i18n::t("channels.create_action"),
                crate::widgets::Btn::Primary,
            )
            .clicked()
            {
                create = true;
            }
        });
        if create {
            let name = self.channel_create_name.trim().to_owned();
            if name.is_empty() {
                self.channel_create_error = Some(crate::i18n::t("channels.name_required"));
            } else {
                self.channel_create_error = None;
                self.send_backend(BackendCommand::CreateChannel {
                    name,
                    description: self.channel_create_desc.trim().to_owned(),
                });
            }
        }
    }

    /// The found channel's preview card: identity, follower
    /// count, description, and the follow state. Following
    /// files the channel's chat row (the backend emits it
    /// ahead of the follow event), so a followed card offers
    /// the way in.
    fn draw_channel_found_card(
        &mut self,
        ui: &mut egui::Ui,
        t: Tokens,
        found: &backend::LiveChannel,
    ) {
        let mut follow = false;
        let mut open = false;
        egui::Frame::new()
            .fill(rgb(t.bg_panel_alt))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&found.name)
                            .font(crate::theme::semibold(15.0))
                            .color(rgb(t.text_primary)),
                    );
                    if found.verified {
                        let (r, _) =
                            ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
                        crate::widgets::verified_badge(ui.painter(), r.center(), 7.5, t);
                    }
                });
                ui.label(
                    RichText::new(followers_text(found.followers))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                if !found.description.is_empty() {
                    ui.add_space(SP_1);
                    ui.label(
                        RichText::new(&found.description)
                            .size(12.0)
                            .color(rgb(t.text_secondary)),
                    );
                }
                ui.add_space(SP_2);
                if found.role == backend::LiveChannelRole::Guest {
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("channels.follow"),
                        crate::widgets::Btn::Primary,
                    )
                    .clicked()
                    {
                        follow = true;
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("channels.following"))
                                .size(12.0)
                                .color(rgb(t.accent)),
                        );
                        ui.add_space(SP_2);
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::i18n::t("channels.open"),
                            crate::widgets::Btn::Secondary,
                        )
                        .clicked()
                        {
                            open = true;
                        }
                    });
                }
            });
        if follow {
            self.send_backend(BackendCommand::FollowChannel { jid: found.jid.clone() });
        }
        if open {
            let chat_id = crate::live::jid_id(&found.jid);
            if self.chats.iter().any(|c| c.id == chat_id) {
                self.channel_found = None;
                self.channel_find_open = false;
                self.select_chat_now(chat_id);
            }
        }
    }

    /// Whether the open conversation is a channel whose role
    /// keeps the composer: admins and owners post, followers
    /// and guests get the bar, and a channel whose metadata
    /// has not landed yet gets the bar too (channels are
    /// one-way until the role proves otherwise).
    pub(crate) fn channel_composer_blocked(&self, chat: &Chat) -> Option<backend::LiveChannelRole> {
        if chat.kind != ChatKind::Channel || !self.live_mode {
            return None;
        }
        let role = self
            .channel_meta_for(chat.id)
            .map(|m| m.role)
            .unwrap_or(backend::LiveChannelRole::Guest);
        if role.can_post() {
            None
        } else {
            Some(role)
        }
    }

    /// The follower bar that replaces the composer in a
    /// channel the user cannot post to: the reason, and for
    /// a guest the follow action itself.
    pub(crate) fn draw_channel_follower_bar(
        &mut self,
        ui: &mut egui::Ui,
        chat: &Chat,
        role: backend::LiveChannelRole,
    ) {
        let t = self.tokens();
        let mut follow = false;
        egui::Frame::new()
            .fill(rgb(t.bg_panel))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.with_layout(
                    egui::Layout::left_to_right(egui::Align::Center)
                        .with_main_align(egui::Align::Center),
                    |ui| {
                        let text = if role == backend::LiveChannelRole::Guest {
                            crate::i18n::t("channels.bar_guest")
                        } else {
                            crate::i18n::t("channels.bar_follower")
                        };
                        ui.label(RichText::new(text).size(12.0).color(rgb(t.text_secondary)));
                        if role == backend::LiveChannelRole::Guest {
                            ui.add_space(SP_3);
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("channels.bar_follow_action"),
                                crate::widgets::Btn::Primary,
                            )
                            .clicked()
                            {
                                follow = true;
                            }
                        }
                    },
                );
            });
        if follow {
            if let Some(jid) = self.live_jid(chat.id) {
                self.send_backend(BackendCommand::FollowChannel { jid });
            }
        }
    }

    /// The channel info dialog: identity and description,
    /// the follower count, the admin count and a follower
    /// sample when the info fetch has landed, the invite
    /// link when the metadata carries one, and the role the
    /// user holds.
    pub(crate) fn draw_channel_info_dialog(&mut self, ctx: &egui::Context) {
        let Some(chat_id) = self.channel_info_for else {
            return;
        };
        let t = self.tokens();
        let chat = self.chats.iter().find(|c| c.id == chat_id).cloned();
        let meta = self.channel_meta_for(chat_id);
        let info = self.live.as_ref().and_then(|s| {
            let jid = s.jids.get(&chat_id)?;
            s.channel_infos.get(jid).cloned()
        });
        let mut close = false;
        egui::Window::new(crate::i18n::t("channels.info_title"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .corner_radius(egui::CornerRadius::same(20))
                    .inner_margin(Margin::same(16)),
            )
            .show(ctx, |ui| {
                ui.set_width(300.0);
                let name = meta
                    .as_ref()
                    .map(|m| m.name.clone())
                    .or_else(|| chat.as_ref().map(|c| c.title.clone()))
                    .unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&name)
                            .font(crate::theme::semibold(15.0))
                            .color(rgb(t.text_primary)),
                    );
                    if meta.as_ref().is_some_and(|m| m.verified) {
                        let (r, _) =
                            ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
                        crate::widgets::verified_badge(ui.painter(), r.center(), 7.5, t);
                    }
                });
                if let Some(m) = &meta {
                    ui.label(
                        RichText::new(followers_text(m.followers))
                            .size(12.0)
                            .color(rgb(t.text_secondary)),
                    );
                    if !m.description.is_empty() {
                        ui.add_space(SP_1);
                        ui.label(
                            RichText::new(&m.description)
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                    }
                    let role_note = match m.role {
                        backend::LiveChannelRole::Owner => {
                            Some(crate::i18n::t("channels.role_owner"))
                        }
                        backend::LiveChannelRole::Admin => {
                            Some(crate::i18n::t("channels.role_admin"))
                        }
                        _ => None,
                    };
                    if let Some(note) = role_note {
                        ui.add_space(SP_1);
                        ui.label(RichText::new(note).size(12.0).color(rgb(t.accent)));
                    }
                    if let Some(code) = &m.invite_code {
                        ui.add_space(SP_2);
                        info_row(
                            ui,
                            t,
                            &crate::i18n::t("channels.info_invite"),
                            &format!("https://whatsapp.com/channel/{code}"),
                        );
                    }
                    if m.created_at > 0 {
                        info_row(
                            ui,
                            t,
                            &crate::i18n::t("channels.info_created"),
                            &crate::live::fmt_day(m.created_at),
                        );
                    }
                }
                if let Some(info) = &info {
                    if let Some(admins) = info.admin_count {
                        ui.add_space(SP_2);
                        info_row(
                            ui,
                            t,
                            &crate::i18n::t("channels.info_admins"),
                            &admins.to_string(),
                        );
                    }
                    if !info.follower_names.is_empty() {
                        ui.add_space(SP_2);
                        ui.label(
                            RichText::new(crate::i18n::t("channels.info_followers_sample"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                        for name in &info.follower_names {
                            ui.label(RichText::new(name).size(12.0).color(rgb(t.text_primary)));
                        }
                    }
                }
                ui.add_space(SP_4);
                if crate::widgets::button(
                    ui,
                    t,
                    &crate::i18n::t("common.close"),
                    crate::widgets::Btn::Secondary,
                )
                .clicked()
                {
                    close = true;
                }
            });
        if close {
            self.channel_info_for = None;
        }
    }

    /// The channel poll composer: question, options, the
    /// multiple-answers toggle, and the post action. Posting
    /// goes through CreateChannelPoll; the poll itself lands
    /// in the stream from the backend's history, and its
    /// failure is a banner.
    pub(crate) fn draw_channel_poll_composer(&mut self, ctx: &egui::Context) {
        let Some(chat_id) = self.channel_poll_for else {
            return;
        };
        let t = self.tokens();
        let mut send = false;
        let mut cancel = false;
        egui::Window::new(crate::i18n::t("channels.poll_title"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .corner_radius(egui::CornerRadius::same(20))
                    .inner_margin(Margin::same(16)),
            )
            .show(ctx, |ui| {
                ui.set_width(300.0);
                let field_w = (ui.available_width() - 20.0).max(120.0);
                crate::widgets::text_field(
                    ui,
                    t,
                    &mut self.channel_poll_question,
                    &crate::i18n::t("channels.poll_question"),
                    field_w,
                );
                let mut add_option = false;
                for (i, opt) in self.channel_poll_options.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}.", i + 1));
                        crate::widgets::text_field(
                            ui,
                            t,
                            opt,
                            &crate::i18n::tf("channels.poll_option", "n", &(i + 1).to_string()),
                            (field_w - 24.0).max(80.0),
                        );
                    });
                }
                if self.channel_poll_options.len() < 12
                    && crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("channels.poll_add_option"),
                        crate::widgets::Btn::Secondary,
                    )
                    .clicked()
                {
                    add_option = true;
                }
                if add_option {
                    self.channel_poll_options.push(String::new());
                }
                ui.checkbox(
                    &mut self.channel_poll_multiple,
                    crate::i18n::t("channels.poll_multiple"),
                );
                ui.add_space(SP_3);
                ui.horizontal(|ui| {
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("common.cancel"),
                        crate::widgets::Btn::Secondary,
                    )
                    .clicked()
                    {
                        cancel = true;
                    }
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("channels.poll_send"),
                        crate::widgets::Btn::Primary,
                    )
                    .clicked()
                    {
                        send = true;
                    }
                });
            });
        if cancel {
            self.channel_poll_for = None;
        }
        if send {
            let options: Vec<String> = self
                .channel_poll_options
                .iter()
                .map(|o| o.trim().to_owned())
                .filter(|o| !o.is_empty())
                .collect();
            if self.channel_poll_question.trim().is_empty() || options.len() < 2 {
                self.notice = Some(Notice::info(crate::i18n::t("channels.poll_needs")));
            } else if let Some(jid) = self.live_jid(chat_id) {
                self.send_backend(BackendCommand::CreateChannelPoll {
                    jid,
                    question: self.channel_poll_question.trim().to_owned(),
                    options,
                    multiple: self.channel_poll_multiple,
                });
                self.channel_poll_for = None;
                self.channel_poll_question.clear();
                self.channel_poll_options = vec![String::new(), String::new()];
                self.channel_poll_multiple = false;
            }
        }
    }
}

/// One label/value row in the channel info dialog.
fn info_row(ui: &mut egui::Ui, t: Tokens, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0).color(rgb(t.text_secondary)));
        ui.label(RichText::new(value).size(12.0).color(rgb(t.text_primary)));
    });
}

/// A Channels row context-menu pick, applied after the
/// panel renders (the menu borrows the panel's state).
#[derive(Debug, Clone, Copy)]
enum ChannelPick {
    Open,
    Info,
    Mute(bool),
    Unfollow,
}
