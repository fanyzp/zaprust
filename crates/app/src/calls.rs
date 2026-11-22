//! Calls panel: the call history as a LEFT panel (the WhatsApp
//! / Telegram pattern for secondary lists), while the
//! conversation stays on the right. One row paradigm serves
//! the panel and portrait alike: a header with a back chevron,
//! the title and a labelled New call primary button; the All /
//! Missed chip strip; the panel's search; the shortcut circles
//! row (recent contacts and the favorites filter); a "Recent"
//! section label; and rows with the contact's avatar and name
//! (missed calls in the danger color), a direction glyph with
//! the day, time and duration in the subtitle line, and a
//! call-back action at the trailing edge whose glyph matches
//! the entry's kind. A row carries no permanent info button:
//! opening the row's chat puts the contact's info one tap away
//! in the conversation header, the surface that owns it.
//!
//! Portrait mode draws its own slice of the same data, cut to
//! the owner's phone reference (the native dialer's log): big
//! title and search icon, the same chips, shortcut row and
//! rows, a bold "Recent" header, and the new call action on
//! the tab's FAB (see `draw_calls_panel_portrait`).
//!
//! Clicks are wired honestly: Zaprust's call flow is
//! backend-driven and exists only in live mode (the offline
//! demo has no call path at all), so a row click or a picker
//! choice opens the chat in demo mode, and in live mode opens
//! the chat and starts the call through the same header path
//! the conversation uses. The history itself comes from
//! [`call_log_source`]: the archive's persisted records in
//! live mode (loaded when the session starts and extended as
//! calls complete), the scripted demo log in demo mode. The
//! two never mix.

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

/// The glyph a row's call-back action carries: the camera for
/// a video entry, the handset for a voice one, so the action
/// restarts the kind of call the entry records.
fn call_back_icon(entry: &CallEntry) -> crate::icons::Icon {
    if entry.kind == CallKind::Video {
        crate::icons::Icon::Video
    } else {
        crate::icons::Icon::Phone
    }
}

/// One trailing action on a new call picker row: the call kind
/// it starts, the glyph it draws and the locale key that names
/// it. The picker renders exactly this list per row, so a row
/// can never offer one kind without the other.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PickerAction {
    video: bool,
    icon: crate::icons::Icon,
    label_key: &'static str,
}

/// The two call actions every picker row carries, voice first.
fn picker_call_actions() -> [PickerAction; 2] {
    [
        PickerAction { video: false, icon: crate::icons::Icon::Phone, label_key: "calls.voice" },
        PickerAction { video: true, icon: crate::icons::Icon::Video, label_key: "calls.video" },
    ]
}

