//! Stereo cameras for a 3DoF headset.
//!
//! The glasses report orientation only. There is no positional tracking and there will not
//! be one: HoloFrame established that inferring translation from this accelerometer does not
//! work — once you have leaned in and settled it reads pure gravity again, identical to
//! sitting upright, so absolute position is *absent* from the signal rather than merely
//! noisy. Three designs were built and removed.
//!
//! That does not mean the eyes sit at the origin. Two things move them, and both matter for
//! whether the world feels solid:
//!
//! * **IPD.** Each eye is offset along the head's right axis by ±half the interpupillary
//!   distance. This is what produces stereo depth at all.
//! * **The neck model.** A real head does not rotate about a point between the eyes; it
//!   rotates about a pivot roughly in the neck, well below and behind them. Modelling that
//!   gives honest parallax from rotation alone — turning your head really does shift near
//!   objects against far ones — for the cost of one vector add. Without it the world feels
//!   subtly like a painted backdrop that spins around you.
//!
//! Coordinates follow the tracker's canonical head frame: **+X forward, +Y left, +Z up**,
//! right-handed. Note this is *not* OpenGL's eye space (right, up, backward), so
//! [`Eye::view_matrix`] applies the conversion rather than leaving it as a trap.

use glam::{DMat4, DQuat, DVec3, Mat4};

/// Which eye. In side-by-side output the left eye occupies the left half of the framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EyeSide {
    Left,
    Right,
}

impl EyeSide {
    /// Sign of the offset along the head's **left** axis (+Y). The left eye is to the left.
    fn offset_sign(self) -> f64 {
        match self {
            EyeSide::Left => 1.0,
            EyeSide::Right => -1.0,
        }
    }

    pub fn both() -> [EyeSide; 2] {
        [EyeSide::Left, EyeSide::Right]
    }
}

/// Per-user and per-device geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StereoConfig {
    /// Interpupillary distance, metres. 63 mm is the population median and the device
    /// default; a per-user value noticeably improves how solid the world feels.
    pub ipd_m: f64,
    /// Horizontal field of view of one eye, degrees.
    pub h_fov_deg: f64,
    /// Resolution of one eye, used for the projection's aspect ratio.
    pub per_eye: (u32, u32),
    /// How far the eyes sit **forward** of the neck pivot, metres.
    pub neck_forward_m: f64,
    /// How far the eyes sit **above** the neck pivot, metres.
    pub neck_up_m: f64,
    pub near_m: f32,
    pub far_m: f32,
}

impl Default for StereoConfig {
    fn default() -> Self {
        Self {
            ipd_m: 0.063,
            h_fov_deg: 40.0,
            per_eye: (1920, 1080),
            // Roughly the offset from the atlanto-occipital joint to the eyes on an adult.
            // Small numbers, but they are the difference between parallax and none.
            neck_forward_m: 0.10,
            neck_up_m: 0.075,
            near_m: 0.05,
            // The skybox is drawn at effectively infinite distance, so this only has to
            // enclose the window layout.
            far_m: 100.0,
        }
    }
}

impl StereoConfig {
    pub fn aspect(&self) -> f64 {
        self.per_eye.0 as f64 / self.per_eye.1 as f64
    }

    /// Vertical FOV implied by the horizontal one and the aspect ratio, degrees.
    ///
    /// Derived rather than configured, because the two are not independent: the panel has a
    /// fixed shape, so specifying both invites a mismatch that shows up as a world which
    /// stretches when you look up.
    pub fn v_fov_deg(&self) -> f64 {
        let half_h = (self.h_fov_deg * 0.5).to_radians();
        2.0 * (half_h.tan() / self.aspect()).atan().to_degrees()
    }
}

/// One eye's view and projection for a given head pose.
#[derive(Debug, Clone, Copy)]
pub struct Eye {
    pub side: EyeSide,
    /// Eye position in world space, metres.
    pub position: DVec3,
    pub orientation: DQuat,
    pub view: Mat4,
    pub projection: Mat4,
}

impl Eye {
    pub fn view_projection(&self) -> Mat4 {
        self.projection * self.view
    }
}

/// Build both eyes for a head orientation.
///
/// `head_position` is where the **neck pivot** is in the world — normally the origin for a
/// 3DoF setup, but taking it as a parameter keeps the door open for a seated-recentre offset
/// or a future 6DoF backend without changing callers.
pub fn eyes_for(orientation: DQuat, head_position: DVec3, cfg: &StereoConfig) -> [Eye; 2] {
    EyeSide::both().map(|side| eye_for(side, orientation, head_position, cfg))
}

