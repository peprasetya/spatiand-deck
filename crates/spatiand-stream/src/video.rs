//! Getting an encoded picture across a link that loses things.
//!
//! A frame is bigger than a datagram, so it goes in pieces, and pieces go missing. What
//! matters is what happens next, and the answer here is blunt on purpose: **a frame with a
//! hole in it is thrown away**, and the decoder is never shown a half-picture. Feeding a
//! video decoder a frame that is missing a slice does not cost one frame, it corrupts
//! everything that refers to it afterwards, which is the smear that game streaming is famous
//! for.
//!
//! So the loss is answered rather than hidden: the reassembler says which frame it gave up
//! on, and the caller asks the host for a keyframe. On a link where that happens often,
//! parity packets (later) will repair the common case of one lost piece without a round trip.
//!
//! ## The header is small and fixed
//!
//! Every packet carries the same 24 bytes, hand-packed rather than serialised, because this is
//! the one message on the wire that is sent thousands of times a second and the layout has to
//! be obvious from both ends:
//!
//! ```text
//! 0      2      4          8         12        14      16               24
//! +------+------+----------+---------+---------+-------+----------------+
//! |window| flags| frame    | viewport| parts   | part  | captured µs    |  payload…
//! | u16  | u16  | seq u32  | seq u32 | u16     | u16   | u64            |
//! +------+------+----------+---------+---------+-------+----------------+
//! ```
//!
//! `viewport seq` is what makes late frames usable: it says which head pose the host was
//! drawing for, so the session can turn the picture by the difference rather than showing it
//! where it no longer belongs.

use serde::{Deserialize, Serialize};

/// What the pictures are encoded as.
///
/// Both ends of this project are AMD, and everything here goes through VAAPI, but the codec is
/// still negotiated: the Deck decodes all three in hardware, while what a host can *encode*
/// varies by machine and by library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
}

/// This packet starts a frame that depends on nothing before it.
pub const FLAG_KEYFRAME: u16 = 1 << 0;
/// The last piece of its frame. Carried in the flags as well as in `parts` so that a frame
/// whose final piece arrives first is still recognisable.
pub const FLAG_LAST: u16 = 1 << 1;

/// The fixed part of every video packet.
pub const HEADER_LEN: usize = 24;

/// One piece of one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Packet<'a> {
    pub window: u16,
    pub flags: u16,
    pub frame: u32,
    /// Which [`crate::control::Viewport`] the host drew this for. Zero when the head had
    /// nothing to do with it, as for an ordinary window.
    pub viewport: u32,
    pub parts: u16,
    pub part: u16,
    /// When the host had the finished picture, on its own clock, in microseconds.
    pub captured_us: u64,
    pub payload: &'a [u8],
}

impl<'a> Packet<'a> {
    pub fn keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    /// Write the packet into `out`, header first.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.clear();
        out.reserve(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.window.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.frame.to_le_bytes());
        out.extend_from_slice(&self.viewport.to_le_bytes());
        out.extend_from_slice(&self.parts.to_le_bytes());
        out.extend_from_slice(&self.part.to_le_bytes());
        out.extend_from_slice(&self.captured_us.to_le_bytes());
        out.extend_from_slice(self.payload);
    }

    /// Read a packet, or `None` if it is too short or says something impossible.
    ///
    /// Nothing here trusts the sender: a packet claiming to be part 9 of 3 is dropped rather
    /// than indexed with.
    pub fn read(bytes: &'a [u8]) -> Option<Packet<'a>> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let parts = u16_at(12);
        let part = u16_at(14);
        if parts == 0 || part >= parts {
            return None;
        }
        Some(Packet {
            window: u16_at(0),
            flags: u16_at(2),
            frame: u32_at(4),
            viewport: u32_at(8),
            parts,
            part,
            captured_us: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            payload: &bytes[HEADER_LEN..],
        })
    }
}

