//! Interactive messages (row 19): the composer builder for
//! buttons, list and carousel messages, the send path, and
//! answering a received one. The shapes and caps mirror what
//! the bubble renderer draws and what the wire builders
//! accept: at most 3 buttons, 5 list sections of 10 rows,
//! 2 carousel cards of 2 buttons.

use super::*;
use crate::state::{InteractiveDraft, InteractiveShape};

/// The backend spec a valid draft describes: every value
/// trimmed, exactly what the wire builder will send.
fn draft_spec(draft: &InteractiveDraft) -> backend::InteractiveSpec {
    match draft.shape {
        InteractiveShape::Buttons => backend::InteractiveSpec::Buttons {
            text: draft.text.trim().to_owned(),
            footer: draft.footer.trim().to_owned(),
            buttons: draft.buttons.iter().map(|b| b.trim().to_owned()).collect(),
        },
        InteractiveShape::List => backend::InteractiveSpec::List {
            title: draft.text.trim().to_owned(),
            description: draft.description.trim().to_owned(),
            button_label: draft.button_label.trim().to_owned(),
            sections: draft
                .sections
                .iter()
                .map(|section| {
                    (
                        section.title.trim().to_owned(),
                        section
                            .rows
                            .iter()
                            .map(|(title, description)| {
                                (title.trim().to_owned(), description.trim().to_owned())
                            })
                            .collect(),
                    )
                })
                .collect(),
        },
        InteractiveShape::Carousel => backend::InteractiveSpec::Carousel {
            body: draft.text.trim().to_owned(),
            cards: draft
                .cards
                .iter()
                .map(|card| backend::LiveCard {
                    title: card.title.trim().to_owned(),
                    body: card.body.trim().to_owned(),
                    image: None,
                    buttons: card.buttons.iter().map(|b| b.trim().to_owned()).collect(),
                })
                .collect(),
        },
    }
}

/// The bubble kind an interactive spec renders as: the same
/// shapes the backend builds from it, with the same
/// positional choice ids.
fn spec_kind(spec: &backend::InteractiveSpec) -> MessageKind {
    match spec {
        backend::InteractiveSpec::Buttons { footer, buttons, .. } => MessageKind::Buttons {
            footer: footer.clone(),
            buttons: buttons
                .iter()
                .enumerate()
                .map(|(index, label)| core::model::MsgButton {
                    id: (index + 1).to_string(),
                    text: label.clone(),
                    action: None,
                })
                .collect(),
        },
        backend::InteractiveSpec::List { description, button_label, sections, .. } => {
            let mut next_row = 0usize;
            MessageKind::ListMsg {
                description: description.clone(),
                button_label: button_label.clone(),
                footer: String::new(),
                sections: sections
                    .iter()
                    .map(|(title, rows)| core::model::MsgListSection {
                        title: title.clone(),
                        rows: rows
                            .iter()
                            .map(|(row_title, row_description)| {
                                next_row += 1;
                                core::model::MsgListRow {
                                    id: next_row.to_string(),
                                    title: row_title.clone(),
                                    description: row_description.clone(),
                                }
                            })
                            .collect(),
                    })
                    .collect(),
            }
        }
        backend::InteractiveSpec::Carousel { body, cards } => MessageKind::Carousel {
            body: body.clone(),
            cards: cards
                .iter()
                .map(|card| core::model::CarouselCard {
                    title: card.title.clone(),
                    body: card.body.clone(),
                    image: card.image.clone(),
                    buttons: card.buttons.clone(),
                })
                .collect(),
        },
    }
}

/// The body text an interactive spec's bubble carries: the
/// buttons text, the list title, or the carousel body.
fn spec_body(spec: &backend::InteractiveSpec) -> String {
    match spec {
        backend::InteractiveSpec::Buttons { text, .. } => text.clone(),
        backend::InteractiveSpec::List { title, .. } => title.clone(),
        backend::InteractiveSpec::Carousel { body, .. } => body.clone(),
    }
}

