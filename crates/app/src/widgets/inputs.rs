//! Text inputs and form controls: text fields, selects, number fields,
//! form rows, radio rows, switches and the search pill.

use crate::theme::{rgb, SP_HALF};
use core::theme::Tokens;
use eframe::egui::{self, Color32, CornerRadius, FontId, Pos2, RichText, Sense, Stroke, Vec2};

/// The field fill shared by every text input: the alt panel stepped
/// toward the app background (same derivation as the global style).
pub(crate) fn field_fill(t: Tokens) -> Color32 {
    rgb(core::theme::mix(t.bg_panel_alt, t.bg_app, 0.35))
}

/// The one text field: rounded recessed frame, frameless inner
/// editor, and an accent ring while it has keyboard focus, so focus
/// is as visible as hover. Replaces bare `TextEdit`s, which rendered
/// with a bright stock outline and no focus state at all.
pub(crate) fn text_field(
    ui: &mut egui::Ui,
    t: Tokens,
    value: &mut String,
    hint: &str,
    width: f32,
) -> egui::Response {
    text_field_impl(ui, t, value, hint, width, false, 1)
}

/// Password variant of [`text_field`] (credential values).
pub(crate) fn text_field_password(
    ui: &mut egui::Ui,
    t: Tokens,
    value: &mut String,
    hint: &str,
    width: f32,
) -> egui::Response {
    text_field_impl(ui, t, value, hint, width, true, 1)
}

fn text_field_impl(
    ui: &mut egui::Ui,
    t: Tokens,
    value: &mut String,
    hint: &str,
    width: f32,
    password: bool,
    rows: usize,
) -> egui::Response {
    let frame = egui::Frame::new()
        .fill(field_fill(t))
        .corner_radius(crate::theme::RADIUS_MICRO)
        .inner_margin(egui::Margin::symmetric(10, 8));
    let shown = frame.show(ui, |ui| {
        let edit = if rows > 1 {
            egui::TextEdit::multiline(value).desired_rows(rows)
        } else {
            let mut e = egui::TextEdit::singleline(value);
            if password {
                e = e.password(true);
            }
            e
        };
        ui.add(edit.frame(egui::Frame::NONE).hint_text(hint).desired_width(width))
    });
    let resp = shown.inner;
    if resp.has_focus() {
        ui.painter().rect_stroke(
            shown.response.rect,
            crate::theme::RADIUS_MICRO,
            Stroke::new(1.5_f32, rgb(t.accent)),
            egui::StrokeKind::Inside,
        );
    }
    resp
}

/// The one dropdown: a ComboBox with consistent width and Medium
/// selected text; the popup inherits the global style table (radius
/// 10 open state, token strokes, real hover fills).
pub(crate) fn select(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    width: f32,
    selected: &str,
    add_contents: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let out = egui::ComboBox::from_id_salt(id)
        .selected_text(RichText::new(selected).font(crate::theme::medium(12.0)))
        .width(width)
        .show_ui(ui, add_contents);
    // The ComboBox popup is a stock egui popup; give it the
    // shared enter treatment (its popup id is the button id with
    // "popup", egui's own derivation).
    crate::anim::stock_popup_enter(
        ui.ctx(),
        out.response.id.with("popup"),
        out.response.rect.left_top(),
    );
    out.response
}

/// The one numeric field: a DragValue wearing the same field chrome
/// as [`text_field`] instead of the stock outlined box. Generic over
/// egui's numeric types (the settings values are u32/u64/usize, the
/// feature fields f64); `speed` is the drag step.
pub(crate) fn number_field<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    t: Tokens,
    value: &mut T,
    range: std::ops::RangeInclusive<T>,
    width: f32,
    speed: f64,
) -> egui::Response {
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.widgets.inactive.bg_fill = field_fill(t);
        v.widgets.inactive.corner_radius = crate::theme::RADIUS_MICRO.into();
        v.widgets.hovered.bg_fill = rgb(core::theme::hover_step(t.bg_panel_alt));
        v.widgets.hovered.corner_radius = crate::theme::RADIUS_MICRO.into();
        v.widgets.active.bg_fill = rgb(core::theme::pressed_step(t.bg_panel_alt));
        v.widgets.active.corner_radius = crate::theme::RADIUS_MICRO.into();
        ui.add_sized([width, 30.0], egui::DragValue::new(value).range(range).speed(speed))
    })
    .inner
}

