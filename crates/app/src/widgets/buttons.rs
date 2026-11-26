//! Buttons and button-like controls: the app button, rail buttons,
//! icon buttons and chips.

use crate::theme::rgb;
use core::theme::{Rgb, Tokens};
use eframe::egui::{self, Color32, CornerRadius, FontId, Pos2, Sense, Stroke, Vec2};

/// Button variants for the one app button component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Btn {
    /// Accent fill, used for the primary action of a surface.
    Primary,
    /// Quiet filled surface, for secondary actions. Never an
    /// outline or ghost style (design standard, button language).
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
    let (fill, hovered, active, stroke, text, focus_ring) = match kind {
        Btn::Primary => (
            t.accent,
            core::theme::hover_step(t.accent),
            core::theme::pressed_step(t.accent),
            Stroke::NONE,
            t.on_accent(),
            t.on_accent(),
        ),
        Btn::Secondary => (
            t.bg_panel_alt,
            core::theme::hover_step(t.bg_panel_alt),
            core::theme::pressed_step(t.bg_panel_alt),
            Stroke::NONE,
            t.text_primary,
            t.accent,
        ),
        Btn::Danger => (
            t.danger,
            core::theme::hover_step(t.danger),
            core::theme::pressed_step(t.danger),
            Stroke::NONE,
            t.on_danger(),
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
    let font = crate::theme::medium(12.0);
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
    let scale = 1.0 - (1.0 - crate::motion::PRESS_SCALE) * press;
    let prect = egui::Rect::from_center_size(rect.center(), rect.size() * scale);
    let radius = CornerRadius::same((prect.height() / 2.0).round() as u8);
    let painter = ui.painter();
    painter.rect_filled(prect, radius, rgb(cur));
    if stroke != Stroke::NONE {
        painter.rect_stroke(prect, radius, stroke, egui::StrokeKind::Inside);
    }
    if enabled && response.has_focus() {
        // The keyboard focus ring, in the same idiom as the text
        // fields (1.5 px, inside the rect): the accent on the
        // surface-toned kind, the on-fill text colour on the
        // filled kinds, where an accent ring would vanish into
        // the fill it outlines.
        painter.rect_stroke(
            prect,
            radius,
            Stroke::new(1.5_f32, rgb(focus_ring)),
            egui::StrokeKind::Inside,
        );
    }
    painter.text(
        prect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        crate::theme::medium(12.0 * scale),
        rgb(text),
    );
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// The pill family's two sanctioned sizes (qc1636 Step 3):
/// Badge for counts and Chip for interactive or label chips.
/// Every pill in the app is one of these two; radius is
/// height/2 everywhere in the family.
pub(crate) const BADGE_HEIGHT: f32 = 19.0;
/// Badge text size.
pub(crate) const BADGE_FONT: f32 = 10.0;
/// Badge horizontal padding, per side.
pub(crate) const BADGE_PAD_X: f32 = 5.0;
/// Chip height: the 12 px text plus its 5 px vertical padding
/// per side, rounded to the family's step.
pub(crate) const CHIP_HEIGHT: f32 = 24.0;
/// Chip text size.
pub(crate) const CHIP_FONT: f32 = 12.0;
/// Chip horizontal padding, per side.
pub(crate) const CHIP_PAD_X: f32 = 10.0;

/// The measured size of one count badge at `scale` (the chat
/// row's pop animation scales the badge; every other caller
/// passes 1.0): the text at the badge font, the badge padding
/// per side, and at least as wide as it is tall, so a one-digit
/// count paints as a circle.
pub(crate) fn badge_size(ui: &egui::Ui, label: &str, scale: f32) -> Vec2 {
    let font = FontId::proportional(BADGE_FONT * scale);
    let text_w =
        ui.ctx().fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font, Color32::WHITE).size().x);
    Vec2::new((text_w + 2.0 * BADGE_PAD_X).max(BADGE_HEIGHT) * scale, BADGE_HEIGHT * scale)
}

