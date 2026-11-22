//! Central motion system: one place owns durations, easing, and
//! the Motion setting, so every animation in the app behaves the
//! same and can be turned down or off in one switch.
//!
//! Repaint discipline: an animation requests repaints only while
//! it runs. Each surface keeps an `Option<Anim>`; the draw code
//! calls [`Anim::drive`] which schedules
//! `request_repaint_after(remaining)` while unfinished and clears
//! the slot when done. When nothing is animating, nothing asks for
//! repaints and egui idles as usual; the state-machine test at the
//! bottom of this file proves an animation reaches a terminal
//! no-repaint state.

/// The user's motion preference (Settings > Appearance).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MotionSetting {
    /// Fades and slides at full duration.
    #[default]
    Full,
    /// Short fades only, no slides.
    Reduced,
    /// Everything appears instantly.
    Off,
}

impl MotionSetting {
    /// Stable string form for settings persistence.
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Reduced => "reduced",
            Self::Off => "off",
        }
    }

    /// Parses the persisted form; unknown values fall back to Full.
    #[must_use]
    pub(crate) fn from_str_or_full(s: &str) -> Self {
        match s {
            "reduced" => Self::Reduced,
            "off" => Self::Off,
            _ => Self::Full,
        }
    }

    /// Whether fade (alpha) animation is enabled.
    #[must_use]
    pub(crate) fn fades(self) -> bool {
        self != Self::Off
    }

    /// Whether slide (position) animation is enabled.
    #[must_use]
    pub(crate) fn slides(self) -> bool {
        self == Self::Full
    }

    /// Scales a base duration for this setting: Reduced shortens,
    /// Off collapses to zero (instant).
    #[must_use]
    pub(crate) fn duration(self, base_ms: f64) -> f64 {
        match self {
            Self::Full => base_ms,
            Self::Reduced => base_ms * 0.6,
            Self::Off => 0.0,
        }
    }
}

/// Standard durations in milliseconds. Everything animated in the
/// app picks one of these; surfaces do not invent their own. All
/// values sit in the 90-300 ms band: shorter reads as a glitch,
/// longer as lag. The press dip is the one deliberate exception
/// below 100 ms, because direct manipulation must feel instant.
pub(crate) mod dur {
    /// Modal and popup fade-ins.
    pub(crate) const FADE: f64 = 160.0;
    /// Popup and menu exits: about three quarters of the entrance,
    /// so closing feels decisive instead of lagging behind the
    /// pointer.
    pub(crate) const EXIT: f64 = 120.0;
    /// The media picker rising into place.
    pub(crate) const SLIDE: f64 = 180.0;
    /// Conversation switch fade.
    pub(crate) const SWITCH: f64 = 140.0;
    /// The send button pulse.
    pub(crate) const PULSE: f64 = 180.0;
    /// A new message fading and rising into the stream.
    pub(crate) const ENTER: f64 = 180.0;
    /// Hover washes on rows and chips.
    pub(crate) const HOVER: f64 = 130.0;
    /// Pressed washes (snappier than hover).
    pub(crate) const PRESS: f64 = 90.0;
    /// The unread badge pop when its count changes.
    pub(crate) const POP: f64 = 300.0;
    /// The bottom navigation icon pop on tab activation: a
    /// single short swell and settle, the restrained emphasis
    /// the native bars give the tab you just chose.
    pub(crate) const TAB_POP: f64 = 160.0;
    /// The scroll-to-bottom button fading in or out.
    pub(crate) const REVEAL: f64 = 150.0;
}

/// Ease-out cubic: fast start, gentle landing. `t` is clamped to
/// [0, 1]; the endpoints are exact.
#[must_use]
pub(crate) fn ease_out_cubic(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Ease-out circ: a very fast attack and a long soft tail. The
/// house curve for menus and popovers, which should feel like
/// they spring from their trigger and settle, never drift.
#[must_use]
pub(crate) fn ease_out_circ(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    (1.0 - (t - 1.0).powi(2)).sqrt()
}

/// One running animation, driven by absolute time (seconds, from
/// the egui input clock) so it is frame-rate independent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Anim {
    start: f64,
    duration: f64,
}

impl Anim {
    /// Starts an animation at `now` lasting `duration_ms`
    /// milliseconds (already setting-scaled by the caller via
    /// [`MotionSetting::duration`]).
    #[must_use]
    pub(crate) fn start(now: f64, duration_ms: f64) -> Self {
        Self { start: now, duration: (duration_ms / 1000.0).max(0.0) }
    }

    /// Eased progress in [0, 1] at `now`. A zero-duration animation
    /// is always complete.
    #[must_use]
    pub(crate) fn progress(&self, now: f64) -> f32 {
        if self.duration <= 0.0 {
            return 1.0;
        }
        let raw = (now - self.start) / self.duration;
        ease_out_cubic(raw) as f32
    }