/// Cut a frame into packets that fit the link.
pub fn split(payload: &[u8], mtu: usize) -> impl Iterator<Item = (u16, u16, &[u8])> {
    let room = mtu.saturating_sub(HEADER_LEN).max(1);
    let parts = payload.len().div_ceil(room).max(1) as u16;
    payload
        .chunks(room)
        .enumerate()
        .map(move |(i, chunk)| (parts, i as u16, chunk))
}

/// A frame put back together, ready for the decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct Assembled {
    pub window: u16,
    pub frame: u32,
    pub viewport: u32,
    pub captured_us: u64,
    pub keyframe: bool,
    pub bytes: Vec<u8>,
}

/// What came of a packet.
#[derive(Debug, Clone, PartialEq)]
pub enum Arrival {
    /// Still waiting for the rest.
    Partial,
    /// A whole frame.
    Frame(Assembled),
    /// A frame was abandoned because a newer one started, and a piece of it never came. The
    /// caller should ask for a keyframe. Carries the frame number that was lost.
    Lost(u32),
    /// The packet was for something already finished or already given up on.
    Stale,
}

/// Puts one window's frames back together.
///
/// It holds **one** frame at a time. Anything older than what is being assembled is stale, and
/// starting a newer frame abandons an unfinished older one — a 72 Hz display has no use for a
/// frame that is already two frames behind, and a reassembler that waits for it adds latency
/// to every frame after it.
#[derive(Debug, Default)]
pub struct Reassembler {
    frame: Option<Pending>,
    /// The last frame handed out, so its stragglers are recognised as stale rather than
    /// starting the frame over again.
    finished: Option<u32>,
}

#[derive(Debug)]
struct Pending {
    frame: u32,
    viewport: u32,
    captured_us: u64,
    keyframe: bool,
    parts: u16,
    have: Vec<Option<Vec<u8>>>,
    count: u16,
}

impl Reassembler {
    pub fn new() -> Reassembler {
        Reassembler::default()
    }

