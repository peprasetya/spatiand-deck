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
//! ## What the hardware actually does
//!
//! Everything below is set from a recording of this pad being swiped, flicked, pressed and
//! released — `tools/capture-pad-trace.py`, replayed in the tests at the bottom. That matters,
//! because two earlier attempts at this filter were built on reasonable-sounding guesses and
//! both were wrong in ways the recording makes obvious:
//!
//! * **There is no speed that means "artefact".** A genuine flick reaches **0.26** pad units in
//!   one frame. The drift at the end of a slow swipe reaches **0.13**. Any fixed limit that
//!   catches the second destroys the first. So a limit on raw speed, or on the change in speed,
//!   cannot work however it is tuned — and that is what the earlier `MAX_JERK` was.
//! * **Pressure is not a measure of touch.** It reads a flat **zero** through an entire ordinary
//!   swipe and only rises once the click switch is closing — it is closer to a click-force
//!   reading than a contact-weight one. A gate built on it never fired at all.
//!
//! What the recording does show is that the artefact is only ever anomalous **relative to the
//! gesture it belongs to**. The lurch at the end of the slow swipe was ten times that swipe's
//! own settled speed; the flick's fastest frames were slower than its own typical ones. That
//! ratio is the discriminator, and it is scale-free — it says the same thing about a careful
//! drag and a hard flick.
//!
//! So two rules, and they cover different ground:
//!
//! **A speed limit that adapts to the gesture.** A delta far faster than what this contact has
//! been doing is the contact coming apart, not the hand. Below [`SPEED_FLOOR`] nothing is
//! questioned, because a gesture starting from rest has no history to be measured against and
//! must be allowed to accelerate.
//!
//! **Corroboration by what happens next.** A real movement is followed by the thumb still being
//! there; an artefact is followed by the contact ending. Every delta is held back [`LAG`]
//! frames and only sent once continued contact has vouched for it, and whatever is still
//! waiting when the thumb goes is dropped unsent. The recording is what set `LAG`: the
//! contamination ran **four** frames deep, and the previous value of two let half of it out.
//!
//! ## Ending a gesture is not the same as stopping it
//!
//! Filtering the deltas fixed the deltas and left the page still lurching, and the reason is
//! that the last thing a gesture sends is not a movement at all. Wayland's `axis_stop` is
//! named for the finger, not for the page: it says the thumb has gone, and a toolkit answers
//! it by taking the speed the scroll was doing and **throwing** the content on from there.
//! GTK, Chromium and Gecko all do this. So the event we were sending to settle a scroll was
//! the one starting a kinetic fling — and we sent it twice over, once when the thumb lifted
//! and again the moment the pad was clicked, which is why a press jumped the page as surely
//! as a release did.
//!
//! Nothing has to receive it. A mouse wheel never sends `axis_stop` and clients cope, so
//! withholding it is not a protocol hole — it simply leaves the page where the thumb left it.
//!
//! That makes inertia a choice, and [`EDGE`] is where the choice is made: a thumb that leaves
//! from the rim was still going when it ran out of pad, so the page keeps going; a thumb
//! lifted in the middle of the pad has arrived where it meant to, and the page stays put. A
//! click never flings, whatever it is over — the press is a button.

use std::collections::VecDeque;

use crate::report::Pad;

/// Absolute ceiling on one frame's movement, in pad units (−1..1).
///
/// A backstop, not the main filter: the fastest frame in the recording — mid-flick, entirely
/// genuine — was 0.26, so anything past a third of the pad is a report artefact rather than a
/// hand however fast the hand was going.
const MAX_STEP: f32 = 0.33;

/// Speed below which nothing is questioned, in pad units per frame.
///
/// A gesture starting from rest has no history to be judged against, so there has to be a
/// floor or the first movement of every swipe is rejected for being infinitely faster than the
/// stillness before it. Set above the settled speed of the slow swipe in the recording
/// (median 0.005, p90 0.013) and below its lift artefact (0.13).
const SPEED_FLOOR: f32 = 0.03;

/// How many times the gesture's own typical speed a single frame may reach before it is read
/// as the contact coming apart.
///
/// From the recording: the lift artefact ran 10x and 4.7x the settled speed of its own
/// gesture, while the flick's fastest frames were 1.3x its own. Four sits in that gap with
/// room on both sides.
const SPEED_RATIO: f32 = 4.0;

/// How quickly the typical-speed estimate follows the gesture.
///
/// Fast enough that a hard flick is believed within a frame or two of starting, slow enough
/// that a single wild sample cannot drag the estimate up far enough to justify the next one.
const SPEED_ADAPT: f32 = 0.35;

