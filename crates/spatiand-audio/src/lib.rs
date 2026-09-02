//! Sound that stays with the window it came out of.
//!
//! A window is somewhere. Its sound should be there too — and it should stay there when the
//! head turns, because the window does. That is the whole idea, and everything in this crate
//! is in service of it.
//!
//! The crate is deliberately in two halves. [`stage`] is arithmetic: it answers "given a
//! window over there and a head pointing this way, which direction is each of this stream's
//! channels coming from?" It touches no hardware, no audio server and no clock, and it is
//! where the design lives. What renders those directions to a pair of ears, and what carries
//! the samples, sit behind it.

pub mod stage;

pub use stage::{Channel, Layout, Speaker, Stage};