/// The one call log row, shared by the desktop panel and the
/// portrait screen so the two layouts can never drift into
/// different paradigms: avatar, name (missed in the danger
/// color, the grouped count after it), the direction glyph
/// with the day, time and duration in the subtitle line, and
/// the call-back action at the trailing edge. Returns true
/// when the row or its call-back action was clicked; both
/// perform the same call-back. There is deliberately no info
/// action here: the chat the click opens carries the contact
/// info affordance in its header.
fn draw_call_row(
    ui: &mut egui::Ui,
    t: core::theme::Tokens,
    entry: &CallEntry,
    count: usize,
    chat: Option<&Chat>,
    photo: Option<egui::TextureId>,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), crate::chat_list::ROW_H),
        Sense::click(),
    );
    let hov = crate::anim::hover_factor(ui, ("call_row", entry.id), resp.hovered());
    if hov > 0.001 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
        );
    }
    let painter = ui.painter();
    let avatar_center = Pos2::new(rect.left() + crate::chat_list::AVATAR_CX, rect.center().y);
    match chat {
        Some(chat) => {
            crate::widgets::paint_chat_avatar_photo(
                painter,
                avatar_center,
                crate::chat_list::AVATAR_R,
                chat,
                photo,
            );
        }
        None => {
            let disc = crate::widgets::avatar_color((entry.chat_id % 251) as u8);
            painter.circle_filled(avatar_center, crate::chat_list::AVATAR_R, disc);
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
    let name_color = if entry.missed { rgb(t.danger) } else { rgb(t.text_primary) };
    // The grouped count rides after the name in the secondary
    // color, and its width comes out of the name budget, which
    // ends at the call-back button's zone at the right edge.
    let count_text = if count > 1 { format!(" ({count})") } else { String::new() };
    let count_font = crate::theme::medium(15.5);
    let count_w = if count_text.is_empty() {
        0.0
    } else {
        ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(count_text.clone(), count_font.clone(), rgb(t.text_secondary))
                .size()
                .x
        })
    };
    let name_budget = (rect.right() - 52.0 - count_w - text_left).max(0.0);
    let name_text = crate::widgets::ellipsize(ui, name, crate::theme::medium(15.5), name_budget);
    let name_pos = Pos2::new(text_left, rect.top() + crate::chat_list::TITLE_DY);
    let name_w = ui.ctx().fonts_mut(|f| {
        f.layout_no_wrap(name_text.clone(), crate::theme::medium(15.5), name_color).size().x
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
    // Subtitle: the direction glyph (danger for missed, the
    // accent for connected), then the day, time and duration.
    let status_y = rect.top() + crate::chat_list::SNIPPET_DY;
    let glyph_color = if entry.missed { rgb(t.danger) } else { rgb(t.accent) };
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
    // Call-back button at the right edge, centered on the row.
    // Its own click zone sits over the row's, and both clicks
    // land on the same action below.
    let back_rect = egui::Rect::from_center_size(
        Pos2::new(rect.right() - 22.0, rect.center().y),
        Vec2::splat(34.0),
    );
    let back_resp =
        ui.interact(back_rect, egui::Id::unique(("call_back", entry.id)), Sense::click());
    if back_resp.hovered() {
        painter.circle_filled(back_rect.center(), 17.0, rgb(t.bg_panel_alt));
    }
    crate::icons::draw(
        painter,
        call_back_icon(entry),
        back_rect.center(),
        19.0,
        rgb(t.text_secondary),
        rgb(t.bg_panel),
    );
    let back_resp = back_resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(crate::i18n::t("calls.call_back"));
    let row_clicked = resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
    back_resp.clicked() || row_clicked
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

/// The entry list both Calls surfaces render: the persisted
/// live history in live mode, the scripted demo log in demo
/// mode. The gate is the same one the rest of the demo fixtures
/// use, and this is the only place the choice is made, so the
/// two sources cannot mix. (A free function over the two lists
/// rather than a method: the panels hold the chosen list across
/// a closure that mutates the neighboring filter state.)
fn call_log_source<'a>(
    live_mode: bool,
    live: &'a [CallEntry],
    demo: &'a [CallEntry],
) -> &'a [CallEntry] {
    if live_mode {
        live
    } else {
        demo
    }
}

