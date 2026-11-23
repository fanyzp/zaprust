//! Rich-message UI: the per-message action implementations
//! (react, edit, delete, forward, star, pin) and the shared
//! popup helpers live here; the pickers are in
//! `pickers`, the menus and dialogs in `menus`, and polls in
//! `poll`. Everything here drives real state; demo and live
//! share the same code paths wherever the backend supports them.

pub(crate) mod emoji;
mod menus;
mod pickers;
mod poll;

use backend::BackendCommand;
use core::model::{Message, MessageKind};
use eframe::egui::{self, Align2, CornerRadius, Margin, RichText, Vec2};

use crate::state::{App, Notice, UiConfirm};
use crate::theme::rgb;

/// The six quick reactions offered by the picker (WhatsApp's set).
pub(crate) const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

/// Runs a picker tile add inside a scope with per-state fills: a
/// resting panel-alt fill, one hover step and one pressed step,
/// the same derivation the shared chip uses. The stock tile
/// buttons otherwise gave no pointer feedback at all.
fn tile_add<R>(
    ui: &mut egui::Ui,
    t: core::theme::Tokens,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.widgets.inactive.bg_fill = rgb(t.bg_panel_alt);
        v.widgets.inactive.corner_radius = CornerRadius::same(8);
        v.widgets.hovered.bg_fill = rgb(core::theme::hover_step(t.bg_panel_alt));
        v.widgets.hovered.corner_radius = CornerRadius::same(8);
        v.widgets.active.bg_fill = rgb(core::theme::pressed_step(t.bg_panel_alt));
        v.widgets.active.corner_radius = CornerRadius::same(8);
        add(ui)
    })
    .inner
}

/// Shows a popup frame and paints its glass specular edge once the
/// frame's rect is known. A free function, not a method, so the
/// caller can pass `self.popup_frame()` (whose borrow of self ends
/// as soon as the frame is built) alongside a closure that borrows
/// self mutably.
fn show_popup<R>(
    frame: egui::Frame,
    _t: core::theme::Tokens,
    ui: &mut egui::Ui,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let inner = frame.show(ui, add);
    inner.inner
}

