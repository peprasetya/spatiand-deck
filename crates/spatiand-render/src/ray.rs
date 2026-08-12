//! Pointing at things in 3D.
//!
//! The right touchpad drives a laser pointer. Two facts about the hardware decide the design:
//! the pad is **absolute** (a finger placed at the top-left of the pad means the top-left of
//! your view, not a relative nudge like a mouse), and there is no positional tracking, so the
//! ray has to start somewhere sensible on the head rather than at a tracked hand.
//!
//! So the pad maps to a direction in *head space* and the ray is cast from between the eyes.
//! Aiming is therefore two-handed in a natural way: turn your head for coarse aim, move your
//! thumb for fine aim. A relative mapping was considered and rejected — it needs clutching,
//! and clutching a pointer you cannot see the edges of is miserable.
//!
//! Frame convention throughout: **+X forward, +Y left, +Z up**.

use glam::{DQuat, DVec3};

/// How far from centre the pad can aim, in degrees.
///
/// Deliberately smaller than the display's field of view. The optics are poor at the extreme
/// edge, and reserving a margin means a fully-deflected thumb still lands somewhere readable
/// rather than in the corner where the image smears.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerConfig {
    pub half_fov_x_deg: f64,
    pub half_fov_y_deg: f64,
}

impl Default for PointerConfig {
    fn default() -> Self {
        Self {
            // Very nearly the whole field. The first attempt reserved a wide margin -- ±16°
            // and ±9° against a 40°x23° field -- on the theory that the edges are where the
            // optics are worst. That is true and it made the corners unreachable, which is
            // worse: a pointer that cannot reach a window's close button is not a pointer.
            half_fov_x_deg: 19.0,
            half_fov_y_deg: 11.0,
        }
    }
}

/// A ray in world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    pub origin: DVec3,
    /// Unit length.
    pub direction: DVec3,
}

impl Ray {
    pub fn at(&self, distance: f64) -> DVec3 {
        self.origin + self.direction * distance
    }
}

/// Turn an absolute touchpad contact into a ray.
///
/// `pad_x` and `pad_y` are −1..1 with +x right and +y up, exactly as
/// `spatiand_input::Pad` reports them.
pub fn ray_from_pad(
    pad_x: f32,
    pad_y: f32,
    head: DQuat,
    origin: DVec3,
    cfg: &PointerConfig,
) -> Ray {
    // Yaw about +Z (up) and pitch about +Y (left). Positive pad_x means right, and turning
    // right is a NEGATIVE rotation about up in a right-handed frame with +Y to the left —
    // getting this sign wrong mirrors the pointer, which feels broken long before it looks it.
    let yaw = -(pad_x as f64).clamp(-1.0, 1.0) * cfg.half_fov_x_deg.to_radians();
    // Likewise, pitching up is a negative rotation about +Y.
    let pitch = -(pad_y as f64).clamp(-1.0, 1.0) * cfg.half_fov_y_deg.to_radians();

    let aim = DQuat::from_axis_angle(DVec3::Z, yaw) * DQuat::from_axis_angle(DVec3::Y, pitch);
    Ray {
        origin,
        direction: (head * (aim * DVec3::X)).normalize(),
    }
}

/// A flat rectangle in the world — a window, a bubble's hit area, a HUD panel.
///
/// Local axes follow the world's: **+X is the outward normal** (pointing away from the viewer,
/// into the panel), +Y spans the width leftwards, +Z spans the height upwards. That matches
/// [`crate::window`]-style placements where a window is rotated to face the viewer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    pub centre: DVec3,
    pub orientation: DQuat,
    pub width: f64,
    pub height: f64,
}

/// Where a ray met a quad.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Distance along the ray, metres.
    pub distance: f64,
    /// Surface coordinates, 0..1, with **(0,0) at the top-left** — the same convention Wayland
    /// uses, so feeding a `wl_pointer` needs a multiply and nothing else.
    pub u: f64,
    pub v: f64,
    pub point: DVec3,
}

