//! Events: answering an event, creating one from the builder,
//! and editing or canceling my own (P7, parity rows 47-50).

use super::*;

use crate::state::EventDraft;

/// The validated, wire-ready shape of the event form: parsed
/// from the draft's strict text fields at send time.
struct EventFields {
    name: String,
    description: String,
    start_time: i64,
    end_time: Option<i64>,
    location: Option<String>,
    join_link: Option<String>,
    scheduled_call: bool,
    extra_guests: bool,
}

/// Parses and validates one event draft. On failure the error
/// key is returned and the caller keeps the draft open with
/// the message inside the form; nothing is sent on a guess.
fn validate_event_draft(draft: &EventDraft) -> Result<EventFields, &'static str> {
    let name = draft.name.trim().to_owned();
    if name.is_empty() {
        return Err("event.error_name");
    }
    let Some(start_time) =
        core::rich::parse_event_datetime(draft.date.trim(), draft.start_time.trim())
    else {
        return Err("event.error_start");
    };
    let end_time = if draft.end_time.trim().is_empty() {
        None
    } else {
        let Some(end) = core::rich::parse_event_datetime(draft.date.trim(), draft.end_time.trim())
        else {
            return Err("event.error_start");
        };
        if end <= start_time {
            return Err("event.error_end");
        }
        Some(end)
    };
    let clean = |s: &str| {
        let s = s.trim();
        if s.is_empty() { None } else { Some(s.to_owned()) }
    };
    Ok(EventFields {
        name,
        description: draft.description.trim().to_owned(),
        start_time,
        end_time,
        location: clean(&draft.location),
        join_link: clean(&draft.link),
        scheduled_call: draft.scheduled_call,
        extra_guests: draft.extra_guests,
    })
}

/// Builds a core event kind from validated fields, carrying
/// the tallies and my response of `previous` (an edit never
/// resets the answers).
fn event_kind(fields: &EventFields, previous: Option<&MessageKind>) -> MessageKind {
    let (my_rsvp, going, maybe, not_going, canceled) = match previous {
        Some(MessageKind::Event { my_rsvp, going, maybe, not_going, canceled, .. }) => {
            (*my_rsvp, *going, *maybe, *not_going, *canceled)
        }
        _ => (None, 0, 0, 0, false),
    };
    MessageKind::Event {
        name: fields.name.clone(),
        description: fields.description.clone(),
        start_time: fields.start_time,
        end_time: fields.end_time.unwrap_or(0),
        location: fields.location.clone().unwrap_or_default(),
        join_link: fields.join_link.clone().unwrap_or_default(),
        scheduled_call: fields.scheduled_call,
        extra_guests: fields.extra_guests,
        canceled,
        my_rsvp,
        going,
        maybe,
        not_going,
    }
}

