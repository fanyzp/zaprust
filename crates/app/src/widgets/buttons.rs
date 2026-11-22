//! Buttons and button-like controls: the app button, rail buttons,
//! icon buttons, chips and the segmented control.

use crate::theme::rgb;
use core::theme::{Rgb, Tokens};
use eframe::egui::{self, Color32, CornerRadius, FontId, Pos2, Sense, Stroke, Vec2};

/// Button variants for the one app button component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Btn {
    /// Accent fill, used for the primary action of a surface.
    Primary,
    /// Outline/ghost, for secondary actions.
    Secondary,
    /// Danger fill, for destructive actions.
    Danger,
}

/// The app's button: token-driven fill/stroke, full pill shape
/// (Material 3 button geometry), Inter Medium, consistent height.
/// All screens use this instead of raw `egui::Button` so buttons
/// never render as unstyled defaults.
pub(crate) fn button(ui: &mut egui::Ui, t: Tokens, label: &str, kind: Btn) -> egui::Response {
    button_impl(ui, t, label, kind, true)
}

/// A button that can be disabled (with the reason as hover text
/// where the caller adds it).
/// The shared button with an enabled flag (disabled renders
/// desaturated but legible). Used by states that must show why an
/// action is unavailable.
pub(crate) fn button_enabled(
    ui: &mut egui::Ui,
    t: Tokens,
    label: &str,
    kind: Btn,
    enabled: bool,
) -> egui::Response {
    button_impl(ui, t, label, kind, enabled)
}

