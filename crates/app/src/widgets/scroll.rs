//! Scroll-activity scrollbar boost (qc1636 Step 6).
//!
//! The global scroll style (`theme::apply_visuals`) rests
//! every bar at a whisper: a 3 px sliver at dormant opacity,
//! firming up only when the pointer hovers the area or the
//! bar. That leaves scrolling itself as the one input that
//! does not raise the bar: wheel, touchpad, drag and keyboard
//! scrolling with the pointer parked elsewhere leaves it
//! dormant. This wrapper closes that gap. It watches each
//! area's own reported state (offset and velocity from
//! `ScrollAreaOutput.state`) and, while the area is scrolling
//! or just was (the linger token in `motion::dur`), scopes a
//! style override around that area's `show` that raises its
//! bars to the interact presence the global style already
//! defines. The override is per area and is restored the
//! moment `show` returns, so one area's activity never boosts
//! another area's bars, and at rest every bar returns to the
//! shared dormant state. The tracker only ever observes the
//! output state; it never consumes or alters scrolling.

use eframe::egui;

/// One area's scroll activity, kept in egui's persisted
/// memory under the area's own id: the last offset the area
/// reported and the frame time it last moved. Comparing the
/// reported offset frame over frame is what makes every
/// scroll input count: wheel, touchpad kinetic, scrollbar
/// drag, keyboard and programmatic scrolls all move it.
#[derive(Clone, Copy)]
struct ScrollActivity {
    last_offset: egui::Vec2,
    last_active: f64,
}

fn activity_id(id: egui::Id) -> egui::Id {
    id.with("scroll_activity")
}

/// Shows `area` in `ui` with the scroll-activity boost scoped
/// to it. `salt` names the area: it becomes the area's egui
/// id salt, so callers pass the salt the area already had, or
/// a fresh stable one. The closure is the area's contents,
/// exactly as with `ScrollArea::show`.
pub(crate) fn show_scroll_area<R>(
    ui: &mut egui::Ui,
    salt: &str,
    area: egui::ScrollArea,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    let id = ui.make_persistent_id(salt);
    show_boosted(ui, id, area.id_salt(salt), |area, ui| area.show(ui, contents))
}

/// The `show_rows` counterpart of [`show_scroll_area`].
pub(crate) fn show_scroll_rows<R>(
    ui: &mut egui::Ui,
    salt: &str,
    area: egui::ScrollArea,
    row_height: f32,
    total_rows: usize,
    contents: impl FnOnce(&mut egui::Ui, std::ops::Range<usize>) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    let id = ui.make_persistent_id(salt);
    show_boosted(ui, id, area.id_salt(salt), |area, ui| {
        area.show_rows(ui, row_height, total_rows, contents)
    })
}

fn show_boosted<R>(
    ui: &mut egui::Ui,
    id: egui::Id,
    area: egui::ScrollArea,
    show: impl FnOnce(egui::ScrollArea, &mut egui::Ui) -> egui::scroll_area::ScrollAreaOutput<R>,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    let ctx = ui.ctx().clone();
    let now = ctx.input(|i| i.time);
    let linger = crate::motion::dur::SCROLL_BOOST_LINGER / 1000.0;
    let previous = ctx.data_mut(|d| d.get_persisted::<ScrollActivity>(activity_id(id)));
    let boosted = previous.is_some_and(|a| now - a.last_active < linger);
    // The boost reuses the global style's own interact
    // presence: an active area's resting bar paints at the
    // opacity and width a dragged bar gets, so the boosted
    // and dragged states can never drift into two dialects.
    let resting = ui.style().spacing.scroll;
    if boosted {
        let scroll = &mut ui.style_mut().spacing.scroll;
        scroll.dormant_handle_opacity = resting.interact_handle_opacity;
        scroll.active_handle_opacity = resting.interact_handle_opacity;
        scroll.floating_width = resting.bar_width;
    }
    let out = show(area, ui);
    if boosted {
        ui.style_mut().spacing.scroll = resting;
    }
    // Record this frame's movement for the next: an offset
    // that moved, or a velocity still carrying the area, is
    // activity. An area's first sight only seeds the record:
    // a restored offset is not a scroll.
    let offset = out.state.clamped_offset();
    let moving = previous.is_some()
        && (offset != previous.map_or(offset, |a| a.last_offset)
            || out.state.velocity().length() > 1.0);
    let last_active =
        if moving { now } else { previous.map_or(f64::NEG_INFINITY, |a| a.last_active) };
    ctx.data_mut(|d| {
        d.insert_persisted(activity_id(id), ScrollActivity { last_offset: offset, last_active });
    });
    if moving || boosted {
        // One wake at the linger's end, so the bars can fall
        // back even if nothing else repaints: the wake lands,
        // finds the linger expired, and asks for nothing more.
        let remaining = (linger - (now - last_active)).max(0.0);
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(remaining));
    }
    out
}
