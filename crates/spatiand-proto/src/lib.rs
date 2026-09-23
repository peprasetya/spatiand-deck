//! Spatiand's own Wayland protocols.
//!
//! One protocol, `spatiand_xr_v1`, in `protocol/spatiand-xr-v1.xml`. The XML is the
//! specification and this crate is only the generated bindings for it — read the XML, not
//! this file.
//!
//! Kept in its own crate for the same reason the HAL crates are: a protocol is a promise to
//! other people's programs, and a promise is easier to keep when it is not tangled with the
//! code that happens to implement it this month. It also means an application author can
//! depend on this crate for the client side without pulling in a compositor.

#![allow(clippy::all)]

/// Server-side bindings, for the compositor.
#[allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
#[allow(non_upper_case_globals, non_snake_case, unused_imports)]
#[allow(missing_docs, clippy::all)]
pub mod server {
    //! Server-side API, generated from the XML. Structured exactly as
    //! `wayland-protocols` structures its own: the generated code refers to `wayland_server`
    //! and `wayland_backend` by those names, so both have to be in scope here.
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/spatiand-xr-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("protocol/spatiand-xr-v1.xml");
}

/// Client-side bindings, for a program that asks for these things.
///
/// Behind the `client` feature so a compositor that only answers does not also carry a Wayland
/// client. The first user is spatiand itself: a remote window is drawn by spatiand's own
/// in-process client, and when the application behind it becomes the room, that client claims
/// the layer through this protocol exactly as a local application would — so a remote world and
/// a local one are the same thing to everything downstream, refusals and exclusivity included.
#[cfg(feature = "client")]
#[allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
#[allow(non_upper_case_globals, non_snake_case, unused_imports)]
#[allow(missing_docs, clippy::all)]
pub mod client {
    //! Client-side API, generated from the XML the same way as `server`.
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/spatiand-xr-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocol/spatiand-xr-v1.xml");
}

/// The layout of the shared memory the pose channel hands over.
///
/// Here rather than only in the XML because both sides need it to agree exactly, and a
/// comment in a protocol file is not something a compiler checks. A client in another
/// language should read the XML; a client in Rust can use these.
///
/// `repr(C)` and nothing clever: this is a memory format, and the point of it is that it can
/// be mapped and read without a library.
pub mod pose {
    /// Bumped if the layout below ever changes. A client must check it.
    pub const VERSION: u32 = 1;

    /// How many samples of history the ring holds. A power of two so the index wraps with a
    /// mask; enough to interpolate across a few frames, which is all anyone needs.
    pub const SLOTS: usize = 16;

    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Header {
        pub version: u32,
        pub slot_count: u32,
        pub slot_stride: u32,
        pub slots_offset: u32,
        /// Total slots ever written. The newest is `(write_index - 1) & (slot_count - 1)`.
        pub write_index: u64,
        pub reserved: u64,
    }

    /// One eye, in OpenXR's frame and field order — see the XML's coordinates note.
    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Eye {
        /// x, y, z, w.
        pub orientation: [f32; 4],
        /// Metres.
        pub position: [f32; 3],
        pub _pad: f32,
        /// angleLeft, angleRight, angleUp, angleDown, radians, signed. `XrFovf` exactly.
        pub fov: [f32; 4],
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Slot {
        /// Odd while being written. An ordinary seqlock.
        pub seq: u64,
        /// CLOCK_MONOTONIC nanoseconds when the pose was measured.
        pub sample_ns: i64,
        /// CLOCK_MONOTONIC nanoseconds a frame submitted now is expected to be shown.
        pub predicted_ns: i64,
        pub reserved: i64,
        /// x, y, z, w.
        pub head_orientation: [f32; 4],
        /// Metres.
        pub head_position: [f32; 3],
        pub _pad: f32,
        /// Index 0 is the left eye.
        pub eye: [Eye; 2],
    }

    /// Bytes to map: the header followed by the ring.
    pub const fn channel_size() -> usize {
        std::mem::size_of::<Header>() + SLOTS * std::mem::size_of::<Slot>()
    }

    /// The writing end: the shared memory, and the seqlock that lets it be read while it is
    /// being written.
    ///
    /// One writer, any number of readers, nothing that blocks. Both the session, filling it
    /// from the glasses, and a host, filling it from what the session sends, write through
    /// this — so there is one seqlock in the tree, not two that nearly agree. What goes *in*
    /// a slot is the caller's business; this only guarantees a reader never sees half of one.
    #[cfg(target_os = "linux")]
    pub struct Ring {
        memory: *mut u8,
        fd: std::os::fd::OwnedFd,
        written: u64,
    }

    // The pointer is to a mapping this type owns alone; it is written from one thread at a time
    // and never shared as a reference.
    #[cfg(target_os = "linux")]
    unsafe impl Send for Ring {}

    #[cfg(target_os = "linux")]
    impl Ring {
        /// Make the memory and map it.
        ///
        /// Sealed against growing and shrinking: a reader that could shrink this file could
        /// make the writer fault on its own mapping, which would be a client crashing the
        /// compositor by return of post.
        pub fn new(name: &std::ffi::CStr) -> Result<Ring, String> {
            use std::os::fd::FromRawFd;
            let size = channel_size();
            // SAFETY: a libc call with a valid C string and no borrowed state.
            let raw = unsafe {
                libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
            };
            if raw < 0 {
                return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
            }
            // SAFETY: memfd_create returned a fresh descriptor that is now ours.
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
            // SAFETY: the descriptor is ours, sized to what is about to be mapped.
            if unsafe { libc::ftruncate(raw, size as libc::off_t) } < 0 {
                return Err(format!("ftruncate: {}", std::io::Error::last_os_error()));
            }
            // SAFETY: a libc call on a descriptor we own.
            unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW) };
            // SAFETY: mapping a descriptor we own at the length it has just been given.
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
            // SAFETY: a fresh mapping of at least `channel_size()` bytes; Header is repr(C).
            unsafe { std::ptr::write(memory as *mut Header, header) };
            Ok(Ring { memory, fd, written: 0 })
        }