impl App {
    /// Sends the interactive draft the builder holds. The
    /// Send button enables only on a valid draft, so this
    /// never has to refuse; the local echo is the message the
    /// bubble will render, marked Sending until the backend
    /// binds it (live) like a text send.
    pub(crate) fn send_interactive_draft(&mut self) {
        let Some(draft) = self.composer.interactive_draft.take() else {
            return;
        };
        if !draft.valid() {
            self.composer.interactive_draft = Some(draft);
            return;
        }
        let spec = draft_spec(&draft);
        let kind = spec_kind(&spec);
        let body = spec_body(&spec);
        if self.live_mode {
            let chat_id = self.selected_chat;
            let Some(jid) = self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()) else {
                return;
            };
            self.send_backend(BackendCommand::SendInteractive {
                chat_jid: jid,
                send_id: None,
                spec,
            });
            self.sent_counter += 1;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
            let mut echo = Message {
                delivery: Some(core::model::Delivery::Sending),
                ..Message::mine(9_000_000 + self.sent_counter, body, "…")
            };
            echo.kind = kind;
            if let Some(session) = &mut self.live {
                session.push_message(chat_id, now, echo);
            }
            self.sync_conversation();
            return;
        }
        // Demo without a session: a local message only.
        self.sent_counter += 1;
        let mut msg = Message::mine(900 + self.sent_counter, body, "19:36");
        msg.kind = kind;
        self.conversation.push(msg.clone());
        let chat_id = self.selected_chat;
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.messages.push(msg);
        }
    }

    /// Answers a received interactive message with the
    /// chosen button or row (row 19): the response command
    /// names the original message and the choice's wire id,
    /// and the answer echoes locally as a text message, the
    /// way WhatsApp shows it.
    pub(crate) fn answer_interactive(
        &mut self,
        msg_id: u64,
        is_list: bool,
        choice_id: String,
        choice_text: String,
    ) {
        if !self.live_mode {
            return;
        }
        let chat_id = self.selected_chat;
        let (Some(jid), Some(proto)) = (
            self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()),
            self.protocol_ids.get(&msg_id).cloned(),
        ) else {
            return;
        };
        let sender_jid = self.sender_jids.get(&msg_id).cloned();
        self.send_backend(BackendCommand::SendInteractiveResponse {
            chat_jid: jid,
            send_id: None,
            message_id: proto,
            sender_jid,
            from_me: false,
            is_list,
            choice_id,
            choice_text: choice_text.clone(),
        });
        self.sent_counter += 1;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let echo = Message {
            delivery: Some(core::model::Delivery::Sending),
            ..Message::mine(9_000_000 + self.sent_counter, choice_text, "…")
        };
        if let Some(session) = &mut self.live {
            session.push_message(chat_id, now, echo);
        }
        self.sync_conversation();
    }

    /// The interactive builder popup: a shape switch, then
    /// the shape's fields with add and remove controls. Send
    /// enables exactly when the draft is valid.
    pub(crate) fn draw_interactive_builder(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        if let Some(draft) = &self.composer.interactive_draft {
            ctx.data_mut(|d| {
                d.insert_temp(egui::Id::unique("interactive_snap"), draft.clone());
            });
        }
        let Some(menu_p) = crate::anim::popup_phase(
            ctx,
            "interactive_builder",
            self.composer.interactive_draft.is_some(),
            self.motion,
        ) else {
            return;
        };
        let exiting = self.composer.interactive_draft.is_none();
        let mut exit_draft = if exiting {
            ctx.data(|d| d.get_temp::<InteractiveDraft>(egui::Id::unique("interactive_snap")))
        } else {
            None
        };
        let mut send = false;
        let mut cancel = false;
        egui::Area::new(egui::Id::unique("interactive_builder"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if menu_p < 0.999 {
                    ui.set_opacity(menu_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.set_width(320.0);
                    ui.label(
                        RichText::new(crate::i18n::t("interactive.title")).size(15.0).strong(),
                    );
                    let draft = if exiting {
                        exit_draft.as_mut()
                    } else {
                        self.composer.interactive_draft.as_mut()
                    };
                    let mut valid = false;
                    if let Some(draft) = draft {
                        valid = draft.valid();
                        ui.horizontal(|ui| {
                            for (shape, key) in [
                                (InteractiveShape::Buttons, "interactive.shape_buttons"),
                                (InteractiveShape::List, "interactive.shape_list"),
                                (InteractiveShape::Carousel, "interactive.shape_carousel"),
                            ] {
                                if crate::widgets::chip(
                                    ui,
                                    &crate::i18n::t(key),
                                    draft.shape == shape,
                                    t,
                                )
                                .clicked()
                                {
                                    draft.shape = shape;
                                }
                            }
                        });
                        ui.add_space(crate::theme::SP_2);
                        let field_w = (ui.available_width() - 20.0).max(120.0);
                        match draft.shape {
                            InteractiveShape::Buttons => {
                                crate::widgets::text_field(
                                    ui,
                                    t,
                                    &mut draft.text,
                                    &crate::i18n::t("interactive.text_hint"),
                                    field_w,
                                );
                                crate::widgets::text_field(
                                    ui,
                                    t,
                                    &mut draft.footer,
                                    &crate::i18n::t("interactive.footer_hint"),
                                    field_w,
                                );
                                let mut remove: Option<usize> = None;
                                let button_count = draft.buttons.len();
                                for (i, label) in draft.buttons.iter_mut().enumerate() {
                                    ui.horizontal(|ui| {
                                        ui.label(format!("{}.", i + 1));
                                        crate::widgets::text_field(
                                            ui,
                                            t,
                                            label,
                                            &crate::i18n::t("interactive.button_hint"),
                                            (field_w - 90.0).max(80.0),
                                        );
                                        if button_count > 1
                                            && crate::widgets::button(
                                                ui,
                                                t,
                                                &crate::i18n::t("common.remove"),
                                                crate::widgets::Btn::Secondary,
                                            )
                                            .clicked()
                                        {
                                            remove = Some(i);
                                        }
                                    });
                                }
                                if let Some(i) = remove {
                                    draft.buttons.remove(i);
                                }
                                if draft.buttons.len() < 3
                                    && crate::widgets::button(
                                        ui,
                                        t,
                                        &crate::i18n::t("interactive.add_button"),
                                        crate::widgets::Btn::Secondary,
                                    )
                                    .clicked()
                                {
                                    draft.buttons.push(String::new());
                                }
                            }
                            InteractiveShape::List => {
                                crate::widgets::text_field(
                                    ui,
                                    t,
                                    &mut draft.text,
                                    &crate::i18n::t("interactive.list_title_hint"),
                                    field_w,
                                );
                                crate::widgets::text_field(
                                    ui,
                                    t,
                                    &mut draft.description,
                                    &crate::i18n::t("interactive.list_description_hint"),
                                    field_w,
                                );
                                crate::widgets::text_field(
                                    ui,
                                    t,
                                    &mut draft.button_label,
                                    &crate::i18n::t("interactive.list_button_hint"),
                                    field_w,
                                );
                                let mut remove_section: Option<usize> = None;
                                let mut remove_row: Option<(usize, usize)> = None;
                                let mut add_row: Option<usize> = None;
                                let section_count = draft.sections.len();
                                for (si, section) in draft.sections.iter_mut().enumerate() {
                                    let row_count = section.rows.len();
                                    ui.horizontal(|ui| {
                                        crate::widgets::text_field(
                                            ui,
                                            t,
                                            &mut section.title,
                                            &crate::i18n::t("interactive.section_hint"),
                                            (field_w - 90.0).max(80.0),
                                        );
                                        if section_count > 1
                                            && crate::widgets::button(
                                                ui,
                                                t,
                                                &crate::i18n::t("common.remove"),
                                                crate::widgets::Btn::Secondary,
                                            )
                                            .clicked()
                                        {
                                            remove_section = Some(si);
                                        }
                                    });
                                    for (ri, row) in section.rows.iter_mut().enumerate() {
                                        ui.horizontal(|ui| {
                                            crate::widgets::text_field(
                                                ui,
                                                t,
                                                &mut row.0,
                                                &crate::i18n::t("interactive.row_hint"),
                                                (field_w - 150.0).max(60.0),
                                            );
                                            crate::widgets::text_field(
                                                ui,
                                                t,
                                                &mut row.1,
                                                &crate::i18n::t("interactive.row_description_hint"),
                                                (field_w - 150.0).max(60.0),
                                            );
                                            if row_count > 1
                                                && crate::widgets::button(
                                                    ui,
                                                    t,
                                                    &crate::i18n::t("common.remove"),
                                                    crate::widgets::Btn::Secondary,
                                                )
                                                .clicked()
                                            {
                                                remove_row = Some((si, ri));
                                            }
                                        });
                                    }
                                    if section.rows.len() < 10
                                        && crate::widgets::button(
                                            ui,
                                            t,
                                            &crate::i18n::t("interactive.add_row"),
                                            crate::widgets::Btn::Secondary,
                                        )
                                        .clicked()
                                    {
                                        add_row = Some(si);
                                    }
                                }
                                if let Some(si) = remove_section {
                                    draft.sections.remove(si);
                                }
                                if let Some((si, ri)) = remove_row {
                                    draft.sections[si].rows.remove(ri);
                                }
                                if let Some(si) = add_row {
                                    draft.sections[si].rows.push((String::new(), String::new()));
                                }
                                if draft.sections.len() < 5
                                    && crate::widgets::button(
                                        ui,
                                        t,
                                        &crate::i18n::t("interactive.add_section"),
                                        crate::widgets::Btn::Secondary,
                                    )
                                    .clicked()
                                {
                                    draft.sections.push(crate::state::InteractiveSectionDraft {
                                        title: String::new(),
                                        rows: vec![(String::new(), String::new())],
                                    });
                                }
                            }
                            InteractiveShape::Carousel => {
                                crate::widgets::text_field(
                                    ui,
                                    t,
                                    &mut draft.text,
                                    &crate::i18n::t("interactive.carousel_body_hint"),
                                    field_w,
                                );
                                let mut remove_card: Option<usize> = None;
                                let mut remove_button: Option<(usize, usize)> = None;
                                let mut add_button: Option<usize> = None;
                                let card_count = draft.cards.len();
                                for (ci, card) in draft.cards.iter_mut().enumerate() {
                                    let card_button_count = card.buttons.len();
                                    ui.horizontal(|ui| {
                                        ui.label(format!("{}.", ci + 1));
                                        crate::widgets::text_field(
                                            ui,
                                            t,
                                            &mut card.title,
                                            &crate::i18n::t("interactive.card_title_hint"),
                                            (field_w - 90.0).max(80.0),
                                        );
                                        if card_count > 1
                                            && crate::widgets::button(
                                                ui,
                                                t,
                                                &crate::i18n::t("common.remove"),
                                                crate::widgets::Btn::Secondary,
                                            )
                                            .clicked()
                                        {
                                            remove_card = Some(ci);
                                        }
                                    });
                                    crate::widgets::text_field(
                                        ui,
                                        t,
                                        &mut card.body,
                                        &crate::i18n::t("interactive.card_body_hint"),
                                        field_w,
                                    );
                                    for (bi, label) in card.buttons.iter_mut().enumerate() {
                                        ui.horizontal(|ui| {
                                            crate::widgets::text_field(
                                                ui,
                                                t,
                                                label,
                                                &crate::i18n::t("interactive.button_hint"),
                                                (field_w - 90.0).max(80.0),
                                            );
                                            if card_button_count > 1
                                                && crate::widgets::button(
                                                    ui,
                                                    t,
                                                    &crate::i18n::t("common.remove"),
                                                    crate::widgets::Btn::Secondary,
                                                )
                                                .clicked()
                                            {
                                                remove_button = Some((ci, bi));
                                            }
                                        });
                                    }
                                    if card.buttons.len() < 2
                                        && crate::widgets::button(
                                            ui,
                                            t,
                                            &crate::i18n::t("interactive.add_button"),
                                            crate::widgets::Btn::Secondary,
                                        )
                                        .clicked()
                                    {
                                        add_button = Some(ci);
                                    }
                                }
                                if let Some(ci) = remove_card {
                                    draft.cards.remove(ci);
                                }
                                if let Some((ci, bi)) = remove_button {
                                    draft.cards[ci].buttons.remove(bi);
                                }
                                if let Some(ci) = add_button {
                                    draft.cards[ci].buttons.push(String::new());
                                }
                                if draft.cards.len() < 2
                                    && crate::widgets::button(
                                        ui,
                                        t,
                                        &crate::i18n::t("interactive.add_card"),
                                        crate::widgets::Btn::Secondary,
                                    )
                                    .clicked()
                                {
                                    draft.cards.push(crate::state::InteractiveCardDraft {
                                        title: String::new(),
                                        body: String::new(),
                                        buttons: vec![String::new()],
                                    });
                                }
                            }
                        }
                    }
                    ui.add_space(crate::theme::SP_2);
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
                        if crate::widgets::button_enabled(
                            ui,
                            t,
                            &crate::i18n::t("interactive.send"),
                            crate::widgets::Btn::Primary,
                            valid,
                        )
                        .clicked()
                        {
                            send = true;
                        }
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("interactive_builder"),
            ctx.content_rect().center(),
            menu_p,
        );
        if exiting {
            // The exit run replays the last frame of the form.
        } else if send {
            self.send_interactive_draft();
        } else if cancel {
            self.composer.interactive_draft = None;
        }
    }
}