/// Paints one count badge centred at `center`, in the badge
/// token pair (the muted variant for muted chats). This is
/// the one badge component: the chat row's unread badge, the
/// rail count pill and the portrait tab pill all paint
/// through here, so the family's geometry cannot drift
/// between surfaces again.
pub(crate) fn paint_badge(
    painter: &egui::Painter,
    center: Pos2,
    label: &str,
    size: Vec2,
    t: Tokens,
    muted: bool,
    scale: f32,
) {
    let (fill, text) =
        if muted { crate::theme::muted_badge(t) } else { (rgb(t.badge_bg), rgb(t.badge_text)) };
    let brect = egui::Rect::from_center_size(center, size);
    painter.rect_filled(brect, CornerRadius::same((size.y / 2.0).round() as u8), fill);
    painter.text(
        brect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        FontId::proportional(BADGE_FONT * scale),
        text,
    );
}

/// Paints the mention badge: the badge family's circle form,
/// one badge diameter across with the "@" at the badge font,
/// in the same token pair as [`paint_badge`].
pub(crate) fn paint_mention_badge(painter: &egui::Painter, center: Pos2, t: Tokens) {
    painter.circle_filled(center, BADGE_HEIGHT / 2.0, rgb(t.badge_bg));
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        "@",
        FontId::proportional(BADGE_FONT),
        rgb(t.badge_text),
    );
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
            CornerRadius::same(crate::theme::RADIUS_SMALL),
            crate::theme::glass_wash(t, t.bg_panel_alt, 150),
        );
    } else {
        // Unselected hover and pressed washes run through the
        // shared factors, crossfading on HOVER and PRESS like
        // every other row and button instead of snapping.
        let hov = crate::anim::hover_factor(ui, ("rail_button", label), response.hovered());
        let press = crate::anim::press_factor(
            ui,
            ("rail_button", label),
            response.is_pointer_button_down_on(),
        );
        if press > 0.001 {
            let pressed = core::theme::pressed_step(t.bg_panel_alt);
            let fill = core::theme::mix(t.bg_panel_alt, pressed, press);
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(crate::theme::RADIUS_SMALL),
                rgb(fill),
            );
        } else if hov > 0.001 {
            let fill = core::theme::mix(t.bg_panel, t.bg_panel_alt, hov);
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(crate::theme::RADIUS_SMALL),
                rgb(fill),
            );
        }
    }
    let color = if selected { accent } else { rgb(t.text_secondary) };
    crate::icons::draw(
        ui.painter(),
        icon,
        rect.center(),
        crate::icons::SIZE_TAB,
        color,
        rgb(t.bg_panel),
    );
    if badge_count > 0 {
        // A count badge at the button's top-right, painted by
        // the shared badge component, anchored to sit on the
        // 46 px rail button.
        let label = core::filter::format_count(u64::from(badge_count));
        let size = badge_size(ui, &label, 1.0);
        let center = rect.right_top() + Vec2::new(-7.0, 7.0);
        paint_badge(ui.painter(), center, &label, size, t, false, 1.0);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label).clicked()
}

pub(crate) fn chip(ui: &mut egui::Ui, label: &str, selected: bool, t: Tokens) -> egui::Response {
    // Hand-painted (like the shared menu rows) so hover and press
    // run through the animation helpers and crossfade smoothly
    // instead of snapping between widget states. Geometry is the
    // pill family's Chip size: the label plus the chip padding,
    // at the fixed chip height. The global button padding made
    // the filter row wide enough to wrap a label mid-word.
    // State fills are solid, in the button component's step
    // discipline: a selected chip rests on the solid accent
    // with the pinned on-accent text, hover is one step from
    // the resting fill and pressed one step the other way;
    // unselected chips step their panel-alt fill the same way.
    // (The selected chip used to rest on a translucent accent
    // wash, the pill family's only translucent resting fill.)
    let font = FontId::proportional(CHIP_FONT);
    let text_size = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE).size());
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(text_size.x + 2.0 * CHIP_PAD_X, CHIP_HEIGHT),
        Sense::click(),
    );
    let hov = crate::anim::hover_factor(ui, ("chip", label, selected), response.hovered());
    let press = crate::anim::press_factor(
        ui,
        ("chip", label, selected),
        response.is_pointer_button_down_on(),
    );
    let fill = if selected {
        let base = core::theme::mix(t.accent, core::theme::hover_step(t.accent), hov);
        rgb(core::theme::mix(base, core::theme::pressed_step(t.accent), press))
    } else {
        let base = core::theme::mix(t.bg_panel_alt, core::theme::hover_step(t.bg_panel_alt), hov);
        rgb(core::theme::mix(base, core::theme::pressed_step(t.bg_panel_alt), press))
    };
    // True pill geometry: the radius derives from the chip
    // height, like every pill in the family.
    let radius = CornerRadius::same((rect.height() / 2.0).round() as u8);
    ui.painter().rect_filled(rect, radius, fill);
    if response.has_focus() {
        // The keyboard focus ring, the text fields' idiom
        // (1.5 px, inside the rect): the accent on an unselected
        // chip, the on-accent color on a selected one, where an
        // accent ring would vanish into the fill it outlines
        // (the Primary button's rule).
        let ring = if selected { t.on_accent() } else { t.accent };
        ui.painter().rect_stroke(
            rect,
            radius,
            Stroke::new(1.5_f32, rgb(ring)),
            egui::StrokeKind::Inside,
        );
    }
    let text = if selected { t.on_accent() } else { t.text_secondary };
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

