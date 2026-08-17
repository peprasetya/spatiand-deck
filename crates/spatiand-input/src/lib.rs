//! Physical input for the spatial shell.
//!
//! Reading raw hidraw rather than evdev is not a preference. The kernel's `hid-steam` driver
//! disables the touchpads' pointer input and exposes neither absolute pad coordinates, pad
//! pressure, nor the gyro — all three of which a 3D pointer needs. The vendor reports carry
//! everything. See `docs/steam-deck-controller.md`.
//!
//! The crate is layered so that almost none of it needs hardware to test:
//!
//! * [`layout`] — which bit is which, as data.
//! * [`report`] — bytes to [`ControllerState`], a pure function.
//! * [`gesture`] — two-thumb pan and scale, a pure state machine.
//! * [`scroll`] — an absolute pad read as a wheel, including what to do about the thumb
//!   leaving it. Also a pure state machine.
//! * [`trigger`] — an analogue trigger read as a button.
//! * [`takeover`] — the feature reports that claim the device. The only part that must talk
//!   to a real controller.
//! * [`touch`] — a touchscreen, via evdev. The one device here the kernel already decodes
//!   properly, and the only one found by capability rather than by vendor id.
//!
//! [`DeckController`] is the thin layer that joins them to a file descriptor.

use std::time::Duration;

use spatiand_hmd::hid::{self, HidDevice};

pub mod gesture;
pub mod haptics;
pub mod layout;
pub mod report;
pub mod scroll;
#[cfg(test)]
mod scroll_trace;
pub mod takeover;
pub mod touch;
pub mod trigger;

pub use gesture::{GestureDelta, TwoPadGesture};
pub use haptics::{Feel, Pad as HapticPad};
pub use layout::{Confidence, Control};
pub use report::{Buttons, ControllerState, Pad};
pub use scroll::{PadScroll, Scroll};
pub use touch::{Contact, TouchEvent, Touchscreen};
pub use trigger::Trigger;

/// Valve's vendor/product for the Deck's built-in controls.
const VALVE_VID: u16 = 0x28DE;
const DECK_PID: u16 = 0x1205;
/// The vendor interface. Match on this rather than a node number: `/dev/hidrawN` numbering
/// is not stable across boots or across which USB devices enumerate first.
const VENDOR_INTERFACE: u8 = 2;

/// Reports arrive at ~250 Hz. A slow frame leaves a backlog, and an unbounded drain would let
/// that backlog stall the render loop, so each poll takes at most this many.
const MAX_REPORTS_PER_POLL: usize = 64;

/// The Deck's own controls.
pub struct DeckController {
    device: HidDevice,
    buf: [u8; report::REPORT_LEN],
    state: ControllerState,
    /// Button field as of the last report *seen*, not the last frame. Edges are computed
    /// against this incrementally so that a button pressed and released inside a single
    /// polling batch is still reported — at 250 Hz a firm tap easily fits in one frame.
    last_buttons: Buttons,
    pressed_this_frame: Vec<Control>,
    warned_about_steam: bool,
}

impl DeckController {
    /// Open the controller, or `None` if it is not present or not permitted.
    ///
    /// Permission is the likely failure: the hidraw ACL exists only for the active seat0
    /// session, so outside one this needs the udev rule from `tools/install-udev-rules.sh`.
    pub fn open() -> Option<Self> {
        // Claiming the controller reconfigures it, and that reconfiguration outlives us. Under
        // a desktop that is still running Steam -- the nested development case -- it takes the
        // pads away from whoever is using them and leaves them inert until restored. So there
        // is a way to say "look, do not touch".
        if std::env::var("SPATIAND_INPUT").as_deref() == Ok("off") {
            log::info!("SPATIAND_INPUT=off — not opening the controller");
            return None;
        }
        let node = hid::find(VALVE_VID, DECK_PID, VENDOR_INTERFACE)?;
        let device = match HidDevice::open(&node) {
            Ok(d) => d,
            Err(e) => {
                log::warn!(
                    "found the controller at {} but could not open it: {e}",
                    node.path.display()
                );
                log::warn!("  if this is permissions, run: sudo tools/install-udev-rules.sh");
                return None;
            }
        };
        log::info!("controller: {}", node.path.display());

        // Claim it before reading. Left alone the pads emulate a mouse and report nothing
        // absolute, so the pointer would have no input and the cause would not be obvious.
        if let Err(e) = takeover::take(&device) {
            log::warn!("could not reconfigure the controller ({e}); pads may be unusable");
        }

        Some(Self {
            device,
            buf: [0u8; report::REPORT_LEN],
            state: ControllerState::default(),
            last_buttons: Buttons::default(),
            pressed_this_frame: Vec::new(),
            warned_about_steam: false,
        })
    }

