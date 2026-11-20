//! Conversation screen: the header and the panel shell live
//! here; the message stream is in `stream`, the composer in
//! `composer`.

mod composer;
mod stream;

use crate::bubble::{message_bubble, BubbleAction, MediaCtx};
use crate::state::App;
use crate::state::VoicePlayback;
use crate::theme::rgb;
use core::model::{Delivery, Message};
use core::theme::Tokens;
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Margin, Pos2, RichText, Sense, Stroke, Vec2,
};

/// Native quick replies (storage + composer panel state),
/// declared here, where its only UI lives.
pub(crate) mod quick_replies;

/// The conversation column's side margin: the stream, the
/// composer stack and the scroll-to-bottom button all inset
/// from the panel edges by this one measure.
pub(super) fn side_margin(panel_w: f32) -> f32 {
    (panel_w * 0.055).clamp(14.0, 72.0)
}

impl App {
    /// The conversation header overflow menu: chat-level and
    /// feature actions live here, not as standing buttons in the
    /// chat surface.
    pub(crate) fn draw_header_menu(&mut self, ctx: &egui::Context) {
        // Visibility runs through the popup phase machine: the
        // menu keeps drawing while its exit runs, then stops.
        let Some(menu_p) =
            crate::anim::popup_phase(ctx, "header_menu", self.header_menu_open, self.motion)
        else {
            return;
        };
        let exiting = !self.header_menu_open;
        let t = self.tokens();
        let chat = self.selected().clone();
        let export_active = self.feature_active("export-chat");
        let mut close = false;
        let mut action: Option<&'static str> = None;

        if !exiting {
            egui::Area::new(egui::Id::unique("header_menu_catcher"))
                .order(egui::Order::Foreground)
                .fixed_pos(Pos2::ZERO)
                .show(ctx, |ui| {
                    let screen = ui.ctx().content_rect();
                    if ui.allocate_rect(screen, Sense::click()).clicked() {
                        close = true;
                    }
                });
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
        }
        let screen = ctx.content_rect();
        // Fade in while the menu's layer scales from 0.95 around
        // the overflow button that opened it, on the house circ
        // curve; the exit is the same motion, shorter, reversed.
        egui::Area::new(egui::Id::unique("header_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::new(screen.right() - 252.0, 58.0))
            .show(ctx, |ui| {
                if menu_p < 0.999 {
                    ui.set_opacity(menu_p);
                }
                let menu_frame =
                    crate::theme::glass_frame(t, 14).inner_margin(Margin::same(8)).show(ui, |ui| {
                        ui.set_width(220.0);
                        let mut row = |ui: &mut egui::Ui,
                                       icon: crate::icons::Icon,
                                       label: &str,
                                       key: &'static str| {
                            let (rect, resp) =
                                ui.allocate_exact_size(Vec2::new(204.0, 32.0), Sense::click());
                            if resp.hovered() {
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(8),
                                    rgb(t.bg_panel_alt),
                                );
                            }
                            crate::icons::draw(
                                ui.painter(),
                                icon,
                                Pos2::new(rect.left() + 17.0, rect.center().y),
                                17.0,
                                rgb(t.text_secondary),
                                rgb(t.bg_panel),
                            );
                            ui.painter().text(
                                Pos2::new(rect.left() + 38.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                label,
                                FontId::proportional(13.0),
                                rgb(t.text_primary),
                            );
                            if resp.clicked() && !exiting {
                                action = Some(key);
                            }
                        };
                        let caption = |ui: &mut egui::Ui, label: &str| {
                            ui.label(
                                RichText::new(label).size(10.5).strong().color(rgb(t.text_faint)),
                            );
                            ui.add_space(2.0);
                        };
                        row(
                            ui,
                            crate::icons::Icon::Person,
                            &crate::i18n::t("conversation.view_profile"),
                            "profile",
                        );
                        ui.add_space(4.0);
                        caption(ui, &crate::i18n::t("conversation.chat_actions").to_uppercase());
                        if export_active {
                            row(
                                ui,
                                crate::icons::Icon::Download,
                                &crate::i18n::t("common.export_chat"),
                                "export",
                            );
                        }
                        let pin_label = if chat.pinned {
                            crate::i18n::t("conversation.unpin")
                        } else {
                            crate::i18n::t("conversation.pin")
                        };
                        row(ui, crate::icons::Icon::Pin, &pin_label, "pinchat");
                        ui.add_space(4.0);
                        caption(ui, &crate::i18n::t("conversation.this_chat").to_uppercase());
                        let mute_label = if chat.muted {
                            crate::i18n::t("conversation.unmute_notifications")
                        } else {
                            crate::i18n::t("conversation.mute_notifications")
                        };
                        row(ui, crate::icons::Icon::Bell, &mute_label, "mute");
                    });
                crate::theme::paint_glass_edge(ui.painter(), menu_frame.response.rect, 14.0, t);
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("header_menu"),
            Pos2::new(screen.right() - 24.0, 44.0),
            menu_p,
        );
        match action {
            Some("profile") => {
                self.profile_open = true;
                close = true;
            }
            Some("export") => {
                self.export_current_chat();
                close = true;
            }
            Some("mute") => {
                if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat.id) {
                    c.muted = !c.muted;
                } else if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat.id) {
                    // The announcement chat lives in the detached
                    // slot; muting it is a local state change too.
                    c.muted = !c.muted;
                }
                close = true;
            }
            Some("pinchat") => {
                self.apply_chat_action(chat.id, chat.archived, !chat.pinned, chat.muted);
                close = true;
            }
            _ => {}
        }
        if close {
            self.header_menu_open = false;
        }
    }