impl App {
    /// Answers an event (Going / Maybe / Not going), or updates
    /// my extra-guest count with a Going answer (demo + live).
    /// Optimistic with the poll vote's settle/rollback pair: my
    /// previous answer is parked in `pending_event_rsvps`, the
    /// echo settles it, a failure re-applies it.
    pub(crate) fn respond_event(
        &mut self,
        msg_id: u64,
        response: core::model::EventRsvp,
        guests: Option<i32>,
    ) {
        let chat_id = self.selected_chat;
        let previous = self.conversation.iter().find(|m| m.id == msg_id).and_then(|m| match &m.kind {
            MessageKind::Event { my_rsvp, .. } => Some(*my_rsvp),
            _ => None,
        });
        let Some(previous) = previous else {
            return;
        };
        // Optimistic apply on the open conversation copy: my
        // old answer leaves its bucket, the new one joins its
        // bucket (the full ballot-correct aggregation runs on
        // the mirror through apply_event_response in live mode).
        let apply_mine = |kind: &mut MessageKind, response: core::model::EventRsvp| {
            if let MessageKind::Event { going, maybe, not_going, my_rsvp, .. } = kind {
                let (mut g, mut m, mut n) = (*going, *maybe, *not_going);
                match *my_rsvp {
                    Some(core::model::EventRsvp::Going) => g = g.saturating_sub(1),
                    Some(core::model::EventRsvp::Maybe) => m = m.saturating_sub(1),
                    Some(core::model::EventRsvp::NotGoing) => n = n.saturating_sub(1),
                    None => {}
                }
                match response {
                    core::model::EventRsvp::Going => g += 1,
                    core::model::EventRsvp::Maybe => m += 1,
                    core::model::EventRsvp::NotGoing => n += 1,
                }
                *going = g;
                *maybe = m;
                *not_going = n;
                *my_rsvp = Some(response);
            }
        };
        if let Some(msg) = self.conversation.iter_mut().find(|m| m.id == msg_id) {
            apply_mine(&mut msg.kind, response);
        }
        if !self.live_mode {
            // Demo: mirror into the stored chat so the answer
            // survives a chat switch, like a poll vote.
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                if let Some(msg) = chat.messages.iter_mut().find(|m| m.id == msg_id) {
                    apply_mine(&mut msg.kind, response);
                }
            }
            return;
        }
        if let (Some(jid), Some(proto)) = (
            self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()),
            self.protocol_ids.get(&msg_id).cloned(),
        ) {
            self.pending_event_rsvps
                .entry(msg_id)
                .or_insert_with(|| previous.map(crate::live::rsvp_to_backend));
            let backend_response = crate::live::rsvp_to_backend(response);
            self.send_backend(BackendCommand::RespondEvent {
                chat_jid: jid.clone(),
                message_id: proto.clone(),
                response: backend_response,
                extra_guest_count: guests,
            });
            // Durable at once in the mirror and the archive;
            // the echo re-applies the identical answer and
            // changes nothing.
            self.apply_event_response(&jid, &proto, "", true, backend_response);
        }
    }

    /// Sends the event built in the composer builder.
    pub(crate) fn send_event_draft(&mut self) {
        let Some(draft) = self.composer.event_draft.take() else {
            return;
        };
        let fields = match validate_event_draft(&draft) {
            Ok(fields) => fields,
            Err(key) => {
                let mut draft = draft;
                draft.error = Some(crate::i18n::t(key));
                self.composer.event_draft = Some(draft);
                return;
            }
        };
        if self.live_mode {
            let chat_id = self.selected_chat;
            if let Some(jid) = self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()) {
                self.send_backend(BackendCommand::CreateEvent {
                    chat_jid: jid,
                    name: fields.name,
                    description: fields.description,
                    start_time: fields.start_time,
                    end_time: fields.end_time.unwrap_or(0),
                    location: fields.location.unwrap_or_default(),
                    join_link: fields.join_link.unwrap_or_default(),
                    scheduled_call: fields.scheduled_call,
                    extra_guests: fields.extra_guests,
                });
                // Busy while the create is in flight (P2-32);
                // the event's own upsert (or the failure event)
                // replaces it.
                self.notice = Some(Notice::busy(crate::i18n::t("notice.creating_event")));
            }
            return;
        }
        // Demo: a local event message, fully answerable.
        self.sent_counter += 1;
        let msg = Message {
            kind: event_kind(&fields, None),
            ..Message::mine(900 + self.sent_counter, fields.name.clone(), "19:36")
        };
        self.conversation.push(msg.clone());
        let chat_id = self.selected_chat;
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.messages.push(msg);
        }
    }

    /// Opens the edit form for one of my events, pre-filled
    /// from the event itself.
    pub(crate) fn open_event_edit(&mut self, msg_id: u64) {
        let chat_id = self.selected_chat;
        let Some(msg) = self.conversation.iter().find(|m| m.id == msg_id) else {
            return;
        };
        let MessageKind::Event {
            name,
            description,
            start_time,
            end_time,
            location,
            join_link,
            scheduled_call,
            extra_guests,
            ..
        } = &msg.kind
        else {
            return;
        };
        let (date, start) = crate::live::fmt_event_form_fields(*start_time);
        // The form's end field is same-day only: an end on
        // another day (or none) starts empty rather than
        // silently moving the event when saved untouched.
        let end = if *end_time > 0 && end_time.div_euclid(86_400) == start_time.div_euclid(86_400) {
            crate::live::fmt_event_form_fields(*end_time).1
        } else {
            String::new()
        };
        self.event_edit_draft = Some((
            chat_id,
            msg_id,
            EventDraft {
                name: name.clone(),
                description: description.clone(),
                date,
                start_time: start,
                end_time: end,
                location: location.clone(),
                link: join_link.clone(),
                scheduled_call: *scheduled_call,
                extra_guests: *extra_guests,
                error: None,
            },
        ));
    }

    /// Submits the open edit form: optimistic apply with the
    /// previous kind parked for exact rollback, then the
    /// EditEvent command (the protocol replaces the whole
    /// event payload, encrypted under its creation secret).
    pub(crate) fn submit_event_edit(&mut self) {
        let Some((chat_id, msg_id, draft)) = self.event_edit_draft.take() else {
            return;
        };
        let fields = match validate_event_draft(&draft) {
            Ok(fields) => fields,
            Err(key) => {
                let mut draft = draft;
                draft.error = Some(crate::i18n::t(key));
                self.event_edit_draft = Some((chat_id, msg_id, draft));
                return;
            }
        };
        self.apply_event_fields(chat_id, msg_id, &fields, false);
    }

    /// Cancels one of my events (the confirmed destructive
    /// action): the same whole-payload edit with `canceled`
    /// set, optimistic with exact rollback like any edit.
    pub(crate) fn cancel_event(&mut self, chat_id: u64, msg_id: u64) {
        let Some(msg) = self.conversation.iter().find(|m| m.id == msg_id) else {
            return;
        };
        let MessageKind::Event {
            name,
            description,
            start_time,
            end_time,
            location,
            join_link,
            scheduled_call,
            extra_guests,
            ..
        } = &msg.kind
        else {
            return;
        };
        let fields = EventFields {
            name: name.clone(),
            description: description.clone(),
            start_time: *start_time,
            end_time: if *end_time > 0 { Some(*end_time) } else { None },
            location: if location.is_empty() { None } else { Some(location.clone()) },
            join_link: if join_link.is_empty() { None } else { Some(join_link.clone()) },
            scheduled_call: *scheduled_call,
            extra_guests: *extra_guests,
        };
        self.apply_event_fields(chat_id, msg_id, &fields, true);
    }

    /// Shared tail of edit and cancel: park the previous kind,
    /// apply the new fields optimistically everywhere the
    /// event shows, and send the command in live mode.
    fn apply_event_fields(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        fields: &EventFields,
        canceled: bool,
    ) {
        let previous_kind =
            self.conversation.iter().find(|m| m.id == msg_id).map(|m| m.kind.clone());
        let Some(previous_kind) = previous_kind else {
            return;
        };
        let mut new_kind = event_kind(fields, Some(&previous_kind));
        if let MessageKind::Event { canceled: c, .. } = &mut new_kind {
            *c = canceled;
        }
        if !self.live_mode {
            // Demo: the conversation copy and the stored chat
            // copy move together, like a poll vote.
            if let Some(msg) = self.conversation.iter_mut().find(|m| m.id == msg_id) {
                msg.kind = new_kind.clone();
            }
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                if let Some(msg) = chat.messages.iter_mut().find(|m| m.id == msg_id) {
                    msg.kind = new_kind;
                }
            }
            return;
        }
        let (Some(jid), Some(proto)) = (
            self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()),
            self.protocol_ids.get(&msg_id).cloned(),
        ) else {
            return;
        };
        self.pending_event_edits.entry(msg_id).or_insert(previous_kind);
        self.apply_event_edit(&jid, &proto, chat_id, msg_id, new_kind);
        self.send_backend(BackendCommand::EditEvent {
            chat_jid: jid,
            message_id: proto,
            name: fields.name.clone(),
            description: fields.description.clone(),
            start_time: fields.start_time,
            end_time: fields.end_time.unwrap_or(0),
            location: fields.location.clone().unwrap_or_default(),
            join_link: fields.join_link.clone().unwrap_or_default(),
            scheduled_call: fields.scheduled_call,
            extra_guests: fields.extra_guests,
            canceled,
        });
    }

    /// The event builder popup (create) and the event edit
    /// popup: one form shape, two hosts, mirroring the poll
    /// builder's snapshot/phase handling.
    pub(crate) fn draw_event_builder(&mut self, ctx: &egui::Context) {
        self.draw_event_form_popup(ctx, false);
        self.draw_event_form_popup(ctx, true);
    }

    fn draw_event_form_popup(&mut self, ctx: &egui::Context, edit: bool) {
        let t = self.tokens;
        let (phase_key, snap_key, area_key) = if edit {
            ("event_edit", "event_edit_snap", "event_edit_area")
        } else {
            ("event_builder", "event_snap", "event_builder_area")
        };
        let open = if edit {
            self.event_edit_draft.is_some()
        } else {
            self.composer.event_draft.is_some()
        };
        // Snapshot the draft while the form is open: the exit
        // animation keeps drawing it for a few frames after the
        // draft itself has been sent or discarded.
        if open {
            let snap = if edit {
                self.event_edit_draft.as_ref().map(|(_, _, d)| d.clone())
            } else {
                self.composer.event_draft.clone()
            };
            if let Some(snap) = snap {
                ctx.data_mut(|d| d.insert_temp(egui::Id::new(snap_key), snap));
            }
        }
        let Some(menu_p) = crate::anim::popup_phase(ctx, phase_key, open, self.motion) else {
            return;
        };
        let exiting = !open;
        let mut exit_draft =
            if exiting { ctx.data(|d| d.get_temp::<EventDraft>(egui::Id::new(snap_key))) } else { None };
        let mut submit = false;
        let mut cancel = false;
        egui::Area::new(egui::Id::new(area_key))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if menu_p < 0.999 {
                    ui.set_opacity(menu_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.set_width(300.0);
                    ui.label(
                        RichText::new(crate::i18n::t(if edit {
                            "event.edit_title"
                        } else {
                            "event.create_title"
                        }))
                        .size(15.0)
                        .strong(),
                    );
                    let draft = if exiting {
                        exit_draft.as_mut()
                    } else if edit {
                        self.event_edit_draft.as_mut().map(|(_, _, d)| d)
                    } else {
                        self.composer.event_draft.as_mut()
                    };
                    if let Some(draft) = draft {
                        event_form_fields(ui, t, draft);
                    }
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
                            &crate::i18n::t(if edit { "event.save" } else { "event.create" }),
                            crate::widgets::Btn::Primary,
                        )
                        .clicked()
                        {
                            submit = true;
                        }
                    });
                });
            });
        crate::anim::popup_transform(ctx, egui::Id::new(area_key), ctx.content_rect().center(), menu_p);
        if exiting {
            // The exit run only replays the last frame of the
            // form; its buttons and fields edit the snapshot.
        } else if submit {
            if edit {
                self.submit_event_edit();
            } else {
                self.send_event_draft();
            }
        } else if cancel {
            if edit {
                self.event_edit_draft = None;
            } else {
                self.composer.event_draft = None;
            }
        }
    }
}

