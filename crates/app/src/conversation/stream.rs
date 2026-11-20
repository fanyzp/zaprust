//! The message stream: day chips, the unread divider, the
//! encryption notice, the bubble loop and its scroll
//! management (stick-to-bottom, the scroll-to-bottom button).

use super::*;

impl App {
    /// Draws the message stream into the panel under the
    /// header and dispatches the bubble actions it produced.
    /// Returns the scroll area's output (the scroll-to-bottom
    /// button reads it) and the stream's full rect (the
    /// floating composer zones itself from it).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_stream(
        &mut self,
        ui: &mut egui::Ui,
        chat: &core::model::Chat,
        t: Tokens,
        now: f64,
        composer_clearance: f32,
        pending_actions: &mut Vec<BubbleAction>,
        engaged: &mut bool,
    ) -> (egui::scroll_area::ScrollAreaOutput<()>, egui::Rect) {
        let available = ui.available_size();
        let (full_rect, _) =
            ui.allocate_exact_size(Vec2::new(available.x, available.y.max(120.0)), Sense::hover());
        // The chat background is the panel's solid bg_chat fill;
        // the messages sit directly on it.
        let col_margin = side_margin(full_rect.width());
        let rect = full_rect.shrink2(Vec2::new(col_margin, 0.0));
        let mut child = ui
            .new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)));
        // Whether a starred-locate target is pending at the start
        // of this draw. The locate is one-shot: it is cleared after
        // this pass whether or not the target rendered as a bubble
        // (see the clear after the closure), so a target that lives
        // in an album run or a system row can never wedge
        // stick-to-bottom off for this view.
        let locate_pending = self.scroll_to_message.is_some();
        let scroll_out = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(!self.qa_scroll_top && self.scroll_to_message.is_none())
            .show(&mut child, |ui| {
                ui.add_space(10.0);
                if self.live_mode {
                    // History sync is a live-mode concept; in
                    // demo mode there is no phone to load from.
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("Loading older messages from your phone…")
                                .size(11.0)
                                .color(rgb(t.text_faint)),
                        );
                    });
                    ui.add_space(4.0);
                }
                // An empty conversation still opens with
                // the encryption notice (below, the loop
                // paints it under the first day chip when
                // there are messages).
                if self.conversation.is_empty() {
                    encryption_notice(ui, t);
                    ui.add_space(8.0);
                }
                let mut actions: Vec<BubbleAction> = Vec::new();
                crate::anim::begin_message_stream(ui, chat.id);
                let total = self.conversation.len();
                // Separators are derived from the message
                // data, not hardcoded: a day chip wherever
                // the day label changes, and the unread
                // divider above the first unread message.
                // The divider is the snapshot taken when
                // the chat was opened, so the
                // read-by-action flush clearing the badge
                // as the user engages cannot move or
                // remove it mid-view.
                let divider = self.unread_divider;
                // Group sender portraits, resolved up front: the
                // divider closure below borrows the conversation
                // for the whole loop, so textures load before it
                // exists.
                let sender_photos: std::collections::HashMap<String, egui::TextureId> = {
                    let names: std::collections::HashSet<String> =
                        self.conversation.iter().filter_map(|m| m.sender.clone()).collect();
                    let mut map = std::collections::HashMap::new();
                    for name in names {
                        if let Some(tex) = self.demo_avatar(ui.ctx(), &name) {
                            map.insert(name, tex);
                        }
                    }
                    map
                };
                let divider_before = |i: usize| {
                    divider.is_some_and(|d| d.first_unread_id == self.conversation[i].id)
                };
                // Sender grouping (WhatsApp's rhythm):
                // consecutive messages from the same sender
                // pack tight (3 px) and share one sender
                // name and one tail (both on the first);
                // groups separate by 14 px. System messages
                // break a group, and so do day boundaries
                // and the unread divider.
                // A manual index (not an iterator) so an album run
                // can consume its whole run of messages in one step.
                let mut idx = 0;
                while idx < total {
                    let msg = &self.conversation[idx];
                    let day_break =
                        idx == 0 || msg.day_label() != self.conversation[idx - 1].day_label();
                    if day_break {
                        day_chip(ui, t, msg.day_label());
                        ui.add_space(8.0);
                    } else if idx > 0 && time_chip_between(&self.conversation[idx - 1], msg) {
                        // A centered time chip when a real pause
                        // separates the runs, the reference
                        // wireframe's "9:41 PM" separator.
                        day_chip(ui, t, &msg.time);
                        ui.add_space(8.0);
                    }
                    // The encryption notice is a rendered
                    // row, never a stored message: it opens
                    // the stream under the first day
                    // separator, once per conversation view.
                    // It is painted outside the bubbles, so
                    // grouping, the unread divider and the
                    // scroll math never see it.
                    if idx == 0 {
                        encryption_notice(ui, t);
                        ui.add_space(8.0);
                    }
                    if divider_before(idx) {
                        if let Some(d) = divider {
                            unread_divider_row(ui, t, d.count);
                        }
                        ui.add_space(8.0);
                    }
                    let sep_break = day_break || divider_before(idx);
                    // Album: two or more consecutive photos or videos
                    // from the same sender collapse into one grid
                    // card; the run is consumed in a single step.
                    let run = album_run_len(&self.conversation, idx);
                    if run >= 2 {
                        let run_items = &self.conversation[idx..idx + run];
                        let (alpha, rise) = crate::anim::message_entrance(
                            ui,
                            chat.id,
                            msg.id,
                            idx + 3 >= total,
                            self.motion.fades(),
                        );
                        let rise = if self.motion.slides() { rise } else { 0.0 };
                        if rise > 0.01 {
                            ui.add_space(rise);
                        }
                        if self.media.photo_texture.is_none() {
                            self.media.photo_texture = Some(crate::media::demo_photo(ui.ctx()));
                        }
                        let album_actions = ui
                            .scope(|ui| {
                                if alpha < 0.999 {
                                    ui.set_opacity(alpha);
                                }
                                crate::bubble::message_album(
                                    ui,
                                    run_items,
                                    t,
                                    &self.media.textures,
                                    self.media.photo_texture.as_ref().map(egui::TextureHandle::id),
                                )
                            })
                            .inner;
                        actions.extend(album_actions);
                        // Reaction pills stay per message under the
                        // grid, exactly as they hang under bubbles.
                        for item in run_items {
                            self.draw_message_strip(ui, item);
                        }
                        let reacted = run_items.last().is_some_and(|m| !m.reactions.is_empty());
                        ui.add_space(if reacted { 13.0 } else { 14.0 });
                        idx += run;
                        continue;
                    }
                    // System notices (group events, timer
                    // changes) read as centered chips, WhatsApp-style,
                    // never as a participant's bubble.
                    if msg.kind == core::model::MessageKind::System {
                        system_chip(ui, t, &msg.body);
                        ui.add_space(14.0);
                        idx += 1;
                        continue;
                    }
                    let key = group_key(msg);
                    let group_first = sep_break
                        || match (key, idx.checked_sub(1).map(|i| &self.conversation[i])) {
                            (Some(k), Some(prev)) => group_key(prev) != Some(k),
                            _ => true,
                        };
                    let next_sep_break = idx + 1 < total
                        && (self.conversation[idx + 1].day_label() != msg.day_label()
                            || divider_before(idx + 1));
                    let group_last = next_sep_break
                        || match (key, self.conversation.get(idx + 1)) {
                            (Some(k), Some(next)) => group_key(next) != Some(k),
                            _ => true,
                        };
                    // Group runs of incoming messages carry
                    // the sender's avatar: the column is
                    // reserved for the whole run, the disc
                    // paints under its last bubble.
                    let group_incoming = chat.kind == core::model::ChatKind::Group
                        && !msg.is_mine()
                        && msg.kind != core::model::MessageKind::System;
                    let media = MediaCtx {
                        photo: self.media.photo_texture.as_ref().map(egui::TextureHandle::id),
                        own_photo: self.media.textures.get(&msg.id).map(egui::TextureHandle::id),
                        voice: match &self.media.voice_playing {
                            Some(vp) if vp.id == msg.id => {
                                Some(crate::bubble::VoiceSnap {
                                    progress: vp.progress(now),
                                    playing: vp.is_playing(),
                                    // The pill shows the rate the
                                    // session is really running at.
                                    speed: vp.rate as f32,
                                })
                            }
                            _ => None,
                        },
                        view_once_opened: self.media.opened_view_once.contains(&msg.id),
                        sender_photo: if group_incoming {
                            msg.sender.as_deref().and_then(|name| sender_photos.get(name).copied())
                        } else {
                            None
                        },
                    };
                    // Entrance: a genuinely new arrival near
                    // the end of the stream fades in and
                    // rises about 10 px; history opening or
                    // syncing in never replays (the anim
                    // helpers track what was already shown).
                    let (alpha, rise) = crate::anim::message_entrance(
                        ui,
                        chat.id,
                        msg.id,
                        idx + 3 >= total,
                        self.motion.fades(),
                    );
                    // The Motion setting's contract: Reduced
                    // keeps fades but drops slides.
                    let rise = if self.motion.slides() { rise } else { 0.0 };
                    if rise > 0.01 {
                        ui.add_space(rise);
                    }
                    let bubble_scope = ui.scope(|ui| {
                        if alpha < 0.999 {
                            ui.set_opacity(alpha);
                        }
                        message_bubble(
                            ui,
                            msg,
                            chat,
                            t,
                            media,
                            crate::bubble::BubbleLayout {
                                group_first,
                                avatar_column: group_incoming,
                                show_avatar: group_incoming && group_last,
                            },
                        )
                    });
                    let action = bubble_scope.inner;
                    let bubble_rect = bubble_scope.response.rect;
                    if let Some(action) = action {
                        actions.push(action);
                    }
                    // Starred locate: scroll the target message to
                    // the centre of the stream, with the same
                    // animation the scroll-to-bottom path uses.
                    if self.scroll_to_message == Some(msg.id) {
                        ui.scroll_to_rect_animation(
                            bubble_rect,
                            Some(egui::Align::Center),
                            egui::style::ScrollAnimation::new(
                                2600.0,
                                egui::emath::Rangef::new(0.18, 0.22),
                            ),
                        );
                    }
                    // Starred locate highlight: an accent wash over
                    // the bubble that fades out over 1.8 s (long
                    // enough to survive the scroll travel).
                    if self.highlight_message == Some(msg.id) {
                        let now_t = ui.ctx().input(|i| i.time);
                        if self.highlight_started.is_none() {
                            self.highlight_started = Some(now_t);
                        }
                        let start = self.highlight_started.unwrap_or(now_t);
                        let progress = ((now_t - start) / 1.8) as f32;
                        if progress < 1.0 {
                            let alpha = ((1.0 - progress) * 70.0) as u8;
                            let mut wash = rgb(t.accent);
                            wash = egui::Color32::from_rgba_unmultiplied(
                                wash.r(),
                                wash.g(),
                                wash.b(),
                                alpha,
                            );
                            ui.painter().rect_filled(
                                bubble_rect.expand(3.0),
                                CornerRadius::same(crate::bubble::BUBBLE_RADIUS + 2),
                                wash,
                            );
                            ui.ctx().request_repaint();
                        } else {
                            self.highlight_message = None;
                            self.highlight_started = None;
                        }
                    }
                    self.draw_message_strip(ui, msg);
                    // Grouped spacing: tight inside a
                    // sender group, a real breath between
                    // groups; reaction pills keep their
                    // larger clearance either way.
                    ui.add_space(if !msg.reactions.is_empty() {
                        13.0
                    } else if group_last {
                        14.0
                    } else {
                        3.0
                    });
                    idx += 1;
                }
                crate::anim::end_message_stream(ui, chat.id);
                *pending_actions = actions;
                ui.add_space(4.0);
                ui.add_space(composer_clearance);
                // The scroll-to-bottom button's request:
                // point the cursor at the very end of the
                // content and let the scroll area animate
                // to it over about 200 ms (the animation
                // span clamps the duration).
                if self.scroll_bottom_requested {
                    self.scroll_bottom_requested = false;
                    ui.scroll_to_cursor_animation(
                        Some(Align::BOTTOM),
                        egui::style::ScrollAnimation::new(
                            2600.0,
                            egui::emath::Rangef::new(0.18, 0.22),
                        ),
                    );
                }
            });
        if locate_pending {
            // One-shot clear (see locate_pending above): a matched
            // target was scrolled to during this pass; an unmatched
            // one gets no second chance. A highlight that never
            // stamped its start (its message never rendered as a
            // bubble) is cleared with it; a stamped one keeps
            // fading on its own clock.
            self.scroll_to_message = None;
            if self.highlight_started.is_none() {
                self.highlight_message = None;
            }
        }

        // Read-by-action: the user actively scrolled the
        // stream (wheel, trackpad, End or PageDown over the
        // message area) while the newest messages are on
        // screen. The scroll area being parked at the
        // bottom on its own (stick-to-bottom after an
        // arrival) never counts; see live.rs.
        let at_bottom = scroll_out.content_size.y
            - (scroll_out.state.clamped_offset().y + scroll_out.inner_rect.height())
            <= 48.0;
        if at_bottom {
            let (wheel, key_nav, hovering) = ui.ctx().input(|i| {
                (
                    i.smooth_scroll_delta.y != 0.0
                        || i.events.iter().any(|e| matches!(e, egui::Event::MouseWheel { .. })),
                    i.key_pressed(egui::Key::End) || i.key_pressed(egui::Key::PageDown),
                    i.pointer.hover_pos().is_some_and(|p| scroll_out.inner_rect.contains(p)),
                )
            });
            if hovering && (wheel || key_nav) {
                *engaged = true;
            }
        }

        for action in pending_actions.drain(..) {
            match action {
                BubbleAction::Download(id) => {
                    self.download_media(id);
                }
                BubbleAction::ToggleVoice(id) => {
                    let secs = self
                        .conversation
                        .iter()
                        .find(|m| m.id == id)
                        .and_then(|m| match m.kind {
                            core::model::MessageKind::Voice { secs, .. } => Some(secs),
                            _ => None,
                        })
                        .unwrap_or(0);
                    match &mut self.media.voice_playing {
                        Some(vp) if vp.id == id => vp.toggle_pause(now),
                        // A fresh session runs at the rate
                        // stored for this message.
                        _ => {
                            self.media.voice_playing = Some(
                                VoicePlayback::new(id, secs, now)
                                    .with_rate(f64::from(self.voice_speed(id))),
                            );
                        }
                    }
                }
                BubbleAction::CycleVoiceSpeed(id) => {
                    let rate = self.cycle_voice_speed(id);
                    // The pill drives the live session too:
                    // the rate change rebases the playhead,
                    // it never just relabels it.
                    if let Some(vp) = &mut self.media.voice_playing {
                        if vp.id == id {
                            vp.set_rate(f64::from(rate), now);
                        }
                    }
                }
                BubbleAction::SeekVoice(id, fraction) => {
                    let secs = self
                        .conversation
                        .iter()
                        .find(|m| m.id == id)
                        .and_then(|m| match m.kind {
                            core::model::MessageKind::Voice { secs, .. } => Some(secs),
                            _ => None,
                        })
                        .unwrap_or(0);
                    match &mut self.media.voice_playing {
                        Some(vp) if vp.id == id => {
                            vp.seek_to(f64::from(fraction), now);
                        }
                        // No session yet: start one at the
                        // sought position, at the stored rate.
                        _ => {
                            let mut vp = VoicePlayback::new(id, secs, now)
                                .with_rate(f64::from(self.voice_speed(id)));
                            vp.seek_to(f64::from(fraction), now);
                            self.media.voice_playing = Some(vp);
                        }
                    }
                }
                BubbleAction::OpenViewOnce(id) => {
                    self.media.viewing_photo = Some(id);
                }
                BubbleAction::OpenPhoto(id) => {
                    self.media.viewing_photo = Some(id);
                }
                BubbleAction::Vote(id, idx) => {
                    self.vote_poll(id, idx);
                }
                BubbleAction::ContextMenu(id) => {
                    self.bubble_menu = Some(id);
                }
            }
        }
        if let Some(vp) = &self.media.voice_playing {
            if vp.progress(now) >= 1.0 {
                self.media.voice_playing = None;
            } else if vp.is_playing() {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(40));
            }
        }

        (scroll_out, full_rect)
    }

    /// The scroll-to-bottom button: a floating glass circle
    /// that fades in once the user has scrolled well away from
    /// the newest messages, badged with the open chat's unread
    /// count. Clicking it requests the stream's animated scroll
    /// and counts as read engagement.
    pub(crate) fn draw_scroll_bottom(
        &mut self,
        ui: &mut egui::Ui,
        full_rect: egui::Rect,
        chat: &core::model::Chat,
        scroll_out: &egui::scroll_area::ScrollAreaOutput<()>,
        t: Tokens,
        engaged: &mut bool,
    ) {
        let fmargin = side_margin(full_rect.width());
        let dist_bottom = scroll_out.content_size.y
            - (scroll_out.state.clamped_offset().y + scroll_out.inner_rect.height());
        let btn_p = crate::anim::scalar(
            ui,
            ("scroll_bottom_btn", chat.id),
            if dist_bottom > 480.0 { 1.0 } else { 0.0 },
            (crate::motion::dur::REVEAL / 1000.0) as f32,
        );
        if btn_p > 0.01 {
            let size = 46.0;
            let btn_pos = Pos2::new(
                full_rect.right() - fmargin - size,
                full_rect.bottom() - 24.0 - self.composer.stack_h - size,
            );
            let mut btn_clicked = false;
            egui::Area::new(egui::Id::unique("scroll_to_bottom"))
                .fixed_pos(btn_pos)
                .order(egui::Order::Foreground)
                .show(ui.ctx(), |ui| {
                    if btn_p < 0.999 {
                        ui.set_opacity(btn_p);
                    }
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
                    let painter = ui.painter();
                    painter
                        .add(crate::theme::glass_shadow(t).as_shape(rect, CornerRadius::same(23)));
                    painter.circle_filled(
                        rect.center(),
                        23.0,
                        crate::theme::glass_tint(t, t.bg_panel),
                    );
                    painter.circle_stroke(rect.center(), 23.0, Stroke::new(1.0_f32, rgb(t.border)));
                    crate::icons::draw(
                        painter,
                        crate::icons::Icon::ChevronDown,
                        rect.center(),
                        26.0,
                        rgb(t.text_secondary),
                        rgb(t.bg_panel),
                    );
                    if chat.unread > 0 {
                        let (bfill, btext) = if chat.muted {
                            crate::theme::muted_badge(t)
                        } else {
                            (rgb(t.badge_bg), rgb(t.badge_text))
                        };
                        let label = chat.unread.to_string();
                        let font = FontId::proportional(11.0);
                        let text_w =
                            painter.layout_no_wrap(label.clone(), font.clone(), btext).size().x;
                        let bw = (text_w + 11.0).max(20.0);
                        let scale = crate::anim::pop_scale(
                            ui,
                            ("scroll_bottom_badge", chat.id),
                            chat.unread,
                        );
                        let brect = egui::Rect::from_center_size(
                            Pos2::new(rect.right() - 2.0, rect.top() + 2.0),
                            Vec2::new(bw * scale, 20.0 * scale),
                        );
                        painter.rect_filled(brect, CornerRadius::same(10), bfill);
                        painter.text(
                            brect.center(),
                            egui::Align2::CENTER_CENTER,
                            label,
                            font,
                            btext,
                        );
                    }
                    if resp.clicked() {
                        btn_clicked = true;
                    }
                });
            if btn_clicked {
                self.scroll_bottom_requested = true;
                *engaged = true;
            }
        }
    }
}

