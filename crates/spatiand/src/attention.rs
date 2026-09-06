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
//! ## One clock, not one per surface
//!
//! There is one wearer, so there is one idea of whether they are busy. Every surface that has
//! asked for idle fading fades on the same clock and in step. Per-surface timers would mean a
//! toolbar that is still visible while the transport bar beside it is not, which reads as a
//! fault rather than as a design.
//!
//! ## What counts as attention
//!
//! Pointer movement, a button, a scroll, a key — and **turning your head**, which is the one
//! that makes it feel right rather than merely work. Looking back at a film's controls is how
//! a person asks for them, and the threshold is set so that the tremor of a still head does
//! not count while a deliberate glance does.

use std::time::Duration;

use glam::DQuat;

/// How long the wearer must be still and idle before anything fades.
///
/// Long enough not to fight someone who is reading a bar and deciding, short enough that it
/// is out of the way by the time the film has your attention again.
pub const IDLE_AFTER: Duration = Duration::from_secs(4);

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

/// The one idle clock, and the alpha it produces.
#[derive(Debug)]
pub struct Attention {
    /// How long since the wearer last did anything.
    idle_for: Duration,
    /// Where the head was pointing when they last did. Movement is measured from here rather
    /// than from the previous frame, so that a slow deliberate turn accumulates and wakes
    /// things, while sitting still never does however long you sit.
    anchor: Option<DQuat>,
    /// The current alpha, ramped rather than switched.
    alpha: f32,
}

impl Default for Attention {
    fn default() -> Self {
        Self {
            idle_for: Duration::ZERO,
            anchor: None,
            alpha: 1.0,
        }
    }
}

impl Attention {
    /// The wearer did something. Restarts the clock.
    pub fn stir(&mut self) {
        self.idle_for = Duration::ZERO;
        // The anchor is dropped rather than set, because the caller usually has no head pose
        // in its hand -- a button press does not know where you are looking. The next tick
        // re-anchors on whatever the head is doing then.
        self.anchor = None;
    }

    /// True if the pointer has moved far enough to count, remembering where it was.
    ///
    /// Separate from [`Attention::stir`] because the pointer is sampled every frame whether it
    /// moved or not, so "the pointer was serviced" is not the same claim as "the wearer moved
    /// the pointer".
    pub fn reached(&mut self, from: glam::DVec3, to: glam::DVec3) -> bool {
        from.angle_between(to) > REACH_RADIANS
    }

    /// Advance one frame and return the alpha an idle-fading surface should be drawn at.
    ///
    /// `head` is the current head orientation, or `None` where there is no headset — in which
    /// case nothing ever fades, because a desktop window that dims itself for no visible
    /// reason is a bug report.
    pub fn tick(&mut self, head: Option<DQuat>, dt: Duration) -> f32 {
        let Some(head) = head else {
            self.alpha = 1.0;
            return 1.0;
        };
        match self.anchor {
            None => self.anchor = Some(head),
            Some(anchor) => {
                // `angle_between` on quaternions is the angle of the rotation between them,
                // which is exactly "how far the head turned" and is what a dot product of
                // forward vectors would only approximate.
                if anchor.angle_between(head) > GLANCE_RADIANS {
                    self.idle_for = Duration::ZERO;
                    self.anchor = Some(head);
                }
            }
        }
        self.idle_for = self.idle_for.saturating_add(dt);

        let target = if self.idle_for >= IDLE_AFTER { 0.0 } else { 1.0 };
        let seconds = if target > self.alpha {
            FADE_IN_SECS
        } else {
            FADE_OUT_SECS
        };
        let step = dt.as_secs_f32() / seconds.max(1e-3);
        self.alpha = (self.alpha + (target - self.alpha).clamp(-step, step)).clamp(0.0, 1.0);
        self.alpha
    }

    /// The alpha as it stands, without advancing anything.
    pub fn alpha(&self) -> f32 {
        self.alpha
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(14);

    fn still() -> DQuat {
        DQuat::IDENTITY
    }

    #[test]
    fn nothing_fades_while_the_wearer_is_moving() {
        let mut a = Attention::default();
        for step in 0..600 {
            // A slow continuous turn, well past the idle threshold in wall time.
            let head = DQuat::from_rotation_y(step as f64 * 0.01);
            assert_eq!(a.tick(Some(head), FRAME), 1.0);
        }
    }

    #[test]
    fn a_still_head_fades_away_and_stays_away() {
        let mut a = Attention::default();
        let mut elapsed = Duration::ZERO;
        // Up to the frame that *reaches* the threshold, not past it: on that frame the clock
        // has arrived and the ramp starts, which is the behaviour being asserted rather than
        // an off-by-one to paper over.
        while elapsed + FRAME < IDLE_AFTER {
            assert_eq!(a.tick(Some(still()), FRAME), 1.0, "faded before the threshold");
            elapsed += FRAME;
        }
        // Ramp down.
        for _ in 0..200 {
            a.tick(Some(still()), FRAME);
        }
        assert_eq!(a.alpha(), 0.0);
        // And it does not come back on its own.
        for _ in 0..200 {
            a.tick(Some(still()), FRAME);
        }
        assert_eq!(a.alpha(), 0.0);
    }

    #[test]
    fn a_glance_brings_it_back_faster_than_it_left() {
        let mut faded = Attention::default();
        for _ in 0..1000 {
            faded.tick(Some(still()), FRAME);
        }
        assert_eq!(faded.alpha(), 0.0);
        // Look at it: three degrees, which is past the threshold.
        let glance = DQuat::from_rotation_y(0.05);
        let mut frames = 0;
        while faded.alpha() < 1.0 && frames < 200 {
            faded.tick(Some(glance), FRAME);
            frames += 1;
        }
        assert!(faded.alpha() >= 1.0, "never came back");
        // The whole point of the asymmetry: coming back is quicker than going away.
        let coming_back = frames as f32 * FRAME.as_secs_f32();
        assert!(
            coming_back < FADE_OUT_SECS,
            "took {coming_back}s to come back, which is not faster than leaving"
        );
    }

    #[test]
    fn a_press_counts_even_though_it_has_no_direction() {
        let mut a = Attention::default();
        for _ in 0..1000 {
            a.tick(Some(still()), FRAME);
        }
        assert_eq!(a.alpha(), 0.0);
        a.stir();
        a.tick(Some(still()), FRAME);
        assert!(a.alpha() > 0.0, "a button press did not wake it");
    }

    #[test]
    fn a_tremor_is_not_a_glance() {
        // The failure this guards against is a bar that never fades because the tracker is
        // never perfectly still -- which is indistinguishable from the feature not working.
        let mut a = Attention::default();
        let mut elapsed = Duration::ZERO;
        let mut step = 0.0f64;
        while elapsed < IDLE_AFTER * 3 {
            step += 1.0;
            // Half a degree, oscillating: real residual noise, and much larger than the
            // tracker's.
            let head = DQuat::from_rotation_y((step * 0.7).sin() * 0.008);
            a.tick(Some(head), FRAME);
            elapsed += FRAME;
        }
        assert_eq!(a.alpha(), 0.0, "noise kept it awake");
    }

    #[test]
    fn without_a_headset_nothing_fades() {
        let mut a = Attention::default();
        for _ in 0..2000 {
            assert_eq!(a.tick(None, FRAME), 1.0);
        }
    }
}