    pub fn accept(&mut self, packet: &Packet<'_>) -> Arrival {
        let mut lost = None;
        match &self.frame {
            Some(p) if p.frame == packet.frame => {}
            Some(p) if p.frame > packet.frame => return Arrival::Stale,
            Some(p) => {
                lost = Some(p.frame);
                self.frame = None;
            }
            None => {}
        }
        if self.frame.is_none() {
            if self.finished.is_some_and(|f| f >= packet.frame) {
                return Arrival::Stale;
            }
            self.frame = Some(Pending {
                frame: packet.frame,
                viewport: packet.viewport,
                captured_us: packet.captured_us,
                keyframe: packet.keyframe(),
                parts: packet.parts,
                have: (0..packet.parts).map(|_| None).collect(),
                count: 0,
            });
        }
        let pending = self.frame.as_mut().expect("just set");
        // A sender that changes its mind about how many pieces a frame has is not one to
        // reassemble from; treat the frame as lost rather than guessing which count is right.
        if pending.parts != packet.parts || packet.part >= pending.parts {
            let bad = pending.frame;
            self.frame = None;
            self.finished = Some(bad);
            return Arrival::Lost(bad);
        }
        // The keyframe flag rides on every piece of a keyframe, but believing only the first
        // one to arrive would make it depend on packet order.
        pending.keyframe |= packet.keyframe();
        let slot = &mut pending.have[packet.part as usize];
        if slot.is_none() {
            *slot = Some(packet.payload.to_vec());
            pending.count += 1;
        }
        if pending.count == pending.parts {
            let done = self.frame.take().expect("just checked");
            self.finished = Some(done.frame);
            let mut bytes = Vec::with_capacity(done.have.iter().flatten().map(Vec::len).sum());
            for piece in done.have.into_iter().flatten() {
                bytes.extend_from_slice(&piece);
            }
            return Arrival::Frame(Assembled {
                window: packet.window,
                frame: done.frame,
                viewport: done.viewport,
                captured_us: done.captured_us,
                keyframe: done.keyframe,
                bytes,
            });
        }
        match lost {
            Some(frame) => Arrival::Lost(frame),
            None => Arrival::Partial,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(frame: u32, payload: &[u8], mtu: usize, flags: u16) -> Vec<Vec<u8>> {
        split(payload, mtu)
            .map(|(parts, part, chunk)| {
                let mut out = Vec::new();
                Packet {
                    window: 7,
                    flags: flags | if part + 1 == parts { FLAG_LAST } else { 0 },
                    frame,
                    viewport: 42,
                    parts,
                    part,
                    captured_us: 1_000,
                    payload: chunk,
                }
                .write(&mut out);
                out
            })
            .collect()
    }

    #[test]
    fn a_frame_in_pieces_comes_back_whole() {
        let payload: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let wire = packets(1, &payload, 1200, FLAG_KEYFRAME);
        assert!(wire.len() > 4, "should have been split");
        let mut r = Reassembler::new();
        let mut got = None;
        for bytes in &wire {
            let p = Packet::read(bytes).expect("reads back");
            if let Arrival::Frame(f) = r.accept(&p) {
                got = Some(f);
            }
        }
        let f = got.expect("assembled");
        assert_eq!(f.bytes, payload);
        assert!(f.keyframe);
        assert_eq!(f.viewport, 42);
    }

    #[test]
    fn the_order_pieces_arrive_in_does_not_matter() {
        let payload: Vec<u8> = (0..3000u32).map(|i| (i * 7) as u8).collect();
        let mut wire = packets(2, &payload, 900, 0);
        wire.reverse();
        let mut r = Reassembler::new();
        let mut got = None;
        for bytes in &wire {
            if let Arrival::Frame(f) = r.accept(&Packet::read(bytes).unwrap()) {
                got = Some(f);
            }
        }
        assert_eq!(got.expect("assembled").bytes, payload);
    }

    #[test]
    fn a_lost_piece_loses_its_frame_and_says_so() {
        let payload: Vec<u8> = vec![9; 4000];
        let first = packets(1, &payload, 1000, 0);
        let second = packets(2, &payload, 1000, FLAG_KEYFRAME);
        let mut r = Reassembler::new();
        // Everything but the second piece of frame 1.
        for (i, bytes) in first.iter().enumerate() {
            if i == 1 {
                continue;
            }
            assert_eq!(r.accept(&Packet::read(bytes).unwrap()), Arrival::Partial);
        }
        // Frame 2 starting is what reveals frame 1 will never be finished.
        assert_eq!(
            r.accept(&Packet::read(&second[0]).unwrap()),
            Arrival::Lost(1)
        );
        let mut got = None;
        for bytes in &second[1..] {
            if let Arrival::Frame(f) = r.accept(&Packet::read(bytes).unwrap()) {
                got = Some(f);
            }
        }
        assert!(got.is_some(), "the newer frame still arrives");
    }

    #[test]
    fn a_straggler_from_a_finished_frame_is_ignored() {
        let payload = vec![1u8; 2000];
        let wire = packets(5, &payload, 1000, 0);
        let mut r = Reassembler::new();
        for bytes in &wire {
            r.accept(&Packet::read(bytes).unwrap());
        }
        assert_eq!(
            r.accept(&Packet::read(&wire[0]).unwrap()),
            Arrival::Stale,
            "a duplicate must not start the frame again"
        );
    }

    #[test]
    fn a_packet_that_makes_no_sense_is_refused() {
        assert!(Packet::read(&[0u8; HEADER_LEN - 1]).is_none());
        let mut out = Vec::new();
        Packet {
            window: 1,
            flags: 0,
            frame: 1,
            viewport: 0,
            parts: 3,
            part: 9, // past the end
            captured_us: 0,
            payload: &[1, 2, 3],
        }
        .write(&mut out);
        assert!(Packet::read(&out).is_none());
    }

    #[test]
    fn a_single_small_frame_is_one_packet() {
        let wire = packets(1, &[1, 2, 3], 1200, FLAG_KEYFRAME);
        assert_eq!(wire.len(), 1);
        let p = Packet::read(&wire[0]).unwrap();
        assert_eq!(p.parts, 1);
        assert!(p.flags & FLAG_LAST != 0);
    }
}
