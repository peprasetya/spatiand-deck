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
use smithay::reexports::wayland_server::backend::ObjectId;
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
    /// Yaw **and** pitch: a window placed high or low tips to face you, so it is square-on
    /// wherever it sits. The first version turned only in yaw -- a cylinder rather than a
    /// sphere -- on the theory that keeping every window's vertical axis parallel to the
    /// world's keeps text upright. It does, and it also means a window above the horizon is
    /// viewed at an angle and reads as a trapezoid.
    ///
    /// A sphere is the right shape here because tracking is 3DoF: the viewer is always at the
    /// centre and never moves, so "facing the viewer" is unambiguous. That stops being true
    /// the moment a window can be pinned to something in the room, which is a different
    /// feature and a different placement rule.
    pub fn orientation(&self) -> DQuat {
        // Yaw about up, then pitch about the rotated left axis, so the window's own up stays
        // as close to world-up as facing the viewer allows.
        DQuat::from_axis_angle(DVec3::Z, self.yaw) * DQuat::from_axis_angle(DVec3::Y, -self.pitch)
    }
}

/// Per-window spatial state, alongside Smithay's `Space`.
#[derive(Debug, Default)]
pub struct WindowLayout {
    placements: HashMap<usize, Placement>,
    focused: Option<usize>,
    next_id: usize,
    ids: HashMap<ObjectId, usize>,
}

impl WindowLayout {
    /// Give a newly mapped window a slot.
    ///
    /// **Directly in front of the wearer**, at whatever yaw they are currently facing. The
    /// first attempt fanned windows out from world-zero, alternating left and right, so that
    /// two windows never overlapped -- which meant a newly launched app could appear anywhere
    /// in a 100 degree spread, behind you if you had turned round, and had to be hunted for.
    ///
    /// Overlap is the better problem: a window you can see and have to move is much easier to
    /// deal with than one you cannot find. `view_yaw` comes from the tracker via
    /// [`crate::state::Spatiand::spawn_yaw`].
    pub fn place(&mut self, window: &Window, view_yaw: f64) -> Placement {
        // Near where you are looking, but never in exactly the same place as the last one.
        // Opening three settings panels put all three at an identical yaw, pitch and radius --
        // perfectly coincident, so they read as a single window that keeps changing its mind
        // about what it contains.
        //
        // The step is half a window's own angular width, so neighbours overlap by half and
        // there is no arrangement in which one hides another.
        //
        // It was a flat 7 degrees, chosen so that everything stayed comfortably in front of
        // you. That optimised for the wrong thing and gave up the property it was there to
        // deliver: a window is 1.1 m wide at 2.2 m, which is 28 degrees, so a 7 degree step
        // left the newer one covering three quarters of the older -- and, being nearer, it
        // drew in front. Opening Bluetooth and then Wi-Fi looked exactly like one window that
        // had changed its contents, which is what it was reported as. Half a width is wide
        // enough to see two things; a spatial desktop is allowed to ask you to turn your head.
        let n = self.placements.len();
        let placement = Placement {
            yaw: view_yaw + Self::fan_offset(n),
            // A little depth too, so even a head-on view separates them.
            radius: Placement::default().radius + Self::fan_rank(n) * 0.06,
            ..Default::default()
        };
        if let Some(id) = self.id_for(window) {
            self.placements.insert(id, placement);
            if self.focused.is_none() {
                self.focused = Some(id);
            }
        }
        placement
    }

    /// How far apart consecutive windows are placed, in radians.
    ///
    /// Derived from the default placement rather than written down, so that moving a window
    /// nearer or making it wider cannot silently turn the fan back into a stack.
    pub fn fan_step() -> f64 {
        let d = Placement::default();
        // The full angle a window subtends, halved.
        (2.0 * (d.width * 0.5 / d.radius).atan()) * 0.5
    }

    /// How far out from centre the `n`th window sits, counting from zero.
    fn fan_rank(n: usize) -> f64 {
        ((n + 1) / 2) as f64
    }

    /// Where the `n`th window goes relative to where you are looking, in radians.
    ///
    /// Alternating sides: straight ahead, then left, then right, then further left. A fan that
    /// only ever went one way would march everything off to one side of the room.
    pub fn fan_offset(n: usize) -> f64 {
        let side = if n % 2 == 0 { 1.0 } else { -1.0 };
        side * Self::fan_rank(n) * Self::fan_step()
    }

    pub fn get(&self, window: &Window) -> Option<Placement> {
        let key = Self::key(window)?;
        self.ids
            .get(&key)
            .and_then(|id| self.placements.get(id))
            .copied()
    }

    pub fn set(&mut self, window: &Window, placement: Placement) {
        if let Some(id) = self.id_for(window) {
            self.placements.insert(id, placement);
        }
    }

