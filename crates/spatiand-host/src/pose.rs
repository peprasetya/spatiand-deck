//! Where the wearer's head is, for the applications here that take the view.
//!
//! The session sends a [`Viewport`] every frame it draws, as a datagram. This writes each one
//! into the same shared-memory ring `spatiand_xr_v1`'s pose channel uses, so an application
//! on a host reads its head exactly as one on the headset would: a memory read, taken as late
//! as it likes. See `spatiand_proto::pose` for the layout and the seqlock.
//!
//! Written from the network thread the moment a viewport lands, not from the compositor's
//! loop: the loop runs once a frame, and a pose parked there for most of a frame is most of a
//! frame old before anything reads it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use spatiand_proto::pose::{Eye, Ring, Slot};
use spatiand_stream::Viewport;

pub struct Poses {
    ring: Ring,
    /// The newest viewport written, so an older one arriving late is not written over it.
    last_seq: Option<u32>,
    /// The size the session wants an application's two eyes drawn at, packed as
    /// `width << 32 | height`, zero until a session has said. Shared with the compositor's
    /// loop, which passes it to each application that takes the view: the network thread is
    /// where it arrives and the loop is where applications are talked to.
    render_size: Arc<AtomicU64>,
}

/// `(width, height)` from [`Poses::render_size`]'s packing, or `None` before any session said.
pub fn unpack_size(packed: u64) -> Option<(u32, u32)> {
    let (w, h) = ((packed >> 32) as u32, packed as u32);
    (w > 0 && h > 0).then_some((w, h))
}

impl Poses {
    pub fn new() -> Result<Poses, String> {
        Ok(Poses {
            ring: Ring::new(c"spatiand-host-poses")?,
            last_seq: None,
            render_size: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Where the session's wanted render size can be read from another thread.
    pub fn render_size(&self) -> Arc<AtomicU64> {
        self.render_size.clone()
    }

    /// A descriptor to hand to an application: onto the same memory, and read-only.
    pub fn read_only(&self) -> Result<std::os::fd::OwnedFd, String> {
        self.ring.read_only()
    }

    /// A new session has joined. Its viewports count from its own beginning, and the last
    /// session's numbering says nothing about them.
    pub fn reset(&mut self) {
        self.last_seq = None;
    }

    /// Write a viewport the session sent, unless a newer one is already there. Says whether it
    /// was written.
    ///
    /// Datagrams arrive in whatever order the network likes, and a pose written over a newer
    /// one moves the world back in time for a frame — a judder that looks like tracking noise.
    pub fn heard(&mut self, viewport: &Viewport, now_ns: i64) -> bool {
        if let Some(last) = self.last_seq {
            // Wrapping comparison: the count wraps after about two years at 72 a second, and a
            // session that outlives that should not freeze the world.
            if (viewport.seq.wrapping_sub(last) as i32) <= 0 {
                return false;
            }
        }
        self.last_seq = Some(viewport.seq);
        self.ring.write(slot(viewport, now_ns));
        let (w, h) = viewport.render_size;
        self.render_size
            .store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
        true
    }

    #[cfg(test)]
    fn newest(&self) -> Option<Slot> {
        self.ring.newest()
    }
}

/// A viewport as a ring slot.
///
/// A copy, not a conversion: the viewport is already in the ring's frame and its field of view
/// is already `XrFovf`. The times are this machine's own — the session's clock means nothing
/// here — and the prediction is "now", because this end has no idea when the session will
/// show whatever gets drawn; the picture carries the viewport's number back so that the
/// session, which does know, can correct for it.
fn slot(v: &Viewport, now_ns: i64) -> Slot {
    let eye = |i: usize| Eye {
        orientation: v.orientation,
        position: v.eye_position[i],
        _pad: 0.0,
        fov: v.fov[i],
    };
    Slot {
        seq: 0,
        sample_ns: now_ns,
        predicted_ns: now_ns,
        reserved: 0,
        head_orientation: v.orientation,
        head_position: v.position,
        _pad: 0.0,
        eye: [eye(0), eye(1)],
    }
}

/// CLOCK_MONOTONIC nanoseconds: the clock a frame callback's time comes from.
pub fn now_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a libc call filling a struct we own.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport(seq: u32, yaw_hint: f32) -> Viewport {
        Viewport {
            seq,
            time_us: 0,
            orientation: [0.0, yaw_hint, 0.0, 1.0],
            position: [0.0, 1.6, 0.0],
            eye_position: [[-0.032, 1.6, 0.0], [0.032, 1.6, 0.0]],
            fov: [[-0.8, 0.7, 0.6, -0.6], [-0.7, 0.8, 0.6, -0.6]],
            render_size: (3840, 1080),
        }
    }

    #[test]
    fn a_viewport_is_copied_into_the_ring_without_being_changed() {
        let mut poses = Poses::new().expect("ring");
        let v = viewport(1, 0.25);
        assert!(poses.heard(&v, 500));
        let s = poses.newest().expect("written");
        assert_eq!(s.head_orientation, v.orientation);
        assert_eq!(s.head_position, v.position);
        assert_eq!(s.eye[0].position, v.eye_position[0]);
        assert_eq!(s.eye[1].fov, v.fov[1], "field of view is not converted");
        assert_eq!(s.eye[0].orientation, v.orientation, "eyes share the head's turn");
        assert_eq!(s.sample_ns, 500);
    }

    #[test]
    fn a_late_viewport_does_not_move_the_world_backwards() {
        let mut poses = Poses::new().expect("ring");
        assert!(poses.heard(&viewport(10, 0.10), 1));
        assert!(!poses.heard(&viewport(9, 0.09), 2), "an older one was written");
        assert!(!poses.heard(&viewport(10, 0.10), 3), "a repeat was written");
        assert_eq!(poses.newest().expect("written").head_orientation[1], 0.10);
        assert!(poses.heard(&viewport(11, 0.11), 4));
    }

    #[test]
    fn the_count_wrapping_round_does_not_freeze_the_world() {
        let mut poses = Poses::new().expect("ring");
        assert!(poses.heard(&viewport(u32::MAX, 0.1), 1));
        assert!(poses.heard(&viewport(0, 0.2), 2), "wrapped to zero and was refused");
    }

    #[test]
    fn the_size_the_session_wants_is_passed_on() {
        let mut poses = Poses::new().expect("ring");
        let size = poses.render_size();
        assert_eq!(unpack_size(size.load(Ordering::Relaxed)), None, "nothing said yet");
        poses.heard(&viewport(1, 0.0), 1);
        assert_eq!(unpack_size(size.load(Ordering::Relaxed)), Some((3840, 1080)));
    }

    #[test]
    fn a_new_session_starts_counting_again() {
        let mut poses = Poses::new().expect("ring");
        assert!(poses.heard(&viewport(500, 0.1), 1));
        poses.reset();
        assert!(poses.heard(&viewport(1, 0.2), 2), "the next session's first viewport was refused");
    }
}
