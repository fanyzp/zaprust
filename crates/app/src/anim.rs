//! Animation helpers for the living UI: small reusable building
//! blocks on top of egui's built-in animation functions
//! (`animate_bool_with_time_and_easing` and
//! `animate_value_with_time`), so surfaces stop hand-rolling
//! one-off tweens.
//!
//! Repaint discipline: egui's animate functions request repaints
//! only while a value is still moving, and every helper here
//! follows the same rule, so an idle app never spins for a dead
//! animation. The one exception is the typing indicator, which
//! repaints on a slow timer for exactly as long as it is mounted
//! (that is, while somebody is actually typing).
//!
//! Usage patterns:
//! - Hover and press washes: [`hover_factor`] / [`press_factor`]
//!   return a 0..1 factor; mix the resting fill toward the hover
//!   or pressed fill with `core::theme::mix` and paint with the
//!   result. Keys must be unique per widget instance, e.g.
//!   `("chat_row", chat.id)`.
//! - New message entrances: bracket the message loop with
//!   [`begin_message_stream`] / [`end_message_stream`] and ask
//!   [`message_entrance`] for each bubble's opacity and rise.
//! - Count badges: [`pop_scale`] gives a badge a small pop on the
//!   frame its number changes, then settles back to 1.0.
//! - Free-form scalars: [`scalar`] eases any f32 toward a target.

use eframe::egui::{self, Color32, Id, Pos2, Sense, Vec2};
use std::collections::HashSet;
use std::time::Duration;

/// Standard durations in seconds, derived from the single
/// millisecond table in `motion::dur` so the two modules can
/// never drift apart: motion owns the numbers, this module only
/// re-expresses them in the seconds the egui animate functions
/// take.
pub(crate) mod dur {
    /// A new message fading and rising into the stream.
    pub(crate) const ENTER: f32 = (crate::motion::dur::ENTER / 1000.0) as f32;
    /// Hover washes on rows and chips.
    pub(crate) const HOVER: f32 = (crate::motion::dur::HOVER / 1000.0) as f32;
    /// Pressed washes (snappier than hover).
    pub(crate) const PRESS: f32 = (crate::motion::dur::PRESS / 1000.0) as f32;
    /// The unread badge pop when its count changes.
    pub(crate) const POP: f32 = (crate::motion::dur::POP / 1000.0) as f32;
}

/// Ease-out cubic shaped for egui's easing slot; the curve itself
/// lives in `motion`, the single source of truth for easing.
fn ease_out(t: f32) -> f32 {
    crate::motion::ease_out_cubic(f64::from(t)) as f32
}

/// Ease-out circ, the house curve for menus and popovers (see
/// `motion::ease_out_circ`).
fn ease_out_circ(t: f32) -> f32 {
    crate::motion::ease_out_circ(f64::from(t)) as f32
}

/// The pop bump curve: 0 at both ends, 1 at the midpoint, so a
/// scale of `1.0 + 0.3 * bump(p)` swells and settles smoothly.
fn bump(p: f32) -> f32 {
    (p.clamp(0.0, 1.0) * std::f32::consts::PI).sin()
}

/// Smooth 0..1 factor for a boolean state (hovered, pressed,
/// selected) using egui's animation manager: it eases over `secs`
/// and egui repaints only while the value is still moving.
fn bool_factor(ui: &egui::Ui, id: Id, on: bool, secs: f32) -> f32 {
    ui.ctx().animate_bool_with_time_and_easing(id, on, secs, ease_out)
}

/// Eased hover factor for the widget identified by `key`:
/// 0 at rest, 1 fully hovered. Mix fills with it.
pub(crate) fn hover_factor(
    ui: &egui::Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    hovered: bool,
) -> f32 {
    bool_factor(ui, Id::unique(("anim_hover", key)), hovered, dur::HOVER)
}

/// Eased pressed factor, the companion to [`hover_factor`] with a
/// shorter duration so presses feel responsive.
pub(crate) fn press_factor(
    ui: &egui::Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    pressed: bool,
) -> f32 {
    bool_factor(ui, Id::unique(("anim_press", key)), pressed, dur::PRESS)
}

