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
    }
}
