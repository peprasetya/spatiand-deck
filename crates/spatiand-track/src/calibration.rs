//! Working out how the IMU's axes map to head motion.
//!
//! The glasses do not publish their axis convention, and guessing produces exactly the
//! symptom this exists for: nodding rotates the image instead of panning it, because
//! physical pitch lands on the roll axis.
//!
//! Ported from HoloFrame's `AxisCalibration.swift`, but split differently. There, the
//! sequence, its timing and its on-screen prompts were one function. Here the measurement is
//! a pure accumulator and the *driver* owns timing and presentation, so the same logic backs
//! both the CLI probe used during bring-up and the in-world calibration the shell will show
//! on the glasses. It also makes the whole thing unit-testable, which the original was not.
//!
//! Each phase asks for one unambiguous motion and integrates **signed** rate over the
//! window: a sustained turn accumulates while tremor cancels out, giving both the axis and
//! the direction convention.

use glam::DVec3;
use spatiand_hmd::ImuSample;

use crate::axis::AxisMap;

/// The three motions [`AxisMap`] is defined against, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    TurnLeft,
    LookDown,
    TiltLeft,
}

impl Phase {
    pub const ALL: [Phase; 3] = [Phase::TurnLeft, Phase::LookDown, Phase::TiltLeft];

    pub fn heading(self) -> &'static str {
        match self {
            Phase::TurnLeft => "Turn LEFT",
            Phase::LookDown => "Look DOWN",
            Phase::TiltLeft => "Tilt LEFT",
        }
    }

    pub fn instruction(self) -> &'static str {
        match self {
            Phase::TurnLeft => "Slowly turn your head to the left, like saying \"no\", and hold.",
            Phase::LookDown => "Slowly nod your head down, chin toward chest, and hold.",
            Phase::TiltLeft => "Slowly tilt your head to the left, left ear toward left shoulder.",
        }
    }
}

/// Degrees a motion must accumulate before it is believed. Below this the dominant axis is
/// as likely to be noise as intent.
pub const MINIMUM_DEGREES: f64 = 15.0;

/// Integrates signed angular rate for one phase.
#[derive(Debug, Default, Clone)]
pub struct PhaseCollector {
    integral: DVec3,
    last_timestamp: u64,
    samples: u32,
}