/// How many frames a delta waits before it is allowed out.
///
/// The recording's contamination ran exactly four frames deep, so four covers it on its own
/// even if the speed gate above lets something past — and on that trace either mechanism
/// alone is sufficient, which is the point of having both. Costs about 56 ms of latency and
/// discards the last four frames of every genuine gesture; on a slow swipe that is worth
/// roughly a fiftieth of a pad, which is why it is bounded here rather than set to whatever
/// would be safest.
const LAG: usize = 4;

/// How far out a thumb must be when it leaves for the page to carry on without it, as a
/// fraction of the way from the centre of the pad to whichever side it is nearest.
///
/// Measured along each axis separately rather than as a distance from the centre, because the
/// pad reports a square: at the corners [`Pad::radius`] reaches 1.41, so a distance would call
/// a thumb resting diagonally "at the edge" while it sat further from every side than one
/// three quarters of the way up the middle. What this asks is the question worth asking —
/// how close to running out of pad the thumb was.
///
/// Three quarters, checked against the recording, where the two cases fall either side of it
/// with room to spare: the deliberate swipe stops at 0.69 and the flick leaves at 0.84. The
/// exact figure matters less than which way it errs, and it errs outwards on purpose — set
/// too far out, a flick fails to carry and the wearer swipes again; set too far in, the page
/// is thrown when nobody threw it, which is the whole complaint.
const EDGE: f32 = 0.75;

/// What the pad is asking the pointer to do this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scroll {
    /// Nothing to send. The common case: no thumb, or a delta still waiting to be vouched for.
    Idle,
    /// Scroll by this much, in pad units. The caller decides what a pad unit is worth.
    By { dx: f32, dy: f32 },
    /// Let the page carry on from here. Sent once, and only when the thumb left from the rim.
    ///
    /// This is Wayland's `axis_stop`, and the rename is the point: it does not stop anything.
    /// A toolkit hearing it flings the content on at whatever speed the scroll was doing, so
    /// sending it at the end of every gesture — and on every click — was what threw the page.
    /// See [`EDGE`].
    Fling,
}

/// One pad's worth of scroll state.
#[derive(Debug, Default)]
pub struct PadScroll {
    /// Where the thumb was last frame. `None` means there is no baseline to measure from, which
    /// is why a thumb landing never scrolls on its first frame.
    last: Option<(f32, f32)>,
    /// What this gesture has typically been doing, pad units per frame. An estimate, not a
    /// measurement: what it is for is having something to call a frame anomalous *against*.
    typical: f32,
    /// Measured, not yet vouched for.
    held: VecDeque<(f32, f32)>,
    /// Whether anything has actually been emitted since the thumb landed, so that a thumb that
    /// rested without moving does not announce the end of a scroll that never started.
    running: bool,
    /// Set by a click, cleared only by the thumb leaving the pad.
    ///
    /// A press is a whole little event: the thumb rolls forward going down, sits while the
    /// button is held, and unrolls coming back up — and none of that is a scroll, though all
    /// of it moves the contact. Suppressing only the frames where `clicked` is set covers the
    /// middle and misses both ends, which is why releasing the pad still sent the page
    /// drifting. Nothing resumes until the thumb is lifted, because that is the only moment
    /// that unambiguously says the press is finished and the next thing is deliberate.
    disarmed: bool,
}

impl PadScroll {
    /// Drop everything without reporting anything, for when the pad has been taken away for
    /// some other purpose mid-gesture.
    ///
    /// Not the same as the thumb lifting: there is nothing to tell a client, because the pad is
    /// still down and the wearer is still holding it. What matters is that the distance it
    /// travels while it is busy elsewhere is not saved up and delivered as one enormous scroll
    /// the moment it comes back.
    pub fn forget(&mut self) {
        self.last = None;
        self.typical = 0.0;
        self.held.clear();
        self.running = false;
    }

    /// The fastest this frame is allowed to be, given what the gesture has been doing.
    fn limit(&self) -> f32 {
        (self.typical * SPEED_RATIO).max(SPEED_FLOOR).min(MAX_STEP)
    }