/// Label column width shared by every form row, so the labels of a
/// whole form line up in one column instead of hugging each control.
pub(crate) const FORM_LABEL_W: f32 = 128.0;

/// Default control width for form rows: selects and text fields in
/// a settings form share it, so a form reads as one kit.
pub(crate) const FORM_CONTROL_W: f32 = 300.0;

/// One form row: a fixed label column (the label in Medium 12, an
/// optional hint under it in 12 faint), then the control. Returns
/// the control closure's return value, so callers keep using the
/// control's Response (changed, clicked, lost focus) as before,
/// together with the row's rect (the Settings search highlight
/// paints over it).
pub(crate) fn form_row<R>(
    ui: &mut egui::Ui,
    t: Tokens,
    label: &str,
    hint: Option<&str>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> (R, egui::Rect) {
    let resp = ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(FORM_LABEL_W, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.label(RichText::new(label).font(crate::theme::medium(12.0)));
                if let Some(hint) = hint {
                    ui.label(RichText::new(hint).size(12.0).color(rgb(t.text_faint)));
                }
            },
        );
        add(ui)
    });
    (resp.inner, resp.response.rect)
}

/// Stacked variant of [`form_row`] for long controls: the label
/// (and optional hint) above, the control below at the full
/// available width. Returns the control closure's return value
/// and the field's rect, like [`form_row`].
pub(crate) fn form_field<R>(
    ui: &mut egui::Ui,
    t: Tokens,
    label: &str,
    hint: Option<&str>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> (R, egui::Rect) {
    let resp = ui.vertical(|ui| {
        ui.label(RichText::new(label).font(crate::theme::medium(12.0)));
        if let Some(hint) = hint {
            ui.label(RichText::new(hint).size(12.0).color(rgb(t.text_faint)));
        }
        add(ui)
    });
    (resp.inner, resp.response.rect)
}

/// A selectable radio row with an optional theme preview
/// swatch: a tiny rounded window (the theme's app surface with
/// its panel strip), drawn as a rectangle so it can never be
/// misread as a second radio control. Returns true when
/// clicked. Moved here from settings.rs so every screen picks
/// options the same way.
///
/// Fill language: a resting row paints NO fill. Hover paints
/// the panel-alt wash, a press paints the pressed step, and
/// selection is carried by the radio ring and its filled
/// center, never by a row fill, so a filled row always means
/// the pointer is on it.
pub(crate) fn radio_row(
    ui: &mut egui::Ui,
    t: Tokens,
    selected: bool,
    swatch: Option<(Color32, Color32)>,
    label: &str,
    hint: &str,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::click());
    // The row wash runs through the shared hover and press
    // factors, crossfading on HOVER and PRESS (instant under
    // Motion Off) instead of snapping between hard fills.
    let hov = crate::anim::hover_factor(ui, ("radio_row", label), response.hovered());
    let press =
        crate::anim::press_factor(ui, ("radio_row", label), response.is_pointer_button_down_on());
    if press > 0.001 {
        let pressed = core::theme::pressed_step(t.bg_panel_alt);
        let fill = core::theme::mix(t.bg_panel_alt, pressed, press);
        ui.painter().rect_filled(rect, crate::theme::RADIUS_MICRO, rgb(fill));
    } else if hov > 0.001 {
        let fill = core::theme::mix(t.bg_panel, t.bg_panel_alt, hov);
        ui.painter().rect_filled(rect, crate::theme::RADIUS_MICRO, rgb(fill));
    }
    let cy = rect.center().y;
    let ring_c = Pos2::new(rect.left() + 14.0, cy);
    ui.painter().circle_stroke(
        ring_c,
        8.0,
        Stroke::new(1.6_f32, if selected { rgb(t.accent) } else { rgb(t.text_faint) }),
    );
    if selected {
        ui.painter().circle_filled(ring_c, 4.5, rgb(t.accent));
    }
    let mut x = rect.left() + 34.0;
    if let Some((surface, panel)) = swatch {
        let sw = egui::Rect::from_min_size(Pos2::new(x, cy - 8.0), Vec2::new(24.0, 16.0));
        ui.painter().rect_filled(sw, CornerRadius::same(4), surface);
        let strip = egui::Rect::from_min_max(
            Pos2::new(sw.left() + 1.0, sw.top() + 1.0),
            Pos2::new(sw.left() + 7.0, sw.bottom() - 1.0),
        );
        ui.painter().rect_filled(strip, CornerRadius::same(2), panel);
        ui.painter().rect_stroke(
            sw,
            CornerRadius::same(4),
            Stroke::new(1.0_f32, rgb(t.border)),
            egui::StrokeKind::Inside,
        );
        x += 32.0;
    }
    ui.painter().text(
        Pos2::new(x, cy),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(15.0),
        rgb(t.text_primary),
    );
    if !hint.is_empty() {
        let w = ui
            .ctx()
            .fonts_mut(|f| {
                f.layout_no_wrap(label.to_owned(), FontId::proportional(15.0), rgb(t.text_primary))
            })
            .size()
            .x;
        ui.painter().text(
            Pos2::new(x + w + 10.0, cy),
            egui::Align2::LEFT_CENTER,
            hint,
            FontId::proportional(12.0),
            rgb(t.text_faint),
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A Material 3 style switch: when off, a quiet surface track with
/// an outline and a small grey thumb; when on, an accent track with
/// a larger light thumb. The thumb slides and grows on the motion
/// system's SWITCH duration instead of snapping, and the
/// track steps on hover and press like every other control.
/// Toggles `on` in place.
pub(crate) fn switch(ui: &mut egui::Ui, on: &mut bool, t: Tokens) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), Sense::click());
    // The thumb's slide runs through the shared factor, so it
    // takes the SWITCH duration as scaled by the Motion setting
    // (and snaps under Motion Off) instead of running on a raw
    // fixed-length animation the setting could not reach.
    let k = crate::anim::switch_factor(ui, ("switch_thumb", response.id), *on);
    let off_fill = core::theme::mix(t.bg_panel_alt, t.bg_app, 0.4);
    let base = if *on { t.accent } else { off_fill };
    // The track's hover and pressed steps run through the shared
    // factors, so the wash crossfades on HOVER and PRESS (and
    // snaps under Motion Off) instead of jumping between steps.
    let hov = crate::anim::hover_factor(ui, ("switch", response.id), response.hovered());
    let press = crate::anim::press_factor(
        ui,
        ("switch", response.id),
        response.is_pointer_button_down_on(),
    );
    let track = core::theme::mix(
        core::theme::mix(base, core::theme::hover_step(base), hov),
        core::theme::pressed_step(base),
        press,
    );
    ui.painter().rect_filled(rect, CornerRadius::same(11), rgb(track));
    if !*on {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(11),
            Stroke::new(1.5_f32, rgb(t.text_faint)),
            egui::StrokeKind::Inside,
        );
    }
    let knob_r = 6.0 + 2.0 * k;
    let knob_color = if *on { rgb(t.on_accent()) } else { rgb(t.text_secondary) };
    let knob_x = rect.left() + 11.0 + (rect.width() - 22.0) * k;
    ui.painter().circle_filled(Pos2::new(knob_x, rect.center().y), knob_r, knob_color);
    if response.clicked() {
        *on = !*on;
    }
}