/// Eased 0..1 factor for a two-state morph (the composer's
/// microphone becoming the send button is the user): 0 rests at
/// the off state, 1 at the on state, and the run between them
/// takes the popup FADE duration as scaled by the Motion setting,
/// so Reduced shortens the morph and Off snaps it instantly like
/// every other animation. Unlike [`animate_once`] this needs no
/// seeding: a toggle's first frame is its resting state by
/// definition.
pub(crate) fn swap_factor(
    ui: &egui::Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    on: bool,
    motion: crate::motion::MotionSetting,
) -> f32 {
    let secs = (motion.duration(crate::motion::dur::FADE) / 1000.0) as f32;
    if secs <= 0.0 {
        return if on { 1.0 } else { 0.0 };
    }
    ui.ctx().animate_bool_with_time_and_easing(Id::unique(("anim_swap", key)), on, secs, ease_out)
}

/// Eases a scalar toward `target` over `secs`
/// (`animate_value_with_time` under the hood). Good for sizes and
/// offsets that should glide instead of snapping.
pub(crate) fn scalar(
    ui: &egui::Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    target: f32,
    secs: f32,
) -> f32 {
    ui.ctx().animate_value_with_time(Id::unique(("anim_scalar", key)), target, secs)
}

/// Runs a 0..1 animation exactly once for `id`, starting on the
/// first frame the id is seen. egui's animate functions return
/// the target instantly for a brand new id, so the first frame is
/// seeded with `false` through temp data and the run starts on
/// the next frame.
fn animate_once(ui: &egui::Ui, id: Id, secs: f32) -> f32 {
    let started_id = id.with("started");
    let started = ui.data_mut(|d| *d.get_temp_mut_or_default::<bool>(started_id));
    let p = ui.ctx().animate_bool_with_time_and_easing(id, started, secs, ease_out);
    if !started {
        ui.data_mut(|d| d.insert_temp(started_id, true));
    }
    p
}

/// Per-conversation record of which messages have already been
/// rendered, kept in egui temp data. `primed` flips true after
/// the first full frame of a chat: everything on that frame is
/// history and must not animate.
#[derive(Clone, Default)]
struct StreamSeen {
    chat: u64,
    primed: bool,
    seen: HashSet<u64>,
}

fn stream_id() -> Id {
    Id::unique("anim_stream_seen")
}

/// Call once before the message loop of the open conversation.
/// Switching chats resets the record, so the newly opened history
/// is primed silently instead of cascading in.
pub(crate) fn begin_message_stream(ui: &egui::Ui, chat_id: u64) {
    ui.data_mut(|d| {
        let s = d.get_temp_mut_or_default::<StreamSeen>(stream_id());
        if s.chat != chat_id {
            s.chat = chat_id;
            s.primed = false;
            s.seen.clear();
        }
    });
}

/// Call once after the message loop: from the next frame on, any
/// message id not yet seen counts as a fresh arrival.
pub(crate) fn end_message_stream(ui: &egui::Ui, chat_id: u64) {
    ui.data_mut(|d| {
        let s = d.get_temp_mut_or_default::<StreamSeen>(stream_id());
        if s.chat == chat_id {
            s.primed = true;
        }
    });
}

/// The entrance for one bubble: returns `(opacity, rise_px)` to
/// apply while drawing it. A message animates only when it is a
/// genuinely new arrival (its id was not rendered on earlier
/// frames of this chat), sits near the end of the stream (older
/// history synced in later never animates) and `enabled` (the
/// user's Motion setting allows fades). Everything else returns
/// the resting `(1.0, 0.0)`. The animation is a fade from 0.15
/// opacity plus a 10 px upward rise over [`dur::ENTER`].
pub(crate) fn message_entrance(
    ui: &egui::Ui,
    chat_id: u64,
    msg_id: u64,
    near_end: bool,
    enabled: bool,
) -> (f32, f32) {
    let fresh = ui.data_mut(|d| {
        let s = d.get_temp_mut_or_default::<StreamSeen>(stream_id());
        if s.chat != chat_id || !s.primed {
            s.seen.insert(msg_id);
            false
        } else {
            s.seen.insert(msg_id)
        }
    });
    if !fresh || !near_end || !enabled {
        return (1.0, 0.0);
    }
    let p = animate_once(ui, Id::unique(("anim_msg_in", chat_id, msg_id)), dur::ENTER);
    (0.15 + 0.85 * p, (1.0 - p) * 10.0)
}

