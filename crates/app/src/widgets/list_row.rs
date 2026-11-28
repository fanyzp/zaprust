//! The shared list row: one component for every left-panel row
//! built on the chat row's anatomy (chat list, global search
//! hits, starred, calls, updates/status, communities).
//!
//! Why one component: the row used to exist as five-plus
//! hand-painted copies that shared metric constants but not
//! code, so every row fix had to be repeated per panel and the
//! copies drifted into two different hover-wash dialects (an
//! opaque step mix in the chat row, a translucent alpha wash
//! everywhere else). This component owns the row height and
//! padding geometry, the avatar slot, the title/snippet/meta
//! layout positions, and the state washes, so a panel only
//! supplies its own content for those slots.
//!
//! State washes are one uniform OPAQUE step wash (owner vote C
//! for rows): hover is the hover step of the panel surface,
//! pressed is its pressed step, and selected/active is the
//! pressed step held steady. No translucent wash remains on
//! any row this component paints.

use crate::theme::rgb;
use core::theme::Tokens;
use eframe::egui::{self, CornerRadius, FontId, Pos2, Sense, Vec2};

/// Shared list-row metrics: the one geometry every row built on
/// the chat row's anatomy uses, so the rows can never drift
/// apart. All values are in points, relative to the row rect.
pub(crate) const ROW_H: f32 = 62.0;
/// Avatar centre x offset from the row's left edge.
pub(crate) const AVATAR_CX: f32 = 33.0;
/// Avatar radius.
pub(crate) const AVATAR_R: f32 = 23.0;
/// Text column x offset from the row's left edge.
pub(crate) const TEXT_X: f32 = 66.0;
/// Title baseline y offset from the row's top edge.
pub(crate) const TITLE_DY: f32 = 21.0;
/// Snippet baseline y offset from the row's top edge.
pub(crate) const SNIPPET_DY: f32 = 43.0;
/// Height of a compact entry row (the chat list's Archived,
/// Locked and Labels entries): one line, no snippet slot.
pub(crate) const ENTRY_ROW_H: f32 = 48.0;

/// Paints one row's state wash: the uniform opaque step wash.
/// `pressed` is passed in rather than read from the response
/// because one caller (the managed-group row) derives its press
/// from a sub-rect of the row, short of its kebab slot.
pub(crate) fn paint_list_row_wash_with_press(
    ui: &mut egui::Ui,
    t: Tokens,
    rect: egui::Rect,
    key: impl std::hash::Hash + std::fmt::Debug + Clone,
    selected: bool,
    hovered: bool,
    pressed: bool,
) {
    if selected {
        // Selected/active holds the pressed step steady: the
        // same opaque step a press lands on, never a separate
        // translucent constant.
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(crate::theme::RADIUS_SMALL),
            rgb(core::theme::pressed_step(t.bg_panel)),
        );
        return;
    }
    let hov = crate::anim::hover_factor(ui, ("list_row", key.clone()), hovered);
    if hov > 0.001 {
        let fill = core::theme::mix(t.bg_panel, core::theme::hover_step(t.bg_panel), hov);
        ui.painter().rect_filled(rect, CornerRadius::same(crate::theme::RADIUS_SMALL), rgb(fill));
    }
    let press = crate::anim::press_factor(ui, ("list_row", key), pressed);
    if press > 0.001 {
        let fill = core::theme::mix(t.bg_panel, core::theme::pressed_step(t.bg_panel), press);
        ui.painter().rect_filled(rect, CornerRadius::same(crate::theme::RADIUS_SMALL), rgb(fill));
    }
}

/// Paints the state wash for a row whose press state is its
/// response's own pointer-down state (the common case).
pub(crate) fn paint_list_row_wash(
    ui: &mut egui::Ui,
    t: Tokens,
    rect: egui::Rect,
    resp: &egui::Response,
    key: impl std::hash::Hash + std::fmt::Debug + Clone,
    selected: bool,
) {
    paint_list_row_wash_with_press(
        ui,
        t,
        rect,
        key,
        selected,
        resp.hovered(),
        resp.is_pointer_button_down_on(),
    );
}

/// One allocated list row: its rect and response plus the
/// shared slot positions every row content paints into.
pub(crate) struct ListRow {
    pub rect: egui::Rect,
    pub response: egui::Response,
}

