//! Conversation screen: the header and the panel shell live
//! here; the message stream is in `stream`, the composer in
//! `composer`.

mod composer;
mod stream;

/// The stream's centered system pill, shared with the bubble
/// renderer (its system-message fallback draws the same pill).
pub(crate) use stream::{system_pill, SystemPillKind};

/// The floating composer's lift off the bottom edge of the
/// conversation panel: the composer stack, the stream's bottom
/// clearance and the scroll-to-bottom button all derive their
/// positions from this one measure, so the stream can never
/// slide under the composer and the button can never overlap
/// the stream.
pub(crate) const COMPOSER_LIFT: f32 = 14.0;

/// The breathing room between the last bubble and the top of
/// the floating composer stack, reserved as the stream's
/// bottom inset on top of the composer's own height and lift.
pub(crate) const STREAM_COMPOSER_GAP: f32 = 12.0;

/// The gap between the scroll-to-bottom button and the top of
/// the floating composer stack: the button parks above the
/// composer, never over the messages.
pub(crate) const SCROLL_BUTTON_GAP: f32 = 10.0;

/// The stream's bottom inset for a composer stack of height
/// `stack_h`: the stack itself, its lift off the bottom edge,
/// and the gap above it. The last bubble always rests at
/// least this far above the panel's bottom edge, fully clear
/// of the composer.
pub(crate) fn composer_clearance(stack_h: f32) -> f32 {
    stack_h + COMPOSER_LIFT + STREAM_COMPOSER_GAP
}

/// The top edge of the scroll-to-bottom button for a stream
/// panel ending at `full_bottom` with a composer stack of
/// height `stack_h`: the button's bottom sits
/// [`SCROLL_BUTTON_GAP`] above the composer's top edge.
pub(crate) fn scroll_bottom_button_top(full_bottom: f32, stack_h: f32, size: f32) -> f32 {
    full_bottom - COMPOSER_LIFT - stack_h - SCROLL_BUTTON_GAP - size
}

