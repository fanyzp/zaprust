//! Calls panel: the call history as a LEFT panel (the WhatsApp
//! / Telegram pattern for secondary lists), while the
//! conversation stays on the right. The anatomy follows the
//! reference call log: a header with a back chevron, the title
//! and a new call button; a segmented All / Missed control; the
//! panel's search; a "Recent" section label; and rows with the
//! contact's avatar and name (missed calls in the danger
//! color), a direction glyph with the type and duration line, a
//! date, and an info button that opens the contact's info dock.
//! A footer hint at the panel bottom explains the new call
//! button.
//!
//! Clicks are wired honestly: Zaprust's call flow is
//! backend-driven and exists only in live mode (the offline
//! demo has no call path at all), so a row click or a picker
//! choice opens the chat in demo mode, and in live mode opens
//! the chat and starts the call through the same header path
//! the conversation uses. In live mode there is no call-log
//! source in the store or backend yet, so the panel shows its
//! empty state instead of demo data; the new call picker still
//! works there, because it lists chats, not history.

use crate::state::{App, Screen};
use crate::theme::rgb;
use core::filter::ChatFilter;
use core::model::{CallDirection, CallEntry, CallKind, Chat, ChatKind};
use eframe::egui::{self, Align2, CornerRadius, FontId, Margin, Pos2, RichText, Sense, Vec2};

/// The demo call history, seeded from the demo contacts and
/// spread across the past week relative to `now` (unix
/// seconds): a mix of voice and video, incoming and outgoing
/// and missed, with and without recorded durations. Entries 1
/// and 10 are a consecutive pair from the same contact so the
/// grouped count shows in the demo list.
pub(crate) fn demo_call_log(now: i64) -> Vec<CallEntry> {
    const H: i64 = 3_600;
    const D: i64 = 86_400;
    let entry = |id: u64,
                 chat_id: u64,
                 kind: CallKind,
                 direction: CallDirection,
                 missed: bool,
                 duration_secs: u32,
                 timestamp: i64| CallEntry {
        id,
        chat_id,
        kind,
        direction,
        missed,
        duration_secs,
        timestamp,
    };
    vec![
        entry(1, 4, CallKind::Voice, CallDirection::Incoming, false, 131, now - 2 * H),
        entry(10, 4, CallKind::Voice, CallDirection::Incoming, false, 89, now - 3 * H),
        entry(2, 2, CallKind::Video, CallDirection::Outgoing, false, 45, now - 5 * H),
        entry(3, 7, CallKind::Voice, CallDirection::Incoming, true, 0, now - 26 * H),
        entry(4, 4, CallKind::Voice, CallDirection::Outgoing, false, 25, now - 30 * H),
        entry(5, 11, CallKind::Voice, CallDirection::Incoming, true, 0, now - 2 * D - H),
        entry(6, 2, CallKind::Voice, CallDirection::Incoming, false, 367, now - 3 * D),
        entry(7, 7, CallKind::Video, CallDirection::Incoming, true, 0, now - 4 * D - 2 * H),
        entry(8, 4, CallKind::Video, CallDirection::Outgoing, false, 754, now - 5 * D),
        entry(9, 11, CallKind::Voice, CallDirection::Outgoing, false, 61, now - 6 * D - H / 2),
    ]
}

/// Talk time the way the reference status lines write it:
/// "2 min 11 s", "45 s", "2 min", "1 h 5 min".
fn format_duration_label(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h} h {m} min")
    } else if m > 0 {
        if s > 0 {
            format!("{m} min {s} s")
        } else {
            format!("{m} min")
        }
    } else {
        format!("{s} s")
    }
}

/// The row's status line: "Missed voice call", "Voice call ·
/// 2 min 11 s". Missed calls never carry a duration, and a
/// connected call whose duration the source did not record
/// shows no suffix either.
fn status_line(entry: &CallEntry) -> String {
    let base = crate::i18n::t(match (entry.missed, entry.kind) {
        (true, CallKind::Voice) => "calls.missed_voice",
        (true, CallKind::Video) => "calls.missed_video",
        (false, CallKind::Voice) => "calls.voice",
        (false, CallKind::Video) => "calls.video",
    });
    if !entry.missed && entry.duration_secs > 0 {
        format!("{base} · {}", format_duration_label(entry.duration_secs))
    } else {
        base
    }
}

