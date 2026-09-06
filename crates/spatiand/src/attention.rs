//! Whether the wearer is paying attention, and what fades when they are not.
//!
//! This exists because of one thing a client asked for and could not build: a transport bar
//! over a film that gets out of the way, and comes back when you reach for it.
//!
//! The client cannot do it. Pointer motion is delivered only to the surface under the ray, so
//! a surface that has faded out never learns that the wearer is reaching towards it — it is
//! not under the ray until the moment they arrive, and by then it should already be visible.
//! The compositor is the only party that sees every pointer sample and every head movement
//! whatever they are aimed at, so the idle timer belongs here and `set_idle_fade` is one bit
//! saying "you hold it".
//!
//! ## One clock, many thresholds, and no per-surface state
//!
//! There is one wearer, so there is one idea of *how long they have been idle*. There is not
//! one idea of how long is too long: a toolbar over a model wants to stay while you think and
//! a transport bar over a film wants to be gone the moment you stop touching it, and no single
//! constant serves both. So the clock is shared and the threshold is the surface's, through
//! `set_idle_after`.
//!
//! That could have meant a fade value per surface, ticked every frame. It does not, because
//! [`Attention::alpha`] is a pure function of two shared durations and the surface's own
//! threshold — see its note. Nothing is stored per surface, nothing has to be ticked, and a
//! surface that appears mid-fade computes the same answer as one that has been there all
//! along.
//!
//! ## What counts as attention
//!
//! Pointer movement, a button, a scroll, a key — and **turning your head**, which is the one
//! that makes it feel right rather than merely work. Looking back at a film's controls is how
//! a person asks for them, and the threshold is set so that the tremor of a still head does
//! not count while a deliberate glance does.

use std::time::Duration;

use glam::DQuat;

/// How long the wearer must be still and idle before a surface that said nothing fades.
///
/// Two seconds. It was four, and the first person to watch a film through this said so: "the
/// window fade off too long". Four seconds of a lit bar in the middle of a film, every time
/// you touch anything, is a long time to look at something you are finished with.
///
/// A surface that wants otherwise says so with `set_idle_after`, which is the real answer —
/// this is only what a client gets for not having an opinion.
pub const IDLE_AFTER: Duration = Duration::from_secs(2);

/// The shortest idle a client may ask for, and the longest.
///
/// Below about a second the bar is strobing rather than fading: the ramps alone are two thirds
/// of a second, so a threshold under that never reaches full brightness before starting back
/// down. A client asking for 200 ms is asking for a flicker and should not get one.
pub const MIN_IDLE_AFTER: Duration = Duration::from_millis(1000);
pub const MAX_IDLE_AFTER: Duration = Duration::from_secs(60);

/// Seconds to fade out. Slow: something vanishing quickly reads as a glitch.
const FADE_OUT_SECS: f32 = 0.55;

/// Seconds to fade back in. Fast, because it is an answer to something the wearer just did,
/// and a control that takes half a second to admit it heard you feels broken.
const FADE_IN_SECS: f32 = 0.12;

/// How far the head must turn to count as looking at something, in radians.
///
/// About two degrees. The tracker's residual noise with a still head is well under a tenth of
/// this; a glance towards a control is many times it.
const GLANCE_RADIANS: f64 = 0.035;

/// How far the pointer ray must swing to count as reaching for something, in radians.
///
/// Smaller than a glance: the pad is deliberate in a way that a head is not, and a pointer
/// that moves at all was moved on purpose.
const REACH_RADIANS: f64 = 0.008;

/// The one idle clock.
#[derive(Debug, Default)]
pub struct Attention {
    /// How long since the wearer last did anything.
    idle_for: Duration,
    /// How long they had been idle at the moment before that — which is what says how faded
    /// everything was when they came back, and so where each fade resumes from.
    before_wake: Duration,
    /// Where the head was pointing when they last did something. Movement is measured from
    /// here rather than from the previous frame, so that a slow deliberate turn accumulates
    /// and wakes things, while sitting still never does however long you sit.
    anchor: Option<DQuat>,
}