    /// Whether the animation has finished at `now`.
    #[must_use]
    pub(crate) fn done(&self, now: f64) -> bool {
        self.duration <= 0.0 || now - self.start >= self.duration
    }

    /// Whether the animation still needs repaints at `now`. This
    /// is the repaint-discipline invariant: false forever after
    /// completion.
    #[must_use]
    pub(crate) fn needs_repaint(&self, now: f64) -> bool {
        !self.done(now)
    }

    /// Remaining time in milliseconds (for `request_repaint_after`).
    #[must_use]
    pub(crate) fn remaining_ms(&self, now: f64) -> u64 {
        let left = (self.start + self.duration - now) * 1000.0;
        if left <= 0.0 {
            0
        } else {
            left.ceil() as u64
        }
    }

    /// Drives one animation slot for this frame: returns the eased
    /// progress to render with, schedules the next repaint while
    /// running, and clears the slot when finished so an idle app
    /// never keeps repainting for a dead animation.
    pub(crate) fn drive(slot: &mut Option<Anim>, ctx: &eframe::egui::Context, now: f64) -> f32 {
        let Some(anim) = *slot else {
            return 1.0;
        };
        let p = anim.progress(now);
        if anim.needs_repaint(now) {
            ctx.request_repaint_after(std::time::Duration::from_millis(
                anim.remaining_ms(now).min(50),
            ));
        } else {
            *slot = None;
        }
        p
    }
}

impl crate::state::App {
    /// Starts a fresh animation for a surface, honoring the Motion
    /// setting (Off produces an instantly-complete animation).
    pub(crate) fn fresh_anim(&mut self, base_ms: f64, ctx: &eframe::egui::Context) -> Anim {
        self.anim.started += 1;
        let now = ctx.input(|i| i.time);
        Anim::start(now, self.motion.duration(base_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_endpoints_are_exact_and_monotonic() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(-2.0), 0.0, "clamped below");
        assert_eq!(ease_out_cubic(2.0), 1.0, "clamped above");
        let mut prev = 0.0;
        for i in 0..=20 {
            let v = ease_out_cubic(f64::from(i) / 20.0);
            assert!(v >= prev, "monotonic at step {i}");
            prev = v;
        }
    }

    #[test]
    fn circ_endpoints_are_exact_and_it_outruns_cubic_early() {
        assert_eq!(ease_out_circ(0.0), 0.0);
        assert_eq!(ease_out_circ(1.0), 1.0);
        assert_eq!(ease_out_circ(-2.0), 0.0, "clamped below");
        assert_eq!(ease_out_circ(2.0), 1.0, "clamped above");
        // The circ attack is faster than cubic in the first half.
        assert!(ease_out_circ(0.25) > ease_out_cubic(0.25));
        let mut prev = 0.0;
        for i in 0..=20 {
            let v = ease_out_circ(f64::from(i) / 20.0);
            assert!(v >= prev, "monotonic at step {i}");
            prev = v;
        }
    }

    #[test]
    fn setting_maps_to_features_and_durations() {
        assert!(MotionSetting::Full.fades() && MotionSetting::Full.slides());
        assert!(MotionSetting::Reduced.fades() && !MotionSetting::Reduced.slides());
        assert!(!MotionSetting::Off.fades() && !MotionSetting::Off.slides());
        assert_eq!(MotionSetting::Full.duration(200.0), 200.0);
        assert_eq!(MotionSetting::Reduced.duration(200.0), 120.0);
        assert_eq!(MotionSetting::Off.duration(200.0), 0.0);
        assert_eq!(MotionSetting::from_str_or_full("reduced"), MotionSetting::Reduced);
        assert_eq!(MotionSetting::from_str_or_full("junk"), MotionSetting::Full);
        for s in [MotionSetting::Full, MotionSetting::Reduced, MotionSetting::Off] {
            assert_eq!(MotionSetting::from_str_or_full(s.as_str()), s, "persistence round-trip");
        }
    }

    #[test]
    fn animation_completes_and_stops_requesting_repaints() {
        let anim = Anim::start(10.0, 200.0);
        assert_eq!(anim.progress(10.0), 0.0);
        assert!(anim.progress(10.1) > 0.0 && anim.progress(10.1) < 1.0);
        assert!(anim.needs_repaint(10.1), "still running mid-way");
        assert!(anim.done(10.25));
        assert!(!anim.needs_repaint(10.25), "terminal: no repaint at completion");
        assert!(!anim.needs_repaint(99.0), "and never again after");
        assert_eq!(anim.progress(99.0), 1.0);
        // Off-mode (zero duration) is complete from the first frame.
        let instant = Anim::start(3.0, MotionSetting::Off.duration(200.0));
        assert!(instant.done(3.0));
        assert!(!instant.needs_repaint(3.0));
    }
}
