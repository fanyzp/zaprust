//! The profile dock itself: `App::draw_profile_dock` composes the
//! building blocks from `super::dock` into the per-kind info
//! surface, and the App methods under it apply the actions the
//! dock gathers (group edits, participant ops, block toggles).

use super::dock::{
    disappearing_section, dock_card, dock_card_section, dock_member_row, dock_prose, dock_row,
    dock_separator, profile_ai_body, profile_counts_section, profile_header, profile_user_body,
    DOCK_LABEL_X, SP_1, SP_2, SP_3, SP_SECTION,
};
use super::{profile_kind, MemberAction, ProfileIntents, ProfileKind};
use crate::icons::Icon;
use crate::state::{App, Notice, UiConfirm};
use crate::theme::rgb;
use crate::widgets::{button, text_field, Btn};
use core::model::ChatKind;
use eframe::egui::{self, RichText};

impl App {
    /// The chat info surface, drawn into the docked right panel
    /// the conversation reserves for it (see `draw_conversation`,
    /// which owns the panel and its width animation). The dock
    /// header carries the info title and the close button; the
    /// body below scrolls inside the panel. While `exiting` (the
    /// close animation is playing out) the body still renders, so
    /// the panel never blanks mid-slide, but every intent it
    /// gathers is discarded, the same contract the old popup
    /// exit used.
    pub(crate) fn draw_profile_dock(&mut self, ui: &mut egui::Ui, exiting: bool) {
        let Some(chat) = self.chats.iter().find(|c| c.id == self.selected_chat).cloned() else {
            self.profile_open = false;
            return;
        };
        let t = self.tokens();
        let kind = profile_kind(&chat);
        let ctx = ui.ctx().clone();
        let header_photo = self.chat_avatar_photo(&ctx, &chat);
        // Live group: fetch the real member list once per opening.
        if chat.kind == ChatKind::Group
            && self.live.is_some()
            && !self.groups.members_requested.contains(&chat.id)
        {
            self.groups.members_requested.insert(chat.id);
            if let Some(jid) = self.live_jid(chat.id) {
                self.send_backend(backend::BackendCommand::FetchGroupMembers { chat_jid: jid });
            }
        }
        if self.profile_form_for != Some(chat.id) {
            self.profile_form_for = Some(chat.id);
            self.group_subject_edit = chat.title.clone();
            self.group_desc_edit = String::new();
            self.add_participant_input = String::new();
            self.add_member_open = false;
        }
        let live_members = self.groups.live_members.get(&chat.id).cloned();
        let mut close = false;
        let mut toggle_mute = false;
        let mut export = false;
        let mut open_sibling: Option<u64> = None;
        let mut intents = ProfileIntents::default();

        // Member names for groups: the subtitle lists participants
        // in demo data; in live mode fall back to recent senders.
        let members: Vec<String> = if chat.kind == ChatKind::Group {
            if chat.subtitle.contains(',') && !chat.subtitle.contains("live") {
                chat.subtitle
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !s.starts_with('+'))
                    .map(str::to_owned)
                    .collect()
            } else {
                let mut seen: Vec<String> = Vec::new();
                for m in &self.conversation {
                    if let Some(s) = &m.sender {
                        if !seen.contains(s) {
                            seen.push(s.clone());
                        }
                    }
                }
                seen
            }
        } else {
            Vec::new()
        };
        let extra_members = chat
            .subtitle
            .split(',')
            .find_map(|part| {
                let part = part.trim();
                part.strip_prefix('+').and_then(|n| n.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        // The counts come from the conversation's own messages:
        // the loaded conversation, falling back to the chat's
        // archived messages when nothing is loaded, so a count
        // can never contradict the conversation it describes.
        let count_source =
            if self.conversation.is_empty() { &chat.messages } else { &self.conversation };
        let counts = core::model::conversation_counts(count_source);
        let (media_count, link_count, doc_count) = (counts.media, counts.links, counts.docs);
        let siblings: Vec<(u64, String)> = chat
            .community
            .as_ref()
            .map(|name| {
                self.chats
                    .iter()
                    .filter(|c| c.id != chat.id && c.community.as_deref() == Some(name.as_str()))
                    .map(|c| (c.id, c.title.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let export_active = self.feature_active("export-chat");

        // Header status line + variant inputs, resolved per kind.
        let subtitle = match kind {
            ProfileKind::Group => {
                let n = members.len() + extra_members;
                if n > 0 {
                    crate::i18n::tf("profile.member_count", "count", &n.to_string())
                } else {
                    "Group".to_string()
                }
            }
            ProfileKind::Channel => {
                // Live channels carry no subtitle (the backend
                // surfaces no subscriber count), so the line is
                // the bare kind word rather than a dangling
                // separator after nothing.
                if chat.subtitle.is_empty() {
                    crate::i18n::t("profile.channel_plain")
                } else {
                    crate::i18n::tf("profile.channel_line", "subtitle", &chat.subtitle)
                }
            }
            ProfileKind::Ai => crate::i18n::t("profile.meta_ai_line"),
            // The same line the conversation header shows: the
            // chat's own in demo mode, the watched contact's
            // real presence in live mode.
            ProfileKind::User => self.header_subtitle(&chat),
        };
        // The user variant's identifier line and About text: the
        // live JID when live, nothing otherwise (demo data has
        // no address to show, and no caption stands in for one);
        // the About section only appears when the subtitle is a
        // status line rather than a presence line.
        let user_id_line: Option<String> =
            if kind == ProfileKind::User { self.live_jid(chat.id) } else { None };
        let user_about: Option<String> = if kind == ProfileKind::User {
            let s = chat.subtitle.trim();
            let presence = s == "online"
                || s.starts_with("last seen")
                || s.starts_with("typing")
                || s.is_empty();
            if presence {
                None
            } else {
                Some(chat.subtitle.clone())
            }
        } else {
            None
        };
        let ai_description: String = if self.live_mode {
            crate::i18n::t("profile.meta_ai_desc")
        } else {
            crate::i18n::t("profile.meta_ai_desc_demo")
        };
        let ai_suggestions: Vec<String> = [
            "profile.meta_suggest_1",
            "profile.meta_suggest_2",
            "profile.meta_suggest_3",
            "profile.meta_suggest_4",
        ]
        .iter()
        .map(|k| crate::i18n::t(k))
        .collect();
        let blocked = self.blocked_chats.contains(&chat.id);

        // Dock header: the info title for this kind of chat and
        // the close button only. Mute is a row in the body
        // below, where the dock's other settings live.
        let info_title = match kind {
            ProfileKind::Group => crate::i18n::t("profile.group_info"),
            ProfileKind::Channel => crate::i18n::t("profile.channel_info"),
            ProfileKind::User | ProfileKind::Ai => crate::i18n::t("profile.contact_info"),
        };
        // The header row is allocated at a fixed height (the
        // buttons' 28 px): a bare `with_layout` would hand the
        // child the panel's whole remaining rect and centre the
        // row across all of it. The same applies to every
        // horizontal row in the dock (name row, member rows, the
        // add forms): all go through `allocate_ui_with_layout`
        // with an explicit height.
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 28.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(
                    RichText::new(info_title)
                        .font(crate::theme::semibold(14.0))
                        .color(rgb(t.text_primary)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::widgets::icon_button(
                        ui,
                        t,
                        Icon::X,
                        28.0,
                        &crate::i18n::t("common.close"),
                        None,
                    )
                    .clicked()
                        && !exiting
                    {
                        close = true;
                    }
                });
            },
        );
        ui.add_space(SP_2);
        // Escape closes the dock, like the other surfaces,
        // unless the confirmation modal is up: there Escape
        // belongs to the modal (it cancels the action, and the
        // dock stays where it was).
        if !exiting && self.ui_confirm.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            close = true;
        }
        // The body scrolls inside the dock: the ScrollArea takes
        // the height the panel leaves under the header, so the
        // wheel always engages and the group management and
        // member rows stay reachable (the old centered window
        // needed an explicit height clamp for the same reason).
        egui::ScrollArea::vertical().id_salt("profile_dock_scroll").show(ui, |ui| {
            profile_header(ui, t, &chat, &subtitle, header_photo);
            // Mute as a body row, in the same place for every
            // variant: the bell icon shows the action, the label
            // names it, and the click routes through the same
            // apply_chat_action path as the header menu.
            ui.add_space(SP_3);
            let (mute_icon, mute_key) = if chat.muted {
                (Icon::Bell, "conversation.unmute_notifications")
            } else {
                (Icon::BellOff, "conversation.mute_notifications")
            };
            if dock_row(
                ui,
                t,
                Some((mute_icon, t.text_secondary)),
                &crate::i18n::t(mute_key),
                t.text_primary,
                false,
            )
            .clicked()
                && !exiting
            {
                toggle_mute = true;
            }
            match kind {
                ProfileKind::Ai => {
                    profile_ai_body(ui, t, &ai_description, &ai_suggestions, &mut intents);
                }
                ProfileKind::User => {
                    profile_user_body(
                        ui,
                        t,
                        user_id_line.as_deref(),
                        user_about.as_deref(),
                        (media_count, link_count, doc_count),
                        blocked,
                        export_active,
                        chat.ephemeral_secs,
                        &mut intents,
                    );
                }
                ProfileKind::Group | ProfileKind::Channel => {
                    // Export is a row on the shared grid (groups
                    // only, and only when the feature is on).
                    if chat.kind == ChatKind::Group && export_active {
                        ui.add_space(SP_3);
                        let clicked = dock_card(ui, t, |ui| {
                            dock_row(
                                ui,
                                t,
                                Some((Icon::Download, t.text_secondary)),
                                &crate::i18n::t("common.export_chat"),
                                t.text_primary,
                                false,
                            )
                            .clicked()
                        })
                        .inner;
                        if clicked {
                            export = true;
                        }
                    }

                    // Group description (standard layout slot;
                    // demo groups carry no description, live groups
                    // edit theirs in the settings section below).
                    if chat.kind == ChatKind::Group {
                        let desc = if self.group_desc_edit.is_empty() {
                            crate::i18n::t("profile.no_description")
                        } else {
                            self.group_desc_edit.clone()
                        };
                        dock_card_section(
                            ui,
                            t,
                            &crate::i18n::t("profile.description"),
                            None,
                            |ui| {
                                dock_prose(ui, |ui| {
                                    ui.label(
                                        RichText::new(desc)
                                            .size(12.5)
                                            .color(rgb(t.text_secondary)),
                                    );
                                });
                            },
                        );
                    }

                    // Disappearing messages (groups only; a
                    // channel is not an end-to-end chat). Available
                    // in both modes: demo applies it to the demo
                    // state, live sends the group timer command.
                    if chat.kind == ChatKind::Group {
                        disappearing_section(ui, t, chat.ephemeral_secs, &mut intents);
                    }

                    // Community + sibling groups.
                    if let Some(name) = &chat.community {
                        let name = name.clone();
                        dock_card_section(
                            ui,
                            t,
                            &crate::i18n::t("profile.community"),
                            None,
                            |ui| {
                                dock_prose(ui, |ui| {
                                    ui.label(
                                        RichText::new(format!("Community · {name}"))
                                            .size(12.5)
                                            .color(rgb(t.text_secondary)),
                                    );
                                });
                                ui.add_space(SP_1);
                                for (i, (id, title)) in siblings.iter().enumerate() {
                                    if i > 0 {
                                        dock_separator(ui, t);
                                    }
                                    if dock_row(
                                        ui,
                                        t,
                                        Some((Icon::Group, t.text_secondary)),
                                        title,
                                        t.text_primary,
                                        true,
                                    )
                                    .clicked()
                                    {
                                        open_sibling = Some(*id);
                                    }
                                }
                            },
                        );
                    }

                    // Live group management (live chats only).
                    // Form fields take the dock's full content
                    // width, whatever it currently is: the fixed
                    // widths this section used before were sized
                    // for the old popup's stage and overflowed
                    // the dock at narrow window sizes.
                    if chat.kind == ChatKind::Group {
                        if self.live.is_some() {
                            // Read before the settings section opens:
                            // the requests section below it keys off
                            // the same value.
                            let approval_on =
                                self.groups.approval.get(&chat.id).copied().unwrap_or(false);
                            dock_card_section(ui, t, &crate::i18n::t("profile.group_settings"), None, |ui| {
                            ui.label(RichText::new("Name").size(11.5).color(rgb(t.text_secondary)));
                            text_field(
                                ui,
                                t,
                                &mut self.group_subject_edit,
                                "",
                                ui.available_width(),
                            );
                            ui.label(
                                RichText::new("Description")
                                    .size(11.5)
                                    .color(rgb(t.text_secondary)),
                            );
                            text_field(ui, t, &mut self.group_desc_edit, "", ui.available_width());
                            ui.label(
                                RichText::new(
                                    "Leave the description empty to keep the current one.",
                                )
                                .size(11.0)
                                .color(rgb(t.text_faint)),
                            );
                            ui.add_space(SP_2);
                            ui.horizontal(|ui| {
                                if button(
                                    ui,
                                    t,
                                    &crate::i18n::t("profile.save_group_info"),
                                    Btn::Secondary,
                                )
                                .clicked()
                                {
                                    self.save_group_info(chat.id);
                                }
                            });
                            ui.add_space(SP_3);
                            ui.label(
                                RichText::new(crate::i18n::t("profile.add_participant"))
                                    .size(11.5)
                                    .color(rgb(t.text_secondary)),
                            );
                            // The button keeps its natural size on
                            // the right; the field flexes into the
                            // width that remains.
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 40.0),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if button(ui, t, "Add", Btn::Secondary).clicked() {
                                        self.group_participant_op(
                                            chat.id,
                                            None,
                                            backend::ParticipantOp::Add,
                                        );
                                    }
                                    let w = ui.available_width();
                                    text_field(
                                        ui,
                                        t,
                                        &mut self.add_participant_input,
                                        "Phone number or address",
                                        w,
                                    );
                                },
                            );
                            ui.add_space(SP_3);
                            // Invite link: fetched on demand;
                            // copy and reset act on the link the
                            // backend last reported for this chat.
                            ui.label(
                                RichText::new(crate::i18n::t("profile.invite_link"))
                                    .size(11.5)
                                    .color(rgb(t.text_secondary)),
                            );
                            let invite = self
                                .groups.invite_link
                                .as_ref()
                                .filter(|(id, _)| *id == chat.id)
                                .map(|(_, link)| link.clone());
                            if let Some(link) = invite {
                                ui.label(
                                    RichText::new(link.clone())
                                        .size(11.0)
                                        .color(rgb(t.text_faint)),
                                );
                                ui.horizontal(|ui| {
                                    if button(
                                        ui,
                                        t,
                                        &crate::i18n::t("profile.copy_link"),
                                        Btn::Secondary,
                                    )
                                    .clicked()
                                    {
                                        ui.ctx().copy_text(link.clone());
                                        self.notice =
                                            Some(Notice::info(crate::i18n::t("profile.link_copied")));
                                    }
                                    if button(
                                        ui,
                                        t,
                                        &crate::i18n::t("profile.reset_link"),
                                        Btn::Secondary,
                                    )
                                    .clicked()
                                    {
                                        if let Some(jid) = self.live_jid(chat.id) {
                                            self.send_backend(
                                                backend::BackendCommand::FetchGroupInviteLink {
                                                    chat_jid: jid,
                                                    reset: true,
                                                },
                                            );
                                        }
                                    }
                                });
                            } else if button(
                                ui,
                                t,
                                &crate::i18n::t("profile.show_link"),
                                Btn::Secondary,
                            )
                            .clicked()
                            {
                                if let Some(jid) = self.live_jid(chat.id) {
                                    self.send_backend(
                                        backend::BackendCommand::FetchGroupInviteLink {
                                            chat_jid: jid,
                                            reset: false,
                                        },
                                    );
                                }
                            }
                            ui.add_space(SP_3);
                            // Membership approval switch (the
                            // requests themselves get their own
                            // section below, when approval is on).
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 28.0),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let mut on = approval_on;
                                    crate::widgets::switch(ui, &mut on, t);
                                    if on != approval_on {
                                        if let Some(jid) = self.live_jid(chat.id) {
                                            self.send_backend(
                                                backend::BackendCommand::SetMembershipApproval {
                                                    chat_jid: jid,
                                                    enabled: on,
                                                },
                                            );
                                        }
                                    }
                                    ui.label(
                                        RichText::new(crate::i18n::t("profile.approve_members"))
                                            .size(12.0)
                                            .color(rgb(t.text_primary)),
                                    );
                                },
                            );
                            // Announcement and locked modes.
                            let (announce_on, locked_on) =
                                self.groups.modes.get(&chat.id).copied().unwrap_or((false, false));
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 28.0),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let mut on = announce_on;
                                    crate::widgets::switch(ui, &mut on, t);
                                    if on != announce_on {
                                        if let Some(jid) = self.live_jid(chat.id) {
                                            self.send_backend(
                                                backend::BackendCommand::SetGroupAnnounce {
                                                    chat_jid: jid,
                                                    announce: on,
                                                },
                                            );
                                        }
                                    }
                                    ui.label(
                                        RichText::new(crate::i18n::t("profile.announce_mode"))
                                            .size(12.0)
                                            .color(rgb(t.text_primary)),
                                    );
                                },
                            );
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 28.0),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let mut on = locked_on;
                                    crate::widgets::switch(ui, &mut on, t);
                                    if on != locked_on {
                                        if let Some(jid) = self.live_jid(chat.id) {
                                            self.send_backend(
                                                backend::BackendCommand::SetGroupLocked {
                                                    chat_jid: jid,
                                                    locked: on,
                                                },
                                            );
                                        }
                                    }
                                    ui.label(
                                        RichText::new(crate::i18n::t("profile.locked_mode"))
                                            .size(12.0)
                                            .color(rgb(t.text_primary)),
                                    );
                                },
                            );
                            });
                            if approval_on {
                                dock_card_section(ui, t, &crate::i18n::t("profile.member_requests"), None, |ui| {
                                ui.horizontal(|ui| {
                                    ui.add_space(DOCK_LABEL_X);
                                    if button(
                                        ui,
                                        t,
                                        &crate::i18n::t("profile.review_requests"),
                                        Btn::Secondary,
                                    )
                                    .clicked()
                                    {
                                        if let Some(jid) = self.live_jid(chat.id) {
                                            self.send_backend(
                                                backend::BackendCommand::FetchJoinRequests {
                                                    chat_jid: jid,
                                                },
                                            );
                                        }
                                    }
                                });
                                let requests = self
                                    .groups.join_requests
                                    .as_ref()
                                    .filter(|(id, _)| *id == chat.id)
                                    .map(|(_, rows)| rows.clone());
                                if let Some(rows) = requests {
                                    if rows.is_empty() {
                                        ui.label(
                                            RichText::new(crate::i18n::t("profile.no_requests"))
                                                .size(11.0)
                                                .color(rgb(t.text_faint)),
                                        );
                                    }
                                    for (jid, name) in rows {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(name)
                                                    .size(12.0)
                                                    .color(rgb(t.text_primary)),
                                            );
                                            if button(
                                                ui,
                                                t,
                                                &crate::i18n::t("profile.approve"),
                                                Btn::Primary,
                                            )
                                            .clicked()
                                            {
                                                if let Some(gjid) = self.live_jid(chat.id) {
                                                    self.send_backend(backend::BackendCommand::ReviewJoinRequest {
                                                        chat_jid: gjid,
                                                        participant: jid.clone(),
                                                        approve: true,
                                                    });
                                                }
                                            }
                                            if button(
                                                ui,
                                                t,
                                                &crate::i18n::t("profile.reject"),
                                                Btn::Secondary,
                                            )
                                            .clicked()
                                            {
                                                if let Some(gjid) = self.live_jid(chat.id) {
                                                    self.send_backend(backend::BackendCommand::ReviewJoinRequest {
                                                        chat_jid: gjid,
                                                        participant: jid.clone(),
                                                        approve: false,
                                                    });
                                                }
                                            }
                                        });
                                    }
                                }
                                });
                            }
                            // Leave group: a danger row in its
                            // own section; the confirmation is the
                            // house modal, never an inline strip.
                            ui.add_space(SP_SECTION);
                            let leave_clicked = dock_card(ui, t, |ui| {
                                dock_row(
                                    ui,
                                    t,
                                    None,
                                    &crate::i18n::t("profile.leave_group"),
                                    t.danger,
                                    false,
                                )
                                .clicked()
                            })
                            .inner;
                            if leave_clicked {
                                self.ui_confirm = Some(UiConfirm::LeaveGroup {
                                    chat_id: chat.id,
                                    name: chat.title.clone(),
                                });
                            }
                        } else {
                            ui.add_space(SP_3);
                            ui.label(
                                RichText::new(crate::i18n::t("profile.group_management_live_only"))
                                    .size(11.5)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                    }

                    // Members (live list wins over demo names).
                    // Each row is one clean unit; every per-member
                    // action lives in the row's kebab menu.
                    if chat.kind == ChatKind::Group {
                        if let Some(live_list) = &live_members {
                            // Member photos resolve before the
                            // section closure borrows the panel.
                            let member_photos: Vec<Option<egui::TextureId>> = live_list
                                .iter()
                                .map(|row| self.member_avatar_photo(&ctx, &row.address, &row.label))
                                .collect();
                            let mut member_action: Option<MemberAction> = None;
                            dock_card_section(
                                ui,
                                t,
                                &format!(
                                    "{} ({})",
                                    crate::i18n::t("profile.members"),
                                    live_list.len()
                                ),
                                None,
                                |ui| {
                                    for (row, photo) in
                                        live_list.iter().zip(member_photos.iter())
                                    {
                                        let Some(kebab) = dock_member_row(
                                            ui,
                                            t,
                                            &row.label,
                                            row.detail.as_deref(),
                                            row.admin,
                                            true,
                                            *photo,
                                        ) else {
                                            continue;
                                        };
                                        egui::Popup::menu(&kebab)
                                            .frame(crate::theme::glass_panel_frame(t, 12))
                                            .width(208.0)
                                            .show(|ui| {
                                                // Message is offered
                                                // only when a DM chat
                                                // with this member
                                                // actually exists.
                                                let dm_id = self.live.as_ref().and_then(|s| {
                                                    s.jids
                                                        .iter()
                                                        .find(|(_, jid)| **jid == row.address)
                                                        .map(|(id, _)| *id)
                                                });
                                                if let Some(dm_id) = dm_id {
                                                    if crate::widgets::menu_item_icon(
                                                        ui,
                                                        t,
                                                        Some(Icon::Chat),
                                                        &crate::i18n::t("profile.message"),
                                                        None,
                                                    )
                                                    .clicked()
                                                    {
                                                        member_action =
                                                            Some(MemberAction::Message(dm_id));
                                                        ui.close();
                                                    }
                                                }
                                                let (label_key, op) = if row.admin {
                                                    (
                                                        "profile.remove_admin",
                                                        backend::ParticipantOp::Demote,
                                                    )
                                                } else {
                                                    (
                                                        "profile.make_admin",
                                                        backend::ParticipantOp::Promote,
                                                    )
                                                };
                                                if crate::widgets::menu_item_icon(
                                                    ui,
                                                    t,
                                                    Some(Icon::Shield),
                                                    &crate::i18n::t(label_key),
                                                    None,
                                                )
                                                .clicked()
                                                {
                                                    member_action = Some(MemberAction::Op(
                                                        row.address.clone(),
                                                        op,
                                                    ));
                                                    ui.close();
                                                }
                                                // A painted hairline, not
                                                // ui.separator(): the split
                                                // before the danger item
                                                // takes the standard soft
                                                // form, inset to the item
                                                // text line at the soft
                                                // alpha (design standard,
                                                // hairline discipline).
                                                ui.add_space(4.0);
                                                let (line, _) = ui.allocate_exact_size(
                                                    egui::vec2(ui.available_width(), 1.0),
                                                    egui::Sense::hover(),
                                                );
                                                let inset =
                                                    crate::theme::LIST_PANEL_INSET_X as f32;
                                                let c = rgb(t.border);
                                                ui.painter().hline(
                                                    (line.left() + inset)
                                                        ..=(line.right() - inset),
                                                    line.center().y,
                                                    egui::Stroke::new(
                                                        1.0_f32,
                                                        egui::Color32::from_rgba_unmultiplied(
                                                            c.r(),
                                                            c.g(),
                                                            c.b(),
                                                            100,
                                                        ),
                                                    ),
                                                );
                                                ui.add_space(4.0);
                                                if crate::widgets::menu_item_danger(
                                                    ui,
                                                    t,
                                                    Icon::Delete,
                                                    &crate::i18n::t("common.remove"),
                                                )
                                                .clicked()
                                                {
                                                    member_action = Some(MemberAction::Remove(
                                                        row.address.clone(),
                                                        row.label.clone(),
                                                    ));
                                                    ui.close();
                                                }
                                            });
                                        ui.add_space(SP_1);
                                    }
                                },
                            );
                            match member_action {
                                Some(MemberAction::Message(dm_id)) => {
                                    self.select_chat_now(dm_id);
                                    close = true;
                                }
                                Some(MemberAction::Op(address, op)) => {
                                    self.group_participant_op(chat.id, Some(address), op);
                                }
                                Some(MemberAction::Remove(address, name)) => {
                                    self.ui_confirm = Some(UiConfirm::RemoveMember {
                                        chat_id: chat.id,
                                        address,
                                        name,
                                    });
                                }
                                None => {}
                            }
                        }
                    }
                    // Members (demo names from the subtitle).
                    // Demo rows carry no actions (there is no
                    // backend behind them), so they render
                    // without a kebab: a menu with no working
                    // items would be a dead control.
                    if chat.kind == ChatKind::Group && live_members.is_none() && !members.is_empty()
                    {
                        dock_card_section(
                            ui,
                            t,
                            &format!(
                                "{} ({})",
                                crate::i18n::t("profile.members"),
                                members.len() + extra_members
                            ),
                            None,
                            |ui| {
                        // Add member row: in live groups it
                        // opens the inline add form right here
                        // (the form used to hide in the group
                        // settings section below the fold); demo
                        // mode says group management is
                        // live-only.
                        if dock_row(
                            ui,
                            t,
                            Some((Icon::Plus, t.accent)),
                            &crate::i18n::t("profile.add_member"),
                            t.accent,
                            false,
                        )
                        .on_hover_text(crate::i18n::t("profile.add_member_caption"))
                        .clicked()
                        {
                            if self.live.is_some() {
                                self.add_member_open = !self.add_member_open;
                                self.add_participant_input.clear();
                            } else {
                                self.notice = Some(Notice::info(crate::i18n::t("profile.add_member_caption")));
                            }
                        }
                        if self.add_member_open && self.live.is_some() {
                            ui.add_space(SP_1);
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 40.0),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if button(ui, t, "Add", Btn::Primary).clicked() {
                                        self.group_participant_op(
                                            chat.id,
                                            None,
                                            backend::ParticipantOp::Add,
                                        );
                                        self.add_participant_input.clear();
                                        self.add_member_open = false;
                                    }
                                    let w = ui.available_width();
                                    text_field(
                                        ui,
                                        t,
                                        &mut self.add_participant_input,
                                        "Phone number or address",
                                        w,
                                    );
                                },
                            );
                        }
                        for (i, name) in members.iter().enumerate() {
                            if i > 0 {
                                dock_separator(ui, t);
                            }
                            let photo = self.demo_avatar(&ctx, name);
                            dock_member_row(ui, t, name, None, i == 0, false, photo);
                            ui.add_space(SP_1);
                        }
                        if extra_members > 0 {
                            ui.horizontal(|ui| {
                                ui.add_space(DOCK_LABEL_X);
                                ui.label(
                                    RichText::new(format!("… and {extra_members} more"))
                                        .size(11.5)
                                        .color(rgb(t.text_faint)),
                                );
                            });
                        }
                            },
                        );
                        // Leave group row (demo: archives the
                        // row locally, like live leave), in its
                        // own section, confirmed by the house modal.
                        ui.add_space(SP_SECTION);
                        let leave_clicked = dock_card(ui, t, |ui| {
                            dock_row(
                                ui,
                                t,
                                None,
                                &crate::i18n::t("profile.leave_group"),
                                t.danger,
                                false,
                            )
                            .clicked()
                        })
                        .inner;
                        if leave_clicked {
                            self.ui_confirm = Some(UiConfirm::LeaveGroup {
                                chat_id: chat.id,
                                name: chat.title.clone(),
                            });
                        }
                    }

                    profile_counts_section(ui, t, media_count, link_count, doc_count);
                }
            }
        });

        if exiting {
            // The exit run only replays the panel; every intent
            // gathered during it is discarded.
            return;
        }
        if intents.toggle_block {
            self.toggle_block_contact(chat.id);
        }
        if let Some(secs) = intents.ephemeral_pick {
            self.set_disappearing_timer(chat.id, secs);
        }
        if intents.export {
            export = true;
        }
        if toggle_mute {
            // Same routing as the header menu: apply_chat_action
            // flips locally at once and sends the backend command
            // in live mode.
            self.apply_chat_action(chat.id, chat.archived, chat.pinned, !chat.muted);
        }
        if export {
            self.export_current_chat();
            close = true;
        }
        if let Some(id) = open_sibling {
            self.select_chat_now(id);
            close = true;
        }
        if intents.focus_search {
            // The chat-list search field carries the stable id
            // "chat_search" (chat_list.rs), so the focus request
            // lands on it as the panel closes back onto the list.
            ctx.memory_mut(|m| m.request_focus(egui::Id::unique("chat_search")));
            close = true;
        }
        if let Some(text) = intents.suggestion {
            self.composer.draft = text;
            close = true;
        }
        if intents.start_call {
            self.start_call_from_header();
        }
        if close || intents.close {
            self.profile_open = false;
            self.profile_form_for = None;
            self.groups.members_requested.remove(&chat.id);
        }
    }

    /// Sets one chat's disappearing-messages timer from the
    /// info dock. State, archive and the system notice update
    /// immediately in both modes through
    /// [`App::apply_disappearing_changed`]; live mode also sends
    /// the protocol command, whose DisappearingChanged echo then
    /// finds the value already in place and stays silent.
    pub(crate) fn set_disappearing_timer(&mut self, chat_id: u64, duration_secs: u32) {
        if self.ephemeral_secs_of(chat_id) == duration_secs {
            return;
        }
        self.apply_disappearing_changed(chat_id, duration_secs);
        if let Some(jid) = self.live_jid(chat_id) {
            self.send_backend(backend::BackendCommand::SetDisappearingTimer {
                chat_jid: jid,
                duration_secs,
            });
        }
    }

    /// Saves the edited group subject/description through the
    /// backend (live groups only).
    pub(crate) fn save_group_info(&mut self, chat_id: u64) {
        let Some(jid) = self.live_jid(chat_id) else {
            return;
        };
        let subject = self.group_subject_edit.trim().to_owned();
        let description = self.group_desc_edit.trim().to_owned();
        self.send_backend(backend::BackendCommand::SetGroupInfo {
            chat_jid: jid,
            subject: if subject.is_empty() { None } else { Some(subject.clone()) },
            // An empty field keeps the current description; the
            // form never clears it implicitly (data-loss guard).
            description: if description.is_empty() { None } else { Some(description) },
        });
        if !subject.is_empty() {
            if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                c.title = subject;
            }
        }
        self.notice = Some(Notice::info("Group info saved.".to_owned()));
    }

    /// One participant operation from the profile panel. `address`
    /// None means: take the address from the add-participant input.
    pub(crate) fn group_participant_op(
        &mut self,
        chat_id: u64,
        address: Option<String>,
        op: backend::ParticipantOp,
    ) {
        let Some(jid) = self.live_jid(chat_id) else {
            return;
        };
        let target = address.unwrap_or_else(|| self.add_participant_input.trim().to_owned());
        if target.is_empty() {
            self.notice = Some(Notice::info("Type a phone number or address first.".to_owned()));
            return;
        }
        self.send_backend(backend::BackendCommand::GroupParticipants {
            chat_jid: jid.clone(),
            participants: vec![target],
            op,
        });
        if matches!(op, backend::ParticipantOp::Add) {
            self.add_participant_input.clear();
        }
        // Refresh the member list so the panel shows the result.
        self.groups.live_members.remove(&chat_id);
        self.send_backend(backend::BackendCommand::FetchGroupMembers { chat_jid: jid });
        self.notice = Some(Notice::info("Sent. The member list will refresh.".to_owned()));
    }

    /// Leaves a live group and archives the local row.
    pub(crate) fn leave_group(&mut self, chat_id: u64) {
        if let Some(jid) = self.live_jid(chat_id) {
            self.send_backend(backend::BackendCommand::LeaveGroup { chat_jid: jid });
        }
        // Archive locally WITHOUT routing through apply_chat_action:
        // its three-way diff also fired Unpin/Unmute (and a backend
        // Archive) for a group the user simply left (logic review
        // F19). Local state + archive write only.
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.archived = true;
        }
        let snapshot = self.live.as_ref().and_then(|session| {
            let jid = session.jids.get(&chat_id)?.clone();
            let ts = session.times.get(&chat_id).copied().unwrap_or(0);
            let chat = self.chats.iter().find(|c| c.id == chat_id)?.clone();
            Some((chat, jid, ts))
        });
        if let (Some((chat, jid, ts)), Some(session)) = (snapshot, &self.live) {
            if let Some(store) = &session.store {
                let _ = store.upsert_chat(&crate::live::chat_to_live(&chat, &jid, ts));
            }
        }
        if self.live.is_none() {
            self.notice = Some(Notice::info("You left the group.".to_owned()));
        }
    }

    /// Blocks/unblocks a DM contact: local state plus the backend
    /// command in live mode.
    pub(crate) fn toggle_block_contact(&mut self, chat_id: u64) {
        let now_blocked = if self.blocked_chats.contains(&chat_id) {
            self.blocked_chats.remove(&chat_id);
            false
        } else {
            self.blocked_chats.insert(chat_id);
            true
        };
        if let Some(jid) = self.live_jid(chat_id) {
            self.send_backend(backend::BackendCommand::BlockContact {
                chat_jid: jid,
                block: now_blocked,
            });
        }
        // The set is persisted (to the encrypted archive store,
        // privacy audit A6) so the panel still shows the right
        // state after a restart, before (or without) the
        // backend's connect-time blocklist fetch.
        self.persist_blocked_chats();
        self.notice = Some(Notice::info(if now_blocked {
            "Contact blocked.".to_owned()
        } else {
            "Contact unblocked.".to_owned()
        }));
    }
}
