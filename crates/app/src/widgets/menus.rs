//! Popup menu rows: the one row style for context menus and popup menus.

use crate::theme::rgb;
use core::theme::{Rgb, Tokens};
use eframe::egui::{self, FontId};

/// The sense a menu row allocates with: click normally, hover
/// only inside a disabled scope, so a menu playing its exit
/// animation (drawn disabled) can never be clicked through.
fn menu_sense(ui: &egui::Ui) -> egui::Sense {
    if ui.is_enabled() {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    }
}

/// Paints one menu row's hover and pressed wash through the
/// shared animation factors, so menu rows crossfade on the
/// motion system's HOVER and PRESS durations (and snap
/// instantly under Motion Off) like every other row and button,
/// instead of snapping between hard fills.
fn menu_wash(ui: &mut egui::Ui, t: Tokens, rect: egui::Rect, resp: &egui::Response, key: &str) {
    let hov = crate::anim::hover_factor(ui, ("menu_row", key), resp.hovered());
    let press = crate::anim::press_factor(ui, ("menu_row", key), resp.is_pointer_button_down_on());
    if press > 0.001 {
        let pressed = core::theme::pressed_step(t.bg_panel_alt);
        let fill = core::theme::mix(t.bg_panel_alt, pressed, press);
        ui.painter().rect_filled(rect, egui::CornerRadius::same(8), rgb(fill));
    } else if hov > 0.001 {
        let fill = core::theme::mix(t.bg_panel, t.bg_panel_alt, hov);
        ui.painter().rect_filled(rect, egui::CornerRadius::same(8), rgb(fill));
    }
}

/// A popup menu row with the standard icon gutter: an 18 px icon
/// slot at a fixed inset on every row, so labels align down the
/// whole menu, plus an optional right-aligned shortcut hint.
/// Hints are only ever passed for shortcuts that really exist;
/// most actions have none and pass `None`.
pub(crate) fn menu_item_icon(
    ui: &mut egui::Ui,
    t: Tokens,
    icon: Option<crate::icons::Icon>,
    label: &str,
    hint: Option<&str>,
) -> egui::Response {
    menu_item_row(ui, t, icon.map(|i| (i, t.text_secondary)), label, hint, false)
}

/// Like [`menu_item_icon`], with the icon drawn in a caller
/// colour (the star on an already-starred message, in the
/// accent, say).
pub(crate) fn menu_item_icon_tinted(
    ui: &mut egui::Ui,
    t: Tokens,
    icon: crate::icons::Icon,
    color: Rgb,
    label: &str,
) -> egui::Response {
    menu_item_row(ui, t, Some((icon, color)), label, None, false)
}

/// A destructive popup menu row: icon and label in the danger
/// colour, same gutter anatomy as [`menu_item_icon`].
pub(crate) fn menu_item_danger(
    ui: &mut egui::Ui,
    t: Tokens,
    icon: crate::icons::Icon,
    label: &str,
) -> egui::Response {
    menu_item_row(ui, t, Some((icon, t.danger)), label, None, true)
}

/// A popup menu row that opens a submenu: the standard row
/// anatomy plus a chevron at the right edge naming the
/// flyout that belongs to it (qc1654 Item E). The flyout
/// itself is the owning popup's business; this row is only
/// the affordance, shared by every grouped action family.
pub(crate) fn menu_item_submenu(
    ui: &mut egui::Ui,
    t: Tokens,
    icon: crate::icons::Icon,
    label: &str,
) -> egui::Response {
    let resp = menu_item_row(ui, t, Some((icon, t.text_secondary)), label, None, false);
    paint_submenu_chevron(ui, t, resp.rect, false);
    resp
}

/// The destructive form of [`menu_item_submenu`]: icon and
/// label in the danger colour, the chevron likewise.
pub(crate) fn menu_item_submenu_danger(
    ui: &mut egui::Ui,
    t: Tokens,
    icon: crate::icons::Icon,
    label: &str,
) -> egui::Response {
    let resp = menu_item_row(ui, t, Some((icon, t.danger)), label, None, true);
    paint_submenu_chevron(ui, t, resp.rect, true);
    resp
}

/// Paints the submenu chevron over a row that
/// [`menu_item_row`] just laid out, at the row's right edge.
fn paint_submenu_chevron(ui: &mut egui::Ui, t: Tokens, rect: egui::Rect, danger: bool) {
    let color = if danger { t.danger } else { t.text_faint };
    crate::icons::draw(
        ui.painter(),
        crate::icons::Icon::ChevronRight,
        egui::pos2(rect.right() - 14.0, rect.center().y),
        crate::icons::SIZE_ROW,
        rgb(color),
        rgb(t.bg_panel),
    );
}

fn menu_item_row(
    ui: &mut egui::Ui,
    t: Tokens,
    icon: Option<(crate::icons::Icon, Rgb)>,
    label: &str,
    hint: Option<&str>,
    danger: bool,
) -> egui::Response {
    let width = ui.available_width().max(120.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 36.0), menu_sense(ui));
    menu_wash(ui, t, rect, &resp, label);
    if let Some((icon, color)) = icon {
        crate::icons::draw(
            ui.painter(),
            icon,
            egui::pos2(rect.left() + 19.0, rect.center().y),
            crate::icons::SIZE_MENU,
            rgb(color),
            rgb(t.bg_panel),
        );
    }
    ui.painter().text(
        egui::pos2(rect.left() + 38.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        crate::theme::medium(12.0),
        if danger { rgb(t.danger) } else { rgb(t.text_primary) },
    );
    if let Some(hint) = hint {
        ui.painter().text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            hint,
            FontId::proportional(12.0),
            rgb(t.text_faint),
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