        /// The descriptor the ring lives in. Writable: for handing over a Wayland connection
        /// to a client that maps it read-only, as the protocol asks.
        pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
            use std::os::fd::AsFd;
            self.fd.as_fd()
        }

        /// A second descriptor onto the same memory that **cannot** be written through.
        ///
        /// For handing to a process started by the writer rather than one that asked over
        /// Wayland: a program given the writable one could scribble over the poses every other
        /// program is reading. Reopening through `/proc` is the one way to get a read-only
        /// descriptor onto a memfd that is still mapped writable here.
        pub fn read_only(&self) -> Result<std::os::fd::OwnedFd, String> {
            use std::os::fd::{AsRawFd, FromRawFd};
            let path = std::ffi::CString::new(format!("/proc/self/fd/{}", self.fd.as_raw_fd()))
                .map_err(|e| e.to_string())?;
            // SAFETY: a libc call with a valid C string.
            let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            if raw < 0 {
                return Err(format!("reopening the pose channel read-only: {}", std::io::Error::last_os_error()));
            }
            // SAFETY: open returned a fresh descriptor that is now ours.
            Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) })
        }

        pub fn size(&self) -> u32 {
            channel_size() as u32
        }

        /// Write one sample. `slot.seq` is ignored and set here.
        ///
        /// An ordinary seqlock: the sequence is made odd, the body written, the sequence made
        /// even again, and only then does the write index move. A reader that catches a slot
        /// mid-write sees an odd sequence, or two different ones either side of its read, and
        /// looks again.
        pub fn write(&mut self, mut slot: Slot) {
            use std::sync::atomic::{fence, Ordering};
            let index = (self.written as usize) & (SLOTS - 1);
            // SAFETY: `index` is masked into the ring, which follows the header.
            let at = unsafe { (self.memory.add(std::mem::size_of::<Header>()) as *mut Slot).add(index) };
            let seq = self.written * 2 + 1;
            // SAFETY: `at` is inside our own mapping, and only this thread writes it.
            unsafe { std::ptr::addr_of_mut!((*at).seq).write_volatile(seq) };
            // The odd sequence has to be visible before the body it guards is disturbed.
            fence(Ordering::Release);
            slot.seq = seq;
            // SAFETY: as above. `seq` is overwritten straight after, so writing all of it is fine.
            unsafe {
                std::ptr::write(at, slot);
                fence(Ordering::Release);
                std::ptr::addr_of_mut!((*at).seq).write_volatile(seq + 1);
            }
            self.written += 1;
            fence(Ordering::Release);
            // SAFETY: the header is at offset zero of our mapping.
            unsafe {
                std::ptr::addr_of_mut!((*(self.memory as *mut Header)).write_index).write_volatile(self.written)
            };
        }

        /// The newest complete sample, read the way a client reads it. For tests, and for a
        /// writer that wants to know what it last said.
        pub fn newest(&self) -> Option<Slot> {
            // SAFETY: reading our own mapping exactly as a client reads theirs.
            unsafe { read_newest(self.memory) }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for Ring {
        fn drop(&mut self) {
            // SAFETY: unmapping exactly what `new` mapped.
            unsafe { libc::munmap(self.memory as *mut libc::c_void, channel_size()) };
        }
    }

    /// The reader's half of the seqlock, over a mapping of the channel.
    ///
    /// `None` when nothing has been written yet, or when the writer was in every slot tried —
    /// both of which a client treats the same way: use the pose it already had.
    ///
    /// # Safety
    /// `memory` must point at a mapping of at least [`channel_size`] bytes laid out as above.
    pub unsafe fn read_newest(memory: *const u8) -> Option<Slot> {
        use std::sync::atomic::{fence, Ordering};
        let header = memory as *const Header;
        let written = std::ptr::addr_of!((*header).write_index).read_volatile();
        if written == 0 {
            return None;
        }
        let slots = memory.add((*header).slots_offset as usize) as *const Slot;
        // Newest first; stepping back one is enough if the writer is mid-way through it.
        for back in 1..=2u64 {
            if written < back {
                break;
            }
            let at = slots.add(((written - back) as usize) & (SLOTS - 1));
            let before = std::ptr::addr_of!((*at).seq).read_volatile();
            fence(Ordering::Acquire);
            let body = std::ptr::read_volatile(at);
            fence(Ordering::Acquire);
            let after = std::ptr::addr_of!((*at).seq).read_volatile();
            if before % 2 == 0 && before == after {
                return Some(body);
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_layout_is_what_the_protocol_says_it_is() {
            // The XML documents these offsets to clients written in other languages, and a
            // client reading at the wrong offset gets plausible-looking rubbish rather than an
            // error. So the numbers are checked rather than described.
            // 16 bytes of u32, then two u64 which the compiler aligns to 8.
            assert_eq!(std::mem::size_of::<Header>(), 32);
            assert_eq!(std::mem::size_of::<Slot>(), 160);
            assert_eq!(std::mem::size_of::<Eye>(), 48);
            assert_eq!(std::mem::align_of::<Slot>(), 8);
        }

        #[test]
        fn a_slot_is_field_compatible_with_openxr() {
            // The reason the wire uses OpenXR's frame and field order: an adapter should be a
            // memcpy. XrQuaternionf is four floats x,y,z,w; XrVector3f is three floats;
            // XrFovf is four floats angleLeft, angleRight, angleUp, angleDown. If these ever
            // stop being contiguous in that order, the adapter stops being free.
            let eye = Eye::default();
            let base = &eye as *const Eye as usize;
            assert_eq!(&eye.orientation as *const _ as usize - base, 0);
            assert_eq!(&eye.position as *const _ as usize - base, 16);
            assert_eq!(&eye.fov as *const _ as usize - base, 32);
        }

        #[test]
        fn the_ring_wraps_with_a_mask() {
            assert!(SLOTS.is_power_of_two());
        }

        #[cfg(target_os = "linux")]
        fn sample(n: i64) -> Slot {
            Slot {
                sample_ns: n,
                predicted_ns: n + 1,
                head_orientation: [0.0, 0.0, 0.0, 1.0],
                ..Slot::default()
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn a_written_slot_reads_back_whole() {
            let mut ring = Ring::new(c"test-poses").expect("ring");
            assert!(ring.newest().is_none(), "nothing written yet");
            ring.write(sample(111));
            let newest = ring.newest().expect("written");
            assert_eq!(newest.seq % 2, 0, "a reader would have seen a torn slot");
            assert_eq!(newest.sample_ns, 111);
            assert_eq!(newest.predicted_ns, 112);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn the_ring_wraps_without_losing_the_newest() {
            let mut ring = Ring::new(c"test-poses").expect("ring");
            for n in 0..(SLOTS as i64 * 3) {
                ring.write(sample(n));
            }
            assert_eq!(ring.newest().expect("written").sample_ns, SLOTS as i64 * 3 - 1);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn the_read_only_descriptor_sees_the_poses_and_cannot_change_them() {
            use std::os::fd::AsRawFd;
            let mut ring = Ring::new(c"test-poses").expect("ring");
            ring.write(sample(7));
            let ro = ring.read_only().expect("read-only descriptor");
            let size = channel_size();
            // SAFETY: mapping a descriptor we own; checked against MAP_FAILED below.
            let writable = unsafe {
                libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE,
                           libc::MAP_SHARED, ro.as_raw_fd(), 0)
            };
            assert_eq!(writable, libc::MAP_FAILED, "a launched program could overwrite the poses");
            // SAFETY: as above.
            let readable = unsafe {
                libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, ro.as_raw_fd(), 0)
            };
            assert_ne!(readable, libc::MAP_FAILED);
            // SAFETY: a mapping of `size` bytes of this channel.
            let seen = unsafe { read_newest(readable as *const u8) }.expect("the same memory");
            assert_eq!(seen.sample_ns, 7);
            ring.write(sample(8));
            // SAFETY: as above -- and it is live, not a copy.
            let seen = unsafe { read_newest(readable as *const u8) }.expect("still the same memory");
            assert_eq!(seen.sample_ns, 8);
            // SAFETY: unmapping what was mapped above.
            unsafe { libc::munmap(readable, size) };
        }
    }
}