/// Intersect a ray with a quad, front or back.
///
/// Returns `None` for a miss, for a hit behind the origin, and for a ray running parallel to
/// the surface. Back faces are accepted deliberately: a window placed behind you is still a
/// window, and refusing to point at it produces a dead zone nobody can explain.
pub fn intersect_quad(ray: &Ray, quad: &Quad) -> Option<Hit> {
    let normal = quad.orientation * DVec3::X;
    let denominator = ray.direction.dot(normal);
    // Parallel, or near enough that the division would explode into a hit kilometres away.
    if denominator.abs() < 1e-9 {
        return None;
    }
    let distance = (quad.centre - ray.origin).dot(normal) / denominator;
    if distance <= 0.0 {
        return None;
    }

    let point = ray.at(distance);
    let local = quad.orientation.inverse() * (point - quad.centre);
    // +Y is left, so u — which grows rightwards — runs against it.
    let u = 0.5 - local.y / quad.width;
    let v = 0.5 - local.z / quad.height;
    if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
        return None;
    }
    Some(Hit {
        distance,
        u,
        v,
        point,
    })
}

/// The nearest quad a ray meets, as an index into `quads`.
///
/// Nearest rather than first: windows overlap on a cylinder, and picking whichever happened to
/// be earlier in the list would make the pointer select things hidden behind other things.
pub fn pick(ray: &Ray, quads: &[Quad]) -> Option<(usize, Hit)> {
    quads
        .iter()
        .enumerate()
        .filter_map(|(i, q)| intersect_quad(ray, q).map(|h| (i, h)))
        .min_by(|(_, a), (_, b)| a.distance.total_cmp(&b.distance))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    /// A quad two metres straight ahead, facing the viewer.
    fn ahead() -> Quad {
        Quad {
            centre: DVec3::new(2.0, 0.0, 0.0),
            orientation: DQuat::IDENTITY,
            width: 1.6,
            height: 0.9,
        }
    }

    fn forward_ray() -> Ray {
        Ray {
            origin: DVec3::ZERO,
            direction: DVec3::X,
        }
    }

    #[test]
    fn a_centred_thumb_points_straight_ahead() {
        let r = ray_from_pad(0.0, 0.0, DQuat::IDENTITY, DVec3::ZERO, &PointerConfig::default());
        assert!((r.direction - DVec3::X).length() < 1e-12, "{:?}", r.direction);
    }

    #[test]
    fn the_pointer_is_not_mirrored() {
        // The single most important property here. Moving the thumb right must move the
        // pointer right; a sign slip is instantly disorienting but reads as a tracking bug.
        let cfg = PointerConfig::default();
        let right = ray_from_pad(1.0, 0.0, DQuat::IDENTITY, DVec3::ZERO, &cfg);
        let up = ray_from_pad(0.0, 1.0, DQuat::IDENTITY, DVec3::ZERO, &cfg);
        // +Y is left, so pointing right means a negative Y component.
        assert!(right.direction.y < -0.2, "thumb right gave {:?}", right.direction);
        assert!(up.direction.z > 0.1, "thumb up gave {:?}", up.direction);
    }

    #[test]
    fn the_pointer_can_reach_every_corner_of_the_field() {
        // The bug this guards: the pointer reached the bottom of the view but not the top
        // corners, because its range was narrower than the display's and the ray started at
        // the pivot rather than at the eye -- so the whole reachable area sat low.
        let cfg = PointerConfig::default();
        // One eye of the glasses: 40 deg across, 23.14 deg tall.
        assert!(cfg.half_fov_x_deg >= 40.0 / 2.0 * 0.9, "cannot reach the sides");
        assert!(cfg.half_fov_y_deg >= 23.14 / 2.0 * 0.9, "cannot reach the top or bottom");
    }

    #[test]
    fn full_deflection_reaches_the_configured_angle() {
        let cfg = PointerConfig::default();
        let r = ray_from_pad(1.0, 0.0, DQuat::IDENTITY, DVec3::ZERO, &cfg);
        let angle = r.direction.angle_between(DVec3::X).to_degrees();
        assert!(close(angle, cfg.half_fov_x_deg, 1e-6), "got {angle}");
    }

    #[test]
    fn the_ray_follows_the_head() {
        // Turning your head 90° left must carry the pointer with it, or the laser stays
        // pinned to the world and aiming becomes impossible.
        let head = DQuat::from_axis_angle(DVec3::Z, std::f64::consts::FRAC_PI_2);
        let r = ray_from_pad(0.0, 0.0, head, DVec3::ZERO, &PointerConfig::default());
        assert!((r.direction - DVec3::Y).length() < 1e-9, "{:?}", r.direction);
    }

    #[test]
    fn a_forward_ray_hits_a_forward_quad_dead_centre() {
        let hit = intersect_quad(&forward_ray(), &ahead()).expect("should hit");
        assert!(close(hit.distance, 2.0, 1e-12));
        assert!(close(hit.u, 0.5, 1e-12) && close(hit.v, 0.5, 1e-12), "{hit:?}");
    }

    #[test]
    fn surface_coordinates_have_their_origin_at_the_top_left() {
        // Wayland's convention. If this is flipped, clicks land mirrored inside every window
        // and the cause is nowhere near the symptom.
        let quad = ahead();
        // Aim slightly up and to the left of centre.
        let r = Ray {
            origin: DVec3::ZERO,
            direction: DVec3::new(2.0, 0.4, 0.2).normalize(),
        };
        let hit = intersect_quad(&r, &quad).expect("should hit");
        assert!(hit.u < 0.5, "left of centre should be u<0.5, got {}", hit.u);
        assert!(hit.v < 0.5, "above centre should be v<0.5, got {}", hit.v);
    }

    #[test]
    fn misses_outside_the_rectangle() {
        let quad = ahead();
        // Well beyond the half-height of 0.45 m at 2 m.
        let r = Ray {
            origin: DVec3::ZERO,
            direction: DVec3::new(2.0, 0.0, 1.5).normalize(),
        };
        assert!(intersect_quad(&r, &quad).is_none());
    }

    #[test]
    fn nothing_behind_the_viewer_is_ever_hit() {
        let behind = Quad {
            centre: DVec3::new(-2.0, 0.0, 0.0),
            ..ahead()
        };
        assert!(intersect_quad(&forward_ray(), &behind).is_none());
    }

    #[test]
    fn a_ray_parallel_to_the_surface_does_not_hit_it_at_infinity() {
        let r = Ray {
            origin: DVec3::ZERO,
            direction: DVec3::Y,
        };
        assert!(intersect_quad(&r, &ahead()).is_none());
    }

    #[test]
    fn picking_prefers_the_nearer_of_two_overlapping_quads() {
        // Windows on a cylinder overlap constantly. Taking the first match instead of the
        // nearest lets the pointer select things that are visibly behind other things.
        let near = Quad {
            centre: DVec3::new(1.0, 0.0, 0.0),
            ..ahead()
        };
        let far = ahead();
        let (index, hit) = pick(&forward_ray(), &[far, near]).expect("should hit something");
        assert_eq!(index, 1, "picked the far quad");
        assert!(close(hit.distance, 1.0, 1e-12));
    }

    #[test]
    fn picking_an_empty_world_is_a_miss_not_a_panic() {
        assert!(pick(&forward_ray(), &[]).is_none());
    }

    #[test]
    fn a_quad_turned_to_face_you_is_still_hit_squarely() {
        // Mirrors how `WindowLayout` places things: rotate about up, then push out along the
        // rotated forward axis. The hit must stay centred, or windows off to the side develop
        // a pointer offset that grows with their yaw.
        let yaw = 50f64.to_radians();
        let orientation = DQuat::from_axis_angle(DVec3::Z, yaw);
        let quad = Quad {
            centre: orientation * DVec3::new(2.0, 0.0, 0.0),
            orientation,
            width: 1.6,
            height: 0.9,
        };
        let ray = Ray {
            origin: DVec3::ZERO,
            direction: (orientation * DVec3::X).normalize(),
        };
        let hit = intersect_quad(&ray, &quad).expect("should hit");
        assert!(close(hit.u, 0.5, 1e-9) && close(hit.v, 0.5, 1e-9), "{hit:?}");
    }
}
