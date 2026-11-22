//! Portrait mode: the native phone arrangement for windows
//! that are single-pane territory and taller than wide (see
//! `LayoutMode::Portrait` in state.rs). The rail gives way to
//! a floating bottom navigation bar with the four phone tabs
//! (Chat, Updates, Communities, Calls); the chat list grows a
//! phone header, an archived row and a new-chat FAB
//! (chat_list.rs); the Calls tab is re-sliced to the phone
//! reference (calls.rs); and content (an open conversation, a
//! community home) plus the non-tab pages (Starred, Archived,
//! Settings) take the full screen with the bar hidden, the way
//! a phone app behaves. Landscape and square single-pane
//! windows keep the rail shell from round 2; nothing here
//! draws outside portrait mode.

use crate::state::{App, Screen};
use crate::theme::rgb;
use core::filter::total_unread;
use eframe::egui::{self, Align2, CornerRadius, FontId, Margin, Pos2, Rect, Sense, Stroke, Vec2};

/// The height the bottom strip reserves at the window's foot:
/// the floating bar, its margin from the window's bottom edge
/// and a breathing gap above it for its shadow. The screens
/// end at this strip, so the last list rows are never under
/// the bar; the bar itself is painted inside the strip (see
/// [`nav_bar_rect`]).
pub(crate) const BOTTOM_NAV_H: f32 = 88.0;

/// The floating bar's own height: the icon on its selected
/// pill and the label under it, nothing more.
const NAV_BAR_H: f32 = 64.0;

/// The bar's margin from the window's bottom edge (the liquid
/// float: the bar hovers over the panel ground instead of
/// being attached to the window's edges).
const NAV_BAR_BOTTOM: f32 = 14.0;

/// The bar's margin from the window's side edges.
const NAV_BAR_SIDE: f32 = 16.0;

/// The floating bar's rect inside the reserved strip: inset
/// by the side margins, resting on the bottom margin, the
/// strip's remainder above it is the shadow's room. Pure
/// geometry, so the graded sizes can pin it in tests: the bar
/// is always fully inside the window, at any width.
fn nav_bar_rect(strip: Rect) -> Rect {
    Rect::from_min_max(
        Pos2::new(strip.left() + NAV_BAR_SIDE, strip.bottom() - NAV_BAR_BOTTOM - NAV_BAR_H),
        Pos2::new(strip.right() - NAV_BAR_SIDE, strip.bottom() - NAV_BAR_BOTTOM),
    )
}

/// One tab's slot inside the bar: the bar's inner rect (a
/// small horizontal inset so the end pills clear the bar's
/// rounded ends) split into four equal columns. The slots are
/// computed from the bar rect, never laid out by a horizontal
/// layout: egui's item spacing used to stack on top of four
/// quarter-width slots and push the last tab 24 pt past the
/// window's right edge (the round-4 crop).
fn nav_slot(bar: Rect, index: usize) -> Rect {
    let inner = bar.shrink2(Vec2::new(8.0, 0.0));
    let slot_w = inner.width() / 4.0;
    Rect::from_min_size(
        Pos2::new(inner.left() + slot_w * index as f32, bar.top()),
        Vec2::new(slot_w, bar.height()),
    )
}

/// The one floating action button shape: a 56 pt circle, the
/// same shape as the conversation's scroll-to-bottom button.
/// Every FAB on every tab uses this size and shape, so the
/// floating action reads as one language across screens.
const FAB_SIZE: f32 = 56.0;

/// The floating action buttons' rect: bottom right, one gap
/// above the floating bar's top edge (the gap the attached
/// bar's FAB kept), the same side inset as the bar.
fn fab_rect(content: Rect, size: f32) -> Rect {
    Rect::from_min_size(
        Pos2::new(
            content.right() - size - 16.0,
            content.bottom() - NAV_BAR_BOTTOM - NAV_BAR_H - size - 16.0,
        ),
        Vec2::splat(size),
    )
}

impl App {
    /// Whether the bottom navigation bar is on screen this
    /// frame: portrait mode, on one of the tab screens, with no
    /// full-screen content over it. An open conversation or
    /// community home hides the bar; so do Starred, Archived
    /// and Settings, which are full-screen pages, not tabs.
    pub(crate) fn bottom_nav_visible(&self) -> bool {
        self.portrait
            && !self.content_visible()
            && matches!(
                self.screen,
                Screen::Chats
                    | Screen::Channels
                    | Screen::Groups
                    | Screen::Communities
                    | Screen::Calls
                    | Screen::Updates
            )
    }