    /// Feed this frame's pad state.
    ///
    /// A pad that has been clicked is not scrolling, and stays not-scrolling until the thumb
    /// leaves it: the movement either side of a press belongs to the button, not the wheel.
    pub fn update(&mut self, pad: &Pad) -> Scroll {
        if !pad.touched {
            // Where the thumb was on its last real frame. The frame that reports the lift
            // carries no position -- there is nothing on the pad to have one.
            let left_from = self.last;
            let running = self.running;
            // Everything still waiting was measured across the lift. This is the whole point.
            self.forget();
            // The lift is also the one moment a press is unambiguously over.
            self.disarmed = false;
            let at_edge = left_from.is_some_and(|(x, y)| x.abs().max(y.abs()) >= EDGE);
            return if running && at_edge {
                Scroll::Fling
            } else {
                Scroll::Idle
            };
        }

        if pad.clicked {
            self.disarmed = true;
        }
        if self.disarmed {
            // No fling, wherever the thumb is sitting. A press is a button, and a button that
            // threw the page it was aiming at would be unusable at the edge of the pad.
            self.forget();
            return Scroll::Idle;
        }

        let now = (pad.x, pad.y);
        if let Some((px, py)) = self.last {
            let (dx, dy) = (now.0 - px, now.1 - py);
            let speed = (dx * dx + dy * dy).sqrt();
            if speed <= self.limit() {
                self.held.push_back((dx, dy));
            }
            // The estimate follows every frame, believed or not.
            //
            // This is the part that lets a hard flick through. Judging only the frames already
            // accepted would leave the estimate pinned to the stillness a gesture started
            // from, and a flick would be fought for its whole length rather than for its first
            // frame or two. Letting the rejected ones inform it means a sustained fast
            // movement talks the filter round, while a single wild sample cannot -- one frame
            // moves the estimate by a third, which is not enough to justify the next one.
            self.typical += (speed - self.typical) * SPEED_ADAPT;
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

    /// An unremarkable pace: below SPEED_FLOOR, so nothing here is testing the speed gate.
    const STEP: f32 = 0.02;

    #[test]
    fn a_steady_swipe_scrolls_by_what_the_thumb_did() {
        let mut s = PadScroll::default();
        let mut pads = vec![at(0.0, 0.0)];
        for i in 1..=10 {
            pads.push(at(0.0, i as f32 * STEP));
        }
        let out = run(&mut s, &pads);
        // The last LAG deltas are still in hand; everything before comes out at the true size.
        let sent: Vec<Scroll> = out.into_iter().filter(|s| *s != Scroll::Idle).collect();
        assert_eq!(sent.len(), 10 - LAG, "got {sent:?}");
        for step in sent {
            match step {
                Scroll::By { dx, dy } => {
                    assert!(dx.abs() < 1e-6);
                    assert!((dy - STEP).abs() < 1e-6, "got {dy}");
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
        let clean: Vec<Pad> = (0..8).map(|i| at(0.0, i as f32 * STEP)).collect();
        run(&mut s, &clean);
        let out = run(&mut s, &[at(0.06, 0.13), at(-0.05, 0.12), lifted()]);
        // Whatever those two frames measured, none of it reached the pointer. Anything that
        // did come out is a delta from the clean part of the swipe -- straight up the pad,
        // never sideways -- and the gesture ends without throwing the page, because it ended
        // in the middle of the pad.
        for step in &out {
            if let Scroll::By { dx, .. } = step {
                assert!(dx.abs() < 1e-6, "sideways drift leaked out: {out:?}");
            }
        }
        assert_eq!(out.last(), Some(&Scroll::Idle), "got {out:?}");
    }

    #[test]
    fn a_thumb_lifted_in_the_middle_leaves_the_page_where_it_is() {
        // The complaint. A swipe that ends where the wearer meant it to end has arrived, and
        // a page that keeps travelling afterwards has overshot whatever they were reading.
        let mut s = PadScroll::default();
        let swipe: Vec<Pad> = (0..12).map(|i| at(0.0, -0.2 + i as f32 * STEP)).collect();
        run(&mut s, &swipe);
        assert_eq!(s.update(&lifted()), Scroll::Idle);
    }

    #[test]
    fn a_thumb_that_runs_off_the_edge_hands_the_page_on() {
        // The other half. Swiping until the pad runs out is how you ask for more than one
        // pad's worth of page, so the content carries on from there.
        let mut s = PadScroll::default();
        let swipe: Vec<Pad> = (0..12)
            .map(|i| at(0.0, EDGE - 0.1 + i as f32 * STEP))
            .collect();
        run(&mut s, &swipe);
        assert_eq!(s.update(&lifted()), Scroll::Fling);
    }

    #[test]
    fn the_edge_is_whichever_side_is_nearest_not_the_distance_from_the_centre() {
        // A thumb parked diagonally is further from every side than one three quarters of the
        // way straight up, even though it is further from the middle. Measuring the distance
        // from the centre would call it an edge and fling the page off a resting thumb.
        let mut s = PadScroll::default();
        let corner: Vec<Pad> = (0..12)
            .map(|i| at(0.6 + i as f32 * STEP * 0.5, 0.6 + i as f32 * STEP * 0.5))
            .collect();
        assert!(
            corner.last().unwrap().radius() > EDGE,
            "the test needs a point past EDGE as a distance but not as a side"
        );
        run(&mut s, &corner);
        assert_eq!(s.update(&lifted()), Scroll::Idle);
    }

    #[test]
    fn an_edge_that_was_only_rested_on_flings_nothing() {
        // Sitting a thumb on the rim and taking it off again is not a gesture, and there is no
        // speed to carry on at.
        let mut s = PadScroll::default();
        run(&mut s, &[at(0.0, 0.9), at(0.0, 0.9), at(0.0, 0.9)]);
        assert_eq!(s.update(&lifted()), Scroll::Idle);
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
        let first: Vec<Pad> = (0..8).map(|i| at(-0.8, -0.8 + i as f32 * STEP)).collect();
        run(&mut s, &first);
        run(&mut s, &[lifted()]);
        let out = run(&mut s, &[at(0.8, 0.8), at(0.8, 0.8 + STEP)]);
        assert!(out.iter().all(|e| *e == Scroll::Idle), "got {out:?}");
    }

    #[test]
    fn a_click_is_a_button_rather_than_a_wheel() {
        let mut s = PadScroll::default();
        let swipe: Vec<Pad> = (0..8).map(|i| at(0.0, i as f32 * STEP)).collect();
        run(&mut s, &swipe);
        let clicked = Pad {
            clicked: true,
            ..pressing(0.0, 0.2, STEADY)
        };
        assert_eq!(s.update(&clicked), Scroll::Idle);
        assert_eq!(s.update(&clicked), Scroll::Idle);
    }

    #[test]
    fn clicking_at_the_edge_of_the_pad_does_not_throw_the_page() {
        // Pressing is aiming, and the rim is a perfectly ordinary place to aim at. The click
        // used to end the scroll with the same event a flick ends with, which is why a press
        // sent the page off as surely as a release did.
        let mut s = PadScroll::default();
        let swipe: Vec<Pad> = (0..8).map(|i| at(0.0, 0.7 + i as f32 * STEP)).collect();
        run(&mut s, &swipe);
        let clicked = Pad {
            clicked: true,
            ..pressing(0.0, 0.86, STEADY)
        };
        assert_eq!(s.update(&clicked), Scroll::Idle);
        // ...and the lift that follows it does not fling either, though it is at the rim: the
        // press is over, not a gesture that was interrupted.
        assert_eq!(s.update(&lifted()), Scroll::Idle);
    }
}

/// Replaying the recording in `scroll_trace`.
///
/// These are the tests that matter. Everything above them is reasoning about how a touchpad
/// behaves; these are the touchpad behaving. The bounds are stated as what the wearer would
/// feel — "no jolt", "a flick still flicks" — rather than as whatever the current constants
/// happen to produce, so that retuning has to keep the promise rather than move the goalposts.
#[cfg(test)]
mod trace {
    use super::*;
    use crate::scroll_trace::{CONTACT_2, CONTACT_3, CONTACT_4};

    /// What a scroll of this many pad units means on screen: `SCROLL_SCALE` in the compositor
    /// is 260, so a tenth of a pad is 26 units — a couple of lines of text. A jolt is anything
    /// the eye reads as the page having been thrown rather than dragged.
    const JOLT: f32 = 0.05;

    /// Feed a recorded contact through the filter, then lift the thumb.
    fn replay(frames: &[(f32, f32, bool, u16)]) -> Vec<(f32, f32)> {
        let mut scroll = PadScroll::default();
        let mut sent = Vec::new();
        for &(x, y, clicked, pressure) in frames {
            let pad = Pad { x, y, touched: true, clicked, pressure };
            if let Scroll::By { dx, dy } = scroll.update(&pad) {
                sent.push((dx, dy));
            }
        }
        // The lift itself, which is where the recording's contamination lives.
        scroll.update(&Pad::default());
        sent
    }

    /// What the recorded contact did when the thumb came off it.
    fn ending(frames: &[(f32, f32, bool, u16)]) -> Scroll {
        let mut scroll = PadScroll::default();
        for &(x, y, clicked, pressure) in frames {
            scroll.update(&Pad { x, y, touched: true, clicked, pressure });
        }
        scroll.update(&Pad::default())
    }

    fn distance(deltas: &[(f32, f32)]) -> f32 {
        deltas.iter().map(|(dx, dy)| (dx * dx + dy * dy).sqrt()).sum()
    }

    fn fastest(deltas: &[(f32, f32)]) -> f32 {
        deltas
            .iter()
            .map(|(dx, dy)| (dx * dx + dy * dy).sqrt())
            .fold(0.0, f32::max)
    }

    #[test]
    fn the_end_of_a_real_swipe_never_jolts_the_page() {
        // Contact 2 of the capture: a slow deliberate swipe, then a clean lift. The pad
        // reported 0.13 and 0.10 pad-unit steps in the last frames -- about thirty lines of
        // text arriving in two frames, from a thumb that had been ambling along at 0.005.
        let sent = replay(CONTACT_2);
        assert!(
            fastest(&sent) < JOLT,
            "a {:.3} pad-unit step reached the pointer; the swipe itself never exceeded ~0.04",
            fastest(&sent)
        );
    }

    #[test]
    fn a_slow_swipe_still_scrolls_most_of_the_way() {
        // The other half of the promise. A filter that emits nothing passes the test above.
        let sent = replay(CONTACT_2);
        assert!(
            distance(&sent) > 1.0,
            "only {:.3} pad units of a long swipe got through",
            distance(&sent)
        );
    }

    #[test]
    fn a_real_flick_is_not_mistaken_for_a_lift() {
        // Contact 3: a quick flick. Its fastest frames -- 0.26 pad units, faster than anything
        // in the lift artefact above -- are genuine hand movement, and this is the case that
        // rules out every fixed speed threshold.
        let sent = replay(CONTACT_3);
        assert!(
            distance(&sent) > 0.5,
            "the flick was filtered down to {:.3} pad units",
            distance(&sent)
        );
        assert!(
            fastest(&sent) > 0.1,
            "the flick's speed was clipped to {:.3}; it should still feel like a flick",
            fastest(&sent)
        );
    }

    #[test]
    fn nothing_after_a_press_begins_is_a_scroll() {
        // Contact 4: a thumb resting, pressing the pad to click, holding it, releasing, and
        // only then lifting. The reported symptom, and the movement before the press is
        // genuine -- the thumb really did travel while being positioned -- so the claim is
        // specifically about everything from the press onwards.
        //
        // Replayed from the first clicked frame, so what is measured is the press itself, the
        // hold, and the unrolling afterwards. The release was the leak: 0.035 and 0.039 pad
        // unit steps, several times this contact's settled speed, from a thumb doing nothing
        // but coming back up.
        let first_click = CONTACT_4
            .iter()
            .position(|(_, _, clicked, _)| *clicked)
            .expect("contact 4 contains a click");
        // A couple of frames of lead-in, so the filter is warmed up exactly as it would be.
        let from_press = &CONTACT_4[first_click.saturating_sub(2)..];
        let sent = replay(from_press);
        assert!(
            distance(&sent) < 1e-6,
            "a press and release leaked {:.3} pad units of scroll",
            distance(&sent)
        );
    }

    #[test]
    fn no_single_frame_of_any_recorded_gesture_jolts_the_page() {
        // Across everything captured -- swipe, flick, press. A flick is allowed to be fast;
        // what it is not allowed to do is deliver a whole gesture's worth in one frame.
        for (name, contact) in [("swipe", CONTACT_2), ("press", CONTACT_4)] {
            let sent = replay(contact);
            assert!(
                fastest(&sent) < JOLT,
                "{name}: a {:.3} pad-unit step reached the pointer",
                fastest(&sent)
            );
        }
    }

    #[test]
    fn the_recorded_swipe_ends_where_the_thumb_ended() {
        // Contact 2: a long deliberate swipe that comes to rest 0.69 of the way up the pad,
        // short of the rim. The thumb stopped because the reading had arrived, so the page
        // stops with it.
        assert_eq!(ending(CONTACT_2), Scroll::Idle);
    }

    #[test]
    fn the_recorded_flick_carries_on_past_the_pad() {
        // Contact 3: a quick flick that leaves the pad 0.84 of the way up, still moving. It
        // ran out of pad rather than out of intent, which is what inertia is for.
        assert_eq!(ending(CONTACT_3), Scroll::Fling);
    }

    #[test]
    fn the_recorded_press_never_flings() {
        // Contact 4: the reported symptom -- rest, press, hold, release, lift.
        assert_eq!(ending(CONTACT_4), Scroll::Idle);
    }
}