/// Parses "HH:MM" (the stream's clock format) into minutes after
/// midnight; None for times that do not parse (live echoes use
/// "…" until the backend upsert lands).
fn clock_minutes(time: &str) -> Option<u32> {
    let (h, m) = time.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Whether a centered time chip belongs between these two
/// consecutive messages: both show parseable clocks and they sit
/// 30+ minutes apart (wrapping past midnight counts as a gap;
/// day boundaries already carry their own chip).
fn time_chip_between(prev: &Message, msg: &Message) -> bool {
    const GAP_MIN: u32 = 30;
    match (clock_minutes(&prev.time), clock_minutes(&msg.time)) {
        (Some(a), Some(b)) => {
            let delta = if b >= a { b - a } else { b + 24 * 60 - a };
            delta >= GAP_MIN
        }
        _ => false,
    }
}

/// The sender-group key for message grouping: two adjacent
/// messages belong to the same group when their keys match.
/// System messages key to `None`, which matches nothing, so
/// they break any group they sit in.
fn group_key(m: &Message) -> Option<(bool, Option<&str>)> {
    if m.kind == core::model::MessageKind::System {
        None
    } else {
        Some((m.is_mine(), m.sender.as_deref()))
    }
}

/// Whether a message can sit inside an album grid: a plain photo or
/// video, not the view-once kind (those carry their own open-once
/// flow) and not a reply (a quote makes the bubble stand alone).
fn is_album_item(m: &Message) -> bool {
    matches!(
        &m.kind,
        core::model::MessageKind::Image { view_once: false, .. }
            | core::model::MessageKind::Video { view_once: false, .. }
    )
}

/// The length of the album run starting at `start`: consecutive
/// photos or videos from the same sender on the same day with no
/// quote of their own. Runs shorter than two never reach the grid
/// (the caller draws a normal bubble for them).
fn album_run_len(msgs: &[Message], start: usize) -> usize {
    let first = &msgs[start];
    if !is_album_item(first) || first.quoted_body.is_some() {
        return 1;
    }
    let mut n = 1;
    while start + n < msgs.len() {
        let next = &msgs[start + n];
        if !is_album_item(next)
            || next.quoted_body.is_some()
            || group_key(next) != group_key(first)
            || next.day_label() != first.day_label()
        {
            break;
        }
        n += 1;
    }
    n
}

/// A small padlock drawn from primitives (the bundled icon font
/// carries no lock glyph): a shackle arc over a rounded body,
/// with the keyhole punched in the surface's own color.
fn paint_lock(painter: &egui::Painter, center: Pos2, color: Color32, bg: Color32) {
    let c = Pos2::new(center.x, center.y - 1.5);
    let r = 3.4;
    let mut pts = Vec::with_capacity(9);
    for i in 0..=8 {
        let a = std::f32::consts::PI * (1.0 - i as f32 / 8.0);
        pts.push(Pos2::new(c.x + r * a.cos(), c.y - r * a.sin()));
    }
    painter.add(egui::Shape::line(pts, Stroke::new(1.4_f32, color)));
    let body =
        egui::Rect::from_center_size(Pos2::new(center.x, center.y + 2.6), Vec2::new(10.5, 8.0));
    painter.rect_filled(body, CornerRadius::same(2), color);
    painter.circle_filled(body.center() + Vec2::new(0.0, -0.8), 1.5, bg);
    painter.rect_filled(
        egui::Rect::from_center_size(body.center() + Vec2::new(0.0, 1.2), Vec2::new(1.8, 3.0)),
        CornerRadius::same(1),
        bg,
    );
}

/// The end-to-end encryption notice: a centered system chip
/// with a padlock, rendered at the top of every conversation
/// (under the first day separator, or on its own when the
/// conversation is empty). Rendered, never a stored message, so
/// it appears exactly once per conversation view and cannot be
/// quoted, reacted to or counted as history.
fn encryption_notice(ui: &mut egui::Ui, t: Tokens) {
    ui.vertical_centered(|ui| {
        egui::Frame::new()
            .fill(rgb(t.system_chip))
            .corner_radius(CornerRadius::same(8))
            .stroke(Stroke::new(1.0_f32, rgb(t.border)))
            .inner_margin(Margin::symmetric(10, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (slot, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    paint_lock(ui.painter(), slot.center(), rgb(t.system_text), rgb(t.system_chip));
                    ui.label(
                        RichText::new(crate::i18n::t("conversation.encryption_notice"))
                            .size(12.0)
                            .color(rgb(t.system_text)),
                    );
                });
            });
    });
}

