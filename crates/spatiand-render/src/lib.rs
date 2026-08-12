//! Rendering for the spatial world.
//!
//! Currently the geometry only — stereo cameras and the side-by-side split. The GL backend
//! lands next; keeping the maths in a dependency-free module means it can be tested on any
//! machine, with no GPU, no display and no headset, which is where most of the subtle bugs
//! live anyway.

pub mod camera;
pub mod ray;
pub mod sky;
pub mod text;

pub use camera::{eye_for, eyes_for, sbs_viewport, Eye, EyeSide, StereoConfig};
pub use ray::{intersect_quad, pick, Hit, PointerConfig, Quad, Ray};
pub use sky::{Sky, SkyEye, SkyProjection, SkySource, SkyStereo};
pub use text::{TextImage, TextRenderer};
