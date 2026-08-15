//! The device table.
//!
//! Parsed once from a compiled-in `devices.toml`, so adding hardware needs no runtime file
//! but stays a data edit rather than a code edit.

use serde::Deserialize;
use std::sync::OnceLock;

/// A direction in the wearer's head frame, used to say where a sensor axis points.
///
/// The head frame is the canonical one: forward is the nose, left is the left ear, up is the
/// top of the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeadDirection {
    Forward,
    Back,
    Left,
    Right,
    Up,
    Down,
}

/// Which head direction each of the IMU's three axes points along, in order X, Y, Z.
///
/// This is the IMU's physical mounting inside the glasses — where the chip sits on the board
/// and which way round the board is. It is a property of the product, identical across every
/// unit of it, and it cannot change between wearers or between sessions.
///
/// Stating it this way rather than as a ready-made axis map is deliberate. A map is six
/// numbers with a sign convention folded in, which nobody can check by looking; a mounting is
/// a claim about the physical world that anyone can verify in a minute with an accelerometer
/// and a flat table. `AxisMap::from_mounting` turns the checkable statement into the
/// convention-laden one, and is tested against the measurement this row came from.
pub type Mounting = [HeadDirection; 3];

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DeviceSpec {
    pub name: String,
    pub driver: String,
    pub vid: u16,
    pub pid: u16,
    /// Whether this row has been exercised on real hardware. Unverified rows still work,
    /// but the driver logs a warning so a wrong table entry is visible rather than baffling.
    pub verified: bool,
    pub imu_interface: u8,
    pub mcu_interface: u8,
    pub imu_report_len: usize,
    pub per_eye: (u32, u32),
    pub h_fov_deg: f64,
    pub default_ipd_mm: f64,
    pub mode_mono: u8,
    pub mode_stereo: u8,
    pub mode_stereo_fallback: u8,
    /// Where the IMU's X, Y and Z axes point in the head frame, if it has been measured on
    /// real hardware.
    ///
    /// `None` means nobody has held one of these level and looked at the accelerometer, so
    /// the wearer has to calibrate. Present means the answer is known and calibration is not
    /// a per-user question at all — see [`Mounting`].
    #[serde(default)]
    pub sensor_axes: Option<Mounting>,
}

#[derive(Debug, Deserialize)]
struct DeviceFile {
    device: Vec<DeviceSpec>,
}

static DB: OnceLock<Vec<DeviceSpec>> = OnceLock::new();

/// Every known device.
pub fn all() -> &'static [DeviceSpec] {
    DB.get_or_init(|| {
        let raw = include_str!("devices.toml");
        // A malformed table is a build-time authoring error, not a runtime condition worth
        // degrading over — the message names the file so it is obvious what to fix.
        let parsed: DeviceFile =
            toml::from_str(raw).expect("devices.toml is malformed; fix the table");
        parsed.device
    })
}

pub fn lookup(vid: u16, pid: u16) -> Option<&'static DeviceSpec> {
    all().iter().find(|d| d.vid == vid && d.pid == pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_parses_and_has_the_verified_device() {
        let air = lookup(0x3318, 0x0424).expect("XREAL Air must be in the table");
        assert_eq!(air.name, "XREAL Air");
        assert!(air.verified, "the Air gen 1 is the one we confirmed on hardware");
        assert_eq!((air.imu_interface, air.mcu_interface), (3, 4));
        // 0x04 is 3840x1080@72 — confirmed working on the Deck. Guard against a careless
        // edit silently downgrading everyone to 60 Hz.
        assert_eq!(air.mode_stereo, 0x04);
    }

    #[test]
    fn no_duplicate_ids() {
        let mut seen = std::collections::HashSet::new();
        for d in all() {
            assert!(seen.insert((d.vid, d.pid)), "duplicate id for {}", d.name);
        }
    }
}
