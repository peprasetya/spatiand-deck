//! How the IMU's raw axes map onto head motion.
//!
//! The glasses do not publish their axis convention, so it is measured once and stored.
//! Ported from HoloFrame's `AxisMap.swift`, including the reasoning, because getting this
//! wrong produces symptoms that look like filter bugs.
//!
//! Canonical head frame — **right-handed**, which matters more than it looks:
//!
//! ```text
//!     +X = forward (nose)
//!     +Y = left
//!     +Z = up
//! ```
//!
//! `X × Y = Z`, so this is a proper rotation frame. An earlier HoloFrame version used
//! X forward / Y **right** / Z up, which is left-handed; combined with a measured axis swap
//! that produced a mapping with determinant −1 — a mirror rather than a rotation. Gyro
//! integration and gravity correction then disagree about handedness and fight each other,
//! which shows up as pitch creeping to one end and roll continuing past where you stopped.
//!
//! Under the right-hand rule in this frame:
//! rotation about +Z is turning **left**, about +Y is pitching **down**, about +X is rolling
//! **right**.

use glam::DVec3;
use serde::{Deserialize, Serialize};

/// Bumped when the stored meaning changes, so an old file is discarded rather than silently
/// misinterpreted. Version 1 folded signs into a left-handed convention.
pub const CURRENT_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AxisMap {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Sensor axis (0=X, 1=Y, 2=Z) and the sign making **turning left** read positive.
    pub yaw_axis: usize,
    pub yaw_sign: f64,
    /// ...making **nodding down** read positive.
    pub pitch_axis: usize,
    pub pitch_sign: f64,
    /// ...making **tilting left** read positive.
    pub roll_axis: usize,
    pub roll_sign: f64,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

impl Default for AxisMap {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl AxisMap {
    pub const IDENTITY: Self = Self {
        version: CURRENT_VERSION,
        yaw_axis: 2,
        yaw_sign: 1.0,
        pitch_axis: 1,
        pitch_sign: 1.0,
        roll_axis: 0,
        roll_sign: -1.0,
    };

    /// Reorder a raw sensor vector into the canonical head frame.
    ///
    /// Applied to the gyro **and** the accelerometer — gravity correction has to live in the
    /// same frame as the rates, or the filter fights itself.
    pub fn apply(&self, v: DVec3) -> DVec3 {
        let a = v.to_array();
        DVec3::new(
            // +X is roll RIGHT, but calibration measured roll LEFT, hence the negation.
            -self.roll_sign * a[self.roll_axis],
            self.pitch_sign * a[self.pitch_axis], // +Y is pitch DOWN, measured DOWN
            self.yaw_sign * a[self.yaw_axis],     // +Z is yaw LEFT, measured LEFT
        )
    }

    /// Exchange the pitch and roll axes, keeping the frame right-handed.
    ///
    /// The one calibration mistake that is both easy to make and invisible to the measurement:
    /// nodding and tilting are adjacent motions, and if the wearer performs one when asked for
    /// the other, the two axes are recorded swapped. The resulting map is a perfectly valid
    /// rotation with determinant +1, so nothing downstream can tell it is wrong -- it just
    /// makes looking down roll the world and leaning pitch it.
    ///
    /// Note the sign flip. Exchanging two axes of a right-handed frame mirrors it, so one
    /// sense must reverse to stay a rotation; without that this returns a map with determinant
    /// −1, where gyro integration and gravity correction fight each other.
    pub fn with_pitch_roll_swapped(self) -> Self {
        Self {
            pitch_axis: self.roll_axis,
            pitch_sign: self.roll_sign,
            roll_axis: self.pitch_axis,
            roll_sign: -self.pitch_sign,
            ..self
        }
    }

    /// +1 for a proper rotation, −1 for a mirrored frame.
    pub fn determinant(&self) -> f64 {
        let mut m = [[0.0f64; 3]; 3];
        m[0][self.roll_axis] = -self.roll_sign;
        m[1][self.pitch_axis] = self.pitch_sign;
        m[2][self.yaw_axis] = self.yaw_sign;
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }

    /// Whether the three axes are a clean permutation and the file matches this convention.
    pub fn is_usable(&self) -> bool {
        let distinct = self.yaw_axis != self.pitch_axis
            && self.pitch_axis != self.roll_axis
            && self.yaw_axis != self.roll_axis;
        let in_range = [self.yaw_axis, self.pitch_axis, self.roll_axis]
            .iter()
            .all(|&a| a < 3);
        distinct && in_range && self.version == CURRENT_VERSION
    }

    /// A mirrored frame is physically impossible for two right-handed frames, so it means one
    /// measured sign is wrong — usually a motion that accidentally combined two axes.
    pub fn is_right_handed(&self) -> bool {
        self.determinant() > 0.0
    }

    pub fn summary(&self) -> String {
        const NAMES: [&str; 3] = ["X", "Y", "Z"];
        let s = |v: f64| if v < 0.0 { "-" } else { "+" };
        format!(
            "yaw {}{}  pitch {}{}  roll {}{}  det {}",
            s(self.yaw_sign),
            NAMES[self.yaw_axis],
            s(self.pitch_sign),
            NAMES[self.pitch_axis],
            s(self.roll_sign),
            NAMES[self.roll_axis],
            if self.determinant() > 0.0 { "+1" } else { "-1" }
        )
    }
}

#[cfg(test)]
mod tests_swap {
    use super::*;

    #[test]
    fn swapping_pitch_and_roll_stays_a_rotation() {
        // A mirrored map is far worse than a swapped one: the filter fights itself and the
        // symptoms look like drift rather than like a bad axis.
        for map in [
            AxisMap::IDENTITY,
            AxisMap {
                version: CURRENT_VERSION,
                yaw_axis: 2,
                yaw_sign: 1.0,
                pitch_axis: 0,
                pitch_sign: 1.0,
                roll_axis: 1,
                roll_sign: 1.0,
            },
        ] {
            let swapped = map.with_pitch_roll_swapped();
            assert!(
                (swapped.determinant() - map.determinant()).abs() < 1e-9,
                "handedness changed: {} -> {}",
                map.determinant(),
                swapped.determinant()
            );
            assert!(swapped.is_usable(), "{swapped:?} is not usable");
        }
    }

    #[test]
    fn swapping_is_not_an_involution_and_the_caller_must_not_assume_it_is() {
        // Worth pinning down, because it is surprising. Exchanging two axes of a right-handed
        // frame mirrors it, so exactly one sign has to flip to stay a rotation -- and an
        // operation that flips one sign cannot be its own inverse. Pressing the HUD toggle
        // twice therefore lands on a *third* valid-but-wrong map, which is why the caller
        // remembers the previous value instead of swapping again.
        let twice = AxisMap::IDENTITY
            .with_pitch_roll_swapped()
            .with_pitch_roll_swapped();
        assert_ne!(twice, AxisMap::IDENTITY);
        // It is at least still a usable rotation, so a caller that gets this wrong produces a
        // world that is oriented oddly rather than one that tears itself apart.
        assert!(twice.is_usable());
    }

    #[test]
    fn the_measured_map_swaps_to_the_device_default() {
        // The specific case that prompted this: a calibration that recorded the nod and the
        // tilt the wrong way round produces exactly the default map, swapped.
        let measured = AxisMap {
            version: CURRENT_VERSION,
            yaw_axis: 2,
            yaw_sign: 1.0,
            pitch_axis: 0,
            pitch_sign: 1.0,
            roll_axis: 1,
            roll_sign: 1.0,
        };
        assert_eq!(measured.with_pitch_roll_swapped(), AxisMap::IDENTITY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_a_proper_right_handed_rotation() {
        assert!(AxisMap::IDENTITY.is_usable());
        assert!(
            AxisMap::IDENTITY.is_right_handed(),
            "determinant was {}",
            AxisMap::IDENTITY.determinant()
        );
        assert!((AxisMap::IDENTITY.determinant().abs() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_flipped_sign_is_detected_as_mirrored() {
        // This is the exact failure HoloFrame hit: one sign wrong turns the map into a
        // mirror, and the filter then fights itself in ways that look like drift.
        let mut m = AxisMap::IDENTITY;
        m.yaw_sign = -1.0;
        assert!(!m.is_right_handed());
    }

    #[test]
    fn repeated_axes_are_rejected() {
        let mut m = AxisMap::IDENTITY;
        m.pitch_axis = m.yaw_axis;
        assert!(!m.is_usable());
    }

    #[test]
    fn identity_maps_sensor_axes_as_documented() {
        // Sensor +Z (the yaw axis) must become canonical +Z, i.e. turning left.
        let v = AxisMap::IDENTITY.apply(DVec3::new(0.0, 0.0, 1.0));
        assert_eq!(v, DVec3::new(0.0, 0.0, 1.0));
        // Sensor +X is the roll axis with sign -1, and apply() negates again, so it lands
        // on canonical +X.
        let v = AxisMap::IDENTITY.apply(DVec3::new(1.0, 0.0, 0.0));
        assert_eq!(v, DVec3::new(1.0, 0.0, 0.0));
    }

    #[test]
    fn stale_version_is_rejected() {
        let mut m = AxisMap::IDENTITY;
        m.version = 1;
        assert!(!m.is_usable(), "a v1 file used a left-handed convention");
    }
}