fn button_impl(
    ui: &mut egui::Ui,
    t: Tokens,
    label: &str,
    kind: Btn,
    enabled: bool,
) -> egui::Response {
    // State fills come from the theme derivations in core
    // (hover_step / pressed_step): hover is one value step from
    // the resting fill, pressed one step the other way, on every
    // theme. Text on filled variants is the computed on-color
    // (WCAG-checked in core's theme tests).
    let (fill, hovered, active, stroke, text) = match kind {
        Btn::Primary => (
            t.accent,
            core::theme::hover_step(t.accent),
            core::theme::pressed_step(t.accent),
            Stroke::NONE,
            t.on_accent(),
        ),
        Btn::Secondary => (
            t.bg_panel_alt,
            core::theme::hover_step(t.bg_panel_alt),
            core::theme::pressed_step(t.bg_panel_alt),
            Stroke::new(1.0_f32, rgb(t.border)),
            t.text_primary,
        ),
        Btn::Danger => (
            t.danger,
            core::theme::hover_step(t.danger),
            core::theme::pressed_step(t.danger),
            Stroke::NONE,
            t.on_danger(),
        ),
    };
    let (fill, text) = if enabled {
        (fill, text)
    } else {
        // Disabled: fill mixed toward the panel so the control
        // recedes; the label stays legible in the secondary role.
        (core::theme::mix(fill, t.bg_panel, 0.55), t.text_secondary)
    };
    // Hand-painted like the chips and menu rows, so hover and
    // press crossfade through the animation helpers instead of
    // snapping, and a press dips the pill to 0.97 scale around
    // its center, the tactile dip native buttons have. Geometry
    // on the spacing scale: 40 px tall (the old 36 read cramped
    // next to other chat apps), 14 px of horizontal padding.
    let kind_key = match kind {
        Btn::Primary => "primary",
        Btn::Secondary => "secondary",
        Btn::Danger => "danger",
    };
    let font = crate::theme::medium(13.0);
    let text_w = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE).size().x);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(text_w + 28.0, 40.0),
        if enabled { Sense::click() } else { Sense::hover() },
    );
    let hov = if enabled {
        crate::anim::hover_factor(ui, ("button", label, kind_key), response.hovered())
    } else {
        0.0
    };
    let press = if enabled {
        crate::anim::press_factor(
            ui,
            ("button", label, kind_key),
            response.is_pointer_button_down_on(),
        )
    } else {
        0.0
    };
    let cur = core::theme::mix(core::theme::mix(fill, hovered, hov), active, press);
    let scale = 1.0 - 0.03 * press;
    let prect = egui::Rect::from_center_size(rect.center(), rect.size() * scale);
    let radius = CornerRadius::same((prect.height() / 2.0).round() as u8);
    let painter = ui.painter();
    painter.rect_filled(prect, radius, rgb(cur));
    if stroke != Stroke::NONE {
        painter.rect_stroke(prect, radius, stroke, egui::StrokeKind::Inside);
    }
    painter.text(
        prect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        crate::theme::medium(13.0 * scale),
        rgb(text),
    );
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// A navigation rail button: icon in a 46 px square with the
/// selected wash, plus the one badge language every surface
/// shares: a numeric count pill at the top-right anchor when
/// `badge_count > 0`, and no marker at all when there is no
/// count to show.
pub(crate) fn rail_button(
    ui: &mut egui::Ui,
    icon: crate::icons::Icon,
    label: &str,
    selected: bool,
    badge_count: u32,
    t: Tokens,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(46.0, 46.0), Sense::click());
    let accent = rgb(t.accent);
    if selected {
        // The glass tint wash as the selected pill: the panel-alt
        // base leaned toward the accent, translucent over the
        // rail. The icon itself carries the accent, so the state
        // stays unmistakable.
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(14),
            crate::theme::glass_wash(t, t.bg_panel_alt, 150),
        );
    } else if response.is_pointer_button_down_on() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(14),
            rgb(core::theme::pressed_step(t.bg_panel_alt)),
        );
    } else if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(14), rgb(t.bg_panel_alt));
    }
    let color = if selected { accent } else { rgb(t.text_secondary) };
    crate::icons::draw(ui.painter(), icon, rect.center(), 24.0, color, rgb(t.bg_panel));
    if badge_count > 0 {
        // A count pill at the button's top-right, in the chat
        // row unread badge's language (badge fill, 11 px text,
        // 19 px tall, at least 19 px wide), scaled to sit on the
        // 46 px rail button.
        let label = badge_count.to_string();
        let font = FontId::proportional(11.0);
        let text_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(label.clone(), font.clone(), rgb(t.badge_text)).size().x
        });
        let w = (text_w + 10.0).max(19.0);
        let h = 19.0_f32;
        let center = rect.right_top() + Vec2::new(-7.0, 7.0);
        let brect = egui::Rect::from_center_size(center, Vec2::new(w, h));
        ui.painter().rect_filled(brect, CornerRadius::same(9), rgb(t.badge_bg));
        ui.painter().text(
            brect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            font,
            rgb(t.badge_text),
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label).clicked()
}

