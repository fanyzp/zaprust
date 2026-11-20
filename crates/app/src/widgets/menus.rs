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

/// A full-width popup menu row: left-aligned label, hover fill,
/// radius 8. The one row style for context menus and popup menus.
pub(crate) fn menu_item(ui: &mut egui::Ui, t: Tokens, label: &str) -> egui::Response {
    let width = ui.available_width().max(120.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 36.0), menu_sense(ui));
    if resp.is_pointer_button_down_on() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(8),
            rgb(core::theme::pressed_step(t.bg_panel_alt)),
        );
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, egui::CornerRadius::same(8), rgb(t.bg_panel_alt));
    }
    ui.painter().text(
        egui::pos2(rect.left() + 10.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        crate::theme::medium(12.5),
        rgb(t.text_primary),
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
    if resp.is_pointer_button_down_on() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(8),
            rgb(core::theme::pressed_step(t.bg_panel_alt)),
        );
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, egui::CornerRadius::same(8), rgb(t.bg_panel_alt));
    }
    if let Some((icon, color)) = icon {
        crate::icons::draw(
            ui.painter(),
            icon,
            egui::pos2(rect.left() + 19.0, rect.center().y),
            18.0,
            rgb(color),
            rgb(t.bg_panel),
        );
    }
    ui.painter().text(
        egui::pos2(rect.left() + 38.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        crate::theme::medium(12.5),
        if danger { rgb(t.danger) } else { rgb(t.text_primary) },
    );
    if let Some(hint) = hint {
        ui.painter().text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            hint,
            FontId::proportional(11.5),
            rgb(t.text_faint),
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