    pub(crate) fn draw_conversation(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        // The demo photo texture is built only when this view can
        // actually show it: when the open conversation contains a
        // photo or video message. It used to be built on the first
        // conversation draw regardless (v0.25 perf review).
        if self.media.photo_texture.is_none()
            && self.conversation.iter().any(|m| {
                matches!(
                    m.kind,
                    core::model::MessageKind::Image { .. } | core::model::MessageKind::Video { .. }
                )
            })
        {
            self.media.photo_texture = Some(crate::media::demo_photo(root.ctx()));
        }
        let now = root.ctx().input(|i| i.time);
        // Bottom clearance for the floating composer stack: the
        // stack's height as measured the last time it was drawn
        // (accessory lines, the quick replies panel and the pill
        // itself), plus its 14 px lift off the bottom edge and a
        // small breath. Measured, not estimated, so the clearance
        // cannot desync when the stack changes shape; the
        // measurement is stored where the composer is drawn,
        // further down.
        let composer_clearance = self.composer.stack_h + self.composer.hint_bar_h + 20.0;
        let mut pending_actions: Vec<BubbleAction> = Vec::new();
        let chat = self.selected().clone();
        // Motion: a short fade covers the switch between chats.
        if self.anim.chat_fade_pending {
            self.anim.chat_fade_pending = false;
            self.anim.chat_fade = Some(self.fresh_anim(crate::motion::dur::SWITCH, root.ctx()));
        }
        let switch_p = crate::motion::Anim::drive(&mut self.anim.chat_fade, root.ctx(), now);
        // Read-by-action engagement detected during this draw
        // (scroll to the newest messages, composer use, a sent
        // reply). Flushed after the panel; see live.rs.
        let mut engaged = false;
        // The chat info dock: a right panel that takes layout
        // space from the conversation column (Telegram-style)
        // instead of floating over it as a popup. Its width
        // glides on the house slide duration; while it closes,
        // the body still renders into the shrinking panel but
        // its intents are discarded. The scalar runs every frame
        // so the width state exists before the first open. The
        // width caps at 360 px and at just over half the zone
        // beside the chat list, so at the minimum window size
        // the conversation column keeps a usable share instead
        // of being squeezed to a sliver.
        let dock_target = if self.profile_open {
            (root.available_rect_before_wrap().width() * 0.55).min(360.0)
        } else {
            0.0
        };
        let dock_secs = (self.motion.duration(crate::motion::dur::SLIDE) / 1000.0) as f32;
        let dock_w = crate::anim::scalar(root, "profile_dock_w", dock_target, dock_secs);
        if dock_w > 0.5 {
            let dock_exiting = !self.profile_open;
            let panel = egui::Panel::right("profile_dock")
                .exact_size(dock_w)
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(rgb(t.bg_panel))
                        .inner_margin(Margin::symmetric(16, 12)),
                )
                .show(root, |ui| {
                    self.draw_profile_dock(ui, dock_exiting);
                });
            // The hairline separating the dock from the
            // conversation column it narrows.
            root.painter().vline(
                panel.response.rect.left() + 0.5,
                panel.response.rect.y_range(),
                Stroke::new(1.0_f32, rgb(t.border)),
            );
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(rgb(t.bg_chat))).show(
            root,
            |ui| {
                let panel_rect = ui.available_rect_before_wrap();
                // The header bar is flat and solid: the panel
                // color with a 1 px hairline along its bottom,
                // no glass tint and no top highlight. It is the
                // one chrome surface that stays solid while the
                // floating layers wear glass.
                let header_frame = egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            // Entered through a community home:
                            // a way back to it, offered while the
                            // open chat still belongs to that
                            // community (a chat picked from the
                            // list clears the origin instead).
                            if let Some(origin) = self.community_origin {
                                let belongs =
                                    self.communities().iter().find(|c| c.id == origin).is_some_and(
                                        |c| {
                                            c.announcement_chat_id == chat.id
                                                || c.group_chat_ids.contains(&chat.id)
                                                || c.other_group_ids.contains(&chat.id)
                                        },
                                    );
                                if belongs
                                    && crate::widgets::icon_button(
                                        ui,
                                        t,
                                        crate::icons::Icon::ChevronLeft,
                                        32.0,
                                        &crate::i18n::t("community.back"),
                                        None,
                                    )
                                    .clicked()
                                {
                                    self.open_community = Some(origin);
                                    self.community_origin = None;
                                }
                            }
                            let header_photo = if chat.kind == core::model::ChatKind::Dm {
                                self.demo_avatar(ui.ctx(), &chat.title)
                            } else {
                                None
                            };
                            if header_avatar(ui, &chat, header_photo) {
                                self.profile_open = true;
                            }
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    if ui
                                        .add(
                                            egui::Label::new(
                                                RichText::new(&chat.title)
                                                    .font(crate::theme::semibold(15.0)),
                                            )
                                            .sense(Sense::click()),
                                        )
                                        .on_hover_text("View profile")
                                        .clicked()
                                    {
                                        self.profile_open = true;
                                    }
                                    if chat.verified {
                                        let (r, _) = ui
                                            .allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                                        crate::widgets::verified_badge(
                                            ui.painter(),
                                            r.center(),
                                            7.5,
                                            t,
                                        );
                                    }
                                    if chat.meta_ai {
                                        let (r, _) = ui.allocate_exact_size(
                                            Vec2::new(22.0, 14.0),
                                            Sense::hover(),
                                        );
                                        crate::widgets::ai_chip(
                                            ui.painter(),
                                            Pos2::new(r.left(), r.center().y),
                                            t,
                                        );
                                    }
                                });
                                let typing_now = self
                                    .live_typing
                                    .get(&chat.id)
                                    .is_some_and(|at| at.elapsed().as_secs() < 6);
                                if typing_now && self.live.is_some() {
                                    // Animated typing state: the
                                    // label (without its static
                                    // ellipsis) plus three bouncing
                                    // dots from the anim helpers.
                                    let label = crate::i18n::t("conversation.typing");
                                    let label = label.trim_end_matches(['.', '…']);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(label).size(12.0).color(rgb(t.accent)),
                                        );
                                        crate::anim::typing_dots(ui, rgb(t.accent));
                                    });
                                } else {
                                    ui.label(
                                        RichText::new(chat.subtitle.clone())
                                            .size(12.0)
                                            .color(rgb(t.text_secondary)),
                                    );
                                }
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if crate::widgets::icon_button(
                                    ui,
                                    t,
                                    crate::icons::Icon::MoreVert,
                                    28.0,
                                    "More options",
                                    None,
                                )
                                .clicked()
                                {
                                    self.header_menu_open = !self.header_menu_open;
                                }
                                if crate::widgets::icon_button(
                                    ui,
                                    t,
                                    crate::icons::Icon::Phone,
                                    28.0,
                                    "Voice call",
                                    None,
                                )
                                .clicked()
                                {
                                    self.start_call_from_header();
                                }
                            });
                        });
                    });
                ui.painter().hline(
                    header_frame.response.rect.x_range(),
                    header_frame.response.rect.bottom() - 0.5,
                    Stroke::new(1.0_f32, rgb(t.border)),
                );

                let (scroll_out, full_rect) = self.draw_stream(
                    ui,
                    &chat,
                    t,
                    now,
                    composer_clearance,
                    &mut pending_actions,
                    &mut engaged,
                );
                self.draw_composer(ui, full_rect, t, now, &mut engaged);
                self.draw_scroll_bottom(ui, full_rect, &chat, &scroll_out, t, &mut engaged);
                self.draw_quick_replies_editor(ui, t);
                // Switch fade overlay: bg_chat at fading alpha over
                // the whole panel; gone (and not repainting) once
                // the animation completes.
                if switch_p < 1.0 {
                    let alpha = ((1.0 - switch_p) * 220.0) as u8;
                    ui.painter().rect_filled(
                        panel_rect,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(
                            t.bg_chat.0,
                            t.bg_chat.1,
                            t.bg_chat.2,
                            alpha,
                        ),
                    );
                }
            },
        );
        // Read-by-action flush: if the user acted during this
        // draw (scrolled to the newest messages, used the
        // composer, sent a reply), pending receipts for the open
        // chat go out now and its badge clears. Nothing here runs
        // on passive arrival or on merely opening the chat.
        if engaged && self.live_mode && self.note_read_engagement() > 0 {
            let chat_id = self.selected_chat;
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.unread = 0;
            }
        }
        self.draw_conversation_popups(root.ctx());
        self.draw_header_menu(root.ctx());
    }
}

