//! Starred messages panel: every starred message in one flat
//! chronological list (newest first), shown in the LEFT panel
//! while the conversation stays on the right (the WhatsApp /
//! Telegram pattern for secondary lists). The collection and
//! ordering logic is the pure `core::filter::starred_messages`;
//! this module only builds a starred-only view of the chats
//! (the open chat's working copy replaces its stored messages),
//! draws the panel, and opens a chat when a row is clicked,
//! asking the stream to scroll to and highlight that message.

use crate::state::{App, Screen};
use crate::theme::rgb;
use core::filter::ChatFilter;
use core::model::{Chat, Message, MessageKind, PreviewMedia};
use eframe::egui::{self, CornerRadius, FontId, Margin, Pos2, RichText, Sense, Vec2};

/// Height of one starred row: two lines (chat title + date,
/// then the kind chip or the text snippet).
const ROW_H: f32 = 56.0;

/// The text in a starred row's second line for messages that do
/// not get a kind chip: the body text (an image caption counts
/// as the text), with the deleted placeholder the bubble also
/// substitutes, and the name for the kinds whose text lives
/// outside the body. A view-once message shows only its
/// placeholder: starring one must not pin its caption into
/// this list, where it would outlive the sealed payload.
fn snippet(msg: &Message) -> String {
    if msg.deleted && !msg.deleted_by_sender {
        return "This message was deleted".to_owned();
    }
    if msg.is_view_once() {
        return core::model::VIEW_ONCE_PREVIEW.to_owned();
    }
    if let MessageKind::Image { caption, .. } = &msg.kind {
        if !caption.is_empty() {
            return caption.clone();
        }
    }
    if !msg.body.is_empty() {
        return msg.body.clone();
    }
    match &msg.kind {
        MessageKind::Location { name, .. } | MessageKind::Contact { name, .. } => name.clone(),
        _ => msg.body.clone(),
    }
}

/// The kind chip's label for a starred row, or `None` when the
/// row shows the text snippet instead. Media kinds get a chip
/// carrying the shared `PreviewMedia` wording; the voice label
/// drops its duration so the chip stays compact ("Photo 6:03
/// PM", not "Voice message · 0:42 6:03 PM").
fn chip_label(msg: &Message) -> Option<&'static str> {
    match PreviewMedia::of(&msg.kind) {
        Some(PreviewMedia::Photo) => Some("Photo"),
        Some(PreviewMedia::Video) => Some("Video"),
        Some(PreviewMedia::Voice(_)) => Some("Voice message"),
        Some(PreviewMedia::Document) => Some("Document"),
        Some(PreviewMedia::Poll) => Some("Poll"),
        None => None,
    }
}

/// Parses a "14 November 2026" day label into (year, month,
/// day), with the full English month names the day formatter
/// writes.
fn parse_full_date(label: &str) -> Option<(i64, u32, u32)> {
    let mut parts = label.split_whitespace();
    let d: u32 = parts.next()?.parse().ok()?;
    let month = parts.next()?;
    let m = crate::live::MONTH_NAMES.iter().position(|name| *name == month)? as u32 + 1;
    let y: i64 = parts.next()?.parse().ok()?;
    Some((y, m, d))
}

/// An "HH:MM" display clock in 12-hour form ("17:35" becomes
/// "5:35 PM"). Clocks that do not parse (a live echo still
/// showing its placeholder) pass through unchanged.
fn twelve_hour(time: &str) -> String {
    let parsed = time.split_once(':').and_then(|(h, m)| {
        let h: u32 = h.parse().ok()?;
        let m: u32 = m.parse().ok()?;
        (h <= 23 && m <= 59).then_some((h, m))
    });
    match parsed {
        Some((h, m)) => {
            let (h12, suffix) = match h {
                0 => (12, "AM"),
                1..=11 => (h, "AM"),
                12 => (12, "PM"),
                _ => (h - 12, "PM"),
            };
            format!("{h12}:{m:02} {suffix}")
        }
        None => time.to_owned(),
    }
}

