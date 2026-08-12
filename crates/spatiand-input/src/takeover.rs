//! Taking the controller away from its default behaviour.
//!
//! Out of the box the Deck's pads run in "lizard mode" — the firmware emulates a mouse and
//! keyboard so the device is useful before any software understands it. That emulation is
//! actively in the way here: it consumes the pads and reports no absolute coordinates, so the
//! 3D pointer has nothing to work with. The IMU is off for the same reason.
//!
//! Both are changed with feature reports, listed in `docs/steam-deck-controller.md` §4 and
//! taken from `hid-steam.c`. All of it was confirmed to take effect on hardware.
//!
//! [`release`] exists because these settings persist after the process exits. Leaving the pads
//! in absolute mode means the desktop's mouse stops working when Spatiand quits, which looks
//! like Spatiand having broken the machine.

use std::os::fd::RawFd;

use spatiand_hmd::hid::HidDevice;
use spatiand_hmd::{HmdError, Result};

const ID_CLEAR_DIGITAL_MAPPINGS: u8 = 0x81;
const ID_SET_SETTINGS_VALUES: u8 = 0x87;
const ID_LOAD_DEFAULT_SETTINGS: u8 = 0x8E;

const REG_LPAD_MODE: u8 = 0x07;
const REG_RPAD_MODE: u8 = 0x08;
const REG_RPAD_MARGIN: u8 = 0x18;
const REG_GYRO_MODE: u8 = 0x30;
const REG_LPAD_CLICK_PRESSURE: u8 = 0x34;
const REG_RPAD_CLICK_PRESSURE: u8 = 0x35;

/// Pad mode 7: report absolute position, no emulation.
const PAD_MODE_ABSOLUTE: u16 = 0x07;
/// Gyro mode 0x18: accelerometer *and* gyroscope.
const GYRO_MODE_ACCEL_AND_GYRO: u16 = 0x18;
/// Click pressure at maximum, i.e. the firmware never synthesises a click of its own. We read
/// the click bit out of the report instead, which is what makes a click distinguishable from
/// a firm touch.
const CLICK_PRESSURE_MAX: u16 = 0xFFFF;

/// Reports are 64 bytes; the ioctl buffer carries a leading report-id byte as well.
const FEATURE_BUF_LEN: usize = 65;

/// `HIDIOCSFEATURE(len)` — `_IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06, len)`.
///
/// Built here rather than pulled from a binding crate: it is four shifts, and the alternative
/// is a dependency that exists to hold one number.
fn hidiocsfeature(len: usize) -> u64 {
    const DIR_WRITE_READ: u64 = 3;
    (DIR_WRITE_READ << 30) | ((len as u64) << 16) | ((b'H' as u64) << 8) | 0x06
}

fn send_feature(fd: RawFd, payload: &[u8]) -> Result<()> {
    if payload.len() + 1 > FEATURE_BUF_LEN {
        return Err(HmdError::Protocol(format!(
            "feature report of {} bytes does not fit a {FEATURE_BUF_LEN}-byte buffer",
            payload.len()
        )));
    }
    let mut buf = [0u8; FEATURE_BUF_LEN];
    // Report id 0: these are unnumbered reports, exactly as on the glasses' MCU.
    buf[1..1 + payload.len()].copy_from_slice(payload);
    let rc = unsafe { libc::ioctl(fd, hidiocsfeature(FEATURE_BUF_LEN) as _, buf.as_mut_ptr()) };
    if rc < 0 {
        return Err(HmdError::Io {
            path: "hidraw feature report".into(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// `[0x87, byte_count, reg, lo, hi, ...]` — §4.
fn set_registers(fd: RawFd, pairs: &[(u8, u16)]) -> Result<()> {
    let mut payload = Vec::with_capacity(2 + pairs.len() * 3);
    payload.push(ID_SET_SETTINGS_VALUES);
    payload.push((pairs.len() * 3) as u8);
    for (reg, value) in pairs {
        payload.push(*reg);
        payload.push((value & 0xFF) as u8);
        payload.push((value >> 8) as u8);
    }
    send_feature(fd, &payload)
}

/// Put the controller into the state the spatial shell needs.
///
/// Idempotent, and safe to call on a device Steam has just let go of.
pub fn take(device: &HidDevice) -> Result<()> {
    let fd = device.as_raw_fd();
    set_registers(
        fd,
        &[
            (REG_LPAD_MODE, PAD_MODE_ABSOLUTE),
            (REG_RPAD_MODE, PAD_MODE_ABSOLUTE),
            // No dead margin at the edge of the right pad: the pointer maps the pad
            // absolutely, so a margin would make the edges of the view unreachable.
            (REG_RPAD_MARGIN, 0x0000),
            (REG_LPAD_CLICK_PRESSURE, CLICK_PRESSURE_MAX),
            (REG_RPAD_CLICK_PRESSURE, CLICK_PRESSURE_MAX),
            (REG_GYRO_MODE, GYRO_MODE_ACCEL_AND_GYRO),
        ],
    )?;
    // Drop the firmware's own key/mouse mappings. Without this the pads keep moving the
    // desktop cursor underneath us even though we are reading them ourselves.
    send_feature(fd, &[ID_CLEAR_DIGITAL_MAPPINGS, 0x00])
}

/// Hand the controller back to its defaults.
///
/// Best-effort by design: this runs on the way out, often while something else has already
/// gone wrong, and a failure here must not mask the reason we are exiting.
pub fn release(device: &HidDevice) {
    if let Err(e) = send_feature(device.as_raw_fd(), &[ID_LOAD_DEFAULT_SETTINGS, 0x00]) {
        log::warn!("could not restore the controller's default settings: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ioctl_number_matches_the_kernel_encoding() {
        // _IOC(3, 'H', 0x06, 65): dir 3 at bit 30, size at 16, type 'H' at 8, nr 6.
        let expected = (3u64 << 30) | (65u64 << 16) | (0x48u64 << 8) | 0x06;
        assert_eq!(hidiocsfeature(65), expected);
        // Sanity against a hand-computed value, so a shift typo cannot pass by agreeing with
        // itself: 0xC0414806.
        assert_eq!(hidiocsfeature(65), 0xC041_4806);
    }

    #[test]
    fn a_settings_payload_is_three_bytes_per_register() {
        // Reconstructs what set_registers would build. The byte count field counts the
        // register triples only, not itself — the same trap that cost a day on the glasses'
        // MCU, where the length field *does* count itself.
        let pairs = [(REG_LPAD_MODE, 0x07u16), (REG_GYRO_MODE, 0x18u16)];
        let mut payload = vec![ID_SET_SETTINGS_VALUES, (pairs.len() * 3) as u8];
        for (reg, value) in pairs {
            payload.extend_from_slice(&[reg, (value & 0xFF) as u8, (value >> 8) as u8]);
        }
        assert_eq!(payload, vec![0x87, 6, 0x07, 0x07, 0x00, 0x30, 0x18, 0x00]);
    }

    #[test]
    fn an_oversized_payload_is_refused_rather_than_truncated() {
        // Silently sending a short report would leave the controller half-configured, with
        // the pads absolute but the gyro off — a state that is very hard to recognise.
        let huge = vec![0u8; FEATURE_BUF_LEN];
        assert!(send_feature(-1, &huge).is_err());
    }
}