/// Renders the event form's fields into an open popup: name,
/// description, the strict date/time text fields, location,
/// link, the two toggles, and the validation error when one
/// is parked on the draft.
fn event_form_fields(ui: &mut egui::Ui, t: core::theme::Tokens, draft: &mut EventDraft) {
    let field_w = (ui.available_width() - 20.0).max(120.0);
    crate::widgets::text_field(ui, t, &mut draft.name, &crate::i18n::t("event.name"), field_w);
    crate::widgets::text_field(
        ui,
        t,
        &mut draft.description,
        &crate::i18n::t("event.description"),
        field_w,
    );
    ui.horizontal(|ui| {
        crate::widgets::text_field(
            ui,
            t,
            &mut draft.date,
            "YYYY-MM-DD",
            (field_w * 0.55).max(90.0),
        );
        crate::widgets::text_field(
            ui,
            t,
            &mut draft.start_time,
            &crate::i18n::t("event.start_hint"),
            (field_w * 0.4).max(70.0),
        );
    });
    crate::widgets::text_field(
        ui,
        t,
        &mut draft.end_time,
        &crate::i18n::t("event.end_hint"),
        field_w,
    );
    crate::widgets::text_field(ui, t, &mut draft.location, &crate::i18n::t("event.location"), field_w);
    crate::widgets::text_field(ui, t, &mut draft.link, &crate::i18n::t("event.link"), field_w);
    ui.checkbox(&mut draft.scheduled_call, crate::i18n::t("event.scheduled_call"));
    ui.checkbox(&mut draft.extra_guests, crate::i18n::t("event.extra_guests"));
    if let Some(error) = &draft.error {
        ui.label(RichText::new(error.clone()).size(12.5).color(rgb(t.danger)));
    }
}
