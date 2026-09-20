//! Decoding, on the headset's side of the link.
//!
//! One decoder per remote window, turning the packets that arrive into pictures on the GPU. The
//! pictures never touch the CPU: what comes out is a dmabuf, which is what a Wayland surface
//! takes, so a remote window is shown exactly as a local application's window is.
//!
//! ## Built against headers that are not installed
//!
//! The Deck has ffmpeg 7.1's libraries and no headers at all, and nothing is installed on it to
//! fix that. So the headers are vendored, the libraries are the system's own, and the version
//! is pinned to what SteamOS ships. The same trick was needed for libva, and `docs/remote.md`
//! has the incantation.
//!
//! The first attempt used `cros-codecs` over libva — pure Rust, no headers needed at all — and
//! it cannot run here: it opens a decoder by creating a 16x16 probe context, and this GPU
//! refuses any video context smaller than 64x64. That was measured for HEVC and H.264 both
//! before giving up on it.
//!
//! ## What a decoder is fed
//!
//! Whole frames, reassembled by [`spatiand_stream::video`]. A frame that lost a piece is never
//! passed in: a decoder shown a frame with a hole in it does not lose one picture, it produces
//! wreckage until the next keyframe, and that wreckage is the smear people recognise as bad
//! game streaming.

pub mod convert;
pub mod decode;
pub mod export;
pub mod split;
pub mod voice;

pub use convert::{Converted, Converter};
pub use decode::{Decoder, Picture, VideoError};
pub use split::Units;