    /// Whether the new-chat FAB floats over the chat list:
    /// portrait mode on a chats-family tab, no content open.
    pub(crate) fn portrait_fab_visible(&self) -> bool {
        self.portrait
            && !self.content_visible()
            && matches!(self.screen, Screen::Chats | Screen::Channels | Screen::Groups)
    }

    /// Whether the new-call FAB floats over the Calls tab:
    /// portrait mode on the Calls tab, no content open. The
    /// reference phone log carries its own call FAB, distinct
    /// from the chat list's compose FAB.
    pub(crate) fn portrait_calls_fab_visible(&self) -> bool {
        self.portrait && !self.content_visible() && self.screen == Screen::Calls
    }

    /// Bottom-bar navigation: the rail button's state changes,
    /// minus the rail. Lands on the tab's list (an open
    /// conversation closes back to it) with the tab's filter.
    fn portrait_tab_go(&mut self, screen: Screen) {
        self.screen = screen;
        self.conversation_open = false;
        self.filter = screen.filter();
        self.open_community = None;
        self.community_home_from_overview = false;
        self.community_origin = None;
    }

    /// The bottom navigation bar: four tabs in the phone order
    /// (Chat, Updates, Communities, Calls) on a floating pill.
    /// A bottom panel reserves the strip (its own fill is the
    /// plain panel ground the screens behind use, so the strip
    /// reads as more of the screen), and the bar is painted
    /// inside it as an elevated rounded rect with margins from
    /// every window edge, in the theme's floating-surface
    /// language (glass panel fill, hairline border, soft
    /// shadow, specular top edge).
    pub(crate) fn draw_bottom_nav(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let unread = total_unread(&self.chats);
        let mut go: Option<Screen> = None;
        egui::Panel::bottom("bottom_nav")
            .exact_size(BOTTOM_NAV_H)
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)).inner_margin(Margin::symmetric(0, 0)))
            .show(root, |ui| {
                let bar = nav_bar_rect(ui.max_rect());
                let painter = ui.painter();
                let radius = CornerRadius::same((NAV_BAR_H / 2.0) as u8);
                painter.add(crate::theme::glass_shadow(t).as_shape(bar, radius));
                painter.rect_filled(bar, radius, crate::theme::glass_panel_fill(t));
                painter.rect_stroke(
                    bar,
                    radius,
                    Stroke::new(1.0_f32, rgb(t.border)),
                    egui::StrokeKind::Inside,
                );
                crate::theme::paint_glass_edge(painter, bar, NAV_BAR_H / 2.0, t);
                let chats_selected =
                    matches!(self.screen, Screen::Chats | Screen::Channels | Screen::Groups);
                let tabs = [
                    (
                        Screen::Chats,
                        BottomTab {
                            key: "nav.chats",
                            icon: crate::icons::Icon::Chat,
                            label: crate::i18n::t("nav.chats"),
                            selected: chats_selected,
                            badge: unread,
                        },
                    ),
                    (
                        Screen::Updates,
                        BottomTab {
                            key: "nav.updates",
                            icon: crate::icons::Icon::Status,
                            label: crate::i18n::t("nav.updates"),
                            selected: self.screen == Screen::Updates,
                            badge: 0,
                        },
                    ),
                    (
                        Screen::Communities,
                        BottomTab {
                            key: "nav.communities",
                            icon: crate::icons::Icon::Group,
                            label: crate::i18n::t("nav.communities"),
                            selected: self.screen == Screen::Communities,
                            badge: 0,
                        },
                    ),
                    (
                        Screen::Calls,
                        BottomTab {
                            key: "nav.calls",
                            icon: crate::icons::Icon::Phone,
                            label: crate::i18n::t("nav.calls"),
                            selected: self.screen == Screen::Calls,
                            badge: 0,
                        },
                    ),
                ];
                for (index, (screen, tab)) in tabs.into_iter().enumerate() {
                    if bottom_tab(ui, t, nav_slot(bar, index), tab) {
                        go = Some(screen);
                    }
                }
            });
        if let Some(screen) = go {
            self.portrait_tab_go(screen);
        }
    }

    /// The new-chat floating action button: floats at the chat
    /// list's bottom right, above the floating bar. It triggers
    /// the chat list header's own compose action (the
    /// create-group dialog state), never a second flow.
    pub(crate) fn draw_new_chat_fab(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let size = FAB_SIZE;
        let rect = fab_rect(root.ctx().content_rect(), size);
        let mut clicked = false;
        egui::Area::new(egui::Id::unique("portrait_fab"))
            .fixed_pos(rect.min)
            .order(egui::Order::Foreground)
            .show(root.ctx(), |ui| {
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
                let fill = fab_fill(t, &resp);
                paint_fab(ui.painter(), t, r, fill);
                crate::icons::draw(
                    ui.painter(),
                    crate::icons::Icon::Edit,
                    r.center(),
                    26.0,
                    rgb(t.on_accent()),
                    fill,
                );
                if resp.clicked() {
                    clicked = true;
                }
                resp.on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(crate::i18n::t("chats.new_group"));
            });
        if clicked {
            self.groups.create_open = true;
        }
    }

    /// The Calls tab's floating action button: the reference
    /// phone log's green phone-plus action, floating where the
    /// chat FAB floats on its tab, in the same circle shape and
    /// size. It opens the new call contact picker, the same flow
    /// the Calls header's new call button opens in the other modes.
    pub(crate) fn draw_calls_fab(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let size = FAB_SIZE;
        let rect = fab_rect(root.ctx().content_rect(), size);
        let mut clicked = false;
        egui::Area::new(egui::Id::unique("portrait_calls_fab"))
            .fixed_pos(rect.min)
            .order(egui::Order::Foreground)
            .show(root.ctx(), |ui| {
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
                let fill = fab_fill(t, &resp);
                paint_fab(ui.painter(), t, r, fill);
                crate::icons::draw(
                    ui.painter(),
                    crate::icons::Icon::AddCall,
                    r.center(),
                    26.0,
                    rgb(t.on_accent()),
                    fill,
                );
                if resp.clicked() {
                    clicked = true;
                }
                resp.on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(crate::i18n::t("calls.new_call"));
            });
        if clicked {
            self.calls.picker_search.clear();
            self.calls.picker_open = true;
        }
    }
}

