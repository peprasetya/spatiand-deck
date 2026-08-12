//! Where each window lives in the 3D world.
//!
//! Smithay's `Space` handles mapping, stacking and hit-testing in 2D, and there is no reason
//! to reimplement any of that. What it cannot know is that our "screen" is a sphere around
//! the wearer's head. This module holds that extra per-window state and nothing else.
//!
//! Windows are placed on a **cylinder** rather than a true sphere: at a fixed radius, with
//! yaw spread around the viewer and a small pitch, all facing inward. A cylinder keeps
//! vertical lines vertical, which matters a great deal for reading text — on a sphere,
//! windows away from the horizon have to tilt to face you, and tilted text is tiring.

use std::collections::HashMap;

use glam::{DQuat, DVec3};
use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;

/// Where a window sits, in the viewer-centred frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Angle around the viewer, radians. 0 is straight ahead, positive is to the left
    /// (matching the tracker's +Y-is-left convention).
    pub yaw: f64,
    /// Angle above the horizon, radians.
    pub pitch: f64,
    /// Distance from the viewer, metres.
    pub radius: f64,
    /// Width of the window in the world, metres. Height follows from the surface's aspect.
    pub width: f64,
}

impl Default for Placement {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            // Far enough that the eyes converge comfortably. Closer than about a metre and
            // the vergence/accommodation conflict starts to be felt on a fixed-focus display
            // like this one; much further and the window subtends too little of a 40 degree
            // field to read.
            radius: 2.2,
            // 1.1 m at 2.2 m is about 28 degrees, against one eye's 40. The first attempt used
            // 1.6 m at 2 m -- 44 degrees -- on the theory that a focused window should fill the
            // view. It does: it fills it completely, edges past the field on both sides, and a
            // window whose extent you cannot see is one you cannot aim a pointer at or judge
            // the size of. It also hid the entire world behind it.
            width: 1.1,
        }
    }
}

impl Placement {
    /// Position of the window's centre in world space.
    pub fn position(&self) -> DVec3 {
        // +X forward, +Y left, +Z up.
        let horizontal = self.radius * self.pitch.cos();
        DVec3::new(
            horizontal * self.yaw.cos(),
            horizontal * self.yaw.sin(),
            self.radius * self.pitch.sin(),
        )
    }

    /// Orientation that makes the window face the viewer.
    ///
    /// Yaw only: on a cylinder the window turns to face you horizontally but never tilts, so
    /// its vertical axis stays parallel to the world's. That is what keeps text upright.
    pub fn orientation(&self) -> DQuat {
        DQuat::from_axis_angle(DVec3::Z, self.yaw)
    }
}

/// Per-window spatial state, alongside Smithay's `Space`.
#[derive(Debug, Default)]
pub struct WindowLayout {
    placements: HashMap<usize, Placement>,
    focused: Option<usize>,
    next_id: usize,
    ids: HashMap<String, usize>,
}

impl WindowLayout {
    /// Give a newly mapped window a slot.
    ///
    /// New windows fan out from straight ahead, alternating left and right, so the second
    /// window does not land on top of the first and the wearer never has to hunt for it.
    pub fn place(&mut self, window: &Window, index: usize) -> Placement {
        let step = 50f64.to_radians();
        // 0, +1, -1, +2, -2, ... — outward in both directions from centre.
        let offset = ((index + 1) / 2) as f64 * if index % 2 == 0 { 1.0 } else { -1.0 };
        let placement = Placement {
            yaw: offset * step,
            ..Default::default()
        };
        let id = self.id_for(window);
        self.placements.insert(id, placement);
        if self.focused.is_none() {
            self.focused = Some(id);
        }
        placement
    }

    pub fn get(&self, window: &Window) -> Option<Placement> {
        self.ids.get(&Self::key(window)).and_then(|id| self.placements.get(id)).copied()
    }

