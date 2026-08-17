//! Two-handed gestures across both touchpads.
//!
//! The Deck cannot report a real pinch. Each pad sees one contact in its own coordinate space,
//! so there is no pair of fingers whose separation can be measured directly. What it *can*
//! report is both thumbs at once, and "thumbs moving apart" carries the same intent as a
//! spread — so that is what this maps.
//!
//! Separation is `right.x − left.x`: pushing the left thumb left and the right thumb right
//! both increase it, which is the motion a person makes when they mean "bigger". The centroid
//! of the two contacts moves the object. Both are reported as *deltas since the last frame*,
//! so a caller can apply them to whatever it likes without this module knowing what a window
//! is.
//!
//! The one subtlety worth stating: the gesture anchors on the frame both thumbs land, and the
//! first delta it reports is zero. Without that, putting two thumbs down at arbitrary places
//! on the pads would instantly teleport whatever you were holding.

use crate::report::Pad;

/// Movement below this (in pad units, where the pad spans −1..1) is treated as stillness.
///
/// Thumbs resting on capacitive pads jitter by a few counts continuously. Without a deadband
/// a window "held" between two motionless thumbs drifts steadily off into the world.
const DEADBAND: f32 = 0.004;

/// How strongly a change in thumb separation scales the held object.
///
/// A full sweep — both thumbs travelling the length of their pads in opposite directions —
/// changes separation by 4.0, which at this rate is a little over a 3× scale. Roughly what a
/// two-finger spread across a phone screen does, and comfortable to land on a value.
const SCALE_RATE: f32 = 0.3;

/// What the two thumbs did this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GestureDelta {
    /// Movement of the midpoint between the thumbs, in pad units, +x right and +y up.
    pub pan: (f32, f32),
    /// Multiplicative scale change. 1.0 is no change, >1 means the thumbs moved apart.
    pub scale: f32,
}

impl GestureDelta {
    pub const NONE: GestureDelta = GestureDelta {
        pan: (0.0, 0.0),
        scale: 1.0,
    };