impl App {
    /// Toggles my reaction on a message (demo + live).
    pub(crate) fn apply_reaction(&mut self, msg_id: u64, emoji: &str) {
        let current = self.my_reactions.get(&msg_id).cloned();
        let mut new_reactions: Option<Vec<(String, u32)>> = None;
        let mut new_mine: Option<String> = None;
        if let Some(msg) = self.conversation.iter_mut().find(|m| m.id == msg_id) {
            let (reactions, mine) =
                core::rich::apply_my_reaction(&msg.reactions, current.as_deref(), emoji);
            msg.reactions = reactions.clone();
            new_reactions = Some(reactions);
            new_mine = mine;
        }
        match &new_mine {
            Some(e) => {
                self.my_reactions.insert(msg_id, e.clone());
            }
            None => {
                self.my_reactions.remove(&msg_id);
            }
        }
        if !self.live_mode {
            let chat_id = self.selected_chat;
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                if let Some(msg) = chat.messages.iter_mut().find(|m| m.id == msg_id) {
                    if let Some(r) = &new_reactions {
                        msg.reactions = r.clone();
                    }
                }
            }
        }
        if self.live_mode {
            let chat_id = self.selected_chat;
            let from_me = self
                .conversation
                .iter()
                .find(|m| m.id == msg_id)
                .is_some_and(|m| m.sender.is_none());
            if let (Some(jid), Some(proto)) = (
                self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()),
                self.protocol_ids.get(&msg_id).cloned(),
            ) {
                // A group reaction must name the message's
                // author: the library builds the target key's
                // participant from it and refuses a group target
                // authored by someone else without one. The
                // author is the JID recorded for the message
                // when it arrived (or seeded from the archive).
                // My own messages need none (the library resolves
                // my group identity itself), and neither do direct
                // chats, whose keys carry no participant.
                let sender_jid = if !from_me && jid.ends_with("@g.us") {
                    self.sender_jids.get(&msg_id).cloned()
                } else {
                    None
                };
                // Remember what my reaction was before this
                // change until the backend settles it: the echo
                // confirms, a ReactionFailed restores this value.
                // An unsettled earlier change keeps its older
                // snapshot.
                self.pending_reactions.entry(msg_id).or_insert_with(|| current.clone());
                // The optimistic write goes through the actor
                // map, the same representation the server's echo
                // reconciles into, so the two cannot drift apart
                // or count me twice.
                self.apply_my_reaction_to_actors(chat_id, msg_id, new_mine.as_deref());
                self.send_backend(BackendCommand::React {
                    chat_jid: jid,
                    message_id: proto,
                    sender_jid,
                    from_me,
                    emoji: new_mine.clone().unwrap_or_default(),
                });
            }
        }
        self.reaction_picker_id = None;
    }

    /// Starts editing one of my own text messages.
    pub(crate) fn start_edit(&mut self, msg_id: u64) {
        if let Some(msg) = self.conversation.iter().find(|m| m.id == msg_id) {
            self.composer.draft = msg.body.clone();
            self.composer.editing_id = Some(msg_id);
        }
    }

    /// Applies the composer edit to the message being edited.
    pub(crate) fn apply_edit(&mut self, msg_id: u64, new_body: &str) {
        // The edit target must live in the OPEN conversation.
        // The composer takes the target from whichever chat the
        // edit started in; without this check the live branch
        // below would pair the open chat's JID with another
        // chat's protocol id and edit a message in the wrong
        // conversation. A target that is not here (its chat was
        // left, or it was revoked meanwhile) refuses silently:
        // the composer has already consumed the edit state, so
        // nothing stale survives either.
        if !self.in_open_conversation(msg_id) {
            return;
        }
        // The pre-edit state, captured before the local apply:
        // an EditMessageFailed restores it verbatim.
        let previous =
            self.conversation.iter().find(|m| m.id == msg_id).map(|m| (m.body.clone(), m.edited));
        for msg in &mut self.conversation {
            if msg.id == msg_id {
                msg.body = new_body.to_owned();
                msg.edited = true;
            }
        }
        if !self.live_mode {
            let chat_id = self.selected_chat;
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                for msg in &mut chat.messages {
                    if msg.id == msg_id {
                        msg.body = new_body.to_owned();
                        msg.edited = true;
                    }
                }
            }
        }
        if self.live_mode {
            let chat_id = self.selected_chat;
            if let (Some(jid), Some(proto)) = (
                self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()),
                self.protocol_ids.get(&msg_id).cloned(),
            ) {
                // The backend edits the message for real; the
                // local update above is the immediate view and the
                // archive follows the backend's edit event.
                if let Some(previous) = previous {
                    self.pending_edits.entry(msg_id).or_insert(previous);
                }
                self.send_backend(BackendCommand::EditMessage {
                    chat_jid: jid,
                    message_id: proto,
                    new_body: new_body.to_owned(),
                });
            }
        }
    }

    /// Deletes one of my messages for everyone (local state +
    /// backend command; the backend's library limitation is
    /// surfaced as an event, not hidden).
    pub(crate) fn delete_for_everyone(&mut self, msg_id: u64) {
        // The message's author, for the revoke target: the
        // backend branches on it (my own message revokes as the
        // sender; another member's group message revokes as an
        // admin delete, design standard section 24). The menu
        // only offers the action where the role allows it; the
        // plumbing here just names the author honestly, the
        // same rule star and pin follow.
        let msg = self.conversation.iter().find(|m| m.id == msg_id).cloned();
        for msg in &mut self.conversation {
            if msg.id == msg_id {
                msg.deleted = true;
            }
        }
        if !self.live_mode {
            let chat_id = self.selected_chat;
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                for msg in &mut chat.messages {
                    if msg.id == msg_id {
                        msg.deleted = true;
                    }
                }
            }
        }
        if self.live_mode {
            let chat_id = self.selected_chat;
            if let (Some(jid), Some(proto)) = (
                self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()),
                self.protocol_ids.get(&msg_id).cloned(),
            ) {
                let from_me = msg.as_ref().is_some_and(|m| m.sender.is_none());
                let sender_jid = if !from_me && jid.ends_with("@g.us") {
                    self.sender_jids.get(&msg_id).cloned()
                } else {
                    None
                };
                // The pre-revoke deleted flag (the clone above
                // predates the local apply): a
                // RevokeMessageFailed restores it.
                self.pending_revokes
                    .entry(msg_id)
                    .or_insert(msg.as_ref().is_some_and(|m| m.deleted));
                self.send_backend(BackendCommand::RevokeMessage {
                    chat_jid: jid,
                    message_id: proto,
                    sender_jid,
                    from_me,
                });
            }
        }
    }

    /// Deletes a message for me only (design standard section
    /// 24): it leaves this account's chats on every device and
    /// stays untouched for everyone else. The local apply runs
    /// through the same adoption an inbound delete-for-me
    /// takes; the command follows. In demo mode the removal is
    /// local only, like the demo's other message actions.
    pub(crate) fn delete_for_me(&mut self, msg_id: u64) {
        let chat_id = self.selected_chat;
        let msg = self.conversation.iter().find(|m| m.id == msg_id).cloned();
        if !self.live_mode {
            // Demo mode: the removal is local only, out of the
            // rendered conversation and the stored copy alike.
            self.conversation.retain(|m| m.id != msg_id);
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.messages.retain(|m| m.id != msg_id);
            }
            if let Some(detached) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
                detached.messages.retain(|m| m.id != msg_id);
            }
            return;
        }
        // The command's plumbing is captured BEFORE the local
        // apply: the adoption removes the archive row and forgets
        // the message's helper entries (protocol id, author), so
        // reading them afterwards would find nothing and the
        // command would never be sent.
        let plumbing =
            self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()).and_then(|jid| {
                let proto = self.protocol_ids.get(&msg_id).cloned()?;
                let from_me = msg.as_ref().is_some_and(|m| m.sender.is_none());
                let sender_jid = if !from_me && jid.ends_with("@g.us") {
                    self.sender_jids.get(&msg_id).cloned()
                } else {
                    None
                };
                // The message's archive row, read once for both
                // of its uses: the wire timestamp the facade
                // stamps the app-state patch with, and the
                // rollback snapshot a DeleteForMeFailed restores
                // (the local apply below deletes this very row,
                // so it must be captured first).
                let archived = self.live.as_ref().and_then(|s| {
                    let store = s.store.as_ref()?;
                    let rows = store.messages(&jid).ok()?;
                    rows.iter().find(|(m, _)| m.id == proto).map(|(m, f)| (m.clone(), *f))
                });
                let message_timestamp = archived.as_ref().map(|(m, _)| m.timestamp);
                Some((jid, proto, sender_jid, from_me, message_timestamp, archived))
            });
        if let Some((jid, proto, sender_jid, from_me, message_timestamp, archived)) = plumbing {
            if let Some((row, flags)) = archived {
                self.pending_delete_for_me.entry(msg_id).or_insert((chat_id, row, flags));
            }
            self.apply_message_deleted_for_me(chat_id, &jid, msg_id, &proto);
            self.send_backend(BackendCommand::DeleteMessageForMe {
                chat_jid: jid,
                message_id: proto,
                sender_jid,
                from_me,
                delete_media: true,
                message_timestamp,
            });
        }
    }

    /// Keeps (or stops keeping) a disappearing message in the
    /// chat (design standard section 24), following the star
    /// toggle's shape: optimistic apply, a pending snapshot a
    /// KeepFailed restores, and the flag write-through that
    /// persists the state. Demo mode mirrors locally.
    pub(crate) fn toggle_keep_message(&mut self, msg_id: u64) {
        let Some(pos) = self.conversation.iter().position(|m| m.id == msg_id) else {
            return;
        };
        let msg = self.conversation[pos].clone();
        let on = !msg.kept;
        self.conversation[pos].kept = on;
        if !self.live_mode {
            let chat_id = self.selected_chat;
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                if let Some(m) = chat.messages.iter_mut().find(|m| m.id == msg_id) {
                    m.kept = on;
                }
            }
        }
        if let Some(msgs) = self.live.as_mut().and_then(|s| s.messages.get_mut(&self.selected_chat))
        {
            for (_, m) in msgs.iter_mut() {
                if m.id == msg_id {
                    m.kept = on;
                }
            }
        }
        if self.live.is_some() {
            self.set_live_flag(self.selected_chat, msg_id, store::FLAG_KEPT, on);
            if let (Some(jid), Some(pid)) =
                (self.live_jid(self.selected_chat), self.protocol_ids.get(&msg_id).cloned())
            {
                let from_me = msg.sender.is_none();
                let sender_jid = if !from_me && jid.ends_with("@g.us") {
                    self.sender_jids.get(&msg_id).cloned()
                } else {
                    None
                };
                self.pending_keeps.entry(msg_id).or_insert(msg.kept);
                self.send_backend(backend::BackendCommand::KeepMessage {
                    chat_jid: jid,
                    message_id: pid,
                    sender_jid,
                    from_me,
                    keep: on,
                });
            }
        }
    }

    /// Sends the spam report the confirmation modal approved
    /// (design standard section 24). The report names the
    /// message by its archived timestamp; a message the archive
    /// cannot timestamp is refused with an honest notice rather
    /// than reported with an invented one. The outcome arrives
    /// as SpamReported or SpamReportFailed.
    pub(crate) fn report_spam(&mut self, chat_id: u64, msg_id: u64) {
        let Some(jid) = self.live_jid(chat_id) else {
            self.notice = Some(Notice::error(crate::i18n::t("notice.report_needs_connection")));
            return;
        };
        let Some(pid) = self.protocol_ids.get(&msg_id).cloned() else {
            // The same honest refusal as the un-timestampable
            // case below: a report that cannot name its message
            // is an error the user hears about, never a silent
            // no-op behind a confirmed modal.
            self.notice = Some(Notice::error(crate::i18n::t("notice.report_unavailable")));
            return;
        };
        let timestamp = self.live.as_ref().and_then(|s| {
            let store = s.store.as_ref()?;
            let rows = store.messages(&jid).ok()?;
            rows.iter().find(|(m, _)| m.id == pid).map(|(m, _)| m.timestamp)
        });
        let Some(message_timestamp) = timestamp else {
            self.notice = Some(Notice::error(crate::i18n::t("notice.report_unavailable")));
            return;
        };
        let sender_jid =
            if jid.ends_with("@g.us") { self.sender_jids.get(&msg_id).cloned() } else { None };
        self.send_backend(backend::BackendCommand::ReportSpam {
            chat_jid: jid,
            message_id: pid,
            sender_jid,
            message_timestamp,
        });
    }

    /// The under-bubble strip: the flag lines (forwarded, pinned)
    /// and the small flag chip (retained after delete).
    /// Rendered by the conversation under each
    /// bubble; returns nothing but draws into `ui`. Starred state
    /// is deliberately not a strip line: it lives in the bubble's
    /// own meta row (the small star beside the time) and in the
    /// message menu, so a permanent pill under every starred
    /// message would read as a debug annotation, not product UI.
    pub(crate) fn draw_message_strip(&self, ui: &mut egui::Ui, msg: &Message) {
        let t = self.tokens;
        let mine = msg.sender.is_none();
        // Flag chips carry an icon-font glyph beside the word,
        // never an emoji: the strip is permanent chrome, and
        // chrome speaks the one icon language.
        let mut texts: Vec<(crate::icons::Icon, &'static str)> = Vec::new();
        if msg.forwarded {
            texts.push((crate::icons::Icon::Forward, "Forwarded"));
        }
        if msg.pinned {
            texts.push((crate::icons::Icon::Pin, "Pinned"));
        }
        if msg.kept {
            texts.push((crate::icons::Icon::Bookmark, "Kept"));
        }
        if texts.is_empty() && !msg.deleted_by_sender {
            return;
        }
        ui.with_layout(
            if mine {
                egui::Layout::right_to_left(egui::Align::Min)
            } else {
                egui::Layout::left_to_right(egui::Align::Min)
            },
            |ui| {
                ui.vertical(|ui| {
                    for (icon, text) in texts {
                        egui::Frame::new()
                            .fill(rgb(t.bg_panel_alt))
                            .corner_radius(CornerRadius::same(6))
                            .inner_margin(Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::Vec2::splat(crate::icons::SIZE_BADGE),
                                        egui::Sense::hover(),
                                    );
                                    crate::icons::draw(
                                        ui.painter(),
                                        icon,
                                        rect.center(),
                                        crate::icons::SIZE_BADGE,
                                        rgb(t.text_secondary),
                                        rgb(t.bg_panel_alt),
                                    );
                                    ui.label(
                                        RichText::new(text).size(12.0).color(rgb(t.text_secondary)),
                                    );
                                });
                            });
                    }
                    ui.horizontal(|ui| {
                        if msg.deleted_by_sender {
                            chip_label(
                                ui,
                                crate::icons::Icon::Delete,
                                "deleted by sender, retained locally",
                                t.danger,
                                t,
                            );
                        }
                    });
                });
            },
        );
    }

    /// All conversation popups: context menu, reaction picker,
    /// message info, poll builder, rich menu, picker shell, and
    /// the quote chip.
    pub(crate) fn draw_conversation_popups(&mut self, ctx: &egui::Context) {
        self.draw_create_group_dialog(ctx);
        self.draw_join_group_dialog(ctx);
        self.draw_new_chat_dialog(ctx);
        self.draw_ui_confirm(ctx);
        self.draw_context_menu(ctx);
        self.draw_forward_picker(ctx);
        self.draw_reaction_picker(ctx);
        self.draw_message_info(ctx);
        self.draw_rich_menu(ctx);
        self.draw_poll_builder(ctx);
        self.draw_location_dialog(ctx);
        self.draw_contact_picker(ctx);
        self.draw_picker(ctx);
    }

    /// The attach-menu location form: place name plus editable
    /// coordinates, centered like the poll builder it sits next to.
    fn draw_location_dialog(&mut self, ctx: &egui::Context) {
        let open = self.composer.location_draft.is_some();
        let Some(p) = crate::anim::popup_phase(ctx, "location_dialog", open, self.motion) else {
            return;
        };
        let t = self.tokens;
        egui::Area::new(egui::Id::unique("location_dialog"))
            .anchor(Align2::CENTER_CENTER, Vec2::new(0.0, -40.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if p < 0.999 {
                    ui.set_opacity(p);
                }
                show_popup(self.popup_frame(), t, ui, |ui| {
                    ui.set_width(280.0);
                    let Some(draft) = self.composer.location_draft.as_mut() else { return };
                    ui.label(
                        RichText::new(crate::i18n::t("attach.location"))
                            .font(crate::theme::semibold(15.0)),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(crate::i18n::t("attach.location_name"))
                            .font(crate::theme::medium(12.0)),
                    );
                    crate::widgets::text_field(
                        ui,
                        t,
                        &mut draft.name,
                        &crate::i18n::t("attach.location_name_hint"),
                        ui.available_width(),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let half = (ui.available_width() - 12.0) / 2.0;
                        ui.vertical(|ui| {
                            ui.label(RichText::new(crate::i18n::t("attach.lat")).font(crate::theme::medium(12.0)));
                            crate::widgets::text_field(ui, t, &mut draft.latitude, "-6.2", half);
                        });
                        ui.vertical(|ui| {
                            ui.label(RichText::new(crate::i18n::t("attach.lon")).font(crate::theme::medium(12.0)));
                            crate::widgets::text_field(ui, t, &mut draft.longitude, "106.8", half);
                        });
                    });
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
                            self.composer.location_draft = None;
                        }
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::i18n::t("attach.location_send"),
                            crate::widgets::Btn::Primary,
                        )
                        .clicked()
                        {
                            self.send_location_draft();
                        }
                    });
                });
            });
    }

    /// The attach menu's contact picker (design standard
    /// section 24): the contacts this account's roster knows by
    /// phone number, one row each; picking one sends its card
    /// into the open chat. The roster is the app's own contact
    /// knowledge (direct chats with a phone-form address); a
    /// contact known only by a privacy address has no number
    /// to put on a card and is not listed, and an empty roster
    /// says so instead of showing a dead list.
    fn draw_contact_picker(&mut self, ctx: &egui::Context) {
        let Some(p) =
            crate::anim::popup_phase(ctx, "contact_picker", self.contact_picker_open, self.motion)
        else {
            return;
        };
        let exiting = !self.contact_picker_open;
        let t = self.tokens;
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.contact_picker_open = false;
        }
        let contacts: Vec<(String, String)> = self
            .chats
            .iter()
            .filter(|c| c.kind == core::model::ChatKind::Dm)
            .filter_map(|c| {
                let jid = self.live_jid(c.id)?;
                let user = jid.strip_suffix("@s.whatsapp.net")?;
                if user.chars().all(|ch| ch.is_ascii_digit())
                    && !crate::live::is_raw_identifier(&c.title)
                {
                    Some((c.title.clone(), user.to_owned()))
                } else {
                    None
                }
            })
            .collect();
        let mut picked: Option<(String, String)> = None;
        egui::Area::new(egui::Id::unique("contact_picker"))
            .anchor(Align2::CENTER_CENTER, Vec2::new(0.0, -40.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if p < 0.999 {
                    ui.set_opacity(p);
                }
                show_popup(self.popup_frame(), t, ui, |ui| {
                    ui.set_width(280.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("attach.contact"))
                                .font(crate::theme::semibold(15.0)),
                        );
                        ui.add_space(6.0);
                        if contacts.is_empty() {
                            ui.label(
                                RichText::new(crate::i18n::t("attach.contact_empty"))
                                    .size(12.0)
                                    .color(rgb(t.text_secondary)),
                            );
                        }
                        for (name, phone) in &contacts {
                            if crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Person),
                                name,
                                Some(&format!("+{phone}")),
                            )
                            .clicked()
                            {
                                picked = Some((name.clone(), phone.clone()));
                            }
                        }
                        ui.add_space(6.0);
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::i18n::t("common.cancel"),
                            crate::widgets::Btn::Secondary,
                        )
                        .clicked()
                        {
                            self.contact_picker_open = false;
                        }
                    });
                });
            });
        if let Some((name, phone)) = picked {
            self.contact_picker_open = false;
            let chat_id = self.selected_chat;
            self.send_contact_card(chat_id, &name, &phone);
        }
    }

    /// The house confirmation modal for destructive actions
    /// parked in `ui_confirm` (removing a group member, leaving
    /// a group, replacing a privacy exception list): a centered glass card over the unified dim,
    /// naming its target, the safe action first and the
    /// destructive action last. Escape or a click on the dim
    /// cancels; only the confirm button performs the action, so
    /// the click that asked (a dock row, a kebab menu item) can
    /// never be the click that fires it.
    pub(crate) fn draw_ui_confirm(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        // Snapshot the parked action while it exists, like the
        // other popups: the phase machine keeps drawing through
        // the exit after the decision clears the field.
        if let Some(confirm) = &self.ui_confirm {
            ctx.data_mut(|d| d.insert_temp(egui::Id::unique("ui_confirm_snap"), confirm.clone()));
        }
        let Some(p) =
            crate::anim::popup_phase(ctx, "ui_confirm", self.ui_confirm.is_some(), self.motion)
        else {
            return;
        };
        let exiting = self.ui_confirm.is_none();
        let Some(confirm) =
            ctx.data(|d| d.get_temp::<UiConfirm>(egui::Id::unique("ui_confirm_snap")))
        else {
            return;
        };
        let (title, body, confirm_label) = match &confirm {
            UiConfirm::RemoveMember { name, .. } => (
                crate::i18n::t("profile.remove_member_title"),
                crate::i18n::tf("profile.remove_member_body", "name", name),
                crate::i18n::t("common.remove"),
            ),
            UiConfirm::RemoveGroupPhoto { name, .. } => (
                crate::i18n::t("profile.remove_group_photo_title"),
                crate::i18n::tf("profile.remove_group_photo_body", "name", name),
                crate::i18n::t("common.remove"),
            ),
            UiConfirm::LeaveGroup { name, .. } => (
                crate::i18n::t("profile.leave_group"),
                crate::i18n::tf("profile.leave_group_body", "name", name),
                crate::i18n::t("profile.leave_group"),
            ),
            UiConfirm::ReplaceExceptionList { label, .. } => (
                crate::i18n::t("privacy.exceptions_title"),
                crate::i18n::tf("privacy.exceptions_body", "name", label),
                crate::i18n::t("privacy.exceptions_confirm"),
            ),
            UiConfirm::LogOut => (
                crate::i18n::t("account.log_out_title"),
                crate::i18n::t("account.log_out_body"),
                crate::i18n::t("account.log_out"),
            ),
            UiConfirm::StartFresh => (
                crate::i18n::t("connect.start_fresh_title"),
                crate::i18n::t("connect.start_fresh_body"),
                crate::i18n::t("connect.start_fresh"),
            ),
            UiConfirm::ClearChat { name, .. } => (
                crate::i18n::t("chat.clear_title"),
                crate::i18n::tf("chat.clear_body", "name", name),
                crate::i18n::t("chat.clear_confirm"),
            ),
            UiConfirm::DeleteChat { name, .. } => (
                crate::i18n::t("chat.delete_title"),
                crate::i18n::tf("chat.delete_body", "name", name),
                crate::i18n::t("common.delete"),
            ),
            UiConfirm::ReportSpam { name, .. } => (
                crate::i18n::t("chat.report_title"),
                crate::i18n::tf("chat.report_body", "name", name),
                crate::i18n::t("chat.report_confirm"),
            ),
            UiConfirm::DeleteStickerPack { name, .. } => (
                crate::i18n::t("stickers.delete_pack_title"),
                crate::i18n::tf("stickers.delete_pack_body", "name", name),
                crate::i18n::t("common.delete"),
            ),
        };
        let mut decision: Option<bool> = None;
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            decision = Some(false);
        }
        egui::Area::new(egui::Id::unique("ui_confirm_dim"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ui.ctx().content_rect();
                let resp = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter().rect_filled(screen, 0.0, crate::theme::modal_dim(p));
                if resp.clicked() && !exiting {
                    decision = Some(false);
                }
            });
        egui::Area::new(egui::Id::unique("ui_confirm"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if p < 0.999 {
                    ui.set_opacity(p);
                }
                // A destructive decision gets an opaque card:
                // the glass fill lets a bright surface behind
                // ghost through, and nothing behind a Remove or
                // Leave question may compete with its text.
                let frame = self.popup_frame().fill(rgb(core::theme::elevated_step(t.bg_panel)));
                show_popup(frame, t, ui, |ui| {
                    ui.set_width(320.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        ui.label(RichText::new(&title).size(15.0).strong());
                        ui.add_space(4.0);
                        ui.label(RichText::new(&body).size(12.0).color(rgb(t.text_secondary)));
                        if matches!(confirm, UiConfirm::ClearChat { .. }) {
                            // The keep-starred choice the wire
                            // supports (section 24), defaulting
                            // to keeping them: clearing is the
                            // destructive act, the stars are a
                            // separate decision the user makes
                            // here, not a default taken for them.
                            ui.add_space(6.0);
                            ui.checkbox(
                                &mut self.clear_keep_starred,
                                crate::i18n::t("chat.keep_starred"),
                            );
                        }
                        ui.add_space(10.0);
                        // Dialog convention: the safe action
                        // first (left), the destructive action
                        // last (right), never adjacent to the
                        // surface that asked for it.
                        ui.horizontal(|ui| {
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("common.cancel"),
                                crate::widgets::Btn::Secondary,
                            )
                            .clicked()
                            {
                                decision = Some(false);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if crate::widgets::button(
                                        ui,
                                        t,
                                        &confirm_label,
                                        crate::widgets::Btn::Danger,
                                    )
                                    .clicked()
                                    {
                                        decision = Some(true);
                                    }
                                },
                            );
                        });
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("ui_confirm"),
            ctx.content_rect().center(),
            p,
        );
        match decision {
            Some(true) => match self.ui_confirm.take() {
                Some(UiConfirm::RemoveMember { chat_id, address, .. }) => {
                    self.group_participant_op(
                        chat_id,
                        Some(address),
                        backend::ParticipantOp::Remove,
                    );
                }
                Some(UiConfirm::RemoveGroupPhoto { chat_id, .. }) => {
                    self.set_group_picture(chat_id, None);
                }
                Some(UiConfirm::LeaveGroup { chat_id, .. }) => {
                    self.leave_group(chat_id);
                    self.profile_open = false;
                    self.profile_form_for = None;
                    self.groups.members_requested.remove(&chat_id);
                }
                Some(UiConfirm::ReplaceExceptionList { category, .. }) => {
                    self.cycle_privacy(&category);
                }
                Some(UiConfirm::LogOut) => {
                    self.log_out();
                }
                Some(UiConfirm::StartFresh) => {
                    self.start_fresh(ctx);
                }
                Some(UiConfirm::ClearChat { chat_id, .. }) => {
                    let keep_starred = self.clear_keep_starred;
                    self.clear_chat(chat_id, keep_starred);
                }
                Some(UiConfirm::DeleteChat { chat_id, .. }) => {
                    self.delete_chat(chat_id);
                }
                Some(UiConfirm::ReportSpam { chat_id, msg_id, .. }) => {
                    self.report_spam(chat_id, msg_id);
                }
                Some(UiConfirm::DeleteStickerPack { dir, .. }) => {
                    // The result is spoken either way: a pack
                    // that failed to delete stays listed, and
                    // the user hears why instead of watching a
                    // button silently do nothing.
                    match stickers::delete_pack(&dir) {
                        Ok(()) => {
                            self.notice =
                                Some(Notice::info(crate::i18n::t("stickers.pack_deleted")));
                        }
                        Err(e) => {
                            self.notice = Some(Notice::error(e));
                        }
                    }
                }
                None => {}
            },
            Some(false) => {
                self.ui_confirm = None;
            }
            None => {}
        }
    }

    fn popup_frame(&self) -> egui::Frame {
        // Every conversation popup floats over the message
        // stream, so every one gets the stacked-panel glass
        // treatment: the near-opaque fill whose content behind
        // cannot bleed through (see theme::glass_panel_frame).
        crate::theme::glass_panel_frame(self.tokens, crate::theme::RADIUS_PANEL).inner_margin(Margin::symmetric(12, 10))
    }
}