    pub fn set(&mut self, window: &Window, placement: Placement) {
        let id = self.id_for(window);
        self.placements.insert(id, placement);
    }

    pub fn remove(&mut self, window: &Window) {
        let key = Self::key(window);
        if let Some(id) = self.ids.remove(&key) {
            self.placements.remove(&id);
            if self.focused == Some(id) {
                self.focused = self.placements.keys().copied().next();
            }
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.ids.get(&Self::key(window)).copied() == self.focused
    }

    pub fn focus(&mut self, window: &Window) {
        self.focused = Some(self.id_for(window));
    }

    pub fn len(&self) -> usize {
        self.placements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    fn id_for(&mut self, window: &Window) -> usize {
        let key = Self::key(window);
        if let Some(id) = self.ids.get(&key) {
            return *id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.ids.insert(key, id);
        id
    }

    /// A stable identity for a window. `Window` is not `Hash`, and its underlying surface id
    /// is what actually persists for the window's lifetime.
    fn key(window: &Window) -> String {
        window
            .toplevel()
            .map(|t| format!("{:?}", t.wl_surface().id()))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn a_default_window_fits_inside_one_eye() {
        // The constraint that was missed: a window wider than the field has no visible edges.
        let p = Placement::default();
        let angular = 2.0 * (p.width / 2.0 / p.radius).atan().to_degrees();
        assert!(angular < 34.0, "a default window subtends {angular} deg of a 40 deg field");
        assert!(angular > 20.0, "and should still be big enough to work in: {angular} deg");
    }

    #[test]
    fn straight_ahead_is_along_positive_x() {
        let p = Placement::default();
        let pos = p.position();
        assert!(approx(pos.x, p.radius), "expected +X forward, got {pos:?}");
        assert!(approx(pos.y, 0.0) && approx(pos.z, 0.0));
    }

    #[test]
    fn positive_yaw_puts_a_window_to_the_left() {
        // +Y is left in the canonical frame; a sign slip here mirrors the whole layout, and
        // the symptom — reaching right for a window that is on your left — is disorienting
        // rather than obviously a bug.
        let p = Placement {
            yaw: 45f64.to_radians(),
            ..Default::default()
        };
        assert!(p.position().y > 0.0, "got {:?}", p.position());
    }

    #[test]
    fn positive_pitch_puts_a_window_above() {
        let p = Placement {
            pitch: 30f64.to_radians(),
            ..Default::default()
        };
        assert!(p.position().z > 0.0);
    }

    #[test]
    fn placement_keeps_its_radius_whatever_the_angles() {
        for (yaw, pitch) in [(0.0, 0.0), (1.2, 0.4), (-2.5, -0.8), (3.0, 1.0)] {
            let p = Placement {
                yaw,
                pitch,
                ..Default::default()
            };
            assert!(
                approx(p.position().length(), p.radius),
                "yaw {yaw} pitch {pitch} gave radius {}",
                p.position().length()
            );
        }
    }

    #[test]
    fn windows_face_the_viewer_without_tilting() {
        // The cylinder property: whatever the placement, the window's up vector must stay
        // world-up, or text leans over.
        let p = Placement {
            yaw: 1.0,
            pitch: 0.5,
            ..Default::default()
        };
        let up = p.orientation() * DVec3::Z;
        assert!(
            (up - DVec3::Z).length() < 1e-9,
            "window up should stay vertical, got {up:?}"
        );
    }

    #[test]
    fn a_window_turned_to_face_you_points_back_at_the_origin() {
        let p = Placement {
            yaw: 40f64.to_radians(),
            ..Default::default()
        };
        // The window's local +X (its facing) rotated by its orientation should point away
        // from the viewer, i.e. along its own position.
        let facing = p.orientation() * DVec3::X;
        let to_window = p.position().normalize();
        assert!(
            facing.dot(to_window) > 0.99,
            "facing {facing:?} vs direction {to_window:?}"
        );
    }
}