/// The row's right-aligned date line: "21 Oct 2026 at 5:55 PM".
/// The calendar date resolves from the message's day label
/// through the same civil-date helpers the live layer composes
/// labels with ("Today" and "Yesterday" map to the current and
/// previous civil day, a full-date label parses back); a label
/// that resolves neither way is shown as-is next to the clock.
fn date_line(msg: &Message) -> String {
    let time = twelve_hour(&msg.time);
    let days = (crate::state::now_unix() / 86_400) as i64;
    let (y, m, d) = match msg.day_label() {
        "Today" => crate::live::civil_from_days(days),
        "Yesterday" => crate::live::civil_from_days(days - 1),
        label => match parse_full_date(label) {
            Some(date) => date,
            None => return format!("{label} · {time}"),
        },
    };
    let month = &crate::live::MONTH_NAMES[(m - 1) as usize][..3];
    format!("{d} {month} {y} at {time}")
}

impl App {
    /// Draws the Starred panel in place of the chat list and
    /// applies row clicks. The panel never touches the open
    /// conversation except when a row is clicked, which opens
    /// that row's chat in the right pane and locates the
    /// message in its stream.
    pub(crate) fn draw_starred_panel(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        // Starred-only view of the chats: each chat contributes
        // a clone carrying just its starred messages (the open
        // chat's from the working copy, the detached
        // announcement chat appended when present), so the flat
        // core helper can take one slice. The stored chats are
        // never mutated.
        let mut view: Vec<Chat> = Vec::with_capacity(self.chats.len() + 1);
        for chat in &self.chats {
            let mut entry = chat.clone();
            if chat.id == self.selected_chat {
                entry.messages = self.conversation.clone();
            }
            entry.messages.retain(|m| m.starred);
            view.push(entry);
        }
        if let Some(detached) = &self.detached_chat {
            let mut entry = detached.clone();
            if detached.id == self.selected_chat {
                entry.messages = self.conversation.clone();
            }
            entry.messages.retain(|m| m.starred);
            view.push(entry);
        }
        let entries = core::filter::starred_messages(&view);
        let query = self.starred_search.trim().to_lowercase();
        let visible: Vec<&(u64, &str, &Message)> = entries
            .iter()
            .filter(|(_, title, msg)| {
                query.is_empty()
                    || title.to_lowercase().contains(&query)
                    || snippet(msg).to_lowercase().contains(&query)
            })
            .collect();
        let none_starred = entries.is_empty();
        let mut open: Option<(u64, u64)> = None;
        let mut back = false;
        let panel = egui::Panel::left("chat_list")
            .resizable(true)
            .default_size(350.0)
            .size_range(280.0..=460.0)
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)).inner_margin(Margin::symmetric(10, 8)))
            .show(root, |ui| {
                // Header: back to the chat list, then the title.
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
                        RichText::new(crate::i18n::t("nav.starred"))
                            .font(crate::theme::semibold(17.0))
                            .color(rgb(t.text_primary)),
                    );
                });
                ui.add_space(10.0);
                // Search pill, the shared list-panel component.
                crate::widgets::search_pill(
                    ui,
                    t,
                    egui::Id::unique("starred_search"),
                    &crate::i18n::t("chats.search_hint"),
                    &mut self.starred_search,
                );
                ui.add_space(8.0);
                if none_starred {
                    crate::widgets::empty_state(ui, t, &crate::i18n::t("starred.empty"));
                    return;
                }
                if visible.is_empty() {
                    crate::widgets::empty_state(ui, t, &crate::i18n::t("chats.no_match"));
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (chat_id, title, msg) in &visible {
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), ROW_H),
                            Sense::click(),
                        );
                        let hov =
                            crate::anim::hover_factor(ui, ("starred_row", msg.id), resp.hovered());
                        if hov > 0.001 {
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(10),
                                crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
                            );
                        }
                        let painter = ui.painter();
                        let text_left = rect.left() + 8.0;
                        // First line: the source chat's title and
                        // the message's full date, right-aligned.
                        let date = date_line(msg);
                        let date_font = FontId::proportional(11.5);
                        let date_w = ui.ctx().fonts_mut(|f| {
                            f.layout_no_wrap(date.clone(), date_font.clone(), rgb(t.text_faint))
                                .size()
                                .x
                        });
                        painter.text(
                            Pos2::new(rect.right() - 6.0, rect.top() + 16.0),
                            egui::Align2::RIGHT_CENTER,
                            date,
                            date_font,
                            rgb(t.text_faint),
                        );
                        let title_budget = (rect.right() - 14.0 - date_w - text_left).max(0.0);
                        let title_text = crate::widgets::ellipsize(
                            ui,
                            title,
                            crate::theme::medium(14.0),
                            title_budget,
                        );
                        painter.text(
                            Pos2::new(text_left, rect.top() + 16.0),
                            egui::Align2::LEFT_CENTER,
                            title_text,
                            crate::theme::medium(14.0),
                            rgb(t.text_primary),
                        );
                        // Second line: a kind chip for media, the
                        // text snippet for plain text.
                        let line2_y = rect.top() + 40.0;
                        if let Some(kind) = chip_label(msg) {
                            let chip_text = format!("{kind} {}", twelve_hour(&msg.time));
                            let chip_font = FontId::proportional(11.5);
                            let chip_w = ui.ctx().fonts_mut(|f| {
                                f.layout_no_wrap(
                                    chip_text.clone(),
                                    chip_font.clone(),
                                    rgb(t.text_secondary),
                                )
                                .size()
                                .x
                            }) + 16.0;
                            let chip_rect = egui::Rect::from_min_size(
                                Pos2::new(text_left, line2_y - 9.0),
                                Vec2::new(chip_w, 19.0),
                            );
                            painter.rect_filled(
                                chip_rect,
                                CornerRadius::same(9),
                                rgb(t.bg_panel_alt),
                            );
                            painter.text(
                                chip_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                chip_text,
                                chip_font,
                                rgb(t.text_secondary),
                            );
                        } else {
                            let budget = (rect.right() - 14.0 - text_left).max(0.0);
                            let line = crate::widgets::ellipsize(
                                ui,
                                &snippet(msg),
                                FontId::proportional(12.5),
                                budget,
                            );
                            painter.text(
                                Pos2::new(text_left, line2_y),
                                egui::Align2::LEFT_CENTER,
                                line,
                                FontId::proportional(12.5),
                                rgb(t.text_secondary),
                            );
                        }
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            open = Some((*chat_id, msg.id));
                        }
                    }
                });
            });
        crate::widgets::soft_right_edge(root, panel.response.rect, t);
        if back {
            self.screen = Screen::Chats;
            self.filter = ChatFilter::All;
        }
        if let Some((id, msg_id)) = open {
            // Switching chats: write the working copy back first
            // (the demo rule the chat-list click path follows),
            // then select. Clicking a row of the already-open
            // chat only locates the message. The panel itself
            // stays: the back chevron is the way back to the
            // chat list.
            if id != self.selected_chat {
                if !self.live_mode {
                    let previous = self.selected_chat;
                    self.write_back_conversation(previous);
                }
                self.open_community = None;
                self.community_home_from_overview = false;
                self.community_origin = None;
                self.select_chat_now(id);
            }
            // Locate the starred message in the stream: scroll it
            // into view and flash the highlight over its bubble.
            self.scroll_to_message = Some(msg_id);
            self.highlight_message = Some(msg_id);
            self.highlight_started = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_full_date, snippet, twelve_hour};
    use core::model::{Message, MessageKind};

    #[test]
    fn view_once_snippet_is_only_the_placeholder() {
        // Privacy finding D2: starring a view-once message must
        // not pin its caption into this list.
        let mut vo = Message::theirs(1, "Sarah", "a secret caption", "10:00");
        vo.kind = MessageKind::Image { caption: "a secret caption".to_owned(), view_once: true };
        assert_eq!(snippet(&vo), "View once");
        // A normal photo's caption still serves as its snippet.
        let mut photo = Message::theirs(2, "Sarah", "a normal caption", "10:01");
        photo.kind =
            MessageKind::Image { caption: "a normal caption".to_owned(), view_once: false };
        assert_eq!(snippet(&photo), "a normal caption");
    }

    #[test]
    fn clocks_convert_to_12_hour() {
        assert_eq!(twelve_hour("17:35"), "5:35 PM");
        assert_eq!(twelve_hour("09:05"), "9:05 AM");
        assert_eq!(twelve_hour("00:00"), "12:00 AM");
        assert_eq!(twelve_hour("12:00"), "12:00 PM");
        assert_eq!(twelve_hour("23:59"), "11:59 PM");
        // Unparseable clocks pass through untouched.
        assert_eq!(twelve_hour("…"), "…");
        assert_eq!(twelve_hour("25:00"), "25:00");
    }

    #[test]
    fn full_dates_parse_back() {
        assert_eq!(parse_full_date("14 November 2026"), Some((2026, 11, 14)));
        assert_eq!(parse_full_date("1 January 2027"), Some((2027, 1, 1)));
        assert_eq!(parse_full_date("Today"), None);
        assert_eq!(parse_full_date("14 Nov 2026"), None);
    }
}
