//! Physical input.
//!
//! Only what the shell needs today: knowing that *a* button was pressed. The full router —
//! touchpads as a 3D pointer, gestures, a synthesized gamepad for games — comes later, and
//! this is deliberately shaped so it can grow into that rather than being thrown away.
//!
//! Reading raw hidraw rather than evdev is not a preference. The kernel's `hid-steam` driver
//! disables the touchpads' pointer input and exposes neither absolute pad coordinates,
//! pad pressure, nor the gyro — all three of which a 3D pointer needs. The vendor reports
//! carry everything. See `docs/steam-deck-controller.md`.

use std::time::Duration;

use spatiand_hmd::hid::{self, HidDevice};

/// Valve's vendor/product for the Deck's built-in controls.
const VALVE_VID: u16 = 0x28DE;
const DECK_PID: u16 = 0x1205;
/// The vendor interface. Match on this rather than a node number: `/dev/hidrawN` numbering
/// is not stable across boots or across which USB devices enumerate first.
const VENDOR_INTERFACE: u8 = 2;

/// Header of a Deck input report: version `0x0001`, type `0x09`, length `0x40`.
const REPORT_HEADER: [u8; 4] = [0x01, 0x00, 0x09, 0x40];
/// Buttons occupy a bitfield here. Which bit is which is still unverified, so nothing below
/// depends on the mapping — only on "any of them is set".
const BUTTON_OFFSET: usize = 8;
const BUTTON_BYTES: usize = 8;

/// The Deck's own controls.
pub struct DeckController {
    device: HidDevice,
    buf: [u8; 64],
    /// Buttons held as of the last report, so a press is an edge rather than a level. Without
    /// this, holding a button reads as thousands of presses at 250 Hz.
    held: bool,
}

impl DeckController {
    /// Open the controller, or `None` if it is not present or not permitted.
    ///
    /// Permission is the likely failure: the hidraw ACL exists only for the active seat0
    /// session, so outside one this needs the udev rule from `tools/install-udev-rules.sh`.
    pub fn open() -> Option<Self> {
        let node = hid::find(VALVE_VID, DECK_PID, VENDOR_INTERFACE)?;
        match HidDevice::open(&node) {
            Ok(device) => {
                log::info!("controller: {}", node.path.display());
                Some(Self {
                    device,
                    buf: [0u8; 64],
                    held: false,
                })
            }
            Err(e) => {
                log::warn!(
                    "found the controller at {} but could not open it: {e}",
                    node.path.display()
                );
                log::warn!("  if this is permissions, run: sudo tools/install-udev-rules.sh");
                None
            }
        }
    }

    /// Drain pending reports and return true if a button went from released to pressed.
    ///
    /// Reports arrive at ~250 Hz, so this must be drained every frame or it backs up.
    pub fn poll_button_press(&mut self) -> bool {
        let mut pressed = false;
        // Bounded rather than "until empty": at 250 Hz a slow frame leaves a backlog, and an
        // unbounded drain would let it stall the render loop.
        for _ in 0..64 {
            match self.device.read_report(&mut self.buf, Duration::ZERO) {
                Ok(Some(n)) if n >= BUTTON_OFFSET + BUTTON_BYTES => {
                    if self.buf[..4] != REPORT_HEADER {
                        continue;
                    }
                    let any = self.buf[BUTTON_OFFSET..BUTTON_OFFSET + BUTTON_BYTES]
                        .iter()
                        .any(|&b| b != 0);
                    if any && !self.held {
                        pressed = true;
                    }
                    self.held = any;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(e) => {
                    log::debug!("controller read failed: {e}");
                    break;
                }
            }
        }
        pressed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_the_captured_report() {
        // Observed on hardware: 64-byte reports at 250 Hz beginning 01 00 09 40.
        assert_eq!(REPORT_HEADER, [0x01, 0x00, 0x09, 0x40]);
    }

    #[test]
    fn button_window_stays_inside_the_report() {
        // const_assert in spirit: the window must fit, and a future edit widening it should
        // fail here rather than silently reading past the report.
        const _: () = assert!(BUTTON_OFFSET + BUTTON_BYTES <= 64);
    }
}
