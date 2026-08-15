//! How the IMU's raw axes map onto head motion.
//!
//! The glasses do not publish their axis convention over USB, but it is a fixed property of
//! how the IMU is mounted inside them — so for hardware anyone has measured it comes from the
//! device table via [`AxisMap::from_mounting`], not from the wearer. Calibration is the
//! fallback for headsets nobody has held, not the normal path.
//!
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

    /// The XREAL Air's actual axis convention, measured against gravity.
    ///
    /// Not a guess and not a calibration result. The glasses were held in three known poses
    /// while the accelerometer was recorded: level identifies the sensor axis that is head-UP,
    /// and tipping the nose down leaves the head-LEFT axis unchanged, which names it outright.
    /// Up is +Z, left is +X, and forward is therefore −Y.
    ///
    /// The same measurement now also lives in `devices.toml`, stated as the physical mounting
    /// rather than as a sign convention, and that is what a running session actually uses —
    /// see [`Self::from_mounting`], and the test asserting the two cannot drift apart. This
    /// constant remains the last-resort default for the window before a headset is open.
    ///
    /// A generic identity map is the *wrong* answer for this hardware, and getting it wrong is
    /// not subtle: looking down rolls the world instead of nodding it.
    pub const XREAL_AIR: Self = Self {
        version: CURRENT_VERSION,
        yaw_axis: 2,
        yaw_sign: 1.0,
        pitch_axis: 0,
        pitch_sign: 1.0,
        roll_axis: 1,
        roll_sign: 1.0,
    };

    /// Derive the map from where the IMU physically sits in the headset.
    ///
    /// This is the preferred way to get an `AxisMap`, and it makes calibration unnecessary on
    /// any headset whose mounting has been measured. The mounting is a property of the
    /// product — where the chip is soldered and which way round the board is — so it is the
    /// same on every unit and identical for every wearer. Treating it as something to be
    /// discovered per user is what produced the long-running complaint that pitch and roll
    /// were swapped again after every restart: three motions performed by a human, two of
    /// which (nodding and tilting) are adjacent, cannot settle a question the datasheet
    /// already answers.
    ///
    /// Each entry says which head direction that sensor axis points along, in order X, Y, Z.
    /// Reading the assignments backwards out of [`Self::apply`]: `out.x` is head-forward and
    /// is built as `-roll_sign * a[roll_axis]`, hence the inverted sign on the forward axis;
    /// `out.y` is head-left and `out.z` is head-up, both of which take their sign directly.
    ///
    /// Returns `None` for a mounting that is not three distinct axes, or that describes a
    /// left-handed sensor frame. Both are authoring mistakes in the device table rather than
    /// runtime conditions, and a mirrored frame in particular must never reach the filter:
    /// gyro integration and gravity correction then disagree about handedness and fight each
    /// other, which reads as drift rather than as bad data.
    pub fn from_mounting(mounting: spatiand_hmd::device::Mounting) -> Option<Self> {
        use spatiand_hmd::device::HeadDirection as D;

        let (mut yaw, mut pitch, mut roll) = (None, None, None);
        for (axis, direction) in mounting.into_iter().enumerate() {
            match direction {
                D::Up => yaw = Some((axis, 1.0)),
                D::Down => yaw = Some((axis, -1.0)),
                D::Left => pitch = Some((axis, 1.0)),
                D::Right => pitch = Some((axis, -1.0)),
                // `apply` negates the roll term, so a sensor axis pointing along +forward
                // needs a negative roll sign to come back out as +forward.
                D::Forward => roll = Some((axis, -1.0)),
                D::Back => roll = Some((axis, 1.0)),
            }
        }

        let ((yaw_axis, yaw_sign), (pitch_axis, pitch_sign), (roll_axis, roll_sign)) =
            (yaw?, pitch?, roll?);
        let map = Self {
            version: CURRENT_VERSION,
            yaw_axis,
            yaw_sign,
            pitch_axis,
            pitch_sign,
            roll_axis,
            roll_sign,
        };
        (map.is_usable() && map.is_right_handed()).then_some(map)
    }

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
mod tests_mounting {
    use super::*;
    use spatiand_hmd::device::HeadDirection as D;

