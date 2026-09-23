//! Filling the shared-memory pose channel.
//!
//! The protocol's reasoning is in `spatiand-xr-v1.xml` and the memory layout is in
//! `spatiand_proto::pose`; this is the writer.
//!
//! ## Why a file rather than events
//!
//! An application drawing its own two eye views has to know where the head is at the instant
//! it draws, or the world swims when the wearer moves. An event carrying a pose is already
//! old when it arrives -- it costs a round trip, it is sent when the compositor gets to it
//! rather than when the client needs it, and it cannot be re-read a millisecond later without
//! another one. Here it is a memory read, so a client can take the pose as late as it likes,
//! which is the whole trick.
//!
//! ## The frame on the wire is not ours
//!
//! Spatiand thinks in +X forward, +Y left, +Z up. What goes into the channel is OpenXR's
//! frame -- +X right, +Y up, -Z forward -- and OpenXR's field order, so an adapter is a
//! memcpy rather than a translation layer. The conversion is [`to_openxr`] and it happens
//! exactly once, here, at the boundary.

use std::os::fd::BorrowedFd;

use glam::{DQuat, DVec3};

use spatiand_proto::pose::{Eye as WireEye, Ring, Slot, SLOTS};
use spatiand_render::{EyeSide, StereoConfig};

/// A mapped ring the compositor writes and clients read.
///
/// The memory and the seqlock are `spatiand_proto::pose::Ring`'s, shared with a host that
/// fills its own ring from what this session sends it. What is here is only what a *session*
/// knows and a host does not: the glasses' own frame, and where the eyes sit.
pub struct Channel {
    ring: Ring,
}

impl Channel {
    pub fn new() -> Result<Self, String> {
        let ring = Ring::new(c"spatiand-poses")?;
        log::info!("pose channel: {} bytes, {SLOTS} slots", ring.size());
        Ok(Self { ring })
    }

    /// The descriptor to hand a client. Read-only on their side.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.ring.fd()
    }

    pub fn size(&self) -> u32 {
        self.ring.size()
    }

    /// Write one sample.
    #[cfg(test)]
    pub fn write(
        &mut self,
        orientation: DQuat,
        head_position: DVec3,
        stereo: &StereoConfig,
        sample_ns: i64,
        predicted_ns: i64,
    ) {
        self.write_slot(wire_slot(orientation, head_position, stereo, sample_ns, predicted_ns));
    }

    /// Write one sample already built by [`wire_slot`].
    pub fn write_slot(&mut self, slot: Slot) {
        self.ring.write(slot);
    }
}

/// One sample, in the wire's frame and field order.
///
/// Both the local channel and the viewport sent to a host are built from this, so an
/// application reading poses here and one reading them on a host thirty metres away are
/// handed the same numbers — same eyes, same neck, same field of view.
pub fn wire_slot(
    orientation: DQuat,
    head_position: DVec3,
    stereo: &StereoConfig,
    sample_ns: i64,
    predicted_ns: i64,
) -> Slot {
    let (head_q, head_p) = to_openxr(orientation, head_position);
    Slot {
        seq: 0,
        sample_ns,
        predicted_ns,
        reserved: 0,
        head_orientation: head_q,
        head_position: head_p,
        _pad: 0.0,
        eye: [
            wire_eye(EyeSide::Left, orientation, head_position, stereo),
            wire_eye(EyeSide::Right, orientation, head_position, stereo),
        ],
    }
}

/// Each eye's field of view as `XrFovf`, left first.
///
/// The numbers every pose this session hands out carries, and so what an application drawing
/// from those poses has been told to draw with. The room is drawn back through exactly these,
/// so an application that honoured them fills the view edge to edge and one pixel of its picture
/// is one pixel's worth of the world.
pub fn eye_fovs(stereo: &StereoConfig) -> [[f32; 4]; 2] {
    [
        wire_eye(EyeSide::Left, DQuat::IDENTITY, DVec3::ZERO, stereo).fov,
        wire_eye(EyeSide::Right, DQuat::IDENTITY, DVec3::ZERO, stereo).fov,
    ]
}

