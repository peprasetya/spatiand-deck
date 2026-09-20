//! An application's sound, on a stream of its own.
//!
//! Each application that makes a sound gets one unidirectional QUIC stream from the host,
//! opened the first time it plays after a session attaches. The stream starts with a small
//! header saying whose sound it is and in what shape, and after that it is nothing but samples:
//! signed 16-bit little-endian, interleaved, at [`RATE`].
//!
//! **Raw, not compressed.** Stereo at 48 kHz is 1.5 Mbit/s — a tenth of what a moving picture
//! costs — and leaving it raw keeps a codec library off both ends and adds no delay of its
//! own. Silence is not sent at all, so a quiet app costs nothing.
//!
//! **A stream, not datagrams.** A lost piece of a picture costs a frame; a lost piece of sound
//! is a click, every time. Retransmission on a link with a 10 ms round trip is cheaper than any
//! concealment, and the receiving end keeps its own delay bounded by throwing away what has
//! fallen too far behind.

use serde::{Deserialize, Serialize};

/// Samples per second, per channel.
pub const RATE: u32 = 48_000;
/// Stereo, for now. Wider sound is a matter of the header, not the stream.
pub const CHANNELS: u16 = 2;
/// Bytes of one frame (one sample for every channel).
pub const FRAME_BYTES: usize = CHANNELS as usize * 2;

/// What every sound stream starts with, before its header, so a stream of anything else is
/// recognised and left alone.
pub const MAGIC: [u8; 4] = *b"SPau";

/// Whose sound this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioHeader {
    /// The catalogue id of the application.
    pub app: String,
    pub rate: u32,
    pub channels: u16,
}

impl AudioHeader {
    /// Magic, then the header's length as a little-endian u32, then the header.
    pub fn encode(&self) -> Vec<u8> {
        let body = crate::to_bytes(self).expect("a header always serialises");
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// The header's length, from the eight bytes after the magic check — `None` if these are
    /// not the start of a sound stream.
    pub fn length(first: &[u8; 8]) -> Option<usize> {
        if first[..4] != MAGIC {
            return None;
        }
        let length = u32::from_le_bytes(first[4..8].try_into().ok()?) as usize;
        (length <= 4096).then_some(length)
    }

    pub fn decode(body: &[u8]) -> Option<AudioHeader> {
        crate::from_bytes(body).ok()
    }
}

/// Whether a stretch of samples is all silence, and so not worth sending.
pub fn is_silent(pcm: &[u8]) -> bool {
    pcm.iter().all(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_round_trips() {
        let header = AudioHeader {
            app: "chrome".into(),
            rate: RATE,
            channels: CHANNELS,
        };
        let bytes = header.encode();
        let length = AudioHeader::length(bytes[..8].try_into().unwrap()).unwrap();
        assert_eq!(AudioHeader::decode(&bytes[8..8 + length]), Some(header));
    }

    #[test]
    fn anything_else_is_not_a_sound_stream() {
        assert_eq!(AudioHeader::length(b"\x10\0\0\0abcd"), None);
    }
}