pub fn eye_for(side: EyeSide, orientation: DQuat, head_position: DVec3, cfg: &StereoConfig) -> Eye {
    // In the canonical frame: +X forward, +Y left, +Z up.
    let forward = orientation * DVec3::X;
    let left = orientation * DVec3::Y;
    let up = orientation * DVec3::Z;

    // Neck model first: the eyes ride on the end of a short lever from the pivot, so they
    // translate as the head rotates. This is the entire source of parallax on a 3DoF device.
    let eye_centre = head_position + forward * cfg.neck_forward_m + up * cfg.neck_up_m;
    let position = eye_centre + left * (side.offset_sign() * cfg.ipd_m * 0.5);

    Eye {
        side,
        position,
        orientation,
        view: view_matrix(position, orientation),
        projection: projection_matrix(cfg),
    }
}

/// World → OpenGL eye space.
///
/// Two frames meet here and it is worth being explicit, because a silent mismatch shows up
/// as a world that is mirrored or rotated 90° and is maddening to diagnose:
///
/// | | ours | OpenGL eye space |
/// |---|---|---|
/// | forward | +X | −Z |
/// | left | +Y | −X |
/// | up | +Z | +Y |
fn view_matrix(position: DVec3, orientation: DQuat) -> Mat4 {
    let forward = orientation * DVec3::X;
    let up = orientation * DVec3::Z;
    let m = DMat4::look_to_rh(position, forward, up);
    m.as_mat4()
}

fn projection_matrix(cfg: &StereoConfig) -> Mat4 {
    // Symmetric perspective. The Air's optics are centred, so there is no per-eye frustum
    // shear of the kind a wide-FOV VR headset needs.
    Mat4::perspective_rh_gl(
        (cfg.v_fov_deg() as f32).to_radians(),
        cfg.aspect() as f32,
        cfg.near_m,
        cfg.far_m,
    )
}