/// The composed empty state for a call history with no calls
/// at all: a soft disc with the phone glyph, a title and a
/// supporting line, centred in the space the panel hands it.
/// The search and filter no-match case keeps the plain
/// `widgets::empty_state` label; this one is only for a truly
/// empty history, so it can afford to be a small illustration.
fn calls_empty_state(ui: &mut egui::Ui, t: core::theme::Tokens) {
    let width = ui.available_width();
    let height = ui.available_height().max(220.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter();
    let title = crate::i18n::t("calls.empty");
    let hint = crate::i18n::t("calls.empty_hint");
    // The supporting line is wrapped to a comfortable measure
    // first, because its height decides where the block starts.
    let hint_galley =
        painter.layout(hint, FontId::proportional(13.0), rgb(t.text_secondary), 280.0);
    const DISC_D: f32 = 72.0;
    let total_h = DISC_D + 16.0 + 22.0 + 6.0 + hint_galley.size().y;
    let mut y = rect.top() + ((height - total_h) / 2.0).max(0.0);
    let cx = rect.center().x;
    let disc_center = Pos2::new(cx, y + DISC_D / 2.0);
    painter.circle_filled(disc_center, DISC_D / 2.0, rgb(t.bg_panel_alt));
    crate::icons::draw(
        painter,
        crate::icons::Icon::Phone,
        disc_center,
        38.0,
        rgb(t.accent),
        rgb(t.bg_panel_alt),
    );
    y += DISC_D + 16.0;
    painter.text(
        Pos2::new(cx, y + 11.0),
        Align2::CENTER_CENTER,
        title,
        crate::theme::semibold(16.0),
        rgb(t.text_primary),
    );
    y += 22.0 + 6.0;
    let hint_rect = egui::Rect::from_min_size(
        Pos2::new(cx - hint_galley.size().x / 2.0, y),
        hint_galley.size(),
    );
    painter.galley(hint_rect.min, hint_galley, rgb(t.text_secondary));
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
}

/// The All / Missed filter as a chip row, shared by the
/// panel and portrait: the reference has no segmented track,
/// so the filter takes the app's chip shape and rides the
/// shared chip strip, like every chip row. A free function
/// over the filter flag: the panels call it inside closures
/// that already hold other pieces of the app state.
fn calls_filter_chips(ui: &mut egui::Ui, t: core::theme::Tokens, missed_only: &mut bool) {
    crate::widgets::chip_scroll_row(ui, "calls_filter_chips", t.bg_panel, *missed_only, |ui| {
        let mut selected_resp = None;
        let all = crate::widgets::chip(ui, &crate::i18n::t("calls.all"), !*missed_only, t);
        if !*missed_only {
            selected_resp = Some(all.clone());
        }
        if all.clicked() {
            *missed_only = false;
        }
        let missed = crate::widgets::chip(ui, &crate::i18n::t("calls.missed"), *missed_only, t);
        if *missed_only {
            selected_resp = Some(missed.clone());
        }
        if missed.clicked() {
            *missed_only = true;
        }
        selected_resp
    });
}

/// The shortcut circles row, shared by the panel and
/// portrait: the phone circle opens the new call contact
/// picker, the photo circles are the log's own recent
/// contacts and call that contact, and the favorites circle
/// toggles the favorites filter over the log. Returns the
/// contact a photo circle picked, and whether the phone
/// circle asked for the picker. The reference's schedule and
/// keypad circles have no counterpart in the app (no call
/// scheduling and no dialpad exist), so they are omitted
/// rather than drawn as decoration that does nothing.
fn calls_shortcut_strip(
    ui: &mut egui::Ui,
    t: core::theme::Tokens,
    chats: &[Chat],
    photos: &std::collections::HashMap<u64, egui::TextureId>,
    favorites_only: &mut bool,
    source: &[CallEntry],
) -> (Option<u64>, bool) {
    let contacts = recent_call_contacts(source, 6);
    let mut open: Option<u64> = None;
    let mut new_call = false;
    let shortcuts = egui::ScrollArea::horizontal()
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
                    let Some(chat) = chats.iter().find(|c| c.id == *chat_id) else {
                        continue;
                    };
                    let first = chat.title.split_whitespace().next().unwrap_or(&chat.title);
                    if shortcut_circle(
                        ui,
                        t,
                        ShortcutFace::Photo { chat, photo: photos.get(&chat.id).copied() },
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
                    *favorites_only,
                ) {
                    *favorites_only = !*favorites_only;
                }
            });
        });
    let offset = shortcuts.state.clamped_offset().x;
    let max_offset = (shortcuts.content_size.x - shortcuts.inner_rect.width()).max(0.0);
    crate::widgets::strip_edge_fades(
        ui.painter(),
        shortcuts.inner_rect,
        t.bg_panel,
        offset > 0.5,
        offset < max_offset - 0.5,
    );
    (open, new_call)
}

impl App {
    /// Calls a chat back in the kind the picked entry or
    /// action carries: the chat opens, and in live mode the
    /// call starts through the shared start path (the demo has
    /// no call path, so opening the chat is the whole action
    /// there, never a fake call).
    pub(crate) fn call_chat(&mut self, chat_id: u64, kind: CallKind) {
        self.call_chat_kind(chat_id, kind == CallKind::Video);
    }

    /// [`App::call_chat`] with the kind as the backend's video
    /// flag: the new call picker's voice and video actions land
    /// here directly.
    pub(crate) fn call_chat_kind(&mut self, chat_id: u64, video: bool) {
        self.open_call_target(chat_id);
        if self.live_mode {
            self.start_call_kind(video);
        }
    }