/// The row's right-aligned day: "Today" or "Yesterday" while
/// the day formatter says so, the weekday name after that (the
/// formatter's full-date labels resolve back to their weekday
/// through the same UTC day grid).
fn relative_day(timestamp: i64) -> String {
    let label = crate::live::fmt_day(timestamp);
    if label == "Today" || label == "Yesterday" {
        return label;
    }
    const WEEKDAYS: [&str; 7] =
        ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    // Day 0 of the unix epoch was a Thursday.
    WEEKDAYS[((timestamp.div_euclid(86_400) + 4).rem_euclid(7)) as usize].to_owned()
}

/// The direction glyph a row shows: the missed glyph for a
/// missed call whatever its direction, the made / received
/// glyphs for connected calls.
fn direction_icon(entry: &CallEntry) -> crate::icons::Icon {
    if entry.missed {
        crate::icons::Icon::CallMissed
    } else if entry.direction == CallDirection::Outgoing {
        crate::icons::Icon::CallMade
    } else {
        crate::icons::Icon::CallReceived
    }
}

/// One display row: the newest entry of a run of consecutive
/// entries for the same chat, direction and outcome, plus how
/// many entries the run holds. The reference writes the count
/// after the name ("Name (2)"); the row's status line and date
/// are the newest entry's, the same thing the reference shows.
struct CallRow<'a> {
    entry: &'a CallEntry,
    count: usize,
}

/// Folds a newest-first entry list into display rows, merging
/// consecutive entries that share chat, direction and outcome.
/// Entries for the same chat separated by another chat's entry
/// stay separate rows, exactly like the reference log.
fn group_calls<'a>(entries: &[&'a CallEntry]) -> Vec<CallRow<'a>> {
    let mut rows: Vec<CallRow<'a>> = Vec::with_capacity(entries.len());
    for entry in entries {
        match rows.last_mut() {
            Some(last)
                if last.entry.chat_id == entry.chat_id
                    && last.entry.direction == entry.direction
                    && last.entry.missed == entry.missed =>
            {
                last.count += 1;
            }
            _ => rows.push(CallRow { entry, count: 1 }),
        }
    }
    rows
}