fn chip_label(
    ui: &mut egui::Ui,
    icon: crate::icons::Icon,
    text: &str,
    color: core::theme::Rgb,
    t: core::theme::Tokens,
) {
    egui::Frame::new()
        .fill(rgb(t.bg_panel_alt))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(
                    egui::Vec2::splat(crate::icons::SIZE_BADGE),
                    egui::Sense::hover(),
                );
                crate::icons::draw(
                    ui.painter(),
                    icon,
                    rect.center(),
                    crate::icons::SIZE_BADGE,
                    rgb(color),
                    rgb(t.bg_panel_alt),
                );
                ui.label(RichText::new(text).size(crate::theme::TEXT_CAPTION).color(rgb(color)));
            });
        });
}

impl App {
    /// Forwards one message into another chat. Live mode relays
    /// through the backend (session-scoped); demo mode copies the
    /// message locally with the forwarded flag set.
    pub(crate) fn do_forward(&mut self, msg_id: u64, target_chat: u64) {
        let source_chat = self.selected_chat;
        let Some(msg) = self.conversation.iter().find(|m| m.id == msg_id).cloned() else {
            return;
        };
        // View-once media is never relayed, in either mode: the
        // sender marked it to be viewed once, in place. (The menu
        // also hides Forward for these; this is the backstop for
        // every other entry point, and the backend refuses too.)
        if msg.is_view_once() {
            self.notice = Some(Notice::info("View-once media cannot be forwarded.".to_owned()));
            return;
        }
        if self.live.is_some() {
            let (Some(from_jid), Some(to_jid), Some(pid)) = (
                self.live_jid(source_chat),
                self.live_jid(target_chat),
                self.protocol_ids.get(&msg_id).cloned(),
            ) else {
                self.notice = Some(Notice::info(
                    "That message cannot be forwarded in this session.".to_owned(),
                ));
                return;
            };
            self.send_backend(backend::BackendCommand::ForwardMessage {
                from_chat_jid: from_jid,
                message_id: pid,
                to_chat_jid: to_jid,
            });
            // The forward is in flight (P2-32): its upsert in
            // the target chat (or the failure event) is the
            // outcome; until then the Busy marker stands.
            self.notice = Some(Notice::busy(crate::i18n::t("notice.forwarding")));
            // No optimistic copy: the forwarded message appears when
            // the backend confirms it with its own upsert. A local
            // copy used a synthetic id the real message never
            // matched, so forwards showed twice and a failed forward
            // still claimed success (Round 3 finding).
            return;
        }
        // Demo: append a copy to the target chat's stored messages.
        let new_id = self
            .chats
            .iter()
            .flat_map(|c| c.messages.iter().map(|m| m.id))
            .chain(self.conversation.iter().map(|m| m.id))
            .max()
            .unwrap_or(0)
            + 1;
        let mut copy = msg.clone();
        copy.id = new_id;
        copy.sender = None;
        copy.forwarded = true;
        copy.delivery = Some(core::model::Delivery::Read);
        copy.reactions = Vec::new();
        if target_chat == source_chat {
            self.conversation.push(copy);
        } else if let Some(chat) = self.chats.iter_mut().find(|c| c.id == target_chat) {
            chat.messages.push(copy);
            chat.preview = msg.preview_text();
        }
        self.notice = Some(Notice::info("Message forwarded.".to_owned()));
    }

