//! Polls: voting on a poll, sending a poll draft, and the poll builder popup.

use super::*;

impl App {
    /// Casts or changes my vote on a poll message (demo + live).
    pub(crate) fn vote_poll(&mut self, msg_id: u64, idx: usize) {
        let chat_id = self.selected_chat;
        // My selection before this vote, captured before any
        // apply moves it: a PollVoteFailed re-applies it as my
        // ballot and the tally recomputes back.
        let previous_votes =
            self.conversation.iter().find(|m| m.id == msg_id).and_then(|m| match &m.kind {
                MessageKind::Poll { my_votes, .. } => Some(my_votes.clone()),
                _ => None,
            });
        let mut new_options: Vec<usize> = Vec::new();
        if let Some(msg) = self.conversation.iter_mut().find(|m| m.id == msg_id) {
            if let MessageKind::Poll { options, multiple, my_votes, .. } = &mut msg.kind {
                let (opts, mine) = core::rich::apply_vote(options, my_votes, *multiple, idx);
                *options = opts;
                *my_votes = mine.clone();
                new_options = mine;
            }
        }
        // Mirror into the stored chat (demo) so the vote survives a
        // chat switch.
        if !self.live_mode {
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                if let Some(msg) = chat.messages.iter_mut().find(|m| m.id == msg_id) {
                    if let MessageKind::Poll { options, multiple, my_votes, .. } = &mut msg.kind {
                        let (opts, mine) =
                            core::rich::apply_vote(options, my_votes, *multiple, idx);
                        *options = opts;
                        *my_votes = mine;
                    }
                }
            }
        }
        if self.live_mode {
            if let (Some(jid), Some(proto)) = (
                self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()),
                self.protocol_ids.get(&msg_id).cloned(),
            ) {
                if let Some(previous) = previous_votes {
                    self.pending_poll_votes.entry(msg_id).or_insert(previous);
                }
                self.send_backend(BackendCommand::VotePoll {
                    chat_jid: jid.clone(),
                    message_id: proto.clone(),
                    options: new_options.clone(),
                });
                // My vote is durable at once, in the mirror and
                // the archive, not only when the server echoes it
                // back; the echo, when it arrives, re-applies the
                // identical selection and changes nothing.
                self.apply_poll_vote(&jid, &proto, "", true, &new_options);
            }
        }
    }

    /// Sends a poll built in the composer builder.
    pub(crate) fn send_poll_draft(&mut self) {
        let Some(draft) = self.composer.poll_draft.take() else {
            return;
        };
        let options: Vec<(String, u32)> = draft
            .options
            .iter()
            .map(|o| o.trim().to_owned())
            .filter(|o| !o.is_empty())
            .map(|o| (o, 0))
            .collect();
        if draft.question.trim().is_empty() || options.len() < 2 {
            self.notice =
                Some(Notice::info("A poll needs a question and at least two options.".to_owned()));
            self.composer.poll_draft = Some(draft);
            return;
        }
        if self.live_mode {
            let chat_id = self.selected_chat;
            if let Some(jid) = self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()) {
                self.send_backend(BackendCommand::CreatePoll {
                    chat_jid: jid,
                    question: draft.question.trim().to_owned(),
                    options: options.iter().map(|(o, _)| o.clone()).collect(),
                    multiple: draft.multiple,
                });
                // Busy while the create is in flight (P2-32);
                // the poll's own upsert (or the failure event)
                // replaces it.
                self.notice = Some(Notice::busy(crate::i18n::t("notice.creating_poll")));
            }
            return;
        }
        // Demo: local poll message, fully votable.
        self.sent_counter += 1;
        let msg = Message {
            kind: MessageKind::Poll {
                question: draft.question.trim().to_owned(),
                options,
                multiple: draft.multiple,
                my_votes: Vec::new(),
                correct: None,
            },
            ..Message::mine(900 + self.sent_counter, String::new(), "19:36")
        };
        self.conversation.push(msg.clone());
        let chat_id = self.selected_chat;
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.messages.push(msg);
        }
    }

    pub(crate) fn draw_poll_builder(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        // Snapshot the draft while the builder is open: the exit
        // animation keeps drawing it for a few frames after the
        // draft itself has been sent or discarded.
        if let Some(draft) = &self.composer.poll_draft {
            ctx.data_mut(|d| d.insert_temp(egui::Id::unique("poll_snap"), draft.clone()));
        }
        let Some(menu_p) = crate::anim::popup_phase(
            ctx,
            "poll_builder",
            self.composer.poll_draft.is_some(),
            self.motion,
        ) else {
            return;
        };
        let exiting = self.composer.poll_draft.is_none();
        let mut exit_draft = if exiting {
            ctx.data(|d| d.get_temp::<crate::state::PollDraft>(egui::Id::unique("poll_snap")))
        } else {
            None
        };
        let mut send = false;
        let mut cancel = false;
        egui::Area::new(egui::Id::unique("poll_builder"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if menu_p < 0.999 {
                    ui.set_opacity(menu_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.set_width(300.0);
                    ui.label(RichText::new("Create poll").size(15.0).strong());
                    let draft = if exiting {
                        exit_draft.as_mut()
                    } else {
                        self.composer.poll_draft.as_mut()
                    };
                    if let Some(draft) = draft {
                        let field_w = (ui.available_width() - 20.0).max(120.0);
                        crate::widgets::text_field(ui, t, &mut draft.question, "Question", field_w)
                            .on_hover_text("Question");
                        for (i, opt) in draft.options.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(format!("{}.", i + 1));
                                crate::widgets::text_field(
                                    ui,
                                    t,
                                    opt,
                                    "Option",
                                    (field_w - 24.0).max(80.0),
                                );
                            });
                        }
                        if draft.options.len() < 12
                            && crate::widgets::button(
                                ui,
                                t,
                                "+ Add option",
                                crate::widgets::Btn::Secondary,
                            )
                            .clicked()
                        {
                            draft.options.push(String::new());
                        }
                        ui.checkbox(&mut draft.multiple, "Allow multiple answers");
                    }
                    ui.horizontal(|ui| {
                        if crate::widgets::button(ui, t, "Cancel", crate::widgets::Btn::Secondary)
                            .clicked()
                        {
                            cancel = true;
                        }
                        if crate::widgets::button(ui, t, "Send poll", crate::widgets::Btn::Primary)
                            .clicked()
                        {
                            send = true;
                        }
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("poll_builder"),
            ctx.content_rect().center(),
            menu_p,
        );
        if exiting {
            // The exit run only replays the last frame of the
            // form; its buttons and fields edit the snapshot.
        } else if send {
            self.send_poll_draft();
        } else if cancel {
            self.composer.poll_draft = None;
        }
    }
}
