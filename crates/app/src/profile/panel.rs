//! The profile dock itself: `App::draw_profile_dock` composes the
//! building blocks from `super::dock` into the per-kind info
//! surface, and the App methods under it apply the actions the
//! dock gathers (group edits, participant ops, block toggles).

use super::dock::{
    disappearing_section, dock_card, dock_card_section, dock_member_row, dock_member_row_labeled,
    dock_prose, dock_row, dock_separator, profile_ai_body, profile_counts_section, profile_header,
    profile_user_body, DOCK_LABEL_X,
};
use super::{profile_kind, MemberAction, ProfileIntents, ProfileKind};
use crate::icons::Icon;
use crate::state::{App, Notice, UiConfirm};
use crate::theme::{rgb, SP_1, SP_2, SP_3, SP_SECTION};
use crate::widgets::{button, text_field, Btn};
use core::model::ChatKind;
use eframe::egui::{self, RichText};

impl App {
    /// The messages the profile dock's counts and gallery
    /// describe: the loaded conversation, falling back to the
    /// chat's archived messages when nothing is loaded, so a
    /// count can never contradict the conversation it
    /// describes.
    pub(crate) fn profile_count_source<'a>(
        &'a self,
        chat: &'a core::model::Chat,
    ) -> &'a [core::model::Message] {
        if self.conversation.is_empty() {
            &chat.messages
        } else {
            &self.conversation
        }
    }

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
        // Live contact: fetch the business profile once per
        // opening (parity P10, row 130). A contact with no
        // business profile answers None and gets no section.
        if kind == ProfileKind::User {
            if let Some(jid) = self.live_jid(chat.id) {
                self.ensure_business_profile(&jid);
                // QA hook (--catalog): stage the catalog open
                // for this chat and kick its fetches, exactly
                // as the section's catalog row does on click.
                if self.business.catalog_qa_open && self.business.catalog_open_for.is_none() {
                    self.business.catalog_open_for = Some(jid.clone());
                    if !self.business.catalogs.contains_key(&jid) {
                        self.send_backend(backend::BackendCommand::FetchCollections {
                            chat_jid: jid.clone(),
                        });
                        self.send_backend(backend::BackendCommand::FetchCatalog {
                            chat_jid: jid,
                            after: None,
                        });
                    }
                }
            }
        }
        if self.profile_form_for != Some(chat.id) {
            // The gallery belongs to the chat it was opened
            // from; switching chats starts back at the profile.
            // The first open of a chat (no previous form) keeps
            // a section seeded for it, which is how the QA
            // flags stage the gallery.
            if matches!(self.profile_form_for, Some(prev) if prev != chat.id) {
                self.gallery_section = None;
            }
            self.profile_form_for = Some(chat.id);
            self.group_subject_edit = chat.title.clone();
            self.group_desc_edit = String::new();
            self.add_participant_input = String::new();
            self.add_member_open = false;
            // The member-label draft reseeds for the new chat.
            self.groups.member_label_draft_chat = None;
        }
        let live_members = self.groups.live_members.get(&chat.id).cloned();
        let mut close = false;
        let mut toggle_mute = false;
        let mut export = false;
        let mut open_sibling: Option<u64> = None;
        // The group settings' Change photo button defers its
        // file pick to after the dock draw (the pick is a
        // blocking OS dialog; the section closures only record
        // the intent).
        let mut group_photo_pick = false;
        let mut intents = ProfileIntents::default();
        // Gallery clicks (section 25), applied after the draw
        // like the intents.
        let mut gallery_actions = super::gallery::GalleryActions::default();
        // Backend commands the business surfaces queue during
        // the draw (the scroll closure cannot reach the
        // session); sent when the draw is done.
        let mut dock_commands: Vec<backend::BackendCommand> = Vec::new();
        // Whether demo fixtures may stand in as content in the
        // gallery's tiles (the one predicate every fixture
        // lookup gates on).
        let demo_allowed = self.demo_fixtures_allowed();

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
        let count_source = self.profile_count_source(&chat);
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
                        .font(crate::theme::semibold(15.0))
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
            // With the gallery open, Escape steps back to the
            // profile first; only the profile's Escape closes
            // the dock.
            if self.business.catalog_open_for.is_some() {
                self.business.catalog_open_for = None;
            } else if self.gallery_section.is_some() {
                self.gallery_section = None;
            } else {
                close = true;
            }
        }
        // The body scrolls inside the dock: the ScrollArea takes
        // the height the panel leaves under the header, so the
        // wheel always engages and the group management and
        // member rows stay reachable (the old centered window
        // needed an explicit height clamp for the same reason).
        egui::ScrollArea::vertical().id_salt("profile_dock_scroll").show(ui, |ui| {
            // The catalog (parity P10) takes the dock body over
            // while it is open for this chat, like the gallery.
            if kind == ProfileKind::User {
                if let Some(jid) = &user_id_line {
                    if self.business.catalog_open_for.as_deref() == Some(jid.as_str()) {
                        let jid = jid.clone();
                        super::business::catalog_body(
                            ui,
                            t,
                            &mut self.business,
                            &jid,
                            &mut dock_commands,
                        );
                        return;
                    }
                }
            }
            if let Some(section) = self.gallery_section {
                // The gallery (section 25) takes the dock body
                // over from the counts rows that opened it; the
                // back row inside it returns to the profile.
                let source = self.profile_count_source(&chat).to_vec();
                super::gallery::profile_gallery_body(
                    ui,
                    t,
                    &mut self.media,
                    self.live.as_ref(),
                    demo_allowed,
                    &source,
                    section,
                    &mut gallery_actions,
                );
                return;
            }
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
                    if let Some(jid) = &user_id_line {
                        let jid = jid.clone();
                        super::business::business_section(
                            ui,
                            t,
                            &mut self.business,
                            &jid,
                            &mut dock_commands,
                        );
                    }
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
                                            .size(12.0)
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
                                            .size(12.0)
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
                            // The group's picture, changed from
                            // the group's own surface (design
                            // standard section 23): the same
                            // avatar the header paints. Change
                            // picks a file (prepared exactly like
                            // the account's own photo); Remove
                            // confirms first, like every
                            // destructive action.
                            ui.label(
                                RichText::new(crate::i18n::t("profile.group_photo"))
                                    .size(12.0)
                                    .color(rgb(t.text_secondary)),
                            );
                            ui.horizontal(|ui| {
                                if button(
                                    ui,
                                    t,
                                    &crate::i18n::t("profile.change_photo"),
                                    Btn::Secondary,
                                )
                                .clicked()
                                {
                                    group_photo_pick = true;
                                }
                                if header_photo.is_some()
                                    && button(
                                        ui,
                                        t,
                                        &crate::i18n::t("profile.remove_photo"),
                                        Btn::Secondary,
                                    )
                                    .clicked()
                                {
                                    self.ui_confirm = Some(UiConfirm::RemoveGroupPhoto {
                                        chat_id: chat.id,
                                        name: chat.title.clone(),
                                    });
                                }
                            });
                            ui.add_space(SP_2);
                            ui.label(RichText::new(crate::i18n::t("profile.name_label")).size(12.0).color(rgb(t.text_secondary)));
                            text_field(
                                ui,
                                t,
                                &mut self.group_subject_edit,
                                "",
                                ui.available_width(),
                            );
                            ui.label(
                                RichText::new(crate::i18n::t("profile.description_label"))
                                    .size(12.0)
                                    .color(rgb(t.text_secondary)),
                            );
                            text_field(ui, t, &mut self.group_desc_edit, "", ui.available_width());
                            ui.label(
                                RichText::new(
                                    "Leave the description empty to keep the current one.",
                                )
                                .size(10.0)
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
                                    .size(12.0)
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
                            // Share recent messages with new
                            // members (row 64): the switch arms
                            // the add above; the records ride
                            // the add command itself.
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 28.0),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let mut on = self.groups.share_history;
                                    crate::widgets::switch(ui, &mut on, t);
                                    if on != self.groups.share_history {
                                        self.groups.share_history = on;
                                    }
                                    ui.label(
                                        RichText::new(crate::i18n::t("profile.share_history"))
                                            .size(12.0)
                                            .color(rgb(t.text_primary)),
                                    );
                                },
                            );
                            ui.label(
                                RichText::new(crate::i18n::t("profile.share_history_caption"))
                                    .size(10.0)
                                    .color(rgb(t.text_faint)),
                            );
                            if self.groups.history_retry_chat == Some(chat.id) {
                                ui.add_space(SP_2);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(crate::i18n::t("profile.history_failed"))
                                            .size(12.0)
                                            .color(rgb(t.text_secondary)),
                                    );
                                    if button(
                                        ui,
                                        t,
                                        &crate::i18n::t("profile.history_retry"),
                                        Btn::Secondary,
                                    )
                                    .clicked()
                                    {
                                        if let Some(jid) = self.live_jid(chat.id) {
                                            self.send_backend(
                                                backend::BackendCommand::RetryGroupHistoryShare {
                                                    chat_jid: jid,
                                                },
                                            );
                                        }
                                    }
                                });
                            }
                            ui.add_space(SP_3);
                            // Invite link: fetched on demand;
                            // copy and reset act on the link the
                            // backend last reported for this chat.
                            ui.label(
                                RichText::new(crate::i18n::t("profile.invite_link"))
                                    .size(12.0)
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
                                        .size(10.0)
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
                            ui.add_space(SP_3);
                            // Your member label in this group
                            // (row 65): the label other members
                            // see on your roster row. The draft
                            // seeds from the label the backend
                            // last reported for this group;
                            // saving an empty field clears it.
                            if self.groups.member_label_draft_chat != Some(chat.id) {
                                self.groups.member_label_draft = self
                                    .groups
                                    .own_member_labels
                                    .get(&chat.id)
                                    .cloned()
                                    .unwrap_or_default();
                                self.groups.member_label_draft_chat = Some(chat.id);
                            }
                            ui.label(
                                RichText::new(crate::i18n::t("profile.member_label"))
                                    .size(12.0)
                                    .color(rgb(t.text_secondary)),
                            );
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 40.0),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let pending =
                                        self.groups.member_label_pending == Some(chat.id);
                                    let unchanged = self.groups.member_label_draft.trim()
                                        == self
                                            .groups
                                            .own_member_labels
                                            .get(&chat.id)
                                            .map_or("", String::as_str);
                                    ui.add_enabled_ui(!pending && !unchanged, |ui| {
                                        if button(
                                            ui,
                                            t,
                                            &crate::i18n::t("profile.member_label_save"),
                                            Btn::Secondary,
                                        )
                                        .clicked()
                                        {
                                            self.set_own_member_label(chat.id);
                                        }
                                    });
                                    let w = ui.available_width();
                                    text_field(
                                        ui,
                                        t,
                                        &mut self.groups.member_label_draft,
                                        &crate::i18n::t("profile.member_label_hint"),
                                        w,
                                    );
                                },
                            );
                            });
                            // Reported messages (row 65): an
                            // admin-only section. The fetch runs
                            // once per panel session; the rows
                            // are what the backend answered, with
                            // the archived body when this device
                            // holds the message.
                            if self.groups.own_admin.get(&chat.id).copied().unwrap_or(false) {
                                if !self.groups.reported_requested.contains(&chat.id) {
                                    self.groups.reported_requested.insert(chat.id);
                                    if let Some(jid) = self.live_jid(chat.id) {
                                        self.send_backend(
                                            backend::BackendCommand::FetchReportedMessages {
                                                chat_jid: jid,
                                            },
                                        );
                                    }
                                }
                                let reported_rows = self
                                    .groups
                                    .reported
                                    .as_ref()
                                    .filter(|(id, _)| *id == chat.id)
                                    .map(|(_, rows)| rows.clone());
                                let reported_error = self
                                    .groups
                                    .reported_error
                                    .as_ref()
                                    .filter(|(id, _)| *id == chat.id)
                                    .map(|(_, message)| message.clone());
                                dock_card_section(ui, t, &crate::i18n::t("profile.reported_messages"), None, |ui| {
                                    if let Some(message) = &reported_error {
                                        ui.label(
                                            RichText::new(message.clone())
                                                .size(12.0)
                                                .color(rgb(t.text_secondary)),
                                        );
                                        if button(
                                            ui,
                                            t,
                                            &crate::i18n::t("profile.reported_retry"),
                                            Btn::Secondary,
                                        )
                                        .clicked()
                                        {
                                            self.groups.reported_error = None;
                                            self.groups.reported_requested.remove(&chat.id);
                                        }
                                    } else if let Some(rows) = &reported_rows {
                                        if rows.is_empty() {
                                            ui.label(
                                                RichText::new(crate::i18n::t("profile.reported_empty"))
                                                    .size(12.0)
                                                    .color(rgb(t.text_faint)),
                                            );
                                        }
                                        for row in rows {
                                            match &row.snippet {
                                                Some((sender, body)) => {
                                                    ui.label(
                                                        RichText::new(format!("{sender}: {body}"))
                                                            .size(12.0)
                                                            .color(rgb(t.text_primary)),
                                                    );
                                                }
                                                None => {
                                                    ui.label(
                                                        RichText::new(crate::i18n::t(
                                                            "profile.reported_not_on_device",
                                                        ))
                                                        .size(12.0)
                                                        .color(rgb(t.text_secondary)),
                                                    );
                                                }
                                            }
                                            if !row.reporters.is_empty() {
                                                ui.label(
                                                    RichText::new(crate::i18n::tf(
                                                        "profile.reported_by",
                                                        "names",
                                                        &row.reporters.join(", "),
                                                    ))
                                                    .size(10.0)
                                                    .color(rgb(t.text_faint)),
                                                );
                                            }
                                            ui.add_space(SP_1);
                                        }
                                    } else {
                                        ui.label(
                                            RichText::new(crate::i18n::t("profile.reported_loading"))
                                                .size(12.0)
                                                .color(rgb(t.text_faint)),
                                        );
                                    }
                                });
                            }
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
                                                .size(10.0)
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
                                    .size(12.0)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                    }

                    // Members (live list wins over demo names).
                    // Each row is one clean unit; every per-member
                    // action lives in the row's kebab menu.
                    if chat.kind == ChatKind::Group {
                        if let Some(live_list) = &live_members {
                            // The painted list is capped (P2-66):
                            // a group can exceed 1,000 members,
                            // and painting every row (plus a
                            // per-member avatar lookup) every
                            // frame degrades the whole panel. The
                            // tail states the remainder honestly,
                            // the demo fallback's own pattern.
                            const MEMBER_LIST_CAP: usize = 200;
                            let shown = live_list.len().min(MEMBER_LIST_CAP);
                            let member_photos: Vec<Option<egui::TextureId>> = live_list
                                .iter()
                                .take(shown)
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
                                        live_list.iter().take(shown).zip(member_photos.iter())
                                    {
                                        let Some(kebab) = dock_member_row_labeled(
                                            ui,
                                            t,
                                            &row.label,
                                            row.detail.as_deref(),
                                            row.admin,
                                            true,
                                            *photo,
                                            row.member_label.as_deref(),
                                        ) else {
                                            continue;
                                        };
                                        egui::Popup::menu(&kebab)
                                            .frame(crate::theme::glass_panel_frame(t, crate::theme::RADIUS_PANEL))
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
                                                ui.add_space(SP_1);
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
                                                ui.add_space(SP_1);
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
                                    if live_list.len() > shown {
                                        ui.label(
                                            RichText::new(format!(
                                                "… and {} more",
                                                live_list.len() - shown
                                            ))
                                            .size(12.0)
                                            .color(rgb(t.text_faint)),
                                        );
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
                                        .size(12.0)
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

                    profile_counts_section(ui, t, media_count, link_count, doc_count, &mut intents);
                }
            }
        });

        if exiting {
            // The exit run only replays the panel; every intent
            // gathered during it is discarded.
            return;
        }
        if gallery_actions.back {
            self.gallery_section = None;
        }
        if let Some(section) = gallery_actions.section {
            self.gallery_section = Some(section);
        }
        if let Some(id) = gallery_actions.view_photo {
            self.media.viewing_photo = Some(id);
        }
        if let Some(id) = gallery_actions.locate {
            self.scroll_to_message = Some(id);
            self.highlight_message = Some(id);
            self.highlight_started = None;
        }
        for id in gallery_actions.fetch_photos {
            self.ensure_photo_fetch(id);
        }
        for id in gallery_actions.fetch_stickers {
            self.ensure_sticker_fetch(id);
        }
        if intents.toggle_block {
            self.toggle_block_contact(chat.id);
        }
        if intents.report {
            self.report_chat(chat.id);
        }
        if let Some(secs) = intents.ephemeral_pick {
            self.set_disappearing_timer(chat.id, secs);
        }
        if intents.export {
            export = true;
        }
        if let Some(section) = intents.gallery {
            self.gallery_section = Some(section);
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
        if group_photo_pick {
            // The pick itself: a blocking OS dialog, then the
            // own-profile preparation (square 640 JPEG), then
            // the command. A file that cannot be read or is not
            // an image is an honest error before anything is
            // sent, in the preparation's own words.
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                .pick_file()
            {
                match std::fs::read(&path) {
                    Ok(bytes) => match super::my_profile::prepare_profile_jpeg(&bytes) {
                        Ok(jpeg) => self.set_group_picture(chat.id, Some(jpeg)),
                        Err(e) => self.notice = Some(Notice::error(e)),
                    },
                    Err(e) => {
                        self.notice = Some(Notice::error(format!("Could not read that file: {e}")));
                    }
                }
            }
        }
        if intents.focus_search {
            // The dock's Search action promises search inside
            // this conversation (its placement beside the call
            // action says so): open the in-chat search strip for
            // the chat the dock describes, with focus armed, and
            // close the dock back onto the conversation. (Before
            // this fix the action focused the chat-list search
            // instead, which searches other chats entirely.)
            self.chat_search.open = true;
            self.chat_search.chat_id = self.selected_chat;
            self.chat_search.focus_armed = true;
            close = true;
        }
        if let Some(text) = intents.suggestion {
            self.composer.draft = text;
            close = true;
        }
        if intents.start_call {
            self.start_call_from_header();
        }
        for cmd in dock_commands {
            self.send_backend(cmd);
        }
        if close || intents.close {
            self.profile_open = false;
            self.business.catalog_open_for = None;
            self.gallery_section = None;
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
        let previous = self.ephemeral_secs_of(chat_id);
        if previous == duration_secs {
            return;
        }
        if self.live.is_some() {
            self.pending_timers.entry(chat_id).or_insert(previous);
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
    /// backend (live groups only). Nothing is claimed at click
    /// time: the title keeps its previous value and no notice
    /// speaks until the backend answers, GroupInfoSaved
    /// applying the new title with the saved notice and
    /// GroupInfoFailed speaking the failure. A title asserted
    /// before its outcome existed was a fake success.
    pub(crate) fn save_group_info(&mut self, chat_id: u64) {
        let Some(jid) = self.live_jid(chat_id) else {
            return;
        };
        let subject = self.group_subject_edit.trim().to_owned();
        let description = self.group_desc_edit.trim().to_owned();
        let previous_title = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .map(|c| c.title.clone())
            .unwrap_or_default();
        let new_subject = if subject.is_empty() { None } else { Some(subject) };
        self.pending_group_info.entry(chat_id).or_insert((previous_title, new_subject.clone()));
        self.send_backend(backend::BackendCommand::SetGroupInfo {
            chat_jid: jid,
            subject: new_subject,
            // An empty field keeps the current description; the
            // form never clears it implicitly (data-loss guard).
            description: if description.is_empty() { None } else { Some(description) },
        });
    }

    /// Sets or removes a group's profile picture through the
    /// backend (design standard section 23). `image` is the
    /// prepared square JPEG (the own-profile preparation), or
    /// None to remove. The write can outlast a breath, so a
    /// Busy notice starts it; the outcome events replace it:
    /// GroupPictureChanged refreshes the picture through the
    /// avatar pipeline, GroupPictureFailed surfaces the
    /// refusal or the transport failure in the backend's words.
    pub(crate) fn set_group_picture(&mut self, chat_id: u64, image: Option<Vec<u8>>) {
        let Some(jid) = self.live_jid(chat_id) else {
            return;
        };
        let removing = image.is_none();
        self.send_backend(backend::BackendCommand::SetGroupPicture { chat_jid: jid, image });
        self.notice = Some(Notice::busy(crate::i18n::t(if removing {
            "profile.group_picture_removing"
        } else {
            "profile.group_picture_updating"
        })));
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
        let is_add = matches!(op, backend::ParticipantOp::Add);
        if is_add && self.groups.share_history {
            // Add with history (row 64): the records come from
            // this device's archive, and the outcome arrives
            // as a GroupHistoryReported event (the refreshed
            // member list still lands through the fetch below).
            let history = self.history_records(&jid);
            self.send_backend(backend::BackendCommand::AddGroupParticipantsWithHistory {
                chat_jid: jid.clone(),
                participants: vec![target.clone()],
                history,
            });
            self.groups.participant_add_awaiting = Some((chat_id, target));
            self.groups.live_members.remove(&chat_id);
            self.send_backend(backend::BackendCommand::FetchGroupMembers { chat_jid: jid });
            self.notice = Some(Notice::busy(crate::i18n::t("notice.updating_members")));
            return;
        }
        self.send_backend(backend::BackendCommand::GroupParticipants {
            chat_jid: jid.clone(),
            participants: vec![target.clone()],
            op,
        });
        if is_add {
            // The input clears when the refreshed member list
            // lands, not now (P2-37): at send time the outcome
            // is unknown, and clearing here would eat the typed
            // address of a failed add.
            self.groups.participant_add_awaiting = Some((chat_id, target));
        }
        // Refresh the member list so the panel shows the result.
        self.groups.live_members.remove(&chat_id);
        self.send_backend(backend::BackendCommand::FetchGroupMembers { chat_jid: jid });
        self.notice = Some(Notice::busy(crate::i18n::t("notice.updating_members")));
    }

    /// The shareable history records for a group, from this
    /// device's archive (row 64): plain-text messages only,
    /// the newest 100, oldest first, in the exact shapes the
    /// backend contract carries. The library applies its own
    /// eligibility rules on top; this only bounds and shapes
    /// what the archive can offer.
    fn history_records(&self, chat_jid: &str) -> Vec<backend::HistoryMessageRecord> {
        let Some(store) = self.live.as_ref().and_then(|session| session.store.as_ref()) else {
            return Vec::new();
        };
        let Ok(messages) = store.messages(chat_jid) else {
            return Vec::new();
        };
        let mut records: Vec<backend::HistoryMessageRecord> = messages
            .into_iter()
            .map(|(message, _)| message)
            .filter(|message| {
                matches!(message.kind, backend::LiveMessageKind::Text)
                    && !message.body.trim().is_empty()
            })
            .map(|message| backend::HistoryMessageRecord {
                id: message.id,
                author_jid: message.sender_jid,
                from_me: message.from_me,
                timestamp: message.timestamp,
                body: message.body,
                mentions: message.mentions,
                delivery: message.delivery,
            })
            .collect();
        let excess = records.len().saturating_sub(100);
        records.drain(..excess);
        records
    }

    /// Sends the member-label editor's draft for a group
    /// (row 65): the trimmed text, empty clearing the label.
    /// The outcome event speaks and clears the pending flag;
    /// nothing is applied locally before it lands.
    pub(crate) fn set_own_member_label(&mut self, chat_id: u64) {
        let Some(jid) = self.live_jid(chat_id) else {
            return;
        };
        let label = self.groups.member_label_draft.trim().to_owned();
        self.groups.member_label_pending = Some(chat_id);
        self.send_backend(backend::BackendCommand::SetOwnMemberLabel { chat_jid: jid, label });
    }

    /// Leaves a live group and archives the local row. The
    /// archive is optimistic: a LeaveGroupFailed restores the
    /// row's previous archived flag, since a failed leave means
    /// the account is still a member.
    pub(crate) fn leave_group(&mut self, chat_id: u64) {
        if let Some(jid) = self.live_jid(chat_id) {
            let previous = self.chats.iter().find(|c| c.id == chat_id).is_some_and(|c| c.archived);
            self.pending_leave.entry(chat_id).or_insert(previous);
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

    /// Reports a DM contact from the info dock's Report row.
    /// The report flow is the message menu's own: a
    /// confirmation modal naming the contact, then the
    /// ReportSpam command over a real message. The dock names
    /// no message, so the chat's latest incoming message with
    /// an archived protocol identity carries the report (the
    /// report sends the message with it, as the modal's text
    /// says). A chat with nothing reportable answers with the
    /// honest refusal instead of opening a modal that could
    /// only fail; until this routing existed, the dock's
    /// Report row blocked the contact, which its label never
    /// said.
    pub(crate) fn report_chat(&mut self, chat_id: u64) {
        if self.live.is_none() {
            self.notice = Some(Notice::error(crate::i18n::t("notice.report_needs_connection")));
            return;
        }
        let candidate = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&chat_id))
            .and_then(|msgs| msgs.iter().rev().find(|(_, m)| m.sender.is_some()).map(|(_, m)| m.id))
            .filter(|id| self.protocol_ids.contains_key(id));
        let Some(msg_id) = candidate else {
            self.notice = Some(Notice::error(crate::i18n::t("notice.report_unavailable")));
            return;
        };
        let name = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .map(|c| c.title.clone())
            .unwrap_or_else(|| "this contact".to_owned());
        self.ui_confirm = Some(crate::state::UiConfirm::ReportSpam { chat_id, msg_id, name });
    }

    /// Blocks/unblocks a DM contact: local state plus the backend
    /// command in live mode. The set flips at once, but in live
    /// mode the success is announced only when the server's
    /// Blocklist report confirms the toggle, and a BlockFailed
    /// restores the set: a "Contact blocked." spoken at click
    /// time was a claim the server had not yet agreed to. Demo
    /// mode has no server to wait for, so its notice stays
    /// immediate and local, like the demo's other truths.
    pub(crate) fn toggle_block_contact(&mut self, chat_id: u64) {
        let was_blocked = self.blocked_chats.contains(&chat_id);
        let now_blocked = if was_blocked {
            self.blocked_chats.remove(&chat_id);
            false
        } else {
            self.blocked_chats.insert(chat_id);
            true
        };
        let mut sent = false;
        if let Some(jid) = self.live_jid(chat_id) {
            self.pending_blocks.entry(chat_id).or_insert(was_blocked);
            self.send_backend(backend::BackendCommand::BlockContact {
                chat_jid: jid,
                block: now_blocked,
            });
            sent = true;
        }
        // The set is persisted (to the encrypted archive store,
        // privacy audit A6) so the panel still shows the right
        // state after a restart, before (or without) the
        // backend's connect-time blocklist fetch.
        self.persist_blocked_chats();
        if !sent {
            self.notice = Some(Notice::info(if now_blocked {
                "Contact blocked.".to_owned()
            } else {
                "Contact unblocked.".to_owned()
            }));
        }
    }
}