/// The one chip row: chips laid out left to right at their
/// measured sizes, flowing onto a second row when the row is
/// full, so every chip is fully visible at any panel width.
/// A chip is never squeezed (each allocates its own measured
/// size), so a label cannot break mid-word: that was the
/// failure of the first wrapped row, whose chips were built
/// from the global button padding and shrank with the layout.
/// The scrolling strip that replaced it kept the last chip
/// clipped in the strip's resting state, with a fade and a
/// chevron disc advertising the continuation; two independent
/// audits flagged that resting clip (the v1.6.15 audit's D6,
/// the v1.6.23 verification), so v1.6.24 retires the scroll
/// machinery, the fades and the chevrons for chip rows:
/// flowing removes the clip by construction, and the selected
/// chip needs no scrolling into view because it is never out
/// of view. (The calls shortcut strip still scrolls and still
/// uses `strip_edge_fades`; it is a row of contact circles,
/// not chips, and its content is unbounded by design.)
pub(crate) fn chip_flow_row(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    ui.scope(|ui| {
        // The wrapped rows take the same gap as the chips in
        // a row, so the flow reads as one grid.
        ui.spacing_mut().item_spacing.y = ui.spacing().item_spacing.x;
        ui.horizontal_wrapped(add_contents);
    });
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
    // The wash crossfades through the shared hover and press
    // factors (HOVER and PRESS durations, Motion Off snaps), so
    // icon buttons ease like the rows instead of snapping
    // between hard fills. The resting surface under the button
    // is the caller's hover fill stepped back toward the panel;
    // mixing from the panel keeps the wash's endpoints exact.
    let hov = crate::anim::hover_factor(ui, ("icon_button", tooltip), resp.hovered());
    let press =
        crate::anim::press_factor(ui, ("icon_button", tooltip), resp.is_pointer_button_down_on());
    if press > 0.001 {
        let base = core::theme::mix(t.bg_panel, hover, hov);
        let fill = core::theme::mix(base, pressed, press);
        ui.painter().circle_filled(rect.center(), size / 2.0, rgb(fill));
    } else if hov > 0.001 {
        let fill = core::theme::mix(t.bg_panel, hover, hov);
        ui.painter().circle_filled(rect.center(), size / 2.0, rgb(fill));
    }
    let color = match fg {
        Some(c) => rgb(c),
        None if resp.hovered() => rgb(t.text_primary),
        None => rgb(t.text_secondary),
    };
    if resp.has_focus() {
        // The keyboard focus ring: the text fields' 1.5 px accent
        // ring, drawn as a circle just inside the wash disc so it
        // follows this button's own shape.
        ui.painter().circle_stroke(
            rect.center(),
            size / 2.0 - 0.75,
            Stroke::new(1.5_f32, rgb(t.accent)),
        );
    }
    crate::icons::draw(
        ui.painter(),
        icon,
        rect.center(),
        crate::icons::snap_size(size * 0.58),
        color,
        rgb(t.bg_panel),
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tooltip)
}