impl ListRow {
    /// The avatar slot's centre: every row's avatar, icon or
    /// cover glyph centres on this one point.
    pub(crate) fn avatar_center(&self) -> Pos2 {
        Pos2::new(self.rect.left() + AVATAR_CX, self.rect.center().y)
    }

    /// The text column's left edge.
    pub(crate) fn text_left(&self) -> f32 {
        self.rect.left() + TEXT_X
    }

    /// The title line's paint position.
    pub(crate) fn title_pos(&self) -> Pos2 {
        Pos2::new(self.text_left(), self.rect.top() + TITLE_DY)
    }

    /// The snippet line's paint position.
    pub(crate) fn snippet_pos(&self) -> Pos2 {
        Pos2::new(self.text_left(), self.rect.top() + SNIPPET_DY)
    }
}

/// Allocates a full-width row of `height` and paints its state
/// wash. This is the one entry point panels use for a row built
/// on the shared anatomy.
pub(crate) fn list_row(
    ui: &mut egui::Ui,
    t: Tokens,
    key: impl std::hash::Hash + std::fmt::Debug + Clone,
    height: f32,
    selected: bool,
    sense: Sense,
) -> ListRow {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), sense);
    paint_list_row_wash(ui, t, rect, &response, key, selected);
    ListRow { rect, response }
}

/// Allocates a standard-height ([`ROW_H`]) row and paints its
/// state wash.
pub(crate) fn list_row_std(
    ui: &mut egui::Ui,
    t: Tokens,
    key: impl std::hash::Hash + std::fmt::Debug + Clone,
    selected: bool,
    sense: Sense,
) -> ListRow {
    list_row(ui, t, key, ROW_H, selected, sense)
}

/// Paints a row's title line into the shared title slot,
/// ellipsized to `budget`, and returns the painted rect (rows
/// with badges after the title measure from it).
pub(crate) fn paint_row_title(
    ui: &mut egui::Ui,
    row: &ListRow,
    text: &str,
    font: FontId,
    color: egui::Color32,
    budget: f32,
) -> egui::Rect {
    let shown = core::bidi::display(&crate::widgets::ellipsize(ui, text, font.clone(), budget));
    ui.painter().text(row.title_pos(), egui::Align2::LEFT_CENTER, shown, font, color)
}

/// Paints a row's snippet line into the shared snippet slot,
/// ellipsized to the width between the text column and
/// `right_limit`. An empty snippet paints nothing.
pub(crate) fn paint_row_snippet(
    ui: &mut egui::Ui,
    row: &ListRow,
    text: &str,
    font: FontId,
    color: egui::Color32,
    right_limit: f32,
) {
    if text.is_empty() {
        return;
    }
    let budget = (right_limit - row.text_left()).max(0.0);
    let shown = core::bidi::display(&crate::widgets::ellipsize(ui, text, font.clone(), budget));
    if !shown.is_empty() {
        ui.painter().text(row.snippet_pos(), egui::Align2::LEFT_CENTER, shown, font, color);
    }
}

/// One compact entry row of the chat list (Archived, Locked,
/// Labels and their portrait sibling): a glyph in the avatar
/// slot, a one-line label in the text column, and a count at
/// the right edge, on the shared wash. These rows used to be
/// the same paint block copy-pasted once per entry; they are
/// one call now. Returns the row response.
pub(crate) fn entry_row(
    ui: &mut egui::Ui,
    t: Tokens,
    key: impl std::hash::Hash + std::fmt::Debug + Clone,
    icon: crate::icons::Icon,
    label: &str,
    count: &str,
) -> egui::Response {
    let row = list_row(ui, t, key, ENTRY_ROW_H, false, Sense::click());
    crate::icons::draw(
        ui.painter(),
        icon,
        row.avatar_center(),
        crate::icons::SIZE_CONTROL,
        rgb(t.text_secondary),
        rgb(t.bg_panel),
    );
    ui.painter().text(
        Pos2::new(row.text_left(), row.rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        crate::theme::medium(15.0),
        rgb(t.text_primary),
    );
    ui.painter().text(
        Pos2::new(row.rect.right() - 6.0, row.rect.center().y),
        egui::Align2::RIGHT_CENTER,
        count,
        FontId::proportional(12.0),
        rgb(t.text_faint),
    );
    row.response.on_hover_cursor(egui::CursorIcon::PointingHand)
}