/// Where an eye's image goes in a side-by-side framebuffer, as `(x, y, width, height)`.
///
/// The glasses take a double-width signal and hand each eye half of it. This is the only
/// place that split is expressed; everything else renders into a viewport.
pub fn sbs_viewport(side: EyeSide, cfg: &StereoConfig) -> (i32, i32, i32, i32) {
    let (w, h) = (cfg.per_eye.0 as i32, cfg.per_eye.1 as i32);
    let x = match side {
        EyeSide::Left => 0,
        EyeSide::Right => w,
    };
    (x, 0, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn eyes_are_ipd_apart_and_on_the_correct_sides() {
        let cfg = StereoConfig::default();
        let [l, r] = eyes_for(DQuat::IDENTITY, DVec3::ZERO, &cfg);
        assert!(
            approx((l.position - r.position).length(), cfg.ipd_m, 1e-12),
            "eyes must be exactly one IPD apart"
        );
        // +Y is left in this frame, so the left eye has the greater Y.
        assert!(
            l.position.y > r.position.y,
            "left eye ended up on the right: {:?} vs {:?}",
            l.position,
            r.position
        );
    }

    #[test]
    fn vertical_fov_follows_from_horizontal_and_aspect() {
        let cfg = StereoConfig::default();
        // 40 deg horizontal on 16:9 gives about 23 deg vertical. A wrong derivation here
        // stretches the world when you look up, which reads as motion sickness rather than
        // as a maths bug.
        let v = cfg.v_fov_deg();
        assert!(approx(v, 23.1, 0.3), "expected ~23.1 deg, got {v}");
        assert!(v < cfg.h_fov_deg, "vertical must be the narrower axis");
    }

    #[test]
    fn neck_model_moves_the_eyes_when_the_head_turns() {
        // The whole point of the neck model: rotation alone must produce translation, or
        // there is no parallax and the world reads as a painted backdrop.
        let cfg = StereoConfig::default();
        let straight = eye_for(EyeSide::Left, DQuat::IDENTITY, DVec3::ZERO, &cfg);
        let turned = eye_for(
            EyeSide::Left,
            DQuat::from_axis_angle(DVec3::Z, 90f64.to_radians()),
            DVec3::ZERO,
            &cfg,
        );
        let moved = (straight.position - turned.position).length();
        assert!(
            moved > 0.05,
            "a 90 degree turn should shift the eye appreciably, moved {moved} m"
        );
    }

    #[test]
    fn disabling_the_neck_model_pins_the_eyes_to_the_pivot() {
        let cfg = StereoConfig {
            neck_forward_m: 0.0,
            neck_up_m: 0.0,
            ..Default::default()
        };
        let a = eye_for(EyeSide::Left, DQuat::IDENTITY, DVec3::ZERO, &cfg);
        let b = eye_for(
            EyeSide::Left,
            DQuat::from_axis_angle(DVec3::Z, 90f64.to_radians()),
            DVec3::ZERO,
            &cfg,
        );
        // Only the IPD offset rotates; the eye centre stays put.
        assert!(approx(a.position.length(), cfg.ipd_m * 0.5, 1e-12));
        assert!(approx(b.position.length(), cfg.ipd_m * 0.5, 1e-12));
    }

    #[test]
    fn a_point_ahead_projects_near_the_centre_of_both_eyes() {
        let cfg = StereoConfig::default();
        let target = DVec3::new(10.0, 0.0, cfg.neck_up_m); // straight ahead at eye height
        for eye in eyes_for(DQuat::IDENTITY, DVec3::ZERO, &cfg) {
            let clip = eye.view_projection() * target.as_vec3().extend(1.0);
            let ndc = clip.truncate() / clip.w;
            assert!(clip.w > 0.0, "point ahead must be in front of the camera");
            assert!(
                ndc.x.abs() < 0.05 && ndc.y.abs() < 0.05,
                "{:?} eye put a forward point at {ndc:?}",
                eye.side
            );
        }
    }

    #[test]
    fn stereo_disparity_has_the_right_sign_and_shrinks_with_distance() {
        // A near object must appear further right in the left eye than in the right eye.
        // Getting this backwards produces a world that is inside-out and physically painful,
        // and it is not obvious from a screenshot — only from wearing it.
        let cfg = StereoConfig::default();
        let ndc_x = |dist: f64, side: EyeSide| {
            let eye = eye_for(side, DQuat::IDENTITY, DVec3::ZERO, &cfg);
            let p = DVec3::new(dist, 0.0, cfg.neck_up_m);
            let clip = eye.view_projection() * p.as_vec3().extend(1.0);
            (clip.x / clip.w) as f64
        };
        let near = ndc_x(0.5, EyeSide::Left) - ndc_x(0.5, EyeSide::Right);
        let far = ndc_x(50.0, EyeSide::Left) - ndc_x(50.0, EyeSide::Right);
        assert!(
            near > 0.0,
            "left eye should see a near object further right, disparity {near}"
        );
        assert!(
            near > far.abs() * 10.0,
            "disparity must fall off with distance: near {near}, far {far}"
        );
    }

    #[test]
    fn sbs_viewports_tile_the_double_width_framebuffer_exactly() {
        let cfg = StereoConfig::default();
        let (lx, _, lw, lh) = sbs_viewport(EyeSide::Left, &cfg);
        let (rx, _, rw, _) = sbs_viewport(EyeSide::Right, &cfg);
        assert_eq!((lx, lw), (0, 1920));
        assert_eq!((rx, rw), (1920, 1920));
        assert_eq!(lx + lw, rx, "the halves must abut with no gap or overlap");
        assert_eq!(
            rx + rw,
            3840,
            "together they must fill the 3840 wide signal"
        );
        assert_eq!(lh, 1080);
    }

    #[test]
    fn looking_up_moves_a_forward_point_down_the_image() {
        let cfg = StereoConfig::default();
        let target = DVec3::new(10.0, 0.0, cfg.neck_up_m);
        let level = eye_for(EyeSide::Left, DQuat::IDENTITY, DVec3::ZERO, &cfg);
        // +Y is left, so a positive rotation about +Y pitches DOWN; negate to look up.
        let up = eye_for(
            EyeSide::Left,
            DQuat::from_axis_angle(DVec3::Y, -10f64.to_radians()),
            DVec3::ZERO,
            &cfg,
        );
        let y_of = |e: &Eye| {
            let c = e.view_projection() * target.as_vec3().extend(1.0);
            c.y / c.w
        };
        assert!(
            y_of(&up) < y_of(&level),
            "tilting the head up must move world content down the image"
        );
    }
}