impl App {
    /// Export Chat feature action: renders the open conversation to
    /// Markdown (through the feature guard) and writes it under
    /// `<data dir>/exports/`. The result is reported via the notice
    /// banner, the file is really written, or the error is shown.
    pub(crate) fn export_current_chat(&mut self) {
        if !self.feature_active("export-chat") {
            self.notice = Some(
                "The Chat Export feature is off. Turn it on in Settings > Features.".to_owned(),
            );
            return;
        }
        let chat = self.selected().clone();
        let conversation = self.conversation.clone();
        let Some(markdown) = self.guard_feature("export-chat", |app| {
            let _ = app;
            export_chat::export_markdown(&chat, &conversation, &crate::state::today_string())
        }) else {
            return;
        };
        let dir = backend::data_dir().join("exports");
        let path = dir.join(export_chat::export_filename(&chat));
        let result = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, markdown));
        self.notice = Some(match result {
            Ok(()) => format!("Chat exported to {}", path.display()),
            Err(e) => format!("Export failed: {e}"),
        });
    }
}

/// The conversation header avatar (AI chats get the sparkles
/// avatar). Returns true when clicked (opens the profile panel).
fn header_avatar(
    ui: &mut egui::Ui,
    chat: &core::model::Chat,
    photo: Option<egui::TextureId>,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
    crate::widgets::paint_chat_avatar_photo(ui.painter(), rect.center(), 20.0, chat, photo);
    response.on_hover_text("View profile").clicked()
}
