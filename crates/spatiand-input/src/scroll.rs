//! Turning an absolute touchpad into a scroll that survives the thumb leaving it.
//!
//! The pads report a position, not a movement, so a scroll is the difference between two
//! samples. That is exact while a thumb is planted and fiction on either side of it, because a
//! capacitive pad does not know a finger has gone until it has already been going for a while.
//! As the thumb lifts, its contact patch shrinks unevenly and the reported centroid slides
//! toward whichever part of the print leaves last — several samples' worth of movement that
//! the thumb never made, in a direction nobody chose. That is the scroll running away at the
//! end of every swipe.
//!
//! Rejecting the big jump is not enough, and that is worth being precise about: the single
//! enormous step that arrives when a contact ends is easy to catch on size alone, and catching
//! it is what [`MAX_STEP`] does. The lift-off drift is the opposite problem — a handful of
//! deltas each small enough to be a perfectly ordinary slow scroll. Nothing about one of those
//! samples, looked at on its own, says it is wrong.
//!
//! Three things separate a real movement from an artefact, and it takes all three.
//!
//! **Pressure.** This is the direct measurement and the only one that sees the problem coming.
//! A thumb landing, lifting, or bearing down to click the pad changes how hard it is pressing
//! long before the touch bit changes or the click switch closes — which is why pressing the pad
//! scrolled the page even though a click already suppressed scrolling: by the time `clicked`
//! was true the thumb had finished rolling forward. Any frame where the pressure is moving
//! sharply is a frame where the position is being dragged around by the shape of the contact
//! rather than by the hand, and its delta is dropped.
//!
//! Pressure is read only as a **ratio against its own previous value**, never against a
//! threshold in counts. How hard a particular person rests a thumb on a pad is not a constant
//! worth hard-coding, and a ratio asks the question that actually matters — is the thumb
//! arriving or leaving — in a way that is the same for a heavy hand and a light one. It also
//! means a pad that reports no pressure at all can never trip the gate, so the filter degrades
//! to the other two rather than seizing up.
//!
//! **Jerk.** A thumb has mass and cannot reverse inside a frame. The lift-off artefact can and
//! does. So a delta that differs violently from the one before it is dropped, which is the
//! backstop for whatever the pressure gate misses.
//!
//! **What happens next.** A real movement is followed by the thumb still being there; an
//! artefact is followed by the contact ending. So a delta that passed the first two is still
//! held back for a few frames and only sent once continued contact has vouched for it, and
//! whatever is still waiting when the thumb goes is dropped unsent. The cost is [`LAG`] frames
//! of latency — tens of milliseconds, below the threshold where a scroll feels detached from
//! the thumb.

use std::collections::VecDeque;

use crate::report::Pad;

/// The largest single-frame movement treated as real, in pad units (−1..1).
///
/// A thumb crosses a few hundredths of the pad in one frame. Anything approaching a quarter of
/// it is the report artefact described above, not a hand.
const MAX_STEP: f32 = 0.25;

/// How many frames a delta waits before it is allowed out.
///
/// This is the width of the window at the end of a swipe that gets thrown away, so it wants to
/// be at least as long as the lift takes to register — a couple of frames — and no longer than
/// that, since every frame here is latency on every scroll. Two is about 28 ms at 72 Hz.
const LAG: usize = 2;

/// Pressure falling by more than this fraction of what it was means the thumb is on its way
/// off, whatever the touch bit still says.
const PRESSURE_FALL: f32 = 0.20;

/// Pressure rising by more than this fraction means it is on its way on, or bearing down to
/// click. Looser than the fall: settling onto a pad is a firmer change than leaving it, and a
/// swipe that presses harder as it goes is normal.
const PRESSURE_RISE: f32 = 0.45;

/// The largest frame-to-frame *change* in movement treated as real, in pad units.
///
/// Deliberately generous. A hard flick from rest reaches its speed over a few frames, and this
/// must not clip that; it is here for the violent reversal a lift produces, not for tidying up
/// ordinary movement.
const MAX_JERK: f32 = 0.12;