/// The one search pill for list panels (chat list, starred,
/// calls): a glass pill with the search glyph, a frameless 15 pt
/// editor, and the shared accent focus ring. Each panel passes
/// its own stable [`egui::Id`] so the panels keep separate
/// editor state. Returns the editor's response.
pub(crate) fn search_pill(
    ui: &mut egui::Ui,
    t: Tokens,
    id: egui::Id,
    hint: &str,
    value: &mut String,
) -> egui::Response {
    let search_width = (ui.available_width() - 46.0).max(80.0);
    let search_frame = egui::Frame::new()
        .fill(crate::theme::glass_tint(t, t.bg_panel_alt))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(SP_HALF);
                {
                    let (irect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                    crate::icons::draw(
                        ui.painter(),
                        crate::icons::Icon::Search,
                        irect.center(),
                        crate::icons::SIZE_MENU,
                        rgb(t.text_faint),
                        crate::theme::glass_tint(t, t.bg_panel_alt),
                    );
                }
                let search = egui::TextEdit::singleline(value)
                    .id(id)
                    .hint_text(hint)
                    .font(FontId::proportional(15.0))
                    .desired_width(search_width)
                    .frame(egui::Frame::NONE);
                ui.add(search)
            })
            .inner
        });
    if search_frame.inner.has_focus() {
        ui.painter().rect_stroke(
            search_frame.response.rect,
            CornerRadius::same(20),
            Stroke::new(1.5_f32, rgb(t.accent)),
            egui::StrokeKind::Inside,
        );
    }
    search_frame.inner
}