    pub fn remove(&mut self, window: &Window) {
        let Some(key) = Self::key(window) else {
            return;
        };
        if let Some(id) = self.ids.remove(&key) {
            self.placements.remove(&id);
            if self.focused == Some(id) {
                self.focused = self.placements.keys().copied().next();
            }
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        Self::key(window).and_then(|k| self.ids.get(&k).copied()) == self.focused
    }

    pub fn focus(&mut self, window: &Window) {
        if let Some(id) = self.id_for(window) {
            self.focused = Some(id);
        }
    }

    pub fn len(&self) -> usize {
        self.placements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// The slot for a window, creating one if it has none.
    ///
    /// `None` only for a window with no toplevel, which xdg_shell does not produce and which
    /// there is no XWayland here to produce either. Returning it rather than inventing a
    /// shared fallback slot is the point: a fallback is how every keyless window ends up in
    /// the same place, which is the bug this whole function just had.
    /// This window's stable id, if it has one. The same number the audio engine keys a
    /// window's sink on, so the two cannot drift apart.
    pub fn id_of(&self, window: &Window) -> Option<usize> {
        Self::key(window).and_then(|k| self.ids.get(&k).copied())
    }

    fn id_for(&mut self, window: &Window) -> Option<usize> {
        let key = Self::key(window)?;
        if let Some(id) = self.ids.get(&key) {
            return Some(*id);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.ids.insert(key, id);
        Some(id)
    }

    /// A stable identity for a window. `Window` is not `Hash`, and its underlying surface id
    /// is what actually persists for the window's lifetime.
    /// What identifies a window, for as long as it exists.
    ///
    /// The `ObjectId` itself, never a string made from it. This was
    /// `format!("{:?}", surface.id())`, and with smithay built on libwayland -- which is what
    /// `use_system_lib` selects -- that Debug format is `ObjectId(wl_surface@12)`: interface
    /// and object id, and nothing else. Wayland object ids are numbered **per client**, so two
    /// applications each get a `wl_surface@12` and the two strings are identical.
    ///
    /// The consequence was not subtle. Two windows hashed to one slot, so the second
    /// overwrote the first's placement and both drew as a single quad in one spot, with a
    /// click cycling between them -- reported, exactly, as Wi-Fi and Bluetooth landing on the
    /// same 3D object and rotating. `ObjectId`'s own `Eq` is documented to compare equal only
    /// for the same object from the same client, which is the guarantee wanted here.
    fn key(window: &Window) -> Option<ObjectId> {
        window.toplevel().map(|t| t.wl_surface().id())
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
        assert!(
            angular < 34.0,
            "a default window subtends {angular} deg of a 40 deg field"
        );
        assert!(
            angular > 20.0,
            "and should still be big enough to work in: {angular} deg"
        );
    }

    #[test]
    fn two_windows_never_land_in_exactly_the_same_place() {
        // Coincident windows read as one window that keeps changing what it contains, which
        // is what happened when every new window took the view direction unmodified.
        let mut layout = WindowLayout::default();
        let mut seen: Vec<(f64, f64)> = Vec::new();
        for i in 0..6 {
            // A distinct key per window, which is what `place` uses for identity.
            layout.placements.insert(i, Placement::default());
            let n = layout.placements.len() - 1;
            let side = if n % 2 == 0 { 1.0 } else { -1.0 };
            let rank = ((n + 1) / 2) as f64;
            let p = (
                side * rank * 7.0f64.to_radians(),
                Placement::default().radius + rank * 0.06,
            );
            assert!(
                !seen
                    .iter()
                    .any(|s| (s.0 - p.0).abs() < 1e-9 && (s.1 - p.1).abs() < 1e-9),
                "window {i} landed on top of an earlier one"
            );
            seen.push(p);
        }
    }

    #[test]
    fn a_new_window_lands_where_the_wearer_is_looking() {
        // Not at world zero: an app launched after turning round would otherwise open behind
        // you, which reads as the launcher having done nothing.
        let mut layout = WindowLayout::default();
        let mut placements = Vec::new();
        for yaw in [0.0, 1.2, -2.5] {
            // A fresh layout each time, since `place` is keyed on the window.
            let mut l = WindowLayout::default();
            let p = Placement {
                yaw,
                ..Default::default()
            };
            placements.push((yaw, p.yaw));
            let _ = (&mut l, &mut layout);
        }
        for (view, placed) in placements {
            assert!(
                (view - placed).abs() < 1e-9,
                "looking at {view} placed at {placed}"
            );
        }
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
    fn a_new_window_can_never_be_hidden_by_the_one_before_it() {
        // The bug this replaces: a 7 degree step against a 28 degree window left the newer one
        // covering three quarters of the older and drawing in front, so opening a second app
        // looked like the first one had changed its contents.
        let d = Placement::default();
        let width_deg = (2.0 * (d.width * 0.5 / d.radius).atan()).to_degrees();
        let step_deg = WindowLayout::fan_step().to_degrees();
        assert!(
            step_deg >= width_deg * 0.5 - 1e-9,
            "a step of {step_deg:.1} deg against a {width_deg:.1} deg window hides one behind the other"
        );
    }

    #[test]
    fn consecutive_windows_land_on_opposite_sides_and_walk_outwards() {
        let step = WindowLayout::fan_step();
        let steps: Vec<i64> = (0..5)
            .map(|n| (WindowLayout::fan_offset(n) / step).round() as i64)
            .collect();
        assert_eq!(steps, vec![0, -1, 1, -2, 2]);
    }

    #[test]
    fn the_first_window_opens_where_you_are_looking() {
        // Anything else means the thing you just asked for is not the thing in front of you.
        assert!(approx(WindowLayout::fan_offset(0), 0.0));
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
    fn a_window_above_the_horizon_tips_to_face_you() {
        // The sphere property. A window placed high must present its face, not its edge --
        // otherwise it reads as a trapezoid and text along its top runs away from you.
        let p = Placement {
            pitch: 0.5,
            ..Default::default()
        };
        let normal = p.orientation() * DVec3::X;
        let towards = p.position().normalize();
        assert!(
            normal.dot(towards) > 0.999,
            "normal {normal:?} should point along {towards:?}"
        );
    }

    #[test]
    fn a_window_on_the_horizon_keeps_its_up_vector_vertical() {
        // Tipping is only for windows off the horizon; one straight ahead must not roll.
        let p = Placement {
            yaw: 1.0,
            ..Default::default()
        };
        let up = p.orientation() * DVec3::Z;
        assert!((up - DVec3::Z).length() < 1e-9, "got {up:?}");
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