impl PhaseCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one **raw, un-remapped** sample. Calibration must see the sensor's own axes; the
    /// canonical frame is what it is trying to discover.
    pub fn feed(&mut self, sample: &ImuSample) {
        // Use the device clock rather than a fixed 1 ms, so a dropped report does not quietly
        // shrink the integral and push a good motion under the threshold.
        let mut dt = 0.001;
        if self.last_timestamp != 0 && sample.timestamp_ns > self.last_timestamp {
            let candidate = (sample.timestamp_ns - self.last_timestamp) as f64 * 1e-9;
            if candidate > 0.0 && candidate <= 0.1 {
                dt = candidate;
            }
        }
        self.last_timestamp = sample.timestamp_ns;
        self.integral += sample.gyro * dt;
        self.samples += 1;
    }

    pub fn integral(&self) -> DVec3 {
        self.integral
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// The dominant sensor axis and how far the motion went along it, in degrees.
    pub fn dominant(&self) -> (usize, f64) {
        let a = self.integral.to_array();
        let mut best = 0;
        for i in 1..3 {
            if a[i].abs() > a[best].abs() {
                best = i;
            }
        }
        (best, a[best])
    }

    /// `None` if the motion was too small to trust.
    pub fn accepted(&self) -> Option<(usize, f64)> {
        let (axis, value) = self.dominant();
        (value.abs() >= MINIMUM_DEGREES).then_some((axis, value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CalibrationError {
    #[error("two motions mapped to the same sensor axis — keep each movement separate")]
    RepeatedAxis,
    #[error("the movements came out mirrored — one of them combined two axes")]
    Mirrored,
    #[error("not enough phases measured")]
    Incomplete,
}

/// Turn three measured phases into a map.
///
/// The order must match [`Phase::ALL`]: turn left, look down, tilt left.
pub fn build(measured: &[(usize, f64)]) -> Result<AxisMap, CalibrationError> {
    if measured.len() != 3 {
        return Err(CalibrationError::Incomplete);
    }
    // The sign that makes *this measured motion* read positive. Converting to the canonical
    // frame is AxisMap::apply's job, not this one's.
    let sign = |v: f64| if v < 0.0 { -1.0 } else { 1.0 };
    let map = AxisMap {
        version: crate::axis::CURRENT_VERSION,
        yaw_axis: measured[0].0,
        yaw_sign: sign(measured[0].1),
        pitch_axis: measured[1].0,
        pitch_sign: sign(measured[1].1),
        roll_axis: measured[2].0,
        roll_sign: sign(measured[2].1),
    };

    if !map.is_usable() {
        return Err(CalibrationError::RepeatedAxis);
    }
    // Two right-handed frames cannot differ by a mirror, so a negative determinant means a
    // measured sign is wrong — almost always a motion that combined two axes. Storing it
    // anyway makes the filter fight itself in ways that look like drift, so it is rejected
    // rather than saved with a warning.
    if !map.is_right_handed() {
        return Err(CalibrationError::Mirrored);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t_ns: u64, gyro: DVec3) -> ImuSample {
        ImuSample {
            timestamp_ns: t_ns,
            gyro,
            accel: DVec3::new(0.0, 0.0, 1.0),
            mag: DVec3::ZERO,
            temperature_c: None,
        }
    }

    /// Integrate `rate` for `seconds` at 1 kHz.
    fn collect(rate: DVec3, seconds: f64) -> PhaseCollector {
        let mut c = PhaseCollector::new();
        let n = (seconds * 1000.0) as u64;
        for i in 0..n {
            c.feed(&sample(i * 1_000_000, rate));
        }
        c
    }

    #[test]
    fn a_sustained_turn_is_measured_on_the_right_axis() {
        let c = collect(DVec3::new(0.0, 0.0, 20.0), 3.0); // 20 deg/s about Z for 3 s
        let (axis, value) = c.dominant();
        assert_eq!(axis, 2);
        assert!((value - 60.0).abs() < 1.0, "expected ~60 deg, got {value}");
        assert!(c.accepted().is_some());
    }

    #[test]
    fn tremor_cancels_instead_of_accumulating() {
        // The reason this integrates signed rate rather than magnitude: shaking is motion,
        // but it is not a *direction*, and must not be mistaken for one.
        let mut c = PhaseCollector::new();
        for i in 0..3000u64 {
            let s = if i % 2 == 0 { 200.0 } else { -200.0 };
            c.feed(&sample(i * 1_000_000, DVec3::new(0.0, 0.0, s)));
        }
        assert!(
            c.accepted().is_none(),
            "shaking should not read as a deliberate turn, got {:?}",
            c.dominant()
        );
    }

    #[test]
    fn a_motion_that_is_too_small_is_refused() {
        let c = collect(DVec3::new(0.0, 0.0, 2.0), 1.0); // only 2 degrees
        assert!(c.accepted().is_none());
    }

    #[test]
    fn negative_motion_yields_a_negative_sign() {
        let c = collect(DVec3::new(0.0, 0.0, -20.0), 3.0);
        let (axis, value) = c.dominant();
        assert_eq!(axis, 2);
        assert!(value < 0.0);
    }

    #[test]
    fn builds_the_identity_map_from_ideal_motions() {
        // Sensor axes that happen to match the canonical assumption: yaw on Z positive,
        // pitch on Y positive, roll on X negative.
        let map = build(&[(2, 60.0), (1, 45.0), (0, -30.0)]).expect("should build");
        assert_eq!(map, AxisMap::IDENTITY);
    }

    #[test]
    fn rejects_a_mirrored_result_rather_than_saving_it() {
        // Flip one sign relative to identity: physically impossible between two right-handed
        // frames, so it means a measurement was contaminated.
        let err = build(&[(2, -60.0), (1, 45.0), (0, -30.0)]).unwrap_err();
        assert_eq!(err, CalibrationError::Mirrored);
    }

    #[test]
    fn rejects_two_motions_landing_on_one_axis() {
        let err = build(&[(2, 60.0), (2, 45.0), (0, -30.0)]).unwrap_err();
        assert_eq!(err, CalibrationError::RepeatedAxis);
    }

    #[test]
    fn a_swapped_sensor_frame_round_trips_through_apply() {
        // The real point of calibration. Suppose the sensor has yaw on X, pitch on Z and
        // roll on Y — nothing like the identity guess. The built map must still turn a
        // physical "turn left" into canonical +Z.
        let map = build(&[(0, 60.0), (2, 45.0), (1, -30.0)]).expect("should build");
        assert!(map.is_right_handed());
        // A pure sensor-X rotation is the measured "turn left", so it must land on +Z.
        let canonical = map.apply(DVec3::new(1.0, 0.0, 0.0));
        assert_eq!(canonical, DVec3::new(0.0, 0.0, 1.0));
    }

    #[test]
    fn dropped_reports_do_not_shrink_the_integral() {
        // Using the device clock rather than assuming 1 ms matters: at 986 Hz with occasional
        // gaps, a fixed dt would under-integrate and push good motions below the threshold.
        let mut c = PhaseCollector::new();
        for i in 0..1000u64 {
            c.feed(&sample(i * 2_000_000, DVec3::new(0.0, 0.0, 20.0))); // 500 Hz
        }
        let (_, value) = c.dominant();
        assert!(
            (value - 40.0).abs() < 1.0,
            "2 s at 20 deg/s should read ~40 deg regardless of rate, got {value}"
        );
    }
}
