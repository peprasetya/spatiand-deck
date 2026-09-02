//! Sound that stays with the window it came out of.
//!
//! A window is somewhere. Its sound should be there too — and it should stay there when the
//! head turns, because the window does. That is the whole idea, and everything in this crate
//! is in service of it.
//!
//! The pipeline, in the order a sample travels:
//!
//! 1. [`stage`] — arithmetic. Given a window over there and a head pointing this way, which
//!    direction is each channel of this stream coming from? No hardware, no clock; this is
//!    where the design lives.
//! 2. [`hrtf`] or [`panner`] — a direction becomes a short filter for each ear. Measured from
//!    a real head where a dataset is available, worked out from the geometry of having a head
//!    where it is not.
//! 3. [`render`] — the filters are applied to the samples and everything is summed to two
//!    ears, with the plain stereo fold mixed back in to taste.
//!
//! Only step 3 knows about audio buffers, and only step 2 knows about anything outside the
//! process. Step 1 is pure and is tested as such.

pub mod ears;
pub mod hrtf;
pub mod panner;
pub mod render;
pub mod ring;
#[cfg(feature = "server")]
pub mod server;
pub mod stage;

pub use ears::Ears;
pub use panner::Panner;
pub use render::{Binaural, Directness};
pub use stage::{Channel, Layout, Speaker, Stage};

use glam::DVec3;

/// Turning a direction into what each ear hears.
///
/// Two implementations, and the whole point of the trait is that nothing downstream can tell
/// them apart: [`hrtf::Hrtf`] looks the direction up in measurements taken around a real head,
/// and [`Panner`] works out what it can from the geometry. The first sounds convincingly
/// behind you and the second does not, and that is the only difference that reaches a listener.
pub trait Spatialise: Send {
    /// Write the filter pair for a sound arriving from `direction` into `out`, in the
    /// listener's own frame: +X ahead, +Y left, +Z up.
    ///
    /// Filled in place rather than returned, and that is not a style preference. A head that
    /// is turning asks for a new filter every few milliseconds, on the audio thread, where an
    /// allocator is the one call that can occasionally take longer than the whole deadline.
    /// Reusing the buffer means the allocation happens once, when the voice is created.
    fn ears_into(&self, direction: DVec3, out: &mut Ears);

    /// The longest filter this will ever return.
    ///
    /// Asked once, up front, so the renderer can size its delay lines and never grow them
    /// afterwards.
    fn taps(&self) -> usize;

    /// The filter pair, allocated fresh. For tests and for setting up; not for the audio
    /// thread, which should own its buffers and call [`Spatialise::ears_into`].
    fn ears(&self, direction: DVec3) -> Ears {
        let mut out = Ears::silent(self.taps());
        self.ears_into(direction, &mut out);
        out
    }
}