/// Scale for a count badge: 1.0 at rest, swelling to about 1.3
/// mid-pop. The pop restarts on the frame `value` changes; the
/// first sight of a badge never pops. Time based (the same
/// pattern as `motion::Anim`) because a pop is a pulse, not a
/// transition toward a new resting value. Requests a repaint
/// only while the pop is running.
pub(crate) fn pop_scale(
    ui: &egui::Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    value: u32,
) -> f32 {
    let id = Id::unique(("anim_pop", key));
    let now = ui.input(|i| i.time);
    let start = ui.data_mut(|d| {
        let slot = d.get_temp_mut_or_default::<Option<(u32, f64)>>(id);
        match slot {
            None => {
                *slot = Some((value, f64::NEG_INFINITY));
                None
            }
            Some((v, _)) if *v != value => {
                *slot = Some((value, now));
                Some(now)
            }
            Some((_, t)) => Some(*t),
        }
    });
    let Some(start) = start else { return 1.0 };
    let p = ((now - start) / f64::from(dur::POP)) as f32;
    if p >= 1.0 {
        return 1.0;
    }
    ui.ctx().request_repaint_after(Duration::from_millis(16));
    1.0 + 0.30 * bump(p)
}

/// Which way a popup's visibility animation is travelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupPhase {
    Entering,
    Open,
    Exiting,
}

/// One popup's visibility animation state, kept in egui temp
/// data. `from` is the progress the current run started at, so
/// reversing direction mid-flight never jumps.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PopupAnim {
    phase: PopupPhase,
    start: f64,
    from: f32,
}

/// The visibility of one popup, as an eased progress in 0..1, or
/// `None` when the popup should not be drawn at all. Call this
/// every frame with the popup's open flag:
///
/// - closed and never opened (or fully exited): `None`, draw
///   nothing;
/// - opening: progress runs 0 to 1 over the popup fade duration
///   on the house circ curve;
/// - open: `Some(1.0)`;
/// - closing: progress runs from wherever it was back to 0 over
///   the shorter exit duration, then `None`.
///
/// Because the caller keeps drawing while the exit runs, menus
/// and panels fade and shrink out instead of vanishing the frame
/// their state flips. Callers pass `exiting = progress.is_some()
/// && !open` and ignore interactions while exiting. The Motion
/// setting gates the whole thing: Off means open is `Some(1.0)`
/// and closed is `None`, instantly. Pair the progress with
/// [`popup_transform`] for the scale half.
pub(crate) fn popup_phase(
    ctx: &egui::Context,
    key: impl std::hash::Hash + std::fmt::Debug,
    open: bool,
    motion: crate::motion::MotionSetting,
) -> Option<f32> {
    if !motion.fades() {
        return if open { Some(1.0) } else { None };
    }
    let enter = motion.duration(crate::motion::dur::FADE) / 1000.0;
    let exit = motion.duration(crate::motion::dur::EXIT) / 1000.0;
    if enter <= 0.0 || exit <= 0.0 {
        return if open { Some(1.0) } else { None };
    }
    let id = Id::unique(("anim_popup_phase", key));
    let now = ctx.input(|i| i.time);
    let state = ctx.data_mut(|d| *d.get_temp_mut_or_default::<Option<PopupAnim>>(id));
    // The progress the stored run has reached right now, used
    // when the direction flips mid-flight.
    let current = |a: PopupAnim| -> f32 {
        match a.phase {
            PopupPhase::Open => 1.0,
            PopupPhase::Entering => {
                let x = ((now - a.start) / enter).clamp(0.0, 1.0) as f32;
                a.from + (1.0 - a.from) * ease_out_circ(x)
            }
            PopupPhase::Exiting => {
                let x = ((now - a.start) / exit).clamp(0.0, 1.0) as f32;
                a.from * (1.0 - ease_out_circ(x))
            }
        }
    };
    let (next, progress) = match (open, state) {
        (true, None) => {
            (Some(PopupAnim { phase: PopupPhase::Entering, start: now, from: 0.0 }), 0.0)
        }
        (true, Some(a)) if a.phase == PopupPhase::Exiting => {
            let p = current(a);
            (Some(PopupAnim { phase: PopupPhase::Entering, start: now, from: p }), p)
        }
        (true, Some(a)) => {
            let p = current(a);
            if a.phase == PopupPhase::Open {
                (Some(a), 1.0)
            } else if now - a.start >= enter {
                (Some(PopupAnim { phase: PopupPhase::Open, start: now, from: 1.0 }), 1.0)
            } else {
                ctx.request_repaint_after(Duration::from_millis(16));
                (Some(a), p)
            }
        }
        (false, None) => (None, 0.0),
        (false, Some(a)) if a.phase != PopupPhase::Exiting => {
            let p = current(a);
            (Some(PopupAnim { phase: PopupPhase::Exiting, start: now, from: p }), p)
        }
        (false, Some(a)) => {
            let p = current(a);
            if now - a.start >= exit {
                (None, 0.0)
            } else {
                ctx.request_repaint_after(Duration::from_millis(16));
                (Some(a), p)
            }
        }
    };
    if next != state {
        ctx.data_mut(|d| d.insert_temp(id, next));
    }
    if next.is_none() {
        None
    } else {
        Some(progress)
    }
}