pub(crate) fn chip(ui: &mut egui::Ui, label: &str, selected: bool, t: Tokens) -> egui::Response {
    // Hand-painted (like the shared menu rows) so hover and press
    // run through the animation helpers and crossfade smoothly
    // instead of snapping between widget states. Geometry is the
    // label plus its own padding: the global button padding made
    // the filter row wide enough to wrap a label mid-word.
    // State endpoints are unchanged: selected chips step their
    // accent alpha 54 (rest), 84 (hover), 110 (pressed); unselected
    // chips use the shared hover and pressed steps.
    let font = FontId::proportional(12.5);
    let text_size = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE).size());
    // Padding on the spacing scale, (10, 5): the old (8, 3) made
    // the filter chips the shortest controls in the app.
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(text_size.x + 20.0, text_size.y + 10.0), Sense::click());
    let hov = crate::anim::hover_factor(ui, ("chip", label, selected), response.hovered());
    let press = crate::anim::press_factor(
        ui,
        ("chip", label, selected),
        response.is_pointer_button_down_on(),
    );
    let fill = if selected {
        let a = 54.0 + 30.0 * hov + 26.0 * press;
        Color32::from_rgba_unmultiplied(t.accent.0, t.accent.1, t.accent.2, a as u8)
    } else {
        let base = core::theme::mix(t.bg_panel_alt, core::theme::hover_step(t.bg_panel_alt), hov);
        rgb(core::theme::mix(base, core::theme::pressed_step(t.bg_panel_alt), press))
    };
    // True pill geometry, derived from the measured height the
    // same way the shared button derives it, instead of a fixed
    // radius that drifts from the height when font metrics move.
    let radius = CornerRadius::same((rect.height() / 2.0).round() as u8);
    ui.painter().rect_filled(rect, radius, fill);
    let text = if selected { t.accent } else { t.text_secondary };
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, label, font, rgb(text));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Paints the edge fade of a horizontally scrolling strip: a
/// gradient from transparent to the surface colour `bg` at each
/// edge where content is clipped, so a strip that continues past
/// the panel edge says so instead of cutting a chip mid-label.
/// Flat colour only at full opacity; the fade itself is a
/// two-stop alpha ramp, the one sanctioned gradient in the app
/// (it is a clipping cue, not a surface fill).
pub(crate) fn strip_edge_fades(
    painter: &egui::Painter,
    rect: egui::Rect,
    bg: Rgb,
    fade_left: bool,
    fade_right: bool,
) {
    const W: f32 = 28.0;
    let base = rgb(bg);
    let solid = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 255);
    let clear = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 0);
    let mut mesh = egui::Mesh::default();
    let mut quad = |x0: f32, x1: f32, c0: Color32, c1: Color32| {
        let base_idx = mesh.vertices.len() as u32;
        mesh.colored_vertex(egui::Pos2::new(x0, rect.top()), c0);
        mesh.colored_vertex(egui::Pos2::new(x1, rect.top()), c1);
        mesh.colored_vertex(egui::Pos2::new(x0, rect.bottom()), c0);
        mesh.colored_vertex(egui::Pos2::new(x1, rect.bottom()), c1);
        mesh.indices.extend([
            base_idx,
            base_idx + 1,
            base_idx + 2,
            base_idx + 2,
            base_idx + 1,
            base_idx + 3,
        ]);
    };
    if fade_left {
        quad(rect.left(), rect.left() + W, solid, clear);
    }
    if fade_right {
        quad(rect.right() - W, rect.right(), clear, solid);
    }
    if !mesh.is_empty() {
        painter.add(egui::Shape::mesh(mesh));
    }
}

/// The one horizontally scrolling chip strip: the scrollbar is
/// always hidden, the wheel scrolls sideways, and the clipped
/// edge(s) fade into the surface behind the strip (`bg`). The
/// closure lays the chips out in a horizontal row and returns
/// the selected chip's response; whenever `selected_key` changes
/// (a screen is entered, the selection moves), the strip scrolls
/// until the selected chip is fully inside the viewport, so the
/// active chip is never the one cut off at the edge.
pub(crate) fn chip_scroll_row(
    ui: &mut egui::Ui,
    id_salt: &'static str,
    bg: Rgb,
    selected_key: impl std::hash::Hash,
    add_contents: impl FnOnce(&mut egui::Ui) -> Option<egui::Response>,
) {
    let key_id = ui.scope_id().with(("chip_strip_ensured", id_salt));
    let key_hash = egui::util::hash(selected_key);
    let ensured = ui.data(|d| d.get_temp::<u64>(key_id)) == Some(key_hash);
    let out = egui::ScrollArea::horizontal()
        .id_salt(id_salt)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .scroll_source(egui::scroll_area::ScrollSource::ALL)
        .show(ui, |ui| {
            let selected = ui.horizontal(|ui| add_contents(ui)).inner;
            if !ensured {
                if let Some(resp) = &selected {
                    let clip = ui.clip_rect();
                    if resp.rect.left() >= clip.left() - 0.5
                        && resp.rect.right() <= clip.right() + 0.5
                    {
                        ui.data_mut(|d| d.insert_temp(key_id, key_hash));
                    } else {
                        resp.scroll_to_me(None);
                    }
                } else {
                    ui.data_mut(|d| d.insert_temp(key_id, key_hash));
                }
            }
            selected
        });
    let offset = out.state.clamped_offset().x;
    let max_offset = (out.content_size.x - out.inner_rect.width()).max(0.0);
    strip_edge_fades(ui.painter(), out.inner_rect, bg, offset > 0.5, offset < max_offset - 0.5);
}