use crate::bubble::{message_bubble, BubbleAction, MediaCtx};
use crate::state::App;
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
/// from the panel edges by this one measure. A phone-width
/// pane keeps a small fixed inset instead: the percentage
/// would still leave the composer pill too little width for
/// its input.
pub(super) fn side_margin(panel_w: f32) -> f32 {
    if panel_w < 480.0 {
        10.0
    } else {
        (panel_w * 0.055).clamp(14.0, 72.0)
    }
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
                        row(
                            ui,
                            crate::icons::Icon::Person,
                            &crate::i18n::t("conversation.view_profile"),
                            "profile",
                        );
                        ui.add_space(4.0);
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
                // Through apply_chat_action, like pin: the flip
                // applies locally at once and, in live mode, the
                // backend command follows, so the account's mute
                // state changes too instead of only this window's.
                // The detached announcement chat is covered by
                // the same call's detached fallback (local-only,
                // as before).
                self.apply_chat_action(chat.id, chat.archived, chat.pinned, !chat.muted);
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
        // itself), plus its lift off the bottom edge and the
        // stream's gap above it (see composer_clearance).
        // Measured, not estimated, so the clearance
        // cannot desync when the stack changes shape; the
        // measurement is stored where the composer is drawn,
        // further down.
        let clearance = composer_clearance(self.composer.stack_h);
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
            if self.single_pane {
                // Single-pane: the dock is the only thing on
                // screen, so it takes the whole content width
                // instead of squeezing the conversation beside it.
                root.available_rect_before_wrap().width()
            } else {
                (root.available_rect_before_wrap().width() * 0.55).min(360.0)
            }
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
            // The seam separating the dock from the
            // conversation column it narrows, at the standard
            // soft alpha like every panel seam.
            let seam = rgb(t.border);
            root.painter().vline(
                panel.response.rect.left() + 0.5,
                panel.response.rect.y_range(),
                Stroke::new(
                    1.0_f32,
                    egui::Color32::from_rgba_unmultiplied(seam.r(), seam.g(), seam.b(), 100),
                ),
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
                            // Single-pane: the way back to the
                            // list this conversation was opened
                            // from. Only in single-pane mode (the
                            // list is always visible beside the
                            // conversation otherwise), and yielded
                            // to the community back below when the
                            // chat was entered through a community
                            // home: that one leads back to the
                            // home, whose own back reaches the list.
                            if self.single_pane
                                && self.community_origin.is_none()
                                && crate::widgets::icon_button(
                                    ui,
                                    t,
                                    crate::icons::Icon::ChevronLeft,
                                    32.0,
                                    &crate::i18n::t("common.back"),
                                    None,
                                )
                                .clicked()
                            {
                                self.conversation_open = false;
                            }
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
                                        RichText::new(self.header_subtitle(&chat))
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

                // A blocked direct chat says so under its header,
                // with the way back right there. The conversation
                // stays readable and the history stays put
                // (WhatsApp keeps blocked chats in the list and
                // openable); the strip just stops it presenting
                // as a normal conversation.
                if chat.kind == core::model::ChatKind::Dm && self.blocked_chats.contains(&chat.id) {
                    let mut unblock = false;
                    egui::Frame::new()
                        .fill(rgb(t.bg_panel_alt))
                        .inner_margin(Margin::symmetric(12, 6))
                        .show(ui, |ui| {
                            ui.with_layout(
                                Layout::left_to_right(Align::Center).with_main_align(Align::Center),
                                |ui| {
                                    ui.label(
                                        RichText::new(crate::i18n::t("conversation.blocked_note"))
                                            .size(12.5)
                                            .color(rgb(t.text_secondary)),
                                    );
                                    ui.add_space(10.0);
                                    if crate::widgets::button(
                                        ui,
                                        t,
                                        &crate::i18n::t("common.unblock"),
                                        crate::widgets::Btn::Secondary,
                                    )
                                    .clicked()
                                    {
                                        unblock = true;
                                    }
                                },
                            );
                        });
                    if unblock {
                        self.toggle_block_contact(chat.id);
                    }
                }

                let (scroll_out, full_rect) = self.draw_stream(
                    ui,
                    &chat,
                    t,
                    now,
                    clearance,
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
    /// Markdown (through the feature guard) and hands the write to
    /// a worker thread, which writes it under `<data dir>/exports/`
    /// and reports the outcome back through `poll_workers` as a
    /// notice. The write used to happen inline in this click
    /// handler, stalling the UI for a large chat, and the fixed
    /// per-chat filename silently overwrote the previous export
    /// (audit finding 16); the worker writes to a timestamped,
    /// never-before-used name via a temp file plus rename, so a
    /// crash mid-write cannot leave a truncated export behind.
    pub(crate) fn export_current_chat(&mut self) {
        if !self.feature_active("export-chat") {
            self.notice = Some(
                "The Chat Export feature is off. Turn it on in Settings > Features.".to_owned(),
            );
            return;
        }
        if self.export_rx.is_some() {
            self.notice = Some("An export is already in progress.".to_owned());
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
        let base = export_chat::export_filename(&chat);
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned =
            std::thread::Builder::new().name("zaprust-export".to_owned()).spawn(move || {
                let result = write_export_file(&dir, &base, &markdown);
                let _ = tx.send(result);
            });
        match spawned {
            Ok(_) => self.export_rx = Some(rx),
            Err(e) => self.notice = Some(format!("Export failed: {e}")),
        }
    }
}

/// Picks a fresh export path for `base` (e.g.
/// "zaprust-chat-foo.md") inside `dir`: the name gains a
/// Unix-timestamp suffix, and if that file already exists (two
/// exports of the same chat within one second) a counter is
/// appended until the name is free. An earlier export is never
/// overwritten.
fn unique_export_path(dir: &std::path::Path, base: &str, now: u64) -> std::path::PathBuf {
    let stem = base.strip_suffix(".md").unwrap_or(base);
    let first = dir.join(format!("{stem}-{now}.md"));
    if !first.exists() {
        return first;
    }
    let mut n = 2;
    loop {
        let candidate = dir.join(format!("{stem}-{now}-{n}.md"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Writes one export: creates the exports dir, picks a unique
/// timestamped name, writes the Markdown to a temp file in the
/// same directory and renames it into place, so the visible
/// file is never a half-written export.
fn write_export_file(
    dir: &std::path::Path,
    base: &str,
    markdown: &str,
) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = unique_export_path(dir, base, crate::state::now_unix());
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{file_name}.tmp"));
    std::fs::write(&tmp, markdown).map_err(|e| e.to_string())?;
    // An export is the full chat in plaintext: owner-only on
    // Unix, asserted on the temp so the renamed file inherits
    // it (privacy audit A5).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_clearance_clears_the_floating_composer() {
        // For any stack height, the reserved inset is the stack
        // plus its lift plus the gap: the last bubble can never
        // rest under the composer or flush against it.
        for stack_h in [0.0, 52.0, 84.0, 160.0] {
            assert_eq!(composer_clearance(stack_h), stack_h + COMPOSER_LIFT + STREAM_COMPOSER_GAP);
            assert!(composer_clearance(stack_h) > stack_h + COMPOSER_LIFT);
        }
    }

    #[test]
    fn scroll_button_parks_above_the_composer() {
        // The button's bottom edge sits exactly SCROLL_BUTTON_GAP
        // above the composer stack's top edge, for any stack
        // height: it floats over the gap, never over messages
        // and never over the composer.
        let size = 46.0;
        for stack_h in [52.0, 84.0, 160.0] {
            let top = scroll_bottom_button_top(900.0, stack_h, size);
            let button_bottom = top + size;
            let composer_top = 900.0 - COMPOSER_LIFT - stack_h;
            assert_eq!(composer_top - button_bottom, SCROLL_BUTTON_GAP);
        }
    }

    fn export_test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn export_paths_are_timestamped_and_never_reused() {
        // Audit finding 16: a second export of the same chat
        // must not overwrite the first.
        let dir = export_test_dir("zaprust-export-path-test");
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_export_path(&dir, "zaprust-chat-foo.md", 1_700_000_000);
        assert_eq!(first.file_name().unwrap().to_string_lossy(), "zaprust-chat-foo-1700000000.md");
        // The name is only taken once the file exists; a second
        // export within the same second must move on to a
        // counter suffix instead of reusing it.
        std::fs::write(&first, "one").unwrap();
        let second = unique_export_path(&dir, "zaprust-chat-foo.md", 1_700_000_000);
        assert_eq!(
            second.file_name().unwrap().to_string_lossy(),
            "zaprust-chat-foo-1700000000-2.md"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_export_file_lands_the_full_markdown() {
        let dir = export_test_dir("zaprust-export-write-test");
        let path = write_export_file(&dir, "zaprust-chat-foo.md", "# Chat\nhello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Chat\nhello");
        // The temp file was renamed into place, not left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