    /// Drain pending reports into the current state. Call once per frame.
    pub fn poll(&mut self) {
        self.pressed_this_frame.clear();
        for _ in 0..MAX_REPORTS_PER_POLL {
            match self.device.read_report(&mut self.buf, Duration::ZERO) {
                Ok(Some(n)) => {
                    let Some(state) = ControllerState::parse(&self.buf[..n]) else {
                        // Not an input report — a reply to one of our feature writes, most
                        // likely. Decoding it as input would produce a burst of phantom
                        // presses at exactly the moment the shell starts.
                        continue;
                    };
                    for control in state.buttons.pressed_since(self.last_buttons) {
                        self.pressed_this_frame.push(control);
                    }
                    self.last_buttons = state.buttons;
                    self.state = state;
                }
                Ok(None) => break,
                Err(e) => {
                    log::debug!("controller read failed: {e}");
                    break;
                }
            }
        }

        if self.state.looks_silenced() && !self.warned_about_steam {
            self.warned_about_steam = true;
            log::warn!("the controller is reporting all-zero input with a live sequence counter");
            log::warn!("  Steam is probably still running and holding the device; stop it");
        }
    }

    /// Everything the controller currently reads.
    pub fn state(&self) -> &ControllerState {
        &self.state
    }

    /// Controls that went down since the last [`DeckController::poll`].
    pub fn pressed(&self) -> &[Control] {
        &self.pressed_this_frame
    }

    pub fn just_pressed(&self, control: Control) -> bool {
        self.pressed_this_frame.contains(&control)
    }

    /// Did *any* button go down this frame?
    ///
    /// Used by the waiting screen, where offering one specific button would be worse than
    /// offering all of them.
    pub fn any_pressed(&self) -> bool {
        !self.pressed_this_frame.is_empty()
    }

    /// Buzz one of the pads.
    ///
    /// Best-effort. Haptics stopping is a degraded session, not a broken one, so a failure
    /// here is a debug line rather than something a caller has to handle.
    pub fn pulse(&self, pad: haptics::Pad, feel: haptics::Feel) {
        if let Err(e) = haptics::pulse(&self.device, pad, feel) {
            log::debug!("haptic pulse failed: {e}");
        }
    }
}

impl Drop for DeckController {
    /// Hand the controller back.
    ///
    /// These settings outlive the process. Leaving the pads in absolute mode means the
    /// desktop's mouse stops working after Spatiand exits, which presents as Spatiand having
    /// broken the machine rather than as a missing teardown.
    fn drop(&mut self) {
        takeover::release(&self.device);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendor_interface_is_the_one_the_docs_name() {
        // Interface 2 is the vendor one; 0 and 1 are the emulated keyboard and mouse, which
        // carry none of the fields a 3D pointer needs.
        assert_eq!(VENDOR_INTERFACE, 2);
    }

    #[test]
    fn the_poll_bound_covers_a_full_frame_at_report_rate() {
        // 250 Hz into a 72 Hz render loop is ~3.5 reports a frame; the bound has to leave
        // enough headroom that a slow frame catches up rather than falling permanently behind.
        assert!(MAX_REPORTS_PER_POLL >= 250 / 72 * 4);
    }
}