/// A two-option segmented control: a full-width pill track on
/// the panel-alt rung with the selected option riding the
/// elevated step inside it, the shape the Calls panel's All /
/// Missed filter takes. Hand-painted like the chip so the fills
/// come from the surface ladder. Returns the index of the
/// option a click chose, or `None` when the click landed on
/// the already-selected option or missed the control.
pub(crate) fn segmented(
    ui: &mut egui::Ui,
    labels: [&str; 2],
    selected: usize,
    t: Tokens,
) -> Option<usize> {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click());
    let radius = CornerRadius::same((rect.height() / 2.0).round() as u8);
    ui.painter().rect_filled(rect, radius, rgb(t.bg_panel_alt));
    let half = rect.width() / 2.0;
    let seg_rect = |i: usize| {
        egui::Rect::from_min_size(
            Pos2::new(rect.left() + half * i as f32, rect.top()),
            Vec2::new(half, rect.height()),
        )
        .shrink(3.0)
    };
    // Hover wash on the unselected segment under the pointer.
    if let Some(pos) = response.hover_pos() {
        let hovered = if pos.x < rect.center().x { 0 } else { 1 };
        if hovered != selected {
            ui.painter().rect_filled(
                seg_rect(hovered),
                radius,
                rgb(core::theme::hover_step(t.bg_panel_alt)),
            );
        }
    }
    // The selected segment rides two steps up: one step read
    // as a hover, not a selection, at a glance.
    ui.painter().rect_filled(
        seg_rect(selected),
        radius,
        rgb(core::theme::hover_step(core::theme::elevated_step(t.bg_panel_alt))),
    );
    let font = FontId::proportional(13.0);
    for (i, label) in labels.iter().enumerate() {
        let color = if i == selected { t.text_primary } else { t.text_secondary };
        ui.painter().text(
            seg_rect(i).center(),
            egui::Align2::CENTER_CENTER,
            *label,
            font.clone(),
            rgb(color),
        );
    }
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let idx = if pos.x < rect.center().x { 0 } else { 1 };
            if idx != selected {
                return Some(idx);
            }
        }
    }
    None
}

/// The one icon button: a square hit area with a circular hover
/// wash, a pressed step, the pointing hand and a tooltip. Chat list
/// headers, the composer, conversation headers and the connect
/// screen each hand-rolled this before; they all call this now.
pub(crate) fn icon_button(
    ui: &mut egui::Ui,
    t: Tokens,
    icon: crate::icons::Icon,
    size: f32,
    tooltip: &str,
    fg: Option<Rgb>,
) -> egui::Response {
    icon_button_on(
        ui,
        t,
        icon,
        size,
        tooltip,
        fg,
        t.bg_panel_alt,
        core::theme::pressed_step(t.bg_panel_alt),
    )
}

/// [`icon_button`] with the hover and pressed fills named by
/// the caller: for buttons resting on a surface other than the
/// panel (an elevated card, a glass layer), where the panel
/// steps would be invisible against the resting fill.
#[allow(clippy::too_many_arguments)]
pub(crate) fn icon_button_on(
    ui: &mut egui::Ui,
    t: Tokens,
    icon: crate::icons::Icon,
    size: f32,
    tooltip: &str,
    fg: Option<Rgb>,
    hover: Rgb,
    pressed: Rgb,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if resp.is_pointer_button_down_on() {
        ui.painter().circle_filled(rect.center(), size / 2.0, rgb(pressed));
    } else if resp.hovered() {
        ui.painter().circle_filled(rect.center(), size / 2.0, rgb(hover));
    }
    let color = match fg {
        Some(c) => rgb(c),
        None if resp.hovered() => rgb(t.text_primary),
        None => rgb(t.text_secondary),
    };
    crate::icons::draw(ui.painter(), icon, rect.center(), size * 0.58, color, rgb(t.bg_panel));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tooltip)
}