/// Paints one FAB: the floating layer's soft card shadow under
/// an accent circle. Shared by every FAB so shape, size and lift
/// can never drift apart between tabs.
fn paint_fab(painter: &egui::Painter, t: core::theme::Tokens, r: Rect, fill: egui::Color32) {
    painter
        .add(crate::theme::card_shadow(t).as_shape(r, CornerRadius::same((FAB_SIZE / 2.0) as u8)));
    painter.circle_filled(r.center(), FAB_SIZE / 2.0, fill);
}

/// The FABs' shared fill steps: accent at rest, its hover and
/// pressed steps under the pointer, like the surfaces they
/// float over.
fn fab_fill(t: core::theme::Tokens, resp: &egui::Response) -> egui::Color32 {
    if resp.is_pointer_button_down_on() {
        rgb(core::theme::pressed_step(t.accent))
    } else if resp.hovered() {
        rgb(core::theme::hover_step(t.accent))
    } else {
        rgb(t.accent)
    }
}

/// One bottom navigation tab's content: its identity for the
/// interaction id, its glyph and label, and its state.
struct BottomTab<'a> {
    key: &'a str,
    icon: crate::icons::Icon,
    label: String,
    selected: bool,
    badge: u32,
}

/// One bottom navigation tab, painted inside its slot of the
/// floating bar: the icon on a filled pill while selected
/// (the reference bar's dark accent lozenge), the label under
/// it (semibold while selected), and an unread count pill at
/// the icon's top right, in the chat row badge's language.
/// Everything is positioned from the slot rect, so a tab can
/// never drift out of the bar. Returns true on click.
fn bottom_tab(ui: &mut egui::Ui, t: core::theme::Tokens, slot: Rect, tab: BottomTab<'_>) -> bool {
    let BottomTab { key, icon, label, selected, badge: badge_count } = tab;
    let label = label.as_str();
    let response = ui.interact(slot, egui::Id::unique(("portrait_tab", key)), Sense::click());
    let icon_center = Pos2::new(slot.center().x, slot.top() + 21.0);
    let pill = egui::Rect::from_center_size(icon_center, Vec2::new(56.0, 30.0));
    if selected {
        // The selected pill: the accent pulled most of the way
        // toward the panel, the reference bar's filled dark
        // lozenge, with the bright accent glyph on it.
        ui.painter().rect_filled(
            pill,
            CornerRadius::same(15),
            rgb(core::theme::mix(t.accent, t.bg_panel, 0.70)),
        );
    } else if response.is_pointer_button_down_on() {
        ui.painter().rect_filled(
            pill,
            CornerRadius::same(15),
            rgb(core::theme::pressed_step(t.bg_panel_alt)),
        );
    } else if response.hovered() {
        ui.painter().rect_filled(pill, CornerRadius::same(15), rgb(t.bg_panel_alt));
    }
    let color = if selected { rgb(t.accent) } else { rgb(t.text_secondary) };
    crate::icons::draw(ui.painter(), icon, icon_center, 24.0, color, rgb(t.bg_panel));
    if badge_count > 0 {
        let text = badge_count.to_string();
        let font = FontId::proportional(11.0);
        let text_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(text.clone(), font.clone(), rgb(t.badge_text)).size().x
        });
        let w = (text_w + 10.0).max(19.0);
        let brect =
            egui::Rect::from_center_size(icon_center + Vec2::new(16.0, -11.0), Vec2::new(w, 19.0));
        ui.painter().rect_filled(brect, CornerRadius::same(9), rgb(t.badge_bg));
        ui.painter().text(brect.center(), Align2::CENTER_CENTER, text, font, rgb(t.badge_text));
    }
    let (label_font, label_color) = if selected {
        (crate::theme::semibold(11.0), rgb(t.text_primary))
    } else {
        (FontId::proportional(11.0), rgb(t.text_secondary))
    };
    ui.painter().text(
        Pos2::new(slot.center().x, slot.top() + 47.0),
        Align2::CENTER_CENTER,
        label,
        label_font,
        label_color,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label).clicked()
}

