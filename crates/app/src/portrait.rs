//! Portrait mode: the native phone arrangement for windows
//! that are single-pane territory and taller than wide (see
//! `LayoutMode::Portrait` in state.rs). The rail gives way to
//! a bottom navigation bar with the four phone tabs (Chat,
//! Updates, Communities, Calls); the chat list grows a phone
//! header, an archived row and a new-chat FAB (chat_list.rs);
//! and content (an open conversation, a community home) plus
//! the non-tab pages (Starred, Archived, Settings) take the
//! full screen with the bar hidden, the way a phone app
//! behaves. Landscape and square single-pane windows keep the
//! rail shell from round 2; nothing here draws outside
//! portrait mode.

use crate::state::{App, Screen};
use crate::theme::rgb;
use core::filter::total_unread;
use eframe::egui::{self, Align2, CornerRadius, FontId, Margin, Pos2, Sense, Stroke, Vec2};

/// The bottom navigation bar's height: room for the icon on
/// its selected pill and the label under it, nothing more.
pub(crate) const BOTTOM_NAV_H: f32 = 62.0;

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
    /// (Chat, Updates, Communities, Calls), drawn as a bottom
    /// panel so the screen above reserves its height.
    pub(crate) fn draw_bottom_nav(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let unread = total_unread(&self.chats);
        let mut go: Option<Screen> = None;
        egui::Panel::bottom("bottom_nav")
            .exact_size(BOTTOM_NAV_H)
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)).inner_margin(Margin::symmetric(0, 0)))
            .show(root, |ui| {
                let rect = ui.max_rect();
                ui.painter().hline(
                    rect.x_range(),
                    rect.top() + 0.5,
                    Stroke::new(1.0_f32, rgb(t.border)),
                );
                ui.horizontal(|ui| {
                    let tab_w = ui.available_width() / 4.0;
                    let chats_selected =
                        matches!(self.screen, Screen::Chats | Screen::Channels | Screen::Groups);
                    let tabs = [
                        (
                            Screen::Chats,
                            crate::icons::Icon::Chat,
                            crate::i18n::t("nav.chats"),
                            chats_selected,
                            unread,
                        ),
                        (
                            Screen::Updates,
                            crate::icons::Icon::Status,
                            crate::i18n::t("nav.updates"),
                            self.screen == Screen::Updates,
                            0,
                        ),
                        (
                            Screen::Communities,
                            crate::icons::Icon::Group,
                            crate::i18n::t("nav.communities"),
                            self.screen == Screen::Communities,
                            0,
                        ),
                        (
                            Screen::Calls,
                            crate::icons::Icon::Phone,
                            crate::i18n::t("nav.calls"),
                            self.screen == Screen::Calls,
                            0,
                        ),
                    ];
                    for (screen, icon, label, selected, badge) in tabs {
                        if bottom_tab(ui, t, icon, &label, selected, badge, tab_w) {
                            go = Some(screen);
                        }
                    }
                });
            });
        if let Some(screen) = go {
            self.portrait_tab_go(screen);
        }
    }

    /// The Updates page: the bottom bar's second tab. Status
    /// is an unbuilt parity feature, so the page is honest
    /// about that: the localized title and the empty state, no
    /// fabricated statuses. It fills the content area in every
    /// layout mode (on desktop the rail keeps it company).
    pub(crate) fn draw_updates(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)).inner_margin(Margin::symmetric(16, 14)))
            .show(root, |ui| {
                ui.label(
                    egui::RichText::new(crate::i18n::t("nav.updates"))
                        .font(crate::theme::semibold(20.0))
                        .color(rgb(t.text_primary)),
                );
                crate::widgets::empty_state(ui, t, &crate::i18n::t("updates.empty"));
            });
    }

    /// The new-chat floating action button: floats at the chat
    /// list's bottom right, above the bottom bar. It triggers
    /// the chat list header's own compose action (the
    /// create-group dialog state), never a second flow.
    pub(crate) fn draw_new_chat_fab(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let rect = root.ctx().content_rect();
        let size = 56.0;
        let pos = Pos2::new(rect.right() - size - 16.0, rect.bottom() - BOTTOM_NAV_H - size - 16.0);
        let mut clicked = false;
        egui::Area::new(egui::Id::unique("portrait_fab"))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(root.ctx(), |ui| {
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
                let fill = if resp.is_pointer_button_down_on() {
                    rgb(core::theme::pressed_step(t.accent))
                } else if resp.hovered() {
                    rgb(core::theme::hover_step(t.accent))
                } else {
                    rgb(t.accent)
                };
                ui.painter().rect_filled(r, CornerRadius::same(16), fill);
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
}

/// One bottom navigation tab: the icon on a pill highlight
/// while selected (the phone pattern), the label under it, and
/// an unread count pill at the icon's top right, in the chat
/// row badge's language. Returns true on click.
fn bottom_tab(
    ui: &mut egui::Ui,
    t: core::theme::Tokens,
    icon: crate::icons::Icon,
    label: &str,
    selected: bool,
    badge_count: u32,
    width: f32,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, BOTTOM_NAV_H), Sense::click());
    let icon_center = Pos2::new(rect.center().x, rect.top() + 19.0);
    let pill = egui::Rect::from_center_size(icon_center, Vec2::new(58.0, 30.0));
    if selected {
        // The selected pill: the same glass wash the rail
        // button's selected state wears, widened into the
        // phone tab's lozenge.
        ui.painter().rect_filled(
            pill,
            CornerRadius::same(15),
            crate::theme::glass_wash(t, t.bg_panel_alt, 150),
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
    ui.painter().text(
        Pos2::new(rect.center().x, rect.top() + 45.0),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.0),
        if selected { rgb(t.text_primary) } else { rgb(t.text_secondary) },
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label).clicked()
}