    /// Stars/unstars one message: local state + archive flag +
    /// backend command in live mode.
    pub(crate) fn toggle_star_message(&mut self, msg_id: u64) {
        let Some(pos) = self.conversation.iter().position(|m| m.id == msg_id) else {
            return;
        };
        let msg = self.conversation[pos].clone();
        let on = !msg.starred;
        self.conversation[pos].starred = on;
        // Demo mode: mirror into the store copy too, or switching
        // chats through a path that calls select_chat_now directly
        // (community home, info dock) reloads the conversation
        // from the store copy and silently reverts the star. The
        // listed chat wins when it has the message; otherwise the
        // detached announcement chat is the store copy.
        if !self.live_mode {
            let chat_id = self.selected_chat;
            let mut mirrored = false;
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                if let Some(m) = chat.messages.iter_mut().find(|m| m.id == msg_id) {
                    m.starred = on;
                    mirrored = true;
                }
            }
            if !mirrored {
                if let Some(detached) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
                    if let Some(m) = detached.messages.iter_mut().find(|m| m.id == msg_id) {
                        m.starred = on;
                    }
                }
            }
        }
        // Mirror into the live session copy too, or the next
        // sync_conversation rebuild un-flips the star (logic
        // review F15).
        if let Some(msgs) = self.live.as_mut().and_then(|s| s.messages.get_mut(&self.selected_chat))
        {
            for (_, m) in msgs.iter_mut() {
                if m.id == msg_id {
                    m.starred = on;
                }
            }
        }
        if self.live.is_some() {
            self.set_live_flag(self.selected_chat, msg_id, store::FLAG_STARRED, on);
            if let (Some(jid), Some(pid)) =
                (self.live_jid(self.selected_chat), self.protocol_ids.get(&msg_id).cloned())
            {
                // A group star must name the message's author:
                // the library builds the target key's participant
                // from it and refuses a group target authored by
                // someone else without one. The author is the JID
                // recorded for the message when it arrived (or
                // seeded from the archive). My own messages need
                // none (the library resolves my group identity
                // itself), and neither do direct chats, whose
                // keys carry no participant. The same rule
                // reactions follow.
                let from_me = msg.sender.is_none();
                let sender_jid = if !from_me && jid.ends_with("@g.us") {
                    self.sender_jids.get(&msg_id).cloned()
                } else {
                    None
                };
                // Remember the pre-change value until the
                // backend settles it: a server report confirms,
                // a StarFailed restores this value. An unsettled
                // earlier change keeps its older snapshot.
                self.pending_stars.entry(msg_id).or_insert(msg.starred);
                self.send_backend(backend::BackendCommand::StarMessage {
                    chat_jid: jid,
                    message_id: pid,
                    sender_jid,
                    from_me,
                    star: on,
                });
            }
        }
    }

    /// Pins/unpins one message: same wiring as star.
    pub(crate) fn toggle_pin_message(&mut self, msg_id: u64, duration: backend::PinDuration) {
        let Some(pos) = self.conversation.iter().position(|m| m.id == msg_id) else {
            return;
        };
        let msg = self.conversation[pos].clone();
        let on = !msg.pinned;
        self.conversation[pos].pinned = on;
        // Demo mode: mirror into the store copy too, the same
        // rule as toggle_star_message, or a direct select_chat_now
        // switch silently reverts the pin.
        if !self.live_mode {
            let chat_id = self.selected_chat;
            let mut mirrored = false;
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                if let Some(m) = chat.messages.iter_mut().find(|m| m.id == msg_id) {
                    m.pinned = on;
                    mirrored = true;
                }
            }
            if !mirrored {
                if let Some(detached) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
                    if let Some(m) = detached.messages.iter_mut().find(|m| m.id == msg_id) {
                        m.pinned = on;
                    }
                }
            }
        }
        // Mirror into the live session copy too (logic review F15).
        if let Some(msgs) = self.live.as_mut().and_then(|s| s.messages.get_mut(&self.selected_chat))
        {
            for (_, m) in msgs.iter_mut() {
                if m.id == msg_id {
                    m.pinned = on;
                }
            }
        }
        if self.live.is_some() {
            self.set_live_flag(self.selected_chat, msg_id, store::FLAG_PINNED_MESSAGE, on);
            if let (Some(jid), Some(pid)) =
                (self.live_jid(self.selected_chat), self.protocol_ids.get(&msg_id).cloned())
            {
                // The author-naming rule is the same as for
                // stars (and reactions): a group pin target
                // authored by someone else is refused without
                // its participant, my own messages and direct
                // chats need no sender.
                let from_me = msg.sender.is_none();
                let sender_jid = if !from_me && jid.ends_with("@g.us") {
                    self.sender_jids.get(&msg_id).cloned()
                } else {
                    None
                };
                // The pre-change snapshot a PinFailed restores;
                // an unsettled earlier change keeps its older
                // snapshot.
                self.pending_pins.entry(msg_id).or_insert(msg.pinned);
                self.send_backend(backend::BackendCommand::PinMessage {
                    chat_jid: jid,
                    message_id: pid,
                    sender_jid,
                    from_me,
                    pin: on,
                    duration,
                });
            }
        }
    }

    /// Asks the backend to download a media message's bytes
    /// (session-scoped; the reply arrives as a MediaSaved event).
    /// View-once media is refused here, before any command is
    /// sent: it is viewed inside the app only, never saved.
    pub(crate) fn download_media(&mut self, msg_id: u64) {
        if self.conversation.iter().find(|m| m.id == msg_id).is_some_and(|m| m.is_view_once()) {
            self.notice = Some(Notice::info("View-once media cannot be saved.".to_owned()));
            return;
        }
        if self.media.downloads.contains(&msg_id) {
            // Already in flight: the chip on the message is
            // spinning and the Busy notice is standing; a second
            // command would only double the work.
            return;
        }
        if let (Some(jid), Some(pid)) =
            (self.live_jid(self.selected_chat), self.protocol_ids.get(&msg_id).cloned())
        {
            self.send_backend(backend::BackendCommand::DownloadMedia {
                chat_jid: jid,
                message_id: pid,
                document_ref: self
                    .live
                    .as_ref()
                    .and_then(|s| s.document_refs.get(&msg_id).cloned()),
            });
            self.media.downloads.insert(msg_id);
            self.notice = Some(Notice::busy("Downloading…".to_owned()));
        } else {
            self.notice =
                Some(Notice::info("That media is not available in this session.".to_owned()));
        }
    }
}

