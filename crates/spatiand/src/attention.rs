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
//! ## What counts as attention: hands only, never the head
//!
//! A thumb on a trackpad, a button, a scroll wheel, a key. **Not head movement**, and this was
//! wrong the first time.
//!
//! The original reasoning was that looking back at a film's controls is how a person asks for
//! them. It is not. Watching an immersive video *is* moving your head, continuously, and a
//! transport bar that reappears every time you look around is a bar that is always on screen —
//! in the middle of the one kind of content where anything on screen is the whole problem.
//! Reported from the headset as: the window fades, "but once my head move, the app showing
//! again, give block for me to enjoy the vr video".
//!
//! The subtle half is the pointer. The ray is cast from the head through a pad position, so it
//! sweeps across the room whenever the wearer turns, with no thumb involved. Watching the ray
//! would have re-introduced head tracking through the back door. So what is watched is the pad
//! and mouse coordinates themselves — see [`Hands`] — which move only when a hand moves them.

use std::time::Duration;

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

/// How far a thumb must travel across a pad to count as reaching for something.
///
/// Pad coordinates run -1..1, so this is half a percent of the pad's width. A resting thumb
/// wanders by a few thousandths on a capacitive surface; a deliberate move is many times this.
const REACH: f32 = 0.01;

/// Where the wearer's hands are, this frame.
///
/// Deliberately in the input device's own coordinates rather than in the world. The pointer
/// *ray* is cast from the head, so it moves whenever the head does even with a thumb held
/// perfectly still — watching the ray would mean head tracking woke everything up while
/// appearing not to. These numbers only change when a hand changes them.
///
/// `None` means that device is not being used: a pad with no thumb on it, or no mouse. Picking
/// one up or putting it down is itself attention, which is why the option is compared and not
/// just the coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Hands {
    pub right: Option<(f32, f32)>,
    pub left: Option<(f32, f32)>,
    pub mouse: Option<(f32, f32)>,
}

impl Hands {
    /// Whether a hand has actually moved since `previous`.
    pub fn moved_from(&self, previous: &Hands) -> bool {
        fn one(now: Option<(f32, f32)>, was: Option<(f32, f32)>) -> bool {
            match (now, was) {
                (Some(a), Some(b)) => (a.0 - b.0).abs() > REACH || (a.1 - b.1).abs() > REACH,
                // Arriving or leaving. A thumb landing on a pad is the clearest statement of
                // intent there is, and it must not have to travel before it counts.
                (a, b) => a.is_some() != b.is_some(),
            }
        }
        one(self.right, previous.right)
            || one(self.left, previous.left)
            || one(self.mouse, previous.mouse)
    }
}

/// The one idle clock.
#[derive(Debug, Default)]
pub struct Attention {
    /// How long since the wearer last did anything.
    idle_for: Duration,
    /// How long they had been idle at the moment before that — which is what says how faded
    /// everything was when they came back, and so where each fade resumes from.
    before_wake: Duration,
}

impl Attention {
    /// The wearer did something. Restarts the clock.
    pub fn stir(&mut self) {
        self.before_wake = self.idle_for;
        self.idle_for = Duration::ZERO;
    }

    /// Advance one frame.
    ///
    /// `tracked` says whether there is a headset. Without one nothing ever fades: a window on
    /// an ordinary desktop that dims itself for no visible reason is a bug report.
    ///
    /// The head pose is deliberately not a parameter. It was once, and that was the bug.
    pub fn tick(&mut self, tracked: bool, dt: Duration) {
        if !tracked {
            self.idle_for = Duration::ZERO;
            self.before_wake = Duration::ZERO;
            return;
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

    /// Run the clock forward with nobody touching anything.
    fn idle(a: &mut Attention, seconds: f32) {
        let mut left = Duration::from_secs_f32(seconds);
        while !left.is_zero() {
            let dt = FRAME.min(left);
            a.tick(true, dt);
            left -= dt;
        }
    }

    #[test]
    fn a_still_pair_of_hands_fades_away_and_stays_away() {
        let mut a = Attention::default();
        idle(&mut a, IDLE_AFTER.as_secs_f32() - 0.05);
        assert_eq!(a.default_alpha(), 1.0, "faded before the threshold");
        idle(&mut a, 2.0);
        assert_eq!(a.default_alpha(), 0.0);
        idle(&mut a, 30.0);
        assert_eq!(a.default_alpha(), 0.0, "came back on its own");
    }

    #[test]
    fn a_thumb_brings_it_back_faster_than_it_left() {
        let mut a = Attention::default();
        idle(&mut a, 10.0);
        assert_eq!(a.default_alpha(), 0.0);
        a.stir();
        let mut frames = 0;
        while a.default_alpha() < 1.0 && frames < 200 {
            a.tick(true, FRAME);
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
        a.tick(true, Duration::ZERO);
        let resumed = a.default_alpha();
        assert!(
            (resumed - midway).abs() < 0.05,
            "resumed at {resumed} after fading to {midway}"
        );
    }

    #[test]
    fn without_a_headset_nothing_fades() {
        let mut a = Attention::default();
        for _ in 0..2000 {
            a.tick(false, FRAME);
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

    // --- what counts as a hand moving ---

    #[test]
    fn a_resting_thumb_is_not_a_hand_moving() {
        // The failure this guards against is the whole feature not working: a capacitive pad
        // under a still thumb reports a few thousandths of jitter, and a bar that treats that
        // as attention never fades at all.
        let was = Hands {
            right: Some((0.20, -0.35)),
            ..Default::default()
        };
        let jitter = Hands {
            right: Some((0.203, -0.347)),
            ..Default::default()
        };
        assert!(!jitter.moved_from(&was));
    }

    #[test]
    fn a_thumb_that_actually_moves_counts() {
        let was = Hands {
            right: Some((0.20, -0.35)),
            ..Default::default()
        };
        let moved = Hands {
            right: Some((0.26, -0.35)),
            ..Default::default()
        };
        assert!(moved.moved_from(&was));
    }

    #[test]
    fn arriving_and_leaving_both_count() {
        // A thumb landing on a pad is the clearest statement of intent there is and must not
        // have to travel before it registers.
        let empty = Hands::default();
        let touching = Hands {
            left: Some((0.0, 0.0)),
            ..Default::default()
        };
        assert!(touching.moved_from(&empty));
        assert!(empty.moved_from(&touching));
    }

    #[test]
    fn the_head_cannot_reach_this_at_all() {
        // The bug being locked out: the pointer ray is cast from the head through a pad
        // position, so turning your head sweeps it across the room with no thumb involved.
        // `Hands` holds pad coordinates precisely so that there is nothing here for a head
        // pose to touch -- there is no head pose in this type, and `tick` takes none.
        let still = Hands {
            right: Some((0.4, 0.1)),
            mouse: Some((-0.2, 0.6)),
            left: None,
        };
        assert!(!still.moved_from(&still.clone()));
        let mut a = Attention::default();
        idle(&mut a, 10.0);
        // A whole session of head movement, expressed the only way it can reach this code:
        // not at all.
        for _ in 0..1000 {
            a.tick(true, FRAME);
        }
        assert_eq!(a.default_alpha(), 0.0, "something woke it that was not a hand");
    }
}