#[cfg(test)]
mod tests {
    use super::{fab_rect, nav_bar_rect, nav_slot, BOTTOM_NAV_H, NAV_BAR_BOTTOM, NAV_BAR_SIDE};
    use eframe::egui::{Pos2, Rect, Vec2};

    /// The strip a window of the given physical width reserves
    /// at its foot (a 844-tall window, the graded portrait
    /// height), in points.
    fn strip(width: f32) -> Rect {
        Rect::from_min_max(Pos2::new(0.0, 844.0 - BOTTOM_NAV_H), Pos2::new(width, 844.0))
    }

    #[test]
    fn floating_bar_stays_inside_the_window() {
        // The graded portrait widths, in points at the portrait
        // density: the bar keeps its side and bottom margins
        // and never reaches a window edge.
        for width in [400.0, 433.3] {
            let bar = nav_bar_rect(strip(width));
            assert_eq!(bar.left(), NAV_BAR_SIDE);
            assert_eq!(bar.right(), width - NAV_BAR_SIDE);
            assert_eq!(strip(width).bottom() - bar.bottom(), NAV_BAR_BOTTOM);
            assert!(bar.width() < width);
        }
    }

    #[test]
    fn tab_slots_are_equal_and_inside_the_bar() {
        // The round-4 crop, pinned shut: four slots of exactly
        // the inner width over four, the last slot ending at
        // the bar's inner right edge, never past it.
        for width in [400.0, 433.3] {
            let bar = nav_bar_rect(strip(width));
            let slots: Vec<Rect> = (0..4).map(|i| nav_slot(bar, i)).collect();
            let w = slots[0].width();
            assert!(slots.iter().all(|s| (s.width() - w).abs() < 1e-4));
            assert_eq!(slots[0].left(), bar.left() + 8.0);
            assert!((slots[3].right() - (bar.right() - 8.0)).abs() < 1e-4);
            assert!(slots[3].right() < width);
            for pair in slots.windows(2) {
                assert!((pair[1].left() - pair[0].right()).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn fab_floats_one_gap_above_the_bar() {
        let content = Rect::from_min_max(Pos2::ZERO, Pos2::new(433.3, 937.8));
        let bar = nav_bar_rect(Rect::from_min_max(
            Pos2::new(0.0, 937.8 - BOTTOM_NAV_H),
            Pos2::new(433.3, 937.8),
        ));
        let fab = fab_rect(content, 56.0);
        assert!((bar.top() - fab.bottom() - 16.0).abs() < 1e-4);
        assert!((content.right() - fab.right() - 16.0).abs() < 1e-4);
        assert_eq!(fab.size(), Vec2::splat(56.0));
    }
}