/// The same sample as the viewport a host is sent.
///
/// `seq` counts viewports so a picture can say which one it was drawn for. `render_size` is
/// what the host should draw into, overscan included.
pub fn viewport(slot: &Slot, seq: u32, render_size: (u32, u32)) -> spatiand_stream::Viewport {
    spatiand_stream::Viewport {
        seq,
        // The session's own clock. A host cannot compare it with its own, and is not meant
        // to; it comes back with each picture so this end can measure how old that picture is.
        time_us: (slot.sample_ns / 1000).max(0) as u64,
        orientation: slot.head_orientation,
        position: slot.head_position,
        eye_position: [slot.eye[0].position, slot.eye[1].position],
        fov: [slot.eye[0].fov, slot.eye[1].fov],
        render_size,
    }
}

/// CLOCK_MONOTONIC nanoseconds.
///
/// The same clock a frame callback's timestamp comes from, which is what makes the predicted
/// display time in a slot comparable with anything else a client is holding.
pub fn now_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a libc call filling a struct we own.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// One eye, in OpenXR's frame and field order.
fn wire_eye(
    side: EyeSide,
    orientation: DQuat,
    head_position: DVec3,
    stereo: &StereoConfig,
) -> WireEye {
    // The same function the renderer uses, so a client drawing from these poses and the
    // compositor drawing the room cannot disagree about where the eyes are. That is worth
    // more than it sounds: two nearly-identical camera models is how content ends up
    // half a centimetre out and nobody can say why.
    let eye = spatiand_render::eye_for(side, orientation, head_position, stereo);
    let (q, p) = to_openxr(eye.orientation, eye.position);
    let half_h = (stereo.h_fov_deg.to_radians() * 0.5) as f32;
    // Vertical from the horizontal and the eye's aspect, matching the projection matrix.
    let aspect = stereo.per_eye.1.max(1) as f32 / stereo.per_eye.0.max(1) as f32;
    let half_v = (half_h.tan() * aspect).atan();
    WireEye {
        orientation: q,
        position: p,
        _pad: 0.0,
        // Signed, and symmetric here because the projection is. `XrFovf` exactly.
        fov: [-half_h, half_h, half_v, -half_v],
    }
}

