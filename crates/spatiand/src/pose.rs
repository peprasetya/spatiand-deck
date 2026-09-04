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

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{fence, Ordering};

use glam::{DQuat, DVec3};

use spatiand_proto::pose::{channel_size, Eye as WireEye, Header, Slot, SLOTS, VERSION};
use spatiand_render::{EyeSide, StereoConfig};

/// A mapped ring the compositor writes and clients read.
pub struct Channel {
    memory: *mut u8,
    fd: OwnedFd,
    written: u64,
}

// The pointer is to a private mapping this type owns exclusively; nothing else in the
// compositor touches it, and it never leaves the render loop's thread.
unsafe impl Send for Channel {}

impl Channel {
    /// Create the shared memory and map it.
    ///
    /// Sealed against growing and against being written by anyone who receives it: a client
    /// that could shrink this file could make the compositor fault on its own mapping, which
    /// would be a client crashing the session by return of post.
    pub fn new() -> Result<Self, String> {
        let size = channel_size();
        let name = c"spatiand-poses";
        // SAFETY: a libc call with a valid C string and no borrowed state.
        let raw = unsafe {
            libc::memfd_create(name.as_ptr(), (libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) as u32)
        };
        if raw < 0 {
            return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: memfd_create returned a fresh descriptor we now own.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        // SAFETY: the descriptor is ours and the length is the size we are about to map.
        if unsafe { libc::ftruncate(raw, size as libc::off_t) } < 0 {
            return Err(format!("ftruncate: {}", std::io::Error::last_os_error()));
        }
        // Shrinking is the dangerous one -- a mapping over a truncated file faults on access.
        // SAFETY: a libc call on a descriptor we own.
        unsafe {
            libc::fcntl(
                raw,
                libc::F_ADD_SEALS,
                libc::F_SEAL_SHRINK | libc::F_SEAL_GROW,
            );
        }
        // SAFETY: mapping a descriptor we own, at a length it has been sized to.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        if memory == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        let memory = memory as *mut u8;

        let header = Header {
            version: VERSION,
            slot_count: SLOTS as u32,
            slot_stride: std::mem::size_of::<Slot>() as u32,
            slots_offset: std::mem::size_of::<Header>() as u32,
            write_index: 0,
            reserved: 0,
        };
        // SAFETY: `memory` is a fresh mapping of at least `channel_size()` bytes, and Header
        // is `repr(C)` with no padding requirements beyond its own alignment.
        unsafe { std::ptr::write(memory as *mut Header, header) };
        log::info!("pose channel: {} bytes, {SLOTS} slots", size);
        Ok(Self {
            memory,
            fd,
            written: 0,
        })
    }

    /// The descriptor to hand a client. Read-only on their side.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn size(&self) -> u32 {
        channel_size() as u32
    }

    /// Write one sample.
    ///
    /// An ordinary seqlock: the sequence number is made odd, the body written, the sequence
    /// made even again, and only then is the ring's write index advanced. A reader that
    /// catches a slot mid-write sees an odd sequence, or two different ones either side of
    /// its read, and tries again. Nothing blocks and nothing locks.
    pub fn write(
        &mut self,
        orientation: DQuat,
        head_position: DVec3,
        stereo: &StereoConfig,
        sample_ns: i64,
        predicted_ns: i64,
    ) {
        let index = (self.written as usize) & (SLOTS - 1);
        // SAFETY: `index` is masked into the ring, and the mapping holds SLOTS of them after
        // the header.
        let slot = unsafe {
            (self.memory.add(std::mem::size_of::<Header>()) as *mut Slot).add(index)
        };

        let seq = self.written * 2 + 1;
        // SAFETY: `slot` points into our own mapping; only this thread writes it.
        unsafe { std::ptr::addr_of_mut!((*slot).seq).write_volatile(seq) };
        // The odd sequence must be visible before the body it protects is disturbed.
        fence(Ordering::Release);

        let (head_q, head_p) = to_openxr(orientation, head_position);
        let body = Slot {
            seq,
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
        };
        // SAFETY: as above. `seq` is overwritten below, so writing the whole struct is fine.
        unsafe {
            std::ptr::write(slot, body);
            fence(Ordering::Release);
            std::ptr::addr_of_mut!((*slot).seq).write_volatile(seq + 1);
        }

        self.written += 1;
        fence(Ordering::Release);
        // SAFETY: the header is at offset zero of our mapping.
        unsafe {
            std::ptr::addr_of_mut!((*(self.memory as *mut Header)).write_index)
                .write_volatile(self.written)
        };
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what this type mapped.
        unsafe { libc::munmap(self.memory as *mut libc::c_void, channel_size()) };
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
    fn a_written_slot_can_be_read_back_the_way_a_client_would() {
        // The seqlock as a client sees it: even sequence, and the same one either side of
        // the body.
        let mut channel = Channel::new().expect("channel");
        channel.write(DQuat::IDENTITY, DVec3::ZERO, &cfg(), 111, 222);
        // SAFETY: reading our own mapping, exactly as a client reads theirs.
        unsafe {
            let header = &*(channel.memory as *const Header);
            assert_eq!(header.version, VERSION);
            assert_eq!(header.write_index, 1);
            let slots = channel.memory.add(header.slots_offset as usize) as *const Slot;
            let newest = &*slots.add(((header.write_index - 1) as usize) & (SLOTS - 1));
            assert_eq!(newest.seq % 2, 0, "a reader would see a torn slot");
            assert_eq!(newest.sample_ns, 111);
            assert_eq!(newest.predicted_ns, 222);
            assert_eq!(newest.head_orientation, [0.0, 0.0, 0.0, 1.0]);
        }
    }

    #[test]
    fn the_ring_wraps_without_losing_the_newest() {
        let mut channel = Channel::new().expect("channel");
        for i in 0..(SLOTS as i64 * 3) {
            channel.write(DQuat::IDENTITY, DVec3::ZERO, &cfg(), i, i);
        }
        // SAFETY: as above.
        unsafe {
            let header = &*(channel.memory as *const Header);
            let slots = channel.memory.add(header.slots_offset as usize) as *const Slot;
            let newest = &*slots.add(((header.write_index - 1) as usize) & (SLOTS - 1));
            assert_eq!(newest.sample_ns, SLOTS as i64 * 3 - 1);
        }
    }
}