impl App {
    /// Executes the create-group form. Returns false when
    /// validation fails; the dialog then stays open with the error
    /// shown inline instead of closing on a transient notice.
    pub(crate) fn do_create_group(&mut self) -> bool {
        let name = self.groups.create_name.trim().to_owned();
        if name.is_empty() {
            self.groups.create_error = Some(crate::i18n::t("groups.name_required").clone());
            return false;
        }
        self.groups.create_error = None;
        let participants: Vec<String> = self
            .groups
            .create_members
            .split(',')
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect();
        self.groups.create_name.clear();
        self.groups.create_members.clear();
        if self.live.is_some() {
            self.send_backend(backend::BackendCommand::CreateGroup {
                name: name.clone(),
                participants,
            });
            // Busy while the command is in flight (P2-31); the
            // answering group upsert replaces it with the Info
            // outcome, so the banner never reads "Creating…"
            // after the group exists.
            self.groups.create_pending = true;
            self.notice = Some(Notice::busy(format!("Creating group \"{name}\"…")));
            return true;
        }
        // Demo: a local group entry at the top of the list.
        let new_id = self.chats.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        let subtitle = if participants.is_empty() {
            "Group created in demo mode".to_owned()
        } else {
            participants.join(", ")
        };
        self.chats.insert(
            0,
            core::model::Chat {
                id: new_id,
                kind: core::model::ChatKind::Group,
                title: name.clone(),
                subtitle,
                preview: "Group created".to_owned(),
                preview_sender: None,
                preview_mine: false,
                preview_delivery: None,
                preview_media: None,
                time: "now".to_owned(),
                unread: 0,
                mentions: 0,
                pinned: false,
                muted: false,
                mute_until: 0,
                favorite: false,
                avatar_seed: (new_id % 251) as u8,
                verified: false,
                meta_ai: false,
                community: None,
                archived: false,
                // The account default timer from Settings >
                // Privacy applies to groups created here, the
                // same rule WhatsApp applies to new chats.
                ephemeral_secs: self.default_ephemeral_secs,
                messages: Vec::new(),
            },
        );
        self.notice = Some(Notice::info(format!("Group \"{name}\" created (demo).")));
        true
    }
}