/// Spatiand's frame to OpenXR's.
///
/// Ours is +X forward, +Y left, +Z up. OpenXR's is +X right, +Y up, −Z forward. So a vector
/// `(x, y, z)` becomes `(−y, z, −x)`, which is a pure rotation — the determinant is +1 — and
/// a rotation may be applied to a quaternion's vector part alone, leaving `w` untouched.
fn to_openxr(orientation: DQuat, position: DVec3) -> ([f32; 4], [f32; 3]) {
    let q = [
        -orientation.y as f32,
        orientation.z as f32,
        -orientation.x as f32,
        orientation.w as f32,
    ];
    let p = [-position.y as f32, position.z as f32, -position.x as f32];
    (q, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StereoConfig {
        StereoConfig {
            per_eye: (1920, 1080),
            ..Default::default()
        }
    }

    #[test]
    fn forward_becomes_negative_z() {
        // The one conversion in the whole protocol, and getting it wrong gives a client a
        // world rotated ninety degrees -- which reads as a tracking bug, not a sign error.
        let (_, p) = to_openxr(DQuat::IDENTITY, DVec3::X);
        assert_eq!(p, [0.0, 0.0, -1.0]);
    }

    #[test]
    fn left_becomes_negative_x_and_up_stays_up() {
        let (_, left) = to_openxr(DQuat::IDENTITY, DVec3::Y);
        assert_eq!(left, [-1.0, 0.0, 0.0]);
        let (_, up) = to_openxr(DQuat::IDENTITY, DVec3::Z);
        assert_eq!(up, [0.0, 1.0, 0.0]);
    }

    #[test]
    fn the_conversion_is_a_rotation_and_not_a_reflection() {
        // A reflection would also map the axes onto each other, and would silently mirror
        // every client's world. Checked by handedness: X cross Y must still be Z.
        let (_, x) = to_openxr(DQuat::IDENTITY, DVec3::X);
        let (_, y) = to_openxr(DQuat::IDENTITY, DVec3::Y);
        let (_, z) = to_openxr(DQuat::IDENTITY, DVec3::Z);
        let cross = DVec3::from_array([x[0] as f64, x[1] as f64, x[2] as f64])
            .cross(DVec3::from_array([y[0] as f64, y[1] as f64, y[2] as f64]));
        let zv = DVec3::from_array([z[0] as f64, z[1] as f64, z[2] as f64]);
        assert!((cross - zv).length() < 1e-9, "{cross:?} vs {zv:?}");
    }

    #[test]
    fn identity_orientation_stays_identity() {
        let (q, _) = to_openxr(DQuat::IDENTITY, DVec3::ZERO);
        assert_eq!(q, [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn the_eyes_are_an_ipd_apart_and_the_left_one_is_on_the_left() {
        // In OpenXR's frame +X is right, so the left eye has the smaller X. Getting this
        // backwards swaps the eyes, which looks almost right and is deeply unpleasant.
        let cfg = cfg();
        let left = wire_eye(EyeSide::Left, DQuat::IDENTITY, DVec3::ZERO, &cfg);
        let right = wire_eye(EyeSide::Right, DQuat::IDENTITY, DVec3::ZERO, &cfg);
        assert!(left.position[0] < right.position[0], "eyes are swapped");
        let apart = (right.position[0] - left.position[0]) as f64;
        assert!((apart - cfg.ipd_m).abs() < 1e-6, "{apart} apart");
    }

    #[test]
    fn the_field_of_view_is_signed_the_way_openxr_signs_it() {
        // angleLeft and angleDown are negative, angleRight and angleUp positive. A runtime
        // that gets this wrong builds an inside-out projection.
        let fov = wire_eye(EyeSide::Left, DQuat::IDENTITY, DVec3::ZERO, &cfg()).fov;
        assert!(fov[0] < 0.0 && fov[1] > 0.0, "horizontal: {fov:?}");
        assert!(fov[2] > 0.0 && fov[3] < 0.0, "vertical: {fov:?}");
        // And a 16:9 eye is wider than it is tall.
        assert!(fov[1] > fov[2], "{fov:?}");
    }

    #[test]
    fn a_written_sample_reaches_the_ring() {
        let mut channel = Channel::new().expect("channel");
        channel.write(DQuat::IDENTITY, DVec3::ZERO, &cfg(), 111, 222);
        let newest = channel.ring.newest().expect("written");
        assert_eq!(newest.sample_ns, 111);
        assert_eq!(newest.predicted_ns, 222);
        assert_eq!(newest.head_orientation, [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_host_is_sent_exactly_what_a_local_client_reads() {
        // The whole reason the viewport is built from the slot rather than beside it: two
        // applications, one here and one on a host, must not be handed eyes a few
        // millimetres apart.
        let turned = DQuat::from_rotation_z(0.4) * DQuat::from_rotation_y(-0.2);
        let slot = wire_slot(turned, DVec3::new(0.0, 0.0, 1.6), &cfg(), 5_000, 9_000);
        let v = viewport(&slot, 42, (2304, 1296));
        assert_eq!(v.seq, 42);
        assert_eq!(v.orientation, slot.head_orientation);
        assert_eq!(v.position, slot.head_position);
        assert_eq!(v.eye_position, [slot.eye[0].position, slot.eye[1].position]);
        assert_eq!(v.fov, [slot.eye[0].fov, slot.eye[1].fov]);
        assert_eq!(v.render_size, (2304, 1296));
    }
}
