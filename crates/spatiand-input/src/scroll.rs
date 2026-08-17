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
//! What separates them is what happens *next*. A real movement is followed by the thumb still
//! being there; a lift-off artefact is followed by the contact ending. So a delta is held back
//! for a few frames and only sent once continued contact has vouched for it, and whatever is
//! still waiting when the thumb goes is dropped unsent. The cost is [`LAG`] frames of latency
//! — tens of milliseconds, below the threshold where a scroll feels detached from the thumb —
//! and the benefit is that the contaminated samples are exactly the ones that never make it
//! out.

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
        self.held.clear();
        self.running = false;
    }

    pub fn update(&mut self, pad: &Pad) -> Scroll {
        if !pad.touched || pad.clicked {
            self.last = None;
            // Everything still waiting was measured across the lift. This is the whole point.
            self.held.clear();
            return if std::mem::take(&mut self.running) {
                Scroll::Stop
            } else {
                Scroll::Idle
            };
        }

        let now = (pad.x, pad.y);
        if let Some((px, py)) = self.last {
            let (dx, dy) = (now.0 - px, now.1 - py);
            if dx.abs() <= MAX_STEP && dy.abs() <= MAX_STEP {
                self.held.push_back((dx, dy));
            }
        }
        self.last = Some(now);

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

    fn at(x: f32, y: f32) -> Pad {
        Pad {
            x,
            y,
            touched: true,
            clicked: false,
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
        // Whatever those two frames measured, none of it reached the pointer: the only thing
        // sent is the delta from the clean part, and then the end of the gesture.
        assert!(
            matches!(out[0], Scroll::By { dx, .. } if dx.abs() < 1e-6),
            "sideways drift leaked out: {out:?}"
        );
        assert!(
            matches!(out[1], Scroll::By { dx, .. } if dx.abs() < 1e-6),
            "sideways drift leaked out: {out:?}"
        );
        assert_eq!(out[2], Scroll::Stop);
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
    fn a_click_is_a_button_rather_than_a_wheel() {
        let mut s = PadScroll::default();
        run(&mut s, &[at(0.0, 0.0), at(0.0, 0.05), at(0.0, 0.10), at(0.0, 0.15)]);
        let clicked = Pad {
            x: 0.0,
            y: 0.2,
            touched: true,
            clicked: true,
        };
        assert_eq!(s.update(&clicked), Scroll::Stop);
        assert_eq!(s.update(&clicked), Scroll::Idle, "the stop is sent once");
    }
}