/// Applies the scale half of a popup entrance to an Area drawn
/// this frame: its layer is scaled from 0.95 to 1.0 around
/// `anchor` (the trigger the popup belongs to), so the popup
/// reads as growing out of what opened it. Opacity is the
/// caller's half (`ui.set_opacity` inside the Area). A finished
/// run (p = 1) touches nothing.
pub(crate) fn popup_transform(ctx: &egui::Context, area_id: Id, anchor: Pos2, p: f32) {
    if p >= 0.999 {
        return;
    }
    let s = 0.95 + 0.05 * p;
    let layer = egui::LayerId::new(egui::Order::Foreground, area_id);
    ctx.transform_layer_shapes(
        layer,
        egui::emath::TSTransform {
            translation: Vec2::new(anchor.x * (1.0 - s), anchor.y * (1.0 - s)),
            scaling: s,
        },
    );
}

/// The typing indicator: three dots that bounce and fade in a
/// staggered wave. Paints into a small strip the caller allocates
/// inline (for example next to the header's "typing" label).
/// Unlike the other helpers this one repaints on a 33 ms timer
/// for as long as it is mounted (50 ms visibly stepped the
/// bounce); mount it only while a typing state is actually
/// showing. The wave runs a full cycle in about two seconds,
/// with each dot a fifth of a cycle behind the last, which is
/// the tempo mainstream chat indicators settle at: quick enough
/// to read as alive, slow enough to never feel frantic.
pub(crate) fn typing_dots(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(30.0, 12.0), Sense::hover());
    let time = ui.input(|i| i.time);
    let painter = ui.painter();
    for i in 0..3 {
        let wave = (time * 3.2 + f64::from(i) * 0.65).sin() * 0.5 + 0.5;
        let lift = (wave as f32).powf(1.5);
        let center = Pos2::new(rect.left() + 4.0 + i as f32 * 10.0, rect.center().y - 2.2 * lift);
        let alpha = (110.0 + 130.0 * lift) as u8;
        painter.circle_filled(
            center,
            2.6,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha),
        );
    }
    ui.ctx().request_repaint_after(Duration::from_millis(33));
}

#[cfg(test)]
mod tests {
    use super::{bump, ease_out};

    #[test]
    fn ease_out_endpoints_are_exact() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert_eq!(ease_out(-1.0), 0.0, "clamped below");
        assert_eq!(ease_out(2.0), 1.0, "clamped above");
    }

    #[test]
    fn bump_peaks_midway_and_settles_at_both_ends() {
        assert!(bump(0.0).abs() < 1e-6);
        assert!((bump(0.5) - 1.0).abs() < 1e-6);
        assert!(bump(1.0).abs() < 1e-4);
        assert!(bump(0.25) > 0.0 && bump(0.25) < 1.0);
    }
}
