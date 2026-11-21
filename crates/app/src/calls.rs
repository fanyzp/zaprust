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
//! Portrait mode draws its own slice of the same data, cut to
//! the owner's phone reference (the native dialer's log): big
//! title and search icon, All / Missed chips, a shortcut
//! circles row, a bold "Recent" header, day-and-time rows with
//! a call-back icon, and the new call action on the tab's FAB
//! (see `draw_calls_panel_portrait`).
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
use eframe::egui::{
    self, Align2, CornerRadius, FontId, Margin, Pos2, RichText, Sense, Stroke, Vec2,
};

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

/// The portrait row's second line, in the reference phone
/// log's shape: the day and the clock time ("Yesterday,
/// 18:47"), with the talk time appended for connected calls
/// that recorded one. Missed calls carry no duration, the
/// same rule the panel's status line follows.
fn day_time_line(entry: &CallEntry) -> String {
    let mut line =
        format!("{}, {}", relative_day(entry.timestamp), crate::live::fmt_time(entry.timestamp));
    if !entry.missed && entry.duration_secs > 0 {
        line.push_str(&format!(" · {}", format_duration_label(entry.duration_secs)));
    }
    line
}

/// The shortcut row's contact circles: the distinct chats of
/// the log, newest first (the log's own order after
/// [`core::filter::filter_calls`]), capped at `limit`. These
/// are the reference row's photo circles: the people the log
/// is actually about, in the order it mentions them.
fn recent_call_contacts(entries: &[CallEntry], limit: usize) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for entry in core::filter::filter_calls(entries, false) {
        if !out.contains(&entry.chat_id) {
            out.push(entry.chat_id);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// What a portrait Calls shortcut circle shows: an action
/// glyph on the reference's dark filled circle, or a contact's
/// photo filling the circle.
enum ShortcutFace<'a> {
    Icon(crate::icons::Icon),
    Photo { chat: &'a Chat, photo: Option<egui::TextureId> },
}

/// One shortcut circle of the portrait Calls screen, in the
/// reference's shape: a 56 pt filled circle, its label under
/// it in small muted text, the whole unit one click target.
/// `active` marks a toggled-on filter circle (the favorites
/// one) with an accent-tinted disc and glyph. Returns true on
/// click.
fn shortcut_circle(
    ui: &mut egui::Ui,
    t: core::theme::Tokens,
    face: ShortcutFace<'_>,
    label: &str,
    active: bool,
) -> bool {
    const D: f32 = 56.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(64.0, 78.0), Sense::click());
    let center = Pos2::new(rect.center().x, rect.top() + D / 2.0);
    match face {
        ShortcutFace::Icon(icon) => {
            let disc = if active {
                rgb(core::theme::mix(t.accent, t.bg_panel_alt, 0.55))
            } else {
                rgb(t.bg_panel_alt)
            };
            ui.painter().circle_filled(center, D / 2.0, disc);
            let glyph = if active { rgb(t.accent) } else { rgb(t.text_primary) };
            crate::icons::draw(ui.painter(), icon, center, 25.0, glyph, disc);
        }
        ShortcutFace::Photo { chat, photo } => {
            crate::widgets::paint_chat_avatar_photo(ui.painter(), center, D / 2.0, chat, photo);
        }
    }
    if response.hovered() {
        ui.painter().circle_stroke(center, D / 2.0, Stroke::new(1.0_f32, rgb(t.border)));
    }
    let shown = crate::widgets::ellipsize(ui, label, FontId::proportional(11.0), 64.0);
    ui.painter().text(
        Pos2::new(rect.center().x, rect.top() + D + 6.0),
        egui::Align2::CENTER_TOP,
        shown,
        FontId::proportional(11.0),
        rgb(t.text_secondary),
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label).clicked()
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
    /// The call log's display rows under the panel's current
    /// filters (Missed, the portrait favorites filter, search),
    /// grouped the reference way. Shared by the panel and its
    /// portrait slice so both modes list exactly the same rows.
    fn visible_call_rows<'a>(&self, source: &'a [CallEntry]) -> Vec<CallRow<'a>> {
        let query = self.calls.search.trim().to_lowercase();
        let visible: Vec<&CallEntry> = core::filter::filter_calls(source, self.calls.missed_only)
            .into_iter()
            .filter(|e| {
                !self.calls.favorites_only
                    || self.chats.iter().find(|c| c.id == e.chat_id).is_some_and(|c| c.favorite)
            })
            .filter(|e| {
                query.is_empty()
                    || self
                        .chats
                        .iter()
                        .find(|c| c.id == e.chat_id)
                        .is_some_and(|c| c.title.to_lowercase().contains(&query))
            })
            .collect();
        group_calls(&visible)
    }

    /// Draws the Calls panel in place of the chat list and
    /// applies row, info and header clicks. Entering the panel
    /// never touches the open conversation; a row click calls
    /// the entry's chat (opens it, and starts the call in live
    /// mode), the info button opens its info dock. Portrait
    /// mode draws the phone slice instead (see
    /// [`App::draw_calls_panel_portrait`]).
    pub(crate) fn draw_calls_panel(&mut self, root: &mut egui::Ui) {
        if self.portrait {
            self.draw_calls_panel_portrait(root);
            return;
        }
        let t = self.tokens();
        let photos = self.demo_photos(root.ctx());
        // Live mode has no call-log source yet: the list is
        // honestly empty there rather than filled with demo
        // history.
        let source: &[CallEntry] = if self.live_mode { &[] } else { &self.calls.entries };
        let rows = self.visible_call_rows(source);
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
                // the new call button at the right edge. In
                // portrait mode the back chevron goes away: the
                // bottom navigation bar is the way between tabs.
                ui.horizontal(|ui| {
                    if !self.portrait
                        && crate::widgets::icon_button(
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

    /// The portrait Calls screen, sliced from the owner's
    /// phone reference (the native dialer's call log): a big
    /// title with a search icon at the right; the All / Missed
    /// filter as a subordinate chip row; a horizontally
    /// scrolling row of circular shortcuts; a bold "Recent"
    /// section header; and rows with the avatar, the name
    /// (missed in the danger color, the grouped count after
    /// it), a direction glyph with the day and time under it,
    /// and a call-back icon at the right edge. The new call
    /// action rides the tab's FAB (portrait.rs), the way the
    /// reference carries it, so the header holds no new call
    /// button and the panel needs no footer hint.
    ///
    /// The shortcuts are wired only to actions the app really
    /// has: the phone circle opens the new call contact picker
    /// (the header button's flow in the other modes); the photo
    /// circles are the log's own recent contacts and call that
    /// contact, the row click's action; the favorites circle
    /// toggles the favorites filter over the log. The
    /// reference's Jadwal (schedule) and Keypad circles have no
    /// counterpart in the app (no call scheduling and no
    /// dialpad exist), so they are omitted rather than drawn as
    /// decoration that does nothing.
    fn draw_calls_panel_portrait(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let photos = self.demo_photos(root.ctx());
        // Live mode has no call-log source yet: the list is
        // honestly empty there rather than filled with demo
        // history.
        let source: &[CallEntry] = if self.live_mode { &[] } else { &self.calls.entries };
        let rows = self.visible_call_rows(source);
        let source_empty = source.is_empty();
        let contacts = recent_call_contacts(source, 6);
        let mut open: Option<u64> = None;
        let mut new_call = false;
        let panel = self
            .list_panel(root)
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)).inner_margin(Margin::symmetric(16, 10)))
            .show(root, |ui| {
                // Header: the big title, and at the right edge
                // a search icon (the reference pattern). The
                // search field unfolds under the header while
                // open; the panel's other modes keep their
                // always-open pill.
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(crate::i18n::t("nav.calls"))
                            .font(crate::theme::semibold(20.0))
                            .color(rgb(t.text_primary)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::widgets::icon_button(
                            ui,
                            t,
                            crate::icons::Icon::Search,
                            32.0,
                            &crate::i18n::t("calls.search_hint"),
                            None,
                        )
                        .clicked()
                        {
                            self.calls.search_open = !self.calls.search_open;
                        }
                    });
                });
                if self.calls.search_open {
                    ui.add_space(8.0);
                    crate::widgets::search_pill(
                        ui,
                        t,
                        egui::Id::unique("calls_search"),
                        &crate::i18n::t("calls.search_hint"),
                        &mut self.calls.search,
                    );
                }
                ui.add_space(10.0);
                // The All / Missed filter as a chip row: the
                // reference has no segmented track, so the
                // filter takes the app's chip shape and sits
                // subordinate under the header.
                ui.horizontal(|ui| {
                    if crate::widgets::chip(
                        ui,
                        &crate::i18n::t("calls.all"),
                        !self.calls.missed_only,
                        t,
                    )
                    .clicked()
                    {
                        self.calls.missed_only = false;
                    }
                    if crate::widgets::chip(
                        ui,
                        &crate::i18n::t("calls.missed"),
                        self.calls.missed_only,
                        t,
                    )
                    .clicked()
                    {
                        self.calls.missed_only = true;
                    }
                });
                ui.add_space(12.0);
                // Shortcut circles, scrolling horizontally
                // past the right edge like the reference row.
                egui::ScrollArea::horizontal()
                    .id_salt("calls_shortcuts")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if shortcut_circle(
                                ui,
                                t,
                                ShortcutFace::Icon(crate::icons::Icon::Phone),
                                &crate::i18n::t("calls.shortcut_phone"),
                                false,
                            ) {
                                new_call = true;
                            }
                            for chat_id in &contacts {
                                let Some(chat) = self.chats.iter().find(|c| c.id == *chat_id)
                                else {
                                    continue;
                                };
                                let first =
                                    chat.title.split_whitespace().next().unwrap_or(&chat.title);
                                if shortcut_circle(
                                    ui,
                                    t,
                                    ShortcutFace::Photo {
                                        chat,
                                        photo: photos.get(&chat.id).copied(),
                                    },
                                    first,
                                    false,
                                ) {
                                    open = Some(*chat_id);
                                }
                            }
                            if shortcut_circle(
                                ui,
                                t,
                                ShortcutFace::Icon(crate::icons::Icon::Star),
                                &crate::i18n::t("calls.shortcut_favorites"),
                                self.calls.favorites_only,
                            ) {
                                self.calls.favorites_only = !self.calls.favorites_only;
                            }
                        });
                    });
                ui.add_space(14.0);
                if source_empty {
                    crate::widgets::empty_state(ui, t, &crate::i18n::t("calls.empty"));
                } else if rows.is_empty() {
                    crate::widgets::empty_state(ui, t, &crate::i18n::t("calls.no_match"));
                } else {
                    // The section header, bold like the
                    // reference's "Terbaru".
                    ui.label(
                        RichText::new(crate::i18n::t("calls.recent"))
                            .font(crate::theme::semibold(16.0))
                            .color(rgb(t.text_primary)),
                    );
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        for row in &rows {
                            let entry = row.entry;
                            let chat: Option<&Chat> =
                                self.chats.iter().find(|c| c.id == entry.chat_id);
                            let (rect, resp) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), crate::chat_list::ROW_H),
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
                                    let disc =
                                        crate::widgets::avatar_color((entry.chat_id % 251) as u8);
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
                                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 235),
                                        disc,
                                    );
                                }
                            }
                            let text_left = rect.left() + crate::chat_list::TEXT_X;
                            let name = chat.map_or("", |c| c.title.as_str());
                            let name_color =
                                if entry.missed { rgb(t.danger) } else { rgb(t.text_primary) };
                            // The grouped count rides after the
                            // name in the secondary color, and its
                            // width comes out of the name budget,
                            // which ends at the call-back button's
                            // zone at the right edge.
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
                            let name_budget = (rect.right() - 52.0 - count_w - text_left).max(0.0);
                            let name_text = crate::widgets::ellipsize(
                                ui,
                                name,
                                crate::theme::medium(15.5),
                                name_budget,
                            );
                            let name_pos =
                                Pos2::new(text_left, rect.top() + crate::chat_list::TITLE_DY);
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
                            // Second line: the direction glyph
                            // (danger for missed, the accent for
                            // connected, the reference's colors),
                            // then the day and time in the muted
                            // color for every outcome, again the
                            // reference's split.
                            let status_y = rect.top() + crate::chat_list::SNIPPET_DY;
                            let glyph_color =
                                if entry.missed { rgb(t.danger) } else { rgb(t.accent) };
                            crate::icons::draw(
                                painter,
                                direction_icon(entry),
                                Pos2::new(text_left + 7.0, status_y),
                                15.0,
                                glyph_color,
                                rgb(t.bg_panel),
                            );
                            painter.text(
                                Pos2::new(text_left + 18.0, status_y),
                                egui::Align2::LEFT_CENTER,
                                day_time_line(entry),
                                FontId::proportional(12.5),
                                rgb(t.text_secondary),
                            );
                            // Call-back button at the right edge,
                            // centered on the row: a handset for a
                            // voice entry, the camera for a video
                            // one. It performs the row click's
                            // action (call the contact), like the
                            // reference's per-row call icon.
                            let back_rect = egui::Rect::from_center_size(
                                Pos2::new(rect.right() - 22.0, rect.center().y),
                                Vec2::splat(34.0),
                            );
                            let back_resp = ui.interact(
                                back_rect,
                                egui::Id::unique(("call_back", entry.id)),
                                Sense::click(),
                            );
                            if back_resp.hovered() {
                                painter.circle_filled(
                                    back_rect.center(),
                                    17.0,
                                    rgb(t.bg_panel_alt),
                                );
                            }
                            crate::icons::draw(
                                painter,
                                if entry.kind == CallKind::Video {
                                    crate::icons::Icon::Video
                                } else {
                                    crate::icons::Icon::Phone
                                },
                                back_rect.center(),
                                19.0,
                                rgb(t.text_secondary),
                                rgb(t.bg_panel),
                            );
                            let back_resp = back_resp
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(crate::i18n::t("calls.new_call"));
                            let row_clicked =
                                resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
                            if back_resp.clicked() || row_clicked {
                                open = Some(entry.chat_id);
                            }
                        }
                    });
                }
            });
        crate::widgets::soft_right_edge(root, panel.response.rect, t);
        if new_call {
            self.calls.picker_search.clear();
            self.calls.picker_open = true;
        }
        if let Some(id) = open {
            // A circle or row click calls the chat: it opens
            // the chat, and in live mode starts the call through
            // the real backend path (the demo has no call path,
            // so opening the chat is the whole action there,
            // never a fake call).
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
    use super::{
        day_time_line, format_duration_label, group_calls, recent_call_contacts, relative_day,
    };
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
    fn day_time_line_reads_like_the_reference_log() {
        // The epoch was a Thursday at 00:00 UTC: a connected
        // call appends its talk time, a missed call never does.
        let mut connected = entry(1, 4, CallDirection::Incoming, false, 0);
        connected.duration_secs = 131;
        assert_eq!(day_time_line(&connected), "Thursday, 00:00 · 2 min 11 s");
        let missed = entry(2, 4, CallDirection::Incoming, true, 0);
        assert_eq!(day_time_line(&missed), "Thursday, 00:00");
        // A connected call without a recorded duration shows
        // no suffix either.
        let unrecorded = entry(3, 4, CallDirection::Outgoing, false, 3_600);
        assert_eq!(day_time_line(&unrecorded), "Thursday, 01:00");
    }

    #[test]
    fn recent_contacts_are_distinct_and_newest_first() {
        // Deliberately unordered input: the helper sorts by
        // the log's own order (newest first) before folding.
        let entries = [
            entry(1, 7, CallDirection::Incoming, true, 100),
            entry(2, 4, CallDirection::Incoming, false, 300),
            entry(3, 4, CallDirection::Outgoing, false, 200),
            entry(4, 2, CallDirection::Incoming, false, 250),
            entry(5, 7, CallDirection::Outgoing, false, 50),
        ];
        assert_eq!(recent_call_contacts(&entries, 6), vec![4, 2, 7]);
        assert_eq!(recent_call_contacts(&entries, 2), vec![4, 2]);
        assert!(recent_call_contacts(&[], 6).is_empty());
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
