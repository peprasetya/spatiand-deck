//! The wire between a Spatiand session and a remote application host.
//!
//! One crate, linked by both ends, so that a message can only be written once. It holds the
//! catalogue of what a host can run, the control conversation, and the framing that carries
//! encoded pictures and sound. It touches no GPU, no sound server and no network — which is
//! why all of it is tested on a laptop with none of those.
//!
//! ## What this is not
//!
//! It is not a remote desktop. A host serves **applications**, one window each, and the
//! session draws each of them where the wearer put it. There is no remote screen, no remote
//! cursor to chase, and nothing that assumes the two ends agree about what a display is.
//!
//! ## The shape of a session
//!
//! ```text
//! client                                host
//!   |-- Hello (what I can decode) ------->|
//!   |<------------- Welcome + Catalogue --|
//!   |-- Launch(app) --------------------->|      the host starts it, or adopts the
//!   |<----------------- WindowOpened -----|      one already running from last time
//!   |<====== pictures, sound =============|
//!   |-- input, viewport, bandwidth ------>|
//! ```
//!
//! A disconnection ends none of that on the host's side. The application keeps running, and a
//! later `Hello` from the same client is a reattach: the host answers with the windows it
//! still has, and every stream starts again with a keyframe.
//!
//! ## Two carriers, on purpose
//!
//! Control, keys and the clipboard are **ordered and reliable**; pictures, sound, pointer
//! motion and head poses are **not**. A retransmitted head pose is worse than a lost one,
//! because it is already wrong by the time it arrives. [`control`] is the first kind and
//! [`video`] the second; which transport provides them is the caller's business.

pub mod audio;
pub mod catalog;
pub mod control;
pub mod link;
pub mod transport;
pub mod video;

pub use catalog::{App, AppKind, AudioMode, Catalog, Detach, Eyes, PadProfile};
pub use control::{Bandwidth, ClientMessage, HostMessage, Input, WindowId, WindowInfo};
pub use transport::{pairing_code, Fingerprint, Gate, Identity, Trust};
pub use video::{Codec, Packet, Reassembler};

/// The protocol version both ends must agree on.
///
/// There is no negotiation below this: a mismatch is refused with a message saying both
/// numbers, because "it connects and then nothing appears" is the worst way to learn that two
/// machines are running different builds. Within one version, new fields are added in ways
/// postcard tolerates — at the end of a struct, or as a new enum variant that older peers
/// report as unknown rather than treating as something else.
/// 2: catalogue entries carry the icon the owner chose, beside the rendered one.
pub const VERSION: u32 = 2;

/// Encode a message for the wire.
pub fn to_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(value)
}

/// Decode a message from the wire.
pub fn from_bytes<'a, T: serde::Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}
