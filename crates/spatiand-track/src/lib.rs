//! Head tracking — raw IMU samples in, a world-frame orientation out.
//!
//! Ported from HoloFrame, whose tuning represents real measurement time on this exact
//! hardware. See `tracker.rs` for what each constant defends against.
//!
//! Nothing here knows what a headset is: it takes [`spatiand_hmd::ImuSample`] and produces a
//! quaternion. That keeps it testable without hardware, which is the whole reason the unit
//! tests can assert things like "yaw must not drift more than a degree per minute at rest".

pub mod axis;
pub mod calibration;
pub mod config;
pub mod smoothing;
pub mod tracker;

pub use axis::{AxisMap, CURRENT_VERSION as AXIS_MAP_VERSION};
pub use calibration::{Phase, PhaseCollector};
pub use smoothing::{OneEuroFilter, PoseSmoother};
pub use tracker::{Euler, HeadTracker, MagneticStatus, TrackerConfig};

/// How far ahead to predict, in seconds. One frame at 72 Hz — the refresh the glasses run at
/// in stereo mode.
pub const DEFAULT_PREDICTION_SECONDS: f64 = 1.0 / 72.0;

/// Ceiling on that extrapolation. Overshoot on a fast flick reads far worse than a little lag.
pub const DEFAULT_PREDICTION_MAX_DEGREES: f64 = 8.0;