/// A system notice in the stream: a centered chip in the system
/// palette (same grammar as the encryption notice), so group events
/// never read as a participant's message.
fn system_chip(ui: &mut egui::Ui, t: Tokens, label: &str) {
    ui.vertical_centered(|ui| {
        egui::Frame::new()
            .fill(rgb(t.system_chip))
            .corner_radius(CornerRadius::same(8))
            .stroke(Stroke::new(1.0_f32, rgb(t.border)))
            .inner_margin(Margin::symmetric(10, 5))
            .show(ui, |ui| {
                ui.label(RichText::new(label).size(12.0).color(rgb(t.system_text)));
            });
    });
}

/// A day separator in the message stream: a centered glass chip
/// carrying the day label ("Today", "Yesterday", or a full
/// date), painted wherever the messages' day data changes.
fn day_chip(ui: &mut egui::Ui, t: Tokens, label: &str) {
    // The chip floats over the chat background, so it wears the
    // glass treatment: a translucent chip fill tinted toward the
    // seed, the specular top edge, a hairline border and the card
    // shadow lifting it off the background. It is painted manually
    // at its measured size (like the unread divider's chip) so it
    // always hugs its label as a small centered pill instead of
    // stretching with the layout.
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::hover());
    let painter = ui.painter();
    let font = FontId::proportional(11.5);
    let text_w = ui.ctx().fonts_mut(|f| {
        f.layout_no_wrap(label.to_owned(), font.clone(), rgb(t.text_secondary)).size().x
    });
    let chip_rect = egui::Rect::from_center_size(rect.center(), Vec2::new(text_w + 24.0, 24.0));
    painter.add(egui::Shape::Rect(
        crate::theme::card_shadow(t).as_shape(chip_rect, CornerRadius::same(12)),
    ));
    painter.rect(
        chip_rect,
        CornerRadius::same(12),
        crate::theme::glass_tint(t, t.system_chip),
        Stroke::new(1.0_f32, rgb(t.border)),
        egui::StrokeKind::Inside,
    );
    crate::theme::paint_glass_edge(painter, chip_rect, 12.0, t);
    painter.text(
        chip_rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        font,
        rgb(t.text_secondary),
    );
}