    pub fn is_negligible(&self) -> bool {
        self.pan.0 == 0.0 && self.pan.1 == 0.0 && self.scale == 1.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Anchor {
    centroid: (f32, f32),
    separation: f32,
}

/// Tracks a two-thumb gesture across frames.
#[derive(Debug, Clone, Copy, Default)]
pub struct TwoPadGesture {
    anchor: Option<Anchor>,
}

impl TwoPadGesture {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while both thumbs are down and the gesture owns them.
    ///
    /// Callers use this to suppress the single-pad pointer: without it, the right thumb would
    /// go on driving the laser while it is also half of a two-handed drag, and the pointer
    /// would race across the world as you resize something.
    pub fn is_active(&self) -> bool {
        self.anchor.is_some()
    }

    /// Fold in this frame's pad state.
    ///
    /// Returns `None` whenever the gesture is not running, so `if let Some(d)` is the whole
    /// integration. The frame the gesture *starts* returns [`GestureDelta::NONE`] rather than
    /// `None` — the gesture has begun, it just has not moved yet.
    pub fn update(&mut self, left: &Pad, right: &Pad) -> Option<GestureDelta> {
        if !(left.touched && right.touched) {
            // Either thumb lifting ends it. Re-anchoring on the next touch is what stops a
            // brief loss of contact from smearing the object across the world.
            self.anchor = None;
            return None;
        }

        let current = Anchor {
            centroid: (
                (left.x + right.x) * 0.5,
                (left.y + right.y) * 0.5,
            ),
            separation: right.x - left.x,
        };

        let Some(previous) = self.anchor else {
            self.anchor = Some(current);
            return Some(GestureDelta::NONE);
        };
        self.anchor = Some(current);

        let dx = deadband(current.centroid.0 - previous.centroid.0);
        let dy = deadband(current.centroid.1 - previous.centroid.1);
        let ds = deadband(current.separation - previous.separation);

        Some(GestureDelta {
            pan: (dx, dy),
            // Exponential so the gesture is symmetric: spreading then closing by the same
            // amount returns exactly to where it started, which a linear `1 + k·ds` does not.
            scale: (ds * SCALE_RATE).exp(),
        })
    }
}

fn deadband(v: f32) -> f32 {
    if v.abs() < DEADBAND {
        0.0
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(x: f32, y: f32) -> Pad {
        Pad {
            x,
            y,
            touched: true,
            clicked: false,
            // The gesture reads position only; a plausible resting weight keeps this honest
            // without implying it matters here.
            pressure: 12_000,
        }
    }

    fn lifted() -> Pad {
        Pad::default()
    }

    #[test]
    fn one_thumb_is_not_a_gesture() {
        // The right pad alone must stay available for the laser pointer.
        let mut g = TwoPadGesture::new();
        assert!(g.update(&lifted(), &touch(0.2, 0.0)).is_none());
        assert!(!g.is_active());
    }

    #[test]
    fn the_first_frame_does_not_jump() {
        // Thumbs land wherever they land. Reporting that as movement would fling whatever is
        // being held clear across the world the instant you touch down.
        let mut g = TwoPadGesture::new();
        let d = g.update(&touch(-0.7, 0.3), &touch(0.6, -0.2)).expect("gesture started");
        assert_eq!(d, GestureDelta::NONE);
        assert!(g.is_active());
    }

    #[test]
    fn thumbs_moving_apart_scales_up() {
        let mut g = TwoPadGesture::new();
        g.update(&touch(0.0, 0.0), &touch(0.0, 0.0));
        let d = g.update(&touch(-0.5, 0.0), &touch(0.5, 0.0)).unwrap();
        assert!(d.scale > 1.0, "spreading should grow, got {}", d.scale);
        assert!(d.pan.0.abs() < 1e-6, "a symmetric spread must not pan: {:?}", d.pan);
    }

    #[test]
    fn thumbs_coming_together_scales_down() {
        let mut g = TwoPadGesture::new();
        g.update(&touch(-0.5, 0.0), &touch(0.5, 0.0));
        let d = g.update(&touch(0.0, 0.0), &touch(0.0, 0.0)).unwrap();
        assert!(d.scale < 1.0, "closing should shrink, got {}", d.scale);
    }

    #[test]
    fn scaling_is_symmetric() {
        // Spread then close by the same amount must land exactly back where it started, or
        // repeated adjustment drifts the size in one direction. A linear rate fails this.
        let mut g = TwoPadGesture::new();
        g.update(&touch(0.0, 0.0), &touch(0.0, 0.0));
        let out = g.update(&touch(-0.4, 0.0), &touch(0.4, 0.0)).unwrap();
        let back = g.update(&touch(0.0, 0.0), &touch(0.0, 0.0)).unwrap();
        assert!(
            (out.scale * back.scale - 1.0).abs() < 1e-6,
            "{} then {} should cancel",
            out.scale,
            back.scale
        );
    }

    #[test]
    fn both_thumbs_moving_together_pans_without_scaling() {
        let mut g = TwoPadGesture::new();
        g.update(&touch(-0.3, 0.0), &touch(0.3, 0.0));
        let d = g.update(&touch(-0.1, 0.2), &touch(0.5, 0.2)).unwrap();
        assert!((d.pan.0 - 0.2).abs() < 1e-6, "pan.x = {}", d.pan.0);
        assert!((d.pan.1 - 0.2).abs() < 1e-6, "pan.y = {}", d.pan.1);
        assert!((d.scale - 1.0).abs() < 1e-6, "common motion must not scale");
    }

    #[test]
    fn resting_thumbs_do_not_drift() {
        // Capacitive pads jitter continuously. Without a deadband a held window wanders off
        // on its own, which looks like tracking drift rather than an input problem.
        let mut g = TwoPadGesture::new();
        g.update(&touch(0.0, 0.0), &touch(0.0, 0.0));
        let d = g.update(&touch(0.001, -0.002), &touch(-0.001, 0.002)).unwrap();
        assert!(d.is_negligible(), "jitter leaked through as {d:?}");
    }

    #[test]
    fn lifting_a_thumb_ends_the_gesture_and_the_next_touch_re_anchors() {
        let mut g = TwoPadGesture::new();
        g.update(&touch(-0.5, 0.0), &touch(0.5, 0.0));
        assert!(g.update(&lifted(), &touch(0.5, 0.0)).is_none());
        assert!(!g.is_active());
        // Coming back down far from where the thumbs left must not register as a huge move.
        let d = g.update(&touch(0.4, 0.4), &touch(-0.4, -0.4)).unwrap();
        assert_eq!(d, GestureDelta::NONE);
    }
}