impl App {
    /// Draws the Calls panel in place of the chat list and
    /// applies row, info and header clicks. Entering the panel
    /// never touches the open conversation; a row click calls
    /// the entry's chat (opens it, and starts the call in live
    /// mode), the info button opens its info dock.
    pub(crate) fn draw_calls_panel(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let photos = self.demo_photos(root.ctx());
        // Live mode has no call-log source yet: the list is
        // honestly empty there rather than filled with demo
        // history.
        let source: &[CallEntry] = if self.live_mode { &[] } else { &self.calls.entries };
        let query = self.calls.search.trim().to_lowercase();
        let visible: Vec<&CallEntry> = core::filter::filter_calls(source, self.calls.missed_only)
            .into_iter()
            .filter(|e| {
                query.is_empty()
                    || self
                        .chats
                        .iter()
                        .find(|c| c.id == e.chat_id)
                        .is_some_and(|c| c.title.to_lowercase().contains(&query))
            })
            .collect();
        let rows = group_calls(&visible);
        let source_empty = source.is_empty();
        let mut open: Option<u64> = None;
        let mut info: Option<u64> = None;
        let mut back = false;
        let mut new_call = false;
        let panel = self
            .list_panel(root)
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)).inner_margin(Margin::symmetric(10, 8)))
            .show(root, |ui| {
                // Header: back to the chat list, the title, and
                // the new call button at the right edge.
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
                        RichText::new(crate::i18n::t("nav.calls"))
                            .font(crate::theme::semibold(17.0))
                            .color(rgb(t.text_primary)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::widgets::icon_button(
                            ui,
                            t,
                            crate::icons::Icon::AddCall,
                            32.0,
                            &crate::i18n::t("calls.new_call"),
                            Some(t.accent),
                        )
                        .clicked()
                        {
                            new_call = true;
                        }
                    });
                });
                ui.add_space(10.0);
                // Segmented All / Missed control, full width.
                let selected = usize::from(self.calls.missed_only);
                if let Some(idx) = crate::widgets::segmented(
                    ui,
                    [&crate::i18n::t("calls.all"), &crate::i18n::t("calls.missed")],
                    selected,
                    t,
                ) {
                    self.calls.missed_only = idx == 1;
                }
                ui.add_space(8.0);
                // Search pill, the shared list-panel component.
                crate::widgets::search_pill(
                    ui,
                    t,
                    egui::Id::unique("calls_search"),
                    &crate::i18n::t("calls.search_hint"),
                    &mut self.calls.search,
                );
                ui.add_space(10.0);
                // The footer hint, laid out before the content
                // so the content region can leave exactly its
                // height at the bottom: the hint is a galley
                // with a hard wrap width (a Label asks its
                // layout for the one-line width and never
                // wraps, so at the minimum panel width it bled
                // under the conversation), painted under the
                // content region below.
                let width = ui.available_width();
                let mut job = egui::text::LayoutJob::single_section(
                    crate::i18n::t("calls.footer_hint"),
                    egui::TextFormat {
                        font_id: FontId::proportional(12.0),
                        color: rgb(t.text_faint),
                        ..Default::default()
                    },
                );
                job.wrap.max_width = width;
                // No halign on the job: a center-aligned job's
                // galley paints its rows offset out of the
                // rect it was measured for. The block is
                // centered by hand at paint time instead.
                let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
                let footer_h = 6.0 + galley.size().y + 8.0;
                let content_h = (ui.available_height() - footer_h).max(0.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(width, content_h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        if source_empty {
                            crate::widgets::empty_state(ui, t, &crate::i18n::t("calls.empty"));
                        } else if rows.is_empty() {
                            crate::widgets::empty_state(ui, t, &crate::i18n::t("calls.no_match"));
                        } else {
                            ui.label(
                                RichText::new(crate::i18n::t("calls.recent"))
                                    .font(crate::theme::semibold(12.5))
                                    .color(rgb(t.text_faint)),
                            );
                            ui.add_space(4.0);
                            egui::ScrollArea::vertical().auto_shrink([false, false]).show(
                                ui,
                                |ui| {
                                    for row in &rows {
                                        let entry = row.entry;
                                        let chat: Option<&Chat> =
                                            self.chats.iter().find(|c| c.id == entry.chat_id);
                                        let (rect, resp) = ui.allocate_exact_size(
                                            Vec2::new(
                                                ui.available_width(),
                                                crate::chat_list::ROW_H,
                                            ),
                                            Sense::click(),
                                        );
                                        let hov = crate::anim::hover_factor(
                                            ui,
                                            ("call_row", entry.id),
                                            resp.hovered(),
                                        );
                                        if hov > 0.001 {
                                            ui.painter().rect_filled(
                                                rect,
                                                CornerRadius::same(10),
                                                crate::theme::glass_wash(
                                                    t,
                                                    t.bg_panel_alt,
                                                    (hov * 115.0) as u8,
                                                ),
                                            );
                                        }
                                        let painter = ui.painter();
                                        // Avatar: the chat's own avatar when
                                        // the chat is in the list, a seed disc
                                        // with a person glyph when it is not.
                                        let avatar_center = Pos2::new(
                                            rect.left() + crate::chat_list::AVATAR_CX,
                                            rect.center().y,
                                        );
                                        match chat {
                                            Some(chat) => {
                                                crate::widgets::paint_chat_avatar_photo(
                                                    painter,
                                                    avatar_center,
                                                    crate::chat_list::AVATAR_R,
                                                    chat,
                                                    photos.get(&chat.id).copied(),
                                                );
                                            }
                                            None => {
                                                let disc = crate::widgets::avatar_color(
                                                    (entry.chat_id % 251) as u8,
                                                );
                                                painter.circle_filled(
                                                    avatar_center,
                                                    crate::chat_list::AVATAR_R,
                                                    disc,
                                                );
                                                crate::icons::draw(
                                                    painter,
                                                    crate::icons::Icon::Person,
                                                    avatar_center,
                                                    crate::chat_list::AVATAR_R * 1.15,
                                                    egui::Color32::from_rgba_unmultiplied(
                                                        255, 255, 255, 235,
                                                    ),
                                                    disc,
                                                );
                                            }
                                        }
                                        let text_left = rect.left() + crate::chat_list::TEXT_X;
                                        let name = chat.map_or("", |c| c.title.as_str());
                                        let name_color = if entry.missed {
                                            rgb(t.danger)
                                        } else {
                                            rgb(t.text_primary)
                                        };
                                        // The relative day sits right of the
                                        // name, its right edge sharing the
                                        // info glyph's right margin below
                                        // it, so the row's two right-hand
                                        // elements sit on one line.
                                        let day = relative_day(entry.timestamp);
                                        let day_font = FontId::proportional(11.5);
                                        let day_w = ui.ctx().fonts_mut(|f| {
                                            f.layout_no_wrap(
                                                day.clone(),
                                                day_font.clone(),
                                                rgb(t.text_faint),
                                            )
                                            .size()
                                            .x
                                        });
                                        painter.text(
                                            Pos2::new(
                                                rect.right() - 10.0,
                                                rect.top() + crate::chat_list::TITLE_DY,
                                            ),
                                            egui::Align2::RIGHT_CENTER,
                                            day,
                                            day_font,
                                            rgb(t.text_faint),
                                        );
                                        // The grouped count rides after the
                                        // name in the secondary color, and its
                                        // width comes out of the name budget.
                                        let count_text = if row.count > 1 {
                                            format!(" ({})", row.count)
                                        } else {
                                            String::new()
                                        };
                                        let count_font = crate::theme::medium(15.5);
                                        let count_w = if count_text.is_empty() {
                                            0.0
                                        } else {
                                            ui.ctx().fonts_mut(|f| {
                                                f.layout_no_wrap(
                                                    count_text.clone(),
                                                    count_font.clone(),
                                                    rgb(t.text_secondary),
                                                )
                                                .size()
                                                .x
                                            })
                                        };
                                        let name_budget =
                                            (rect.right() - 18.0 - day_w - count_w - text_left)
                                                .max(0.0);
                                        let name_text = crate::widgets::ellipsize(
                                            ui,
                                            name,
                                            crate::theme::medium(15.5),
                                            name_budget,
                                        );
                                        let name_pos = Pos2::new(
                                            text_left,
                                            rect.top() + crate::chat_list::TITLE_DY,
                                        );
                                        let name_w = ui.ctx().fonts_mut(|f| {
                                            f.layout_no_wrap(
                                                name_text.clone(),
                                                crate::theme::medium(15.5),
                                                name_color,
                                            )
                                            .size()
                                            .x
                                        });
                                        painter.text(
                                            name_pos,
                                            egui::Align2::LEFT_CENTER,
                                            name_text,
                                            crate::theme::medium(15.5),
                                            name_color,
                                        );
                                        if !count_text.is_empty() {
                                            painter.text(
                                                Pos2::new(name_pos.x + name_w, name_pos.y),
                                                egui::Align2::LEFT_CENTER,
                                                count_text,
                                                count_font,
                                                rgb(t.text_secondary),
                                            );
                                        }
                                        // Status line: the direction glyph,
                                        // then the type and duration text.
                                        // Missed paints both in the danger
                                        // color, connected calls take the
                                        // accent for the glyph.
                                        let status_y = rect.top() + crate::chat_list::SNIPPET_DY;
                                        let glyph_color = if entry.missed {
                                            rgb(t.danger)
                                        } else {
                                            rgb(t.accent)
                                        };
                                        crate::icons::draw(
                                            painter,
                                            direction_icon(entry),
                                            Pos2::new(text_left + 7.0, status_y),
                                            15.0,
                                            glyph_color,
                                            rgb(t.bg_panel),
                                        );
                                        let status_color = if entry.missed {
                                            rgb(t.danger)
                                        } else {
                                            rgb(t.text_secondary)
                                        };
                                        painter.text(
                                            Pos2::new(text_left + 18.0, status_y),
                                            egui::Align2::LEFT_CENTER,
                                            status_line(entry),
                                            FontId::proportional(12.5),
                                            status_color,
                                        );
                                        // Info button at the right edge, on
                                        // the status line: opens the chat's
                                        // info dock. Its own click zone over
                                        // the row's, checked first below.
                                        let info_rect = egui::Rect::from_center_size(
                                            Pos2::new(rect.right() - 18.0, status_y),
                                            Vec2::splat(28.0),
                                        );
                                        let info_resp = ui.interact(
                                            info_rect,
                                            egui::Id::unique(("call_info", entry.id)),
                                            Sense::click(),
                                        );
                                        if info_resp.hovered() {
                                            painter.circle_filled(
                                                info_rect.center(),
                                                14.0,
                                                rgb(t.bg_panel_alt),
                                            );
                                        }
                                        crate::icons::draw(
                                            painter,
                                            crate::icons::Icon::Info,
                                            info_rect.center(),
                                            17.0,
                                            rgb(t.text_secondary),
                                            rgb(t.bg_panel),
                                        );
                                        let info_key = match chat.map(|c| c.kind) {
                                            Some(ChatKind::Group) => "profile.group_info",
                                            Some(ChatKind::Channel) => "profile.channel_info",
                                            _ => "profile.contact_info",
                                        };
                                        let info_resp = info_resp
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .on_hover_text(crate::i18n::t(info_key));
                                        if info_resp.clicked() {
                                            info = Some(entry.chat_id);
                                        } else if resp
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .clicked()
                                        {
                                            open = Some(entry.chat_id);
                                        }
                                    }
                                },
                            );
                        }
                    },
                );
                ui.add_space(6.0);
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(width, galley.size().y), Sense::hover());
                let galley_x = rect.min.x + ((width - galley.size().x) / 2.0).max(0.0);
                ui.painter().galley(Pos2::new(galley_x, rect.min.y), galley, rgb(t.text_faint));
            });
        crate::widgets::soft_right_edge(root, panel.response.rect, t);
        if back {
            self.screen = Screen::Chats;
            self.filter = ChatFilter::All;
        }
        if new_call {
            self.calls.picker_search.clear();
            self.calls.picker_open = true;
        }
        if let Some(id) = info {
            // The info dock belongs to the selected chat, so
            // opening the chat is part of opening its info.
            self.open_call_target(id);
            self.profile_open = true;
        } else if let Some(id) = open {
            // A row click calls the chat: it opens the chat,
            // and in live mode starts the call through the real
            // backend path (the demo has no call path, so
            // opening the chat is the whole action there, never
            // a fake call).
            self.open_call_target(id);
            if self.live_mode {
                self.start_call_from_header();
            }
        }
        let ctx = root.ctx().clone();
        self.draw_call_picker(&ctx);
    }

    /// The new call contact picker: a centered popup in the
    /// forward picker's shape, listing the DM chats with a
    /// search field. Choosing one performs the row click's
    /// action: the chat opens, and in live mode the call starts
    /// through the header path. Works in both modes because it
    /// lists chats, not call history.
    fn draw_call_picker(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        let Some(p) =
            crate::anim::popup_phase(ctx, "call_picker", self.calls.picker_open, self.motion)
        else {
            return;
        };
        let exiting = !self.calls.picker_open;
        let query = self.calls.picker_search.trim().to_lowercase();
        // The chats themselves, not just titles: the rows
        // paint the same avatars the call list paints.
        let targets: Vec<Chat> = self
            .chats
            .iter()
            .filter(|c| c.kind == ChatKind::Dm)
            .filter(|c| query.is_empty() || c.title.to_lowercase().contains(&query))
            .cloned()
            .collect();
        let no_targets = targets.is_empty();
        let photos = self.demo_photos(ctx);
        let mut chosen: Option<u64> = None;
        let mut close = false;
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        egui::Area::new(egui::Id::unique("call_picker"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if p < 0.999 {
                    ui.set_opacity(p);
                }
                // The popup frame the conversation popups use,
                // rebuilt from the same theme helper (theirs is
                // private to the rich UI module), with the
                // opaque elevated fill the confirm modal uses:
                // the glass fill let the conversation's text
                // ghost through behind the picker's rows.
                let frame = crate::theme::glass_panel_frame(t, 14)
                    .fill(rgb(core::theme::elevated_step(t.bg_panel)))
                    .inner_margin(Margin::symmetric(12, 10));
                let inner = frame.show(ui, |ui| {
                    ui.set_width(280.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("calls.new_call")).size(13.0).strong(),
                        );
                        ui.add_space(6.0);
                        crate::widgets::text_field(
                            ui,
                            t,
                            &mut self.calls.picker_search,
                            &crate::i18n::t("calls.search_hint"),
                            256.0,
                        );
                        ui.add_space(6.0);
                        if no_targets {
                            ui.label(
                                RichText::new(crate::i18n::t("calls.picker_empty"))
                                    .size(12.5)
                                    .color(rgb(t.text_secondary)),
                            );
                        } else {
                            egui::ScrollArea::vertical()
                                .max_height(320.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    for chat in &targets {
                                        ui.horizontal(|ui| {
                                            let (rect, _) = ui.allocate_exact_size(
                                                Vec2::splat(26.0),
                                                Sense::hover(),
                                            );
                                            crate::widgets::paint_chat_avatar_photo(
                                                ui.painter(),
                                                rect.center(),
                                                13.0,
                                                chat,
                                                photos.get(&chat.id).copied(),
                                            );
                                            if crate::widgets::menu_item(ui, t, &chat.title)
                                                .clicked()
                                            {
                                                chosen = Some(chat.id);
                                            }
                                        });
                                    }
                                });
                        }
                        ui.separator();
                        if crate::widgets::menu_item(ui, t, &crate::i18n::t("common.cancel"))
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
                crate::theme::paint_glass_edge(ui.painter(), inner.response.rect, 14.0, t);
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("call_picker"),
            ctx.content_rect().center(),
            p,
        );
        if let Some(id) = chosen {
            self.open_call_target(id);
            if self.live_mode {
                self.start_call_from_header();
            }
            close = true;
        }
        if close {
            self.calls.picker_open = false;
        }
    }

    /// Opens a call entry's chat in the right pane, with the
    /// same care a chat-list click takes in demo mode (working
    /// copy written back before the switch). The Calls panel
    /// itself stays.
    fn open_call_target(&mut self, id: u64) {
        if id == self.selected_chat {
            return;
        }
        if !self.live_mode {
            let previous = self.selected_chat;
            self.write_back_conversation(previous);
        }
        self.open_community = None;
        self.community_home_from_overview = false;
        self.community_origin = None;
        self.select_chat_now(id);
    }
}