/// What the pad is asking the pointer to do this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scroll {
    /// Nothing to send. The common case: no thumb, or a delta still waiting to be vouched for.
    Idle,
    /// Scroll by this much, in pad units. The caller decides what a pad unit is worth.
    By { dx: f32, dy: f32 },
    /// The gesture ended. Sent once, so clients can settle any kinetic scrolling.
    Stop,
}

/// One pad's worth of scroll state.
#[derive(Debug, Default)]
pub struct PadScroll {
    /// Where the thumb was last frame. `None` means there is no baseline to measure from, which
    /// is why a thumb landing never scrolls on its first frame.
    last: Option<(f32, f32)>,
    /// How hard it was pressing last frame, for the ratio.
    last_pressure: u16,
    /// The last delta that was believed, for the jerk comparison.
    last_delta: (f32, f32),
    /// Measured, not yet vouched for.
    held: VecDeque<(f32, f32)>,
    /// Whether anything has actually been emitted since the thumb landed, so that a thumb that
    /// rested without moving does not announce the end of a scroll that never started.
    running: bool,
}

impl PadScroll {
    /// Feed this frame's pad state.
    ///
    /// A *clicked* pad is not scrolling: the press moves the thumb on its way down, and that
    /// movement belongs to the button rather than to the wheel.
    /// Drop everything without reporting anything, for when the pad has been taken away for
    /// some other purpose mid-gesture.
    ///
    /// Not the same as the thumb lifting: there is nothing to tell a client, because the pad is
    /// still down and the wearer is still holding it. What matters is that the distance it
    /// travels while it is busy elsewhere is not saved up and delivered as one enormous scroll
    /// the moment it comes back.
    pub fn forget(&mut self) {
        self.last = None;
        self.last_pressure = 0;
        self.last_delta = (0.0, 0.0);
        self.held.clear();
        self.running = false;
    }

    /// Is the thumb's weight changing sharply enough that its position cannot be trusted?
    ///
    /// Zero pressure last frame answers "no" for both directions, which is what makes a pad
    /// that reports nothing harmless rather than paralysing: with no baseline there is no
    /// ratio, and the gate simply never fires.
    fn unsettled(&self, now: u16) -> bool {
        let was = self.last_pressure as f32;
        if was <= 0.0 {
            return false;
        }
        let now = now as f32;
        (was - now) > was * PRESSURE_FALL || (now - was) > was * PRESSURE_RISE
    }