/// The unread divider: a hairline across the stream with a
/// glass chip in the middle carrying the unread count as it was
/// when the chat was opened.
fn unread_divider_row(ui: &mut egui::Ui, t: Tokens, count: u32) {
    let label = if count == 1 {
        crate::i18n::t("conversation.unread_one")
    } else {
        crate::i18n::tf("conversation.unread_messages", "count", &count.to_string())
    };
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::hover());
    let painter = ui.painter();
    painter.line_segment(
        [Pos2::new(rect.left(), rect.center().y), Pos2::new(rect.right(), rect.center().y)],
        Stroke::new(1.0_f32, rgb(t.border)),
    );
    let font = FontId::proportional(11.5);
    let text_w = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(label.clone(), font.clone(), rgb(t.accent)).size().x);
    let chip_rect = egui::Rect::from_center_size(rect.center(), Vec2::new(text_w + 24.0, 22.0));
    painter.add(egui::Shape::Rect(
        crate::theme::card_shadow(t).as_shape(chip_rect, CornerRadius::same(11)),
    ));
    painter.rect_filled(
        chip_rect,
        CornerRadius::same(11),
        crate::theme::glass_tint(t, t.system_chip),
    );
    painter.rect_stroke(
        chip_rect,
        CornerRadius::same(11),
        Stroke::new(1.0_f32, rgb(t.border)),
        egui::StrokeKind::Inside,
    );
    crate::theme::paint_glass_edge(painter, chip_rect, 11.0, t);
    painter.text(chip_rect.center(), egui::Align2::CENTER_CENTER, label, font, rgb(t.accent));
}