#[cfg(test)]
mod tests {
    use super::{format_duration_label, group_calls, relative_day};
    use core::model::{CallDirection, CallEntry, CallKind};

    fn entry(
        id: u64,
        chat_id: u64,
        direction: CallDirection,
        missed: bool,
        timestamp: i64,
    ) -> CallEntry {
        CallEntry {
            id,
            chat_id,
            kind: CallKind::Voice,
            direction,
            missed,
            duration_secs: 0,
            timestamp,
        }
    }

    #[test]
    fn durations_read_like_the_reference() {
        assert_eq!(format_duration_label(131), "2 min 11 s");
        assert_eq!(format_duration_label(45), "45 s");
        assert_eq!(format_duration_label(367), "6 min 7 s");
        assert_eq!(format_duration_label(120), "2 min");
        assert_eq!(format_duration_label(3900), "1 h 5 min");
        assert_eq!(format_duration_label(0), "0 s");
    }

    #[test]
    fn epoch_day_was_a_thursday() {
        // 1970-01-01: the day formatter gives its full date, so
        // the row shows the weekday, and that day was a Thursday.
        assert_eq!(relative_day(0), "Thursday");
    }

    #[test]
    fn consecutive_same_chat_entries_group_with_a_count() {
        let entries = [
            entry(1, 4, CallDirection::Incoming, false, 300),
            entry(2, 4, CallDirection::Incoming, false, 200),
            entry(3, 4, CallDirection::Outgoing, false, 150),
            entry(4, 7, CallDirection::Incoming, true, 100),
            entry(5, 4, CallDirection::Incoming, false, 50),
        ];
        let refs: Vec<&CallEntry> = entries.iter().collect();
        let rows = group_calls(&refs);
        // The first pair groups (newest entry kept, count 2);
        // the outgoing entry breaks the run, and the later
        // incoming entry for chat 4 starts a new row because
        // another chat intervened.
        assert_eq!(rows.len(), 4);
        assert_eq!((rows[0].entry.id, rows[0].count), (1, 2));
        assert_eq!((rows[1].entry.id, rows[1].count), (3, 1));
        assert_eq!((rows[2].entry.id, rows[2].count), (4, 1));
        assert_eq!((rows[3].entry.id, rows[3].count), (5, 1));
    }
}