    pub fn update(&mut self, pad: &Pad) -> Scroll {
        if !pad.touched || pad.clicked {
            let running = self.running;
            // Everything still waiting was measured across the lift. This is the whole point.
            self.forget();
            return if running { Scroll::Stop } else { Scroll::Idle };
        }

        let now = (pad.x, pad.y);
        let settled = !self.unsettled(pad.pressure);
        if let Some((px, py)) = self.last {
            let (dx, dy) = (now.0 - px, now.1 - py);
            // A thumb has mass. It can be moving fast, but it cannot change what it is doing
            // between one frame and the next by more than this — whereas the shape of a
            // contact coming apart can, and does, reverse outright.
            let jerk = ((dx - self.last_delta.0).powi(2) + (dy - self.last_delta.1).powi(2)).sqrt();
            let plausible = dx.abs() <= MAX_STEP && dy.abs() <= MAX_STEP && jerk <= MAX_JERK;
            if settled && plausible {
                self.held.push_back((dx, dy));
            }
            // Remembered even when rejected, so a genuine hard flick loses only its first
            // frame instead of being fought all the way: the next delta is compared against
            // what the thumb is actually doing now, not against a speed it has left behind.
            self.last_delta = (dx, dy);
        }
        self.last = Some(now);
        self.last_pressure = pad.pressure;

        if self.held.len() > LAG {
            if let Some((dx, dy)) = self.held.pop_front() {
                if dx != 0.0 || dy != 0.0 {
                    self.running = true;
                    return Scroll::By { dx, dy };
                }
            }
        }
        Scroll::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thumb resting with the same weight throughout, which is the case where the pressure
    /// gate must stay out of the way entirely.
    const STEADY: u16 = 12_000;

    fn at(x: f32, y: f32) -> Pad {
        pressing(x, y, STEADY)
    }

    fn pressing(x: f32, y: f32, pressure: u16) -> Pad {
        Pad {
            x,
            y,
            touched: true,
            clicked: false,
            pressure,
        }
    }

    fn lifted() -> Pad {
        Pad::default()
    }

    /// Drive a whole swipe and collect what came out.
    fn run(scroll: &mut PadScroll, pads: &[Pad]) -> Vec<Scroll> {
        pads.iter().map(|p| scroll.update(p)).collect()
    }

    #[test]
    fn a_thumb_landing_does_not_scroll() {
        // There is no previous position to measure against, and treating the landing point as
        // a movement from wherever the last thumb left would jump the page on every touch.
        let mut s = PadScroll::default();
        assert_eq!(s.update(&at(0.7, -0.4)), Scroll::Idle);
    }

    #[test]
    fn a_steady_swipe_scrolls_by_what_the_thumb_did() {
        let mut s = PadScroll::default();
        let mut pads = vec![at(0.0, 0.0)];
        for i in 1..=6 {
            pads.push(at(0.0, i as f32 * 0.05));
        }
        let out = run(&mut s, &pads);
        // The first LAG deltas are still in hand; everything after comes out at the true size.
        let sent: Vec<Scroll> = out.into_iter().filter(|s| *s != Scroll::Idle).collect();
        assert_eq!(sent.len(), 6 - LAG, "got {sent:?}");
        for step in sent {
            match step {
                Scroll::By { dx, dy } => {
                    assert!(dx.abs() < 1e-6);
                    assert!((dy - 0.05).abs() < 1e-6, "got {dy}");
                }
                other => panic!("expected a scroll, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_last_moments_of_a_swipe_are_never_sent() {
        // The bug this exists for. The thumb slides cleanly, then the pad reports two more
        // small movements in a direction the thumb did not go while the contact fades out.
        let mut s = PadScroll::default();
        let clean = [at(0.0, 0.0), at(0.0, 0.05), at(0.0, 0.10), at(0.0, 0.15)];
        run(&mut s, &clean);
        let out = run(&mut s, &[at(0.06, 0.14), at(-0.05, 0.13), lifted()]);
        // Whatever those two frames measured, none of it reached the pointer. Anything that
        // did come out is a delta from the clean part of the swipe -- straight up the pad,
        // never sideways -- and the gesture ends cleanly.
        for step in &out {
            if let Scroll::By { dx, .. } = step {
                assert!(dx.abs() < 1e-6, "sideways drift leaked out: {out:?}");
            }
        }
        assert_eq!(out.last(), Some(&Scroll::Stop), "got {out:?}");
    }

    #[test]
    fn a_thumb_that_only_rested_ends_nothing() {
        // A stop with no preceding scroll tells a client a gesture it never saw has finished,
        // and some of them answer that with a kinetic flick.
        let mut s = PadScroll::default();
        let out = run(&mut s, &[at(0.2, 0.2), at(0.2, 0.2), at(0.2, 0.2), lifted()]);
        assert!(out.iter().all(|e| *e == Scroll::Idle), "got {out:?}");
    }

    #[test]
    fn a_jump_across_the_pad_is_not_a_scroll() {
        let mut s = PadScroll::default();
        let out = run(
            &mut s,
            &[at(-0.9, 0.0), at(0.9, 0.0), at(0.9, 0.0), at(0.9, 0.0)],
        );
        assert!(out.iter().all(|e| *e == Scroll::Idle), "got {out:?}");
    }

    #[test]
    fn lifting_and_landing_again_measures_from_the_new_place() {
        // Two swipes in opposite corners are two gestures, not one enormous diagonal.
        let mut s = PadScroll::default();
        run(&mut s, &[at(-0.8, -0.8), at(-0.8, -0.7), at(-0.8, -0.6), at(-0.8, -0.5)]);
        run(&mut s, &[lifted()]);
        let out = run(&mut s, &[at(0.8, 0.8), at(0.8, 0.85)]);
        assert!(out.iter().all(|e| *e == Scroll::Idle), "got {out:?}");
    }

    #[test]
    fn a_thumb_bearing_down_to_click_does_not_scroll_on_the_way() {
        // The reported symptom: pressing the pad scrolled the page. `clicked` comes too late
        // to prevent it -- by the time the switch closes the thumb has already rolled forward
        // and dragged the contact with it. Pressure sees it coming.
        let mut s = PadScroll::default();
        run(&mut s, &[at(0.0, 0.0), at(0.0, 0.04), at(0.0, 0.08), at(0.0, 0.12)]);
        // Weight piling on, and the contact smearing as it does.
        let out = run(
            &mut s,
            &[
                pressing(0.02, 0.15, 20_000),
                pressing(0.05, 0.17, 30_000),
                Pad { clicked: true, ..pressing(0.05, 0.17, 32_000) },
            ],
        );
        // Only the clean deltas from before the press get out, and none of them sideways.
        for step in &out {
            if let Scroll::By { dx, .. } = step {
                assert!(dx.abs() < 1e-6, "the press leaked sideways movement: {out:?}");
            }
        }
    }

    #[test]
    fn a_thumb_lightening_off_stops_being_believed() {
        let mut s = PadScroll::default();
        run(&mut s, &[at(0.0, 0.0), at(0.0, 0.04), at(0.0, 0.08), at(0.0, 0.12)]);
        // Still "touched", still moving plausibly, but the weight is coming off.
        let fading = run(
            &mut s,
            &[
                pressing(0.03, 0.14, 8_000),
                pressing(0.06, 0.15, 4_000),
                pressing(0.09, 0.16, 1_000),
            ],
        );
        for step in &fading {
            if let Scroll::By { dx, .. } = step {
                assert!(dx.abs() < 1e-6, "lift-off drift leaked out: {fading:?}");
            }
        }
    }

    #[test]
    fn a_pad_reporting_no_pressure_still_scrolls() {
        // The gate must never be the reason nothing moves. With no pressure signal there is no
        // ratio to take, so it stands aside and the other two filters carry the load.
        let mut s = PadScroll::default();
        let pads: Vec<Pad> = (0..6).map(|i| pressing(0.0, i as f32 * 0.05, 0)).collect();
        let out = run(&mut s, &pads);
        assert!(
            out.iter().any(|e| matches!(e, Scroll::By { .. })),
            "a pad with no pressure field scrolled nothing at all: {out:?}"
        );
    }

    #[test]
    fn a_thumb_cannot_reverse_inside_one_frame() {
        // The jerk backstop, for whatever the pressure gate misses. A steady slide up that
        // suddenly reports a large step back down is the contact coming apart, not the hand.
        let mut s = PadScroll::default();
        run(&mut s, &[at(0.0, 0.0), at(0.0, 0.08), at(0.0, 0.16), at(0.0, 0.24)]);
        let reversed = run(&mut s, &[at(0.0, 0.10), at(0.0, 0.10), at(0.0, 0.10)]);
        // The reversal itself is never emitted; what comes out is the earlier clean movement.
        for step in &reversed {
            if let Scroll::By { dy, .. } = step {
                assert!(*dy > 0.0, "a reversal was believed: {reversed:?}");
            }
        }
    }

    #[test]
    fn a_click_is_a_button_rather_than_a_wheel() {
        let mut s = PadScroll::default();
        run(&mut s, &[at(0.0, 0.0), at(0.0, 0.05), at(0.0, 0.10), at(0.0, 0.15)]);
        let clicked = Pad {
            clicked: true,
            ..pressing(0.0, 0.2, STEADY)
        };
        assert_eq!(s.update(&clicked), Scroll::Stop);
        assert_eq!(s.update(&clicked), Scroll::Idle, "the stop is sent once");
    }
}