impl Attention {
    /// The wearer did something. Restarts the clock.
    pub fn stir(&mut self) {
        self.before_wake = self.idle_for;
        self.idle_for = Duration::ZERO;
        // The anchor is dropped rather than set, because the caller usually has no head pose
        // in its hand -- a button press does not know where you are looking. The next tick
        // re-anchors on whatever the head is doing then.
        self.anchor = None;
    }

    /// True if the pointer has moved far enough to count.
    ///
    /// Separate from [`Attention::stir`] because the pointer is sampled every frame whether it
    /// moved or not, so "the pointer was serviced" is not the same claim as "the wearer moved
    /// the pointer".
    pub fn reached(&mut self, from: glam::DVec3, to: glam::DVec3) -> bool {
        from.angle_between(to) > REACH_RADIANS
    }

    /// Advance one frame.
    ///
    /// `head` is the current head orientation, or `None` where there is no headset — in which
    /// case nothing ever fades, because a desktop window that dims itself for no visible
    /// reason is a bug report.
    pub fn tick(&mut self, head: Option<DQuat>, dt: Duration) {
        let Some(head) = head else {
            self.idle_for = Duration::ZERO;
            self.before_wake = Duration::ZERO;
            return;
        };
        match self.anchor {
            None => self.anchor = Some(head),
            Some(anchor) => {
                // `angle_between` on quaternions is the angle of the rotation between them,
                // which is exactly "how far the head turned" and is what a dot product of
                // forward vectors would only approximate.
                if anchor.angle_between(head) > GLANCE_RADIANS {
                    self.stir();
                    self.anchor = Some(head);
                }
            }
        }
        self.idle_for = self.idle_for.saturating_add(dt);
    }

    /// How visible a surface with this idle threshold should currently be, 0..1.
    ///
    /// Computed rather than accumulated, which is what lets one clock serve every threshold
    /// without storing a fade per surface. Two cases:
    ///
    /// * **Past the threshold**, the surface is on its way out, and how far it has got is how
    ///   long it has been past it.
    /// * **Before the threshold**, it is on its way back in — from wherever it had faded to
    ///   when the wearer last did something, which is what `before_wake` remembers. That value
    ///   is shared, but the alpha it implies is not: each threshold reads a different fade out
    ///   of the same number.
    ///
    /// The result is that a surface which appears halfway through a fade, or changes its
    /// threshold mid-fade, gets the same answer as one that has been there all along — no
    /// state to be stale, and none to initialise.
    pub fn alpha(&self, after: Duration) -> f32 {
        let faded_by = |idle: Duration| {
            let past = idle.saturating_sub(after).as_secs_f32();
            (1.0 - past / FADE_OUT_SECS).clamp(0.0, 1.0)
        };
        if self.idle_for >= after {
            return faded_by(self.idle_for);
        }
        // Coming back, from where it had got to.
        let from = faded_by(self.before_wake);
        (from + self.idle_for.as_secs_f32() / FADE_IN_SECS).clamp(0.0, 1.0)
    }

    /// The alpha for a surface that expressed no preference.
    pub fn default_alpha(&self) -> f32 {
        self.alpha(IDLE_AFTER)
    }
}