    #[test]
    fn the_airs_mounting_derives_the_map_that_was_measured_for_it() {
        // The load-bearing test in this file. `XREAL_AIR` was established by holding the
        // glasses in known poses and reading the accelerometer; the mounting in
        // `devices.toml` records the same measurement in physical terms. If these two ever
        // disagree, one of them has been edited without the other and the wearer is about to
        // be told that nodding rolls the world.
        let derived = AxisMap::from_mounting([D::Left, D::Back, D::Up]).expect("a valid frame");
        assert_eq!(derived, AxisMap::XREAL_AIR);
    }

    #[test]
    fn the_table_and_the_constant_agree() {
        // Same claim, but reading the actual shipped row rather than a literal, so an edit to
        // `devices.toml` cannot quietly diverge from the constant.
        let air = spatiand_hmd::device::lookup(0x3318, 0x0424).expect("the Air is in the table");
        let mounting = air.sensor_axes.expect("the Air's mounting has been measured");
        assert_eq!(AxisMap::from_mounting(mounting), Some(AxisMap::XREAL_AIR));
    }

    #[test]
    fn every_mounting_in_the_table_is_a_proper_rotation() {
        // A left-handed or repeated mounting is an authoring slip that would otherwise only
        // show up as a headset that tracks strangely.
        for device in spatiand_hmd::device::all() {
            let Some(mounting) = device.sensor_axes else {
                continue;
            };
            assert!(
                AxisMap::from_mounting(mounting).is_some(),
                "{} has an impossible mounting: {mounting:?}",
                device.name
            );
        }
    }

    #[test]
    fn a_mirrored_mounting_is_refused_rather_than_returned() {
        // Swapping two axes of a right-handed frame without flipping a sign describes a
        // reflection, which no physical mounting can be.
        assert_eq!(AxisMap::from_mounting([D::Back, D::Left, D::Up]), None);
    }

    #[test]
    fn a_mounting_that_repeats_a_direction_is_refused() {
        assert_eq!(AxisMap::from_mounting([D::Up, D::Up, D::Left]), None);
    }

    #[test]
    fn a_mounting_round_trips_a_sensor_vector_into_the_head_frame() {
        // Stated independently of the constant: with the Air's mounting, sensor +Z is the top
        // of the head and must come out as canonical +Z, and sensor +Y points backwards so it
        // must come out as canonical -X.
        let map = AxisMap::from_mounting([D::Left, D::Back, D::Up]).expect("a valid frame");
        assert_eq!(map.apply(DVec3::new(0.0, 0.0, 1.0)), DVec3::new(0.0, 0.0, 1.0));
        assert_eq!(map.apply(DVec3::new(0.0, 1.0, 0.0)), DVec3::new(-1.0, 0.0, 0.0));
        assert_eq!(map.apply(DVec3::new(1.0, 0.0, 0.0)), DVec3::new(0.0, 1.0, 0.0));
    }

    #[test]
    fn the_generic_identity_is_not_what_this_hardware_needs() {
        // The map that was found stored on the wearer's Deck, and the reason this whole
        // mechanism exists: it is a perfectly valid rotation, so nothing downstream could
        // notice it was wrong for these glasses.
        let derived = AxisMap::from_mounting([D::Left, D::Back, D::Up]).expect("a valid frame");
        assert_ne!(derived, AxisMap::IDENTITY);
        assert!(AxisMap::IDENTITY.is_right_handed(), "which is exactly why it went unnoticed");
    }
}

#[cfg(test)]
mod tests_measured {
    use super::*;

    #[test]
    fn the_measured_map_is_a_usable_rotation() {
        // Still the fallback for a headset with no mounting in the table, so a slip in
        // transcribing it from the measurement would ship to everyone.
        assert!(AxisMap::XREAL_AIR.is_usable());
        assert!(AxisMap::XREAL_AIR.is_right_handed());
    }

    #[test]
    fn the_measured_map_is_not_the_generic_identity() {
        // The two differ by exactly a pitch/roll exchange, which is why the generic identity
        // produced "looking down rolls the world" -- and why a stored copy of it, which is
        // what was found on the wearer's Deck, survived every restart without anything
        // downstream being able to object.
        assert_ne!(AxisMap::XREAL_AIR, AxisMap::IDENTITY);
        assert_eq!(
            AxisMap::XREAL_AIR.pitch_axis,
            AxisMap::IDENTITY.roll_axis,
            "the two differ by exchanging pitch and roll"
        );
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