#[cfg(test)]
mod tests {
    use super::{clock_minutes, time_chip_between};
    use core::model::Message;

    #[test]
    fn clock_minutes_parses_the_stream_clock() {
        assert_eq!(clock_minutes("17:44"), Some(17 * 60 + 44));
        assert_eq!(clock_minutes("0:00"), Some(0));
        assert_eq!(clock_minutes("23:59"), Some(23 * 60 + 59));
        assert_eq!(clock_minutes("…"), None);
        assert_eq!(clock_minutes("24:00"), None);
        // Lenient about unpadded parts: "9:5" is 9:05.
        assert_eq!(clock_minutes("9:5"), Some(9 * 60 + 5));
    }

    #[test]
    fn time_chip_appears_after_a_real_pause_only() {
        let earlier = Message::mine(1, String::new(), "17:10");
        let later = Message::mine(2, String::new(), "17:41");
        assert!(time_chip_between(&earlier, &later), "31 minutes apart");
        let soon = Message::mine(3, String::new(), "17:20");
        assert!(!time_chip_between(&earlier, &soon), "10 minutes apart");
        let midnight_next = Message::mine(4, String::new(), "0:05");
        assert!(time_chip_between(&earlier, &midnight_next), "wraps past midnight");
        let echo = Message::mine(5, String::new(), "…");
        assert!(!time_chip_between(&earlier, &echo), "unparseable clock");
    }
}
