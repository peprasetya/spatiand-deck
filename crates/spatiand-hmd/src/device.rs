//! The device table.
//!
//! Parsed once from a compiled-in `devices.toml`, so adding hardware needs no runtime file
//! but stays a data edit rather than a code edit.

use serde::Deserialize;
use std::sync::OnceLock;

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