/// What a client asked for, held to something sensible.
///
/// Zero means "your idea, not mine", which is the documented way to take the default back.
pub fn idle_after(milliseconds: u32) -> Duration {
    if milliseconds == 0 {
        return IDLE_AFTER;
    }
    Duration::from_millis(milliseconds as u64).clamp(MIN_IDLE_AFTER, MAX_IDLE_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(14);

    fn still() -> DQuat {
        DQuat::IDENTITY
    }

    /// Run the clock forward with a perfectly still head.
    fn idle(a: &mut Attention, seconds: f32) {
        let mut left = Duration::from_secs_f32(seconds);
        while !left.is_zero() {
            let dt = FRAME.min(left);
            a.tick(Some(still()), dt);
            left -= dt;
        }
    }

    #[test]
    fn nothing_fades_while_the_wearer_is_moving() {
        let mut a = Attention::default();
        for step in 0..600 {
            // A slow continuous turn, well past the idle threshold in wall time.
            a.tick(Some(DQuat::from_rotation_y(step as f64 * 0.01)), FRAME);
            assert_eq!(a.default_alpha(), 1.0);
        }
    }

    #[test]
    fn a_still_head_fades_away_and_stays_away() {
        let mut a = Attention::default();
        idle(&mut a, IDLE_AFTER.as_secs_f32() - 0.05);
        assert_eq!(a.default_alpha(), 1.0, "faded before the threshold");
        idle(&mut a, 2.0);
        assert_eq!(a.default_alpha(), 0.0);
        idle(&mut a, 30.0);
        assert_eq!(a.default_alpha(), 0.0, "came back on its own");
    }

    #[test]
    fn a_glance_brings_it_back_faster_than_it_left() {
        let mut a = Attention::default();
        idle(&mut a, 10.0);
        assert_eq!(a.default_alpha(), 0.0);
        // Look at it: three degrees, which is past the threshold.
        let glance = DQuat::from_rotation_y(0.05);
        let mut frames = 0;
        while a.default_alpha() < 1.0 && frames < 200 {
            a.tick(Some(glance), FRAME);
            frames += 1;
        }
        assert!(a.default_alpha() >= 1.0, "never came back");
        let coming_back = frames as f32 * FRAME.as_secs_f32();
        assert!(
            coming_back < FADE_OUT_SECS,
            "took {coming_back}s to come back, which is not faster than leaving"
        );
    }

    #[test]
    fn coming_back_resumes_from_where_it_faded_to() {
        // The failure this guards against is a bar that snaps to black and then ramps up from
        // there, because the wake threw away how visible it still was. Interrupt a fade
        // halfway and it must come back from halfway, not from nothing.
        let mut a = Attention::default();
        idle(&mut a, IDLE_AFTER.as_secs_f32() + FADE_OUT_SECS * 0.5);
        let midway = a.default_alpha();
        assert!(
            (0.3..0.7).contains(&midway),
            "meant to interrupt mid-fade, got {midway}"
        );
        a.stir();
        a.tick(Some(still()), Duration::ZERO);
        let resumed = a.default_alpha();
        assert!(
            (resumed - midway).abs() < 0.05,
            "resumed at {resumed} after fading to {midway}"
        );
    }

    #[test]
    fn a_press_counts_even_though_it_has_no_direction() {
        let mut a = Attention::default();
        idle(&mut a, 10.0);
        assert_eq!(a.default_alpha(), 0.0);
        a.stir();
        a.tick(Some(still()), FRAME);
        assert!(a.default_alpha() > 0.0, "a button press did not wake it");
    }

    #[test]
    fn a_tremor_is_not_a_glance() {
        // The failure this guards against is a bar that never fades because the tracker is
        // never perfectly still -- which is indistinguishable from the feature not working.
        let mut a = Attention::default();
        let mut step = 0.0f64;
        for _ in 0..1200 {
            step += 1.0;
            // Half a degree, oscillating: real residual noise, and much larger than the
            // tracker's.
            a.tick(Some(DQuat::from_rotation_y((step * 0.7).sin() * 0.008)), FRAME);
        }
        assert_eq!(a.default_alpha(), 0.0, "noise kept it awake");
    }

    #[test]
    fn without_a_headset_nothing_fades() {
        let mut a = Attention::default();
        for _ in 0..2000 {
            a.tick(None, FRAME);
            assert_eq!(a.default_alpha(), 1.0);
        }
    }

    #[test]
    fn two_surfaces_with_different_thresholds_fade_at_different_times() {
        // The whole reason the threshold is the surface's rather than the session's: a
        // transport bar over a film and a toolbar over a model want opposite things, and one
        // clock has to be able to answer both.
        let mut a = Attention::default();
        let quick = Duration::from_secs(2);
        let patient = Duration::from_secs(20);
        idle(&mut a, 4.0);
        assert_eq!(a.alpha(quick), 0.0, "the quick one should be gone");
        assert_eq!(a.alpha(patient), 1.0, "the patient one should be untouched");
        idle(&mut a, 20.0);
        assert_eq!(a.alpha(patient), 0.0);
    }

    #[test]
    fn a_client_cannot_ask_for_a_flicker() {
        assert_eq!(idle_after(0), IDLE_AFTER, "zero means the default");
        assert_eq!(idle_after(50), MIN_IDLE_AFTER);
        assert_eq!(idle_after(9_999_999), MAX_IDLE_AFTER);
        assert_eq!(idle_after(3_000), Duration::from_secs(3));
    }
}