    /// Draws the Calls panel in place of the chat list and
    /// applies row and header clicks. Entering the panel never
    /// touches the open conversation; a row click calls the
    /// entry's chat back in the entry's kind (opens it, and
    /// starts the call in live mode). The contact's info dock
    /// stays reachable from the opened chat's header, the
    /// surface that owns it, so the row needs no info button.
    /// Portrait mode draws the phone slice instead (see
    /// [`App::draw_calls_panel_portrait`]).
    pub(crate) fn draw_calls_panel(&mut self, root: &mut egui::Ui) {
        if self.portrait {
            self.draw_calls_panel_portrait(root);
            return;
        }
        let t = self.tokens();
        let photos = self.demo_photos(root.ctx());
        let source: &[CallEntry] =
            call_log_source(self.live_mode, &self.calls.live_entries, &self.calls.entries);
        let rows = self.visible_call_rows(source);
        let source_empty = source.is_empty();
        let mut open: Option<(u64, CallKind)> = None;
        let mut shortcut_open: Option<u64> = None;
        let mut back = false;
        let mut new_call = false;
        let panel = self
            .list_panel(root)
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)).inner_margin(Margin::symmetric(10, 8)))
            .show(root, |ui| {
                // Header: back to the chat list, the title, and
                // the New call primary button at the right edge:
                // the screen's primary action takes the house
                // primary button language with its label showing,
                // not a bare glyph. In portrait mode the back
                // chevron goes away: the bottom navigation bar
                // is the way between tabs.
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
                            .font(crate::theme::medium(16.0))
                            .color(rgb(t.text_primary)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::i18n::t("calls.new_call"),
                            crate::widgets::Btn::Primary,
                        )
                        .clicked()
                        {
                            new_call = true;
                        }
                    });
                });
                ui.add_space(10.0);
                // The All / Missed filter as a chip row, the
                // same strip portrait uses.
                calls_filter_chips(ui, t, &mut self.calls.missed_only);
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
                // Shortcut circles at panel width, the same row
                // portrait carries: recent contacts and the
                // favorites filter over the log.
                let (picked, picker) = calls_shortcut_strip(
                    ui,
                    t,
                    &self.chats,
                    &photos,
                    &mut self.calls.favorites_only,
                    source,
                );
                shortcut_open = picked;
                if picker {
                    new_call = true;
                }
                ui.add_space(10.0);
                // The content region takes the rest of the
                // panel; there is no footer text under it (the
                // new call button explains itself).
                let width = ui.available_width();
                let content_h = ui.available_height();
                ui.allocate_ui_with_layout(
                    Vec2::new(width, content_h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        if source_empty {
                            calls_empty_state(ui, t);
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
                                        if draw_call_row(
                                            ui,
                                            t,
                                            entry,
                                            row.count,
                                            chat,
                                            chat.and_then(|c| photos.get(&c.id).copied()),
                                        ) {
                                            open = Some((entry.chat_id, entry.kind));
                                        }
                                    }
                                },
                            );
                        }
                    },
                );
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
        if let Some(id) = shortcut_open {
            // A shortcut circle calls its contact with a voice
            // call, the circle's own meaning.
            self.call_chat(id, CallKind::Voice);
        }
        if let Some((id, kind)) = open {
            self.call_chat(id, kind);
        }
        let ctx = root.ctx().clone();
        self.draw_call_picker(&ctx, panel.response.rect);
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
    /// button.
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
        let source: &[CallEntry] =
            call_log_source(self.live_mode, &self.calls.live_entries, &self.calls.entries);
        let rows = self.visible_call_rows(source);
        let source_empty = source.is_empty();
        let mut open: Option<(u64, CallKind)> = None;
        let mut shortcut_open: Option<u64> = None;
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
                // The All / Missed filter as a chip row, the
                // same strip the desktop panel uses.
                calls_filter_chips(ui, t, &mut self.calls.missed_only);
                ui.add_space(12.0);
                // Shortcut circles, the same row the desktop
                // panel carries at panel width.
                let (picked, picker) = calls_shortcut_strip(
                    ui,
                    t,
                    &self.chats,
                    &photos,
                    &mut self.calls.favorites_only,
                    source,
                );
                shortcut_open = picked;
                if picker {
                    new_call = true;
                }
                ui.add_space(14.0);
                if source_empty {
                    calls_empty_state(ui, t);
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
                            if draw_call_row(
                                ui,
                                t,
                                entry,
                                row.count,
                                chat,
                                chat.and_then(|c| photos.get(&c.id).copied()),
                            ) {
                                open = Some((entry.chat_id, entry.kind));
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
        if let Some(id) = shortcut_open {
            // A shortcut circle calls its contact with a voice
            // call, the circle's own meaning.
            self.call_chat(id, CallKind::Voice);
        }
        if let Some((id, kind)) = open {
            self.call_chat(id, kind);
        }
        let ctx = root.ctx().clone();
        self.draw_call_picker(&ctx, panel.response.rect);
    }

    /// The new call contact picker: a sheet anchored to the
    /// Calls panel's top edge, listing the DM chats with a
    /// search field. Every row carries trailing voice and
    /// video actions that start that kind of call through the
    /// shared start path; the row body itself starts a voice
    /// call, the picker's original choice. No row is
    /// pre-highlighted: a row paints a fill only while the
    /// pointer is over it, never at rest. Works in both modes
    /// because it lists chats, not call history.
    fn draw_call_picker(&mut self, ctx: &egui::Context, panel_rect: egui::Rect) {
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
        let mut chosen: Option<(u64, bool)> = None;
        let mut close = false;
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        // The sheet hangs from the panel's top edge, inset by
        // the panel's own margin, at the panel's width up to a
        // comfortable measure: it belongs to the Calls panel
        // and its New call trigger, not to the conversation
        // pane behind it.
        let sheet_pos = Pos2::new(panel_rect.left() + 10.0, panel_rect.top() + 10.0);
        let sheet_w = (panel_rect.width() - 20.0).clamp(260.0, 340.0);
        egui::Area::new(egui::Id::unique("call_picker"))
            .fixed_pos(sheet_pos)
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
                    ui.set_width(sheet_w - 24.0);
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
                            sheet_w - 48.0,
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
                                        // One row per contact: avatar,
                                        // name, and the trailing voice
                                        // and video actions from
                                        // `picker_call_actions`. The
                                        // fill is hover-only, so no
                                        // row reads as selected at
                                        // rest.
                                        let (rect, resp) = ui.allocate_exact_size(
                                            Vec2::new(ui.available_width(), 44.0),
                                            Sense::click(),
                                        );
                                        let painter = ui.painter();
                                        if resp.hovered() {
                                            painter.rect_filled(
                                                rect,
                                                CornerRadius::same(8),
                                                rgb(t.bg_panel_alt),
                                            );
                                        }
                                        let avatar_center =
                                            Pos2::new(rect.left() + 20.0, rect.center().y);
                                        crate::widgets::paint_chat_avatar_photo(
                                            painter,
                                            avatar_center,
                                            13.0,
                                            chat,
                                            photos.get(&chat.id).copied(),
                                        );
                                        let name_budget =
                                            (rect.right() - 76.0 - (rect.left() + 44.0)).max(0.0);
                                        let name_text = crate::widgets::ellipsize(
                                            ui,
                                            &chat.title,
                                            crate::theme::medium(13.0),
                                            name_budget,
                                        );
                                        painter.text(
                                            Pos2::new(rect.left() + 44.0, rect.center().y),
                                            egui::Align2::LEFT_CENTER,
                                            name_text,
                                            crate::theme::medium(13.0),
                                            rgb(t.text_primary),
                                        );
                                        // The trailing actions, in
                                        // the list's own order (voice,
                                        // then video). Their click
                                        // zones sit over the row's and
                                        // are checked first, like the
                                        // row paradigm's call-back.
                                        let mut action_chosen: Option<bool> = None;
                                        for (i, action) in
                                            picker_call_actions().iter().enumerate()
                                        {
                                            let btn_rect = egui::Rect::from_center_size(
                                                Pos2::new(
                                                    rect.right() - 56.0 + 34.0 * i as f32,
                                                    rect.center().y,
                                                ),
                                                Vec2::splat(32.0),
                                            );
                                            let btn_resp = ui.interact(
                                                btn_rect,
                                                egui::Id::unique((
                                                    "picker_call",
                                                    chat.id,
                                                    action.video,
                                                )),
                                                Sense::click(),
                                            );
                                            if btn_resp.hovered() {
                                                painter.circle_filled(
                                                    btn_rect.center(),
                                                    16.0,
                                                    rgb(t.bg_panel),
                                                );
                                            }
                                            crate::icons::draw(
                                                painter,
                                                action.icon,
                                                btn_rect.center(),
                                                18.0,
                                                rgb(t.text_secondary),
                                                rgb(t.bg_panel_alt),
                                            );
                                            if btn_resp
                                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                                .on_hover_text(crate::i18n::t(action.label_key))
                                                .clicked()
                                            {
                                                action_chosen = Some(action.video);
                                            }
                                        }
                                        if let Some(video) = action_chosen {
                                            chosen = Some((chat.id, video));
                                        } else if resp
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .clicked()
                                        {
                                            chosen = Some((chat.id, false));
                                        }
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
        crate::anim::popup_transform(ctx, egui::Id::unique("call_picker"), sheet_pos, p);
        if let Some((id, video)) = chosen {
            self.call_chat_kind(id, video);
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
        call_back_icon, call_log_source, day_time_line, format_duration_label, group_calls,
        picker_call_actions, recent_call_contacts, relative_day,
    };
    use crate::state::{App, Args};
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

    /// Item 113: the Calls surfaces render exactly one source
    /// per mode: the persisted live history in live mode, the
    /// scripted demo log in demo mode, never a mixture.
    #[test]
    fn call_log_source_never_mixes_live_and_demo() {
        let live = vec![entry(101, 4, CallDirection::Incoming, true, 300)];
        let demo = vec![entry(1, 4, CallDirection::Incoming, false, 300)];
        let chosen = call_log_source(true, &live, &demo);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].id, 101, "live mode renders the live history");
        let chosen = call_log_source(false, &live, &demo);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].id, 1, "demo mode renders the demo log");
    }

    /// Item 113: the Missed filter and the portrait shortcut row
    /// run over the persisted live entries the same way they run
    /// over the demo log: filtering keeps only the missed rows,
    /// and the shortcuts are the history's own contacts.
    #[test]
    fn missed_filter_and_shortcuts_run_over_live_entries() {
        let ctx = eframe::egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        // App::new may have attempted a real live start against
        // this machine's data dir; the test owns the state from
        // here.
        app.live = None;
        app.live_mode = true;
        app.calls.live_entries = vec![
            entry(11, 4, CallDirection::Incoming, true, 300),
            entry(12, 2, CallDirection::Outgoing, false, 200),
            entry(13, 4, CallDirection::Incoming, false, 100),
        ];
        let source = call_log_source(app.live_mode, &app.calls.live_entries, &app.calls.entries);
        assert_eq!(recent_call_contacts(source, 6), vec![4, 2]);
        app.calls.missed_only = true;
        let source = call_log_source(app.live_mode, &app.calls.live_entries, &app.calls.entries);
        let rows = app.visible_call_rows(source);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.id, 11, "only the missed entry survives the filter");
        assert!(rows[0].entry.missed);
    }

    /// Item 116: the portrait favorites filter runs over the
    /// chats' real favorite flags: with the filter on, only
    /// entries whose chat is flagged favorite survive.
    #[test]
    fn favorites_filter_keeps_only_favorite_chats_entries() {
        let ctx = eframe::egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        // As in the test above, the test owns the state from
        // here; the flags are set explicitly so the fixture
        // defaults cannot leak into the assertion.
        app.live = None;
        app.live_mode = true;
        let mut chats = core::model::demo_chats();
        for c in &mut chats {
            c.favorite = c.id == 4;
        }
        app.chats = chats;
        app.calls.live_entries = vec![
            entry(11, 4, CallDirection::Incoming, false, 300),
            entry(12, 2, CallDirection::Outgoing, false, 200),
        ];
        app.calls.favorites_only = true;
        let source = call_log_source(app.live_mode, &app.calls.live_entries, &app.calls.entries);
        let rows = app.visible_call_rows(source);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.id, 11, "only the favorite chat's entry survives the filter");
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

    /// Batch 6: the row's trailing call-back action carries the
    /// entry's own kind, so calling back a video entry starts a
    /// video call and a voice entry a voice call.
    #[test]
    fn call_back_icon_matches_the_entry_kind() {
        let mut video = entry(1, 4, CallDirection::Incoming, false, 0);
        video.kind = CallKind::Video;
        assert_eq!(call_back_icon(&video), crate::icons::Icon::Video);
        let voice = entry(2, 4, CallDirection::Outgoing, false, 0);
        assert_eq!(call_back_icon(&voice), crate::icons::Icon::Phone);
    }

    /// Batch 6: every new call picker row carries both call
    /// actions, voice and video, each with its own glyph and
    /// locale label.
    #[test]
    fn picker_rows_carry_voice_and_video_actions() {
        let actions = picker_call_actions();
        assert_eq!(actions.len(), 2);
        assert!(!actions[0].video);
        assert_eq!(actions[0].icon, crate::icons::Icon::Phone);
        assert_eq!(actions[0].label_key, "calls.voice");
        assert!(actions[1].video);
        assert_eq!(actions[1].icon, crate::icons::Icon::Video);
        assert_eq!(actions[1].label_key, "calls.video");
    }

    /// Batch 6: the unified row has no per-row info button left
    /// in either layout: the info dock is reached from the
    /// opened chat's header instead. This reads the module's
    /// own source because the button was a hand-painted click
    /// zone, not a widget a frame test can query.
    #[test]
    fn no_per_row_info_button_remains() {
        let source = include_str!("calls.rs");
        // The searched spellings are assembled in pieces so
        // this test's own text cannot satisfy them.
        let zone = concat!("call", "_info");
        let glyph = concat!("Icon::", "Info");
        assert!(!source.contains(zone), "the info click zone is gone");
        assert!(!source.contains(glyph), "the info glyph is gone");
    }

    /// An app in live mode over a hand-fed fake session: the
    /// command receiver stands in for the fake backend, so a
    /// call the UI starts is observed as the exact backend
    /// command it sends.
    fn fake_call_app(
        ctx: &eframe::egui::Context,
    ) -> (App, std::sync::mpsc::Receiver<backend::BackendCommand>) {
        crate::theme::install_fonts(ctx);
        let mut app = App::new(Args::parse(), ctx);
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<backend::BackendCommand>();
        let (_evt_tx, evt_rx) = std::sync::mpsc::channel::<backend::BackendEvent>();
        let mut jids = std::collections::HashMap::new();
        jids.insert(4, "6281234567890@s.whatsapp.net".to_owned());
        app.live = Some(crate::live::LiveSession {
            channels: backend::BackendChannels { commands: cmd_tx, events: evt_rx },
            status: crate::live::LinkStatus::Linked,
            qr: None,
            qr_expires_at: None,
            pair_code: None,
            pair_code_at: None,
            account: "Test account".to_owned(),
            connected_at: None,
            store: None,
            jids,
            messages: std::collections::HashMap::new(),
            times: std::collections::HashMap::new(),
            fake: true,
            logout_requested: false,
            flags: std::collections::HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            photo_refs: std::collections::HashMap::new(),
            voice_refs: std::collections::HashMap::new(),
            video_refs: std::collections::HashMap::new(),
            video_thumbs: std::collections::HashMap::new(),
            poll_ballots: std::collections::HashMap::new(),
        });
        app.live_mode = true;
        app.chats = core::model::demo_chats();
        (app, cmd_rx)
    }

    /// Batch 6: a row's call-back goes through the call flow
    /// with the entry's kind: over the fake session it opens
    /// the chat and sends the backend a StartCall for that
    /// chat with the video flag set, and the backend's Ringing
    /// adoption keeps the video kind instead of relabeling the
    /// call as voice.
    #[test]
    fn call_back_starts_the_entry_kind_through_the_fake_session() {
        let ctx = eframe::egui::Context::default();
        let (mut app, cmd_rx) = fake_call_app(&ctx);
        app.call_chat(4, CallKind::Video);
        assert_eq!(app.selected_chat, 4, "the call-back opens the entry's chat");
        match cmd_rx.try_recv() {
            Ok(backend::BackendCommand::StartCall { chat_jid, video }) => {
                assert_eq!(chat_jid, "6281234567890@s.whatsapp.net");
                assert!(video, "a video entry calls back as a video call");
            }
            other => panic!("expected one StartCall command, got {other:?}"),
        }
        assert!(app.calls.outgoing_video, "the started kind is remembered for adoption");
        app.on_call_changed("fake-call-1", 4, backend::CallPhase::Ringing, 0, None);
        let active = app.calls.active.as_ref().expect("the ringing call is adopted");
        assert!(active.video, "the adopted call keeps the video kind");
        assert!(!app.calls.outgoing_video, "adoption consumes the remembered kind");
    }

    /// Batch 6: in demo mode a call-back opens the chat and
    /// nothing else: the demo has no call path, so no call is
    /// faked and no notice claims one.
    #[test]
    fn call_back_in_demo_opens_the_chat_without_a_call() {
        let ctx = eframe::egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        app.live = None;
        app.live_mode = false;
        app.chats = core::model::demo_chats();
        app.selected_chat = 2;
        app.notice = None;
        app.call_chat(4, CallKind::Voice);
        assert_eq!(app.selected_chat, 4);
        assert!(app.calls.active.is_none(), "no call is simulated in demo mode");
        assert!(app.notice.is_none(), "no notice claims a call happened");
    }
}
