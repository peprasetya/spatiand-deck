//! Making the touchpads feel like something.
//!
//! The pads have actuators under them, and nothing drives them automatically — on the desktop
//! that job belongs to Steam. Inside Spatiand nothing was sending anything, so the pads felt
//! dead: position still arrived, clicks still registered, and the wearer's hands were told
//! nothing at all. That is worse than it sounds. A pad you cannot feel gives no confirmation
//! that a click landed, and the only remaining feedback is whatever the application does next.
//!
//! `ID_TRIGGER_HAPTIC_PULSE` (`0x8f`) is the command, confirmed on hardware by firing every
//! plausible candidate and asking which ones were felt: **pad byte 0 is the right pad, byte 1
//! the left**. The `0xeb` rumble command also works but drives the body motors — a completely
//! different sensation, and the wrong one for a click.
//!
//! A pulse train is described by how long each pulse lasts, how long the gap is, and how many
//! there are, all in microseconds. Short and few reads as a tick; long and many reads as a
//! buzz.

use spatiand_hmd::hid::HidDevice;
use spatiand_hmd::Result;

use crate::takeover::send_feature;

const ID_TRIGGER_HAPTIC_PULSE: u8 = 0x8F;

/// Which actuator. The byte the device wants, not an index we chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pad {
    Right = 0,
    Left = 1,
}

/// What a pulse should feel like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feel {
    /// A crisp tick under a click. The most important one: it is the confirmation that a press
    /// registered, and it has to be short enough not to blur into the next.
    Click,
    /// A lighter tick for crossing a boundary — the cursor entering a window, or reaching the
    /// edge of one. Lighter than a click, but not *near threshold*: the first attempt aimed
    /// for barely-perceptible and landed on imperceptible, which on hardware read as the
    /// effect simply not firing.
    Tick,
    /// A longer buzz for something that needs noticing without a screen — a launch failing.
    Alert,
}

impl Feel {
    /// `(duration_us, interval_us, count)`.
    fn shape(self) -> (u16, u16, u16) {
        match self {
            // Firm. Judged against lighter and heavier alternatives in isolation, where the
            // lighter one felt right -- and then reported as "almost nothing" in use, with a
            // thumb moving and something happening on screen to distract from it. Feedback
            // competes for attention, so it has to be stronger than it seems when you are
            // sitting still paying attention to it.
            Feel::Click => (2000, 2000, 5),
            // A single 700 us pulse was tested on hardware and could not be felt at all. Two
            // pulses of 1 ms can, while still reading as clearly lighter than a click.
            Feel::Tick => (1000, 1000, 2),
            Feel::Alert => (4000, 4000, 20),
        }
    }
}

/// Fire a pulse train at one pad.
///
/// Best-effort: a failure here is worth a debug line and nothing more. Haptics stopping is a
/// degraded session, not a broken one, and turning it into an error would take down a working
/// desktop over a buzz.
pub fn pulse(device: &HidDevice, pad: Pad, feel: Feel) -> Result<()> {
    let (duration, interval, count) = feel.shape();
    let mut payload = Vec::with_capacity(10);
    payload.push(ID_TRIGGER_HAPTIC_PULSE);
    // Byte count of what follows, not counting itself. The glasses' MCU uses the opposite
    // convention for its own length field, which is a trap worth naming.
    payload.push(8);
    payload.push(pad as u8);
    payload.extend_from_slice(&duration.to_le_bytes());
    payload.extend_from_slice(&interval.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    // Gain in decibels. Zero is the device's own idea of full strength.
    payload.push(0);
    send_feature(device.as_raw_fd(), &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rebuild the payload the way `pulse` does, without needing a device.
    fn payload(pad: Pad, feel: Feel) -> Vec<u8> {
        let (duration, interval, count) = feel.shape();
        let mut p = vec![ID_TRIGGER_HAPTIC_PULSE, 8, pad as u8];
        p.extend_from_slice(&duration.to_le_bytes());
        p.extend_from_slice(&interval.to_le_bytes());
        p.extend_from_slice(&count.to_le_bytes());
        p.push(0);
        p
    }

    #[test]
    fn the_pads_are_addressed_by_the_bytes_the_hardware_wants() {
        // Confirmed by feel: byte 0 buzzed the right pad, byte 1 the left. Swapping these
        // makes every click confirm itself under the wrong thumb, which is disorienting in a
        // way that is hard to put a finger on.
        assert_eq!(Pad::Right as u8, 0);
        assert_eq!(Pad::Left as u8, 1);
    }

    #[test]
    fn the_payload_is_ten_bytes_with_a_count_of_eight() {
        // The length field counts what follows it, so a ten-byte payload declares eight.
        let p = payload(Pad::Right, Feel::Click);
        assert_eq!(p.len(), 10);
        assert_eq!(p[0], 0x8F);
        assert_eq!(p[1], 8, "length must not count itself or the command");
    }

    #[test]
    fn a_click_is_short_enough_to_feel_like_one_event() {
        // Total time is count * (duration + interval). Much past ~20 ms and a click starts to
        // read as a buzz, which makes rapid clicking feel mushy.
        let (duration, interval, count) = Feel::Click.shape();
        let total_us = count as u32 * (duration as u32 + interval as u32);
        assert!(total_us <= 20_000, "click lasts {total_us} us");
        assert!(
            total_us > 2_000,
            "click lasts {total_us} us, too short to feel"
        );
    }

    #[test]
    fn a_tick_is_lighter_than_a_click_which_is_lighter_than_an_alert() {
        let total = |f: Feel| {
            let (d, i, c) = f.shape();
            c as u32 * (d as u32 + i as u32)
        };
        assert!(total(Feel::Tick) < total(Feel::Click));
        assert!(total(Feel::Click) < total(Feel::Alert));
    }

    #[test]
    fn a_tick_is_still_strong_enough_to_be_felt() {
        // Measured the hard way: a single 700 us pulse -- 1400 us of total activity -- was
        // reported as nothing at all. Staying comfortably above that is the whole point of a
        // feedback effect, and "subtle" is one step from "absent".
        let (duration, interval, count) = Feel::Tick.shape();
        let total_us = count as u32 * (duration as u32 + interval as u32);
        assert!(
            total_us >= 3_000,
            "tick is {total_us} us, near the threshold that failed"
        );
        assert!(count >= 2, "a single pulse was not perceptible");
    }

    #[test]
    fn every_shape_fits_the_sixteen_bit_fields() {
        for feel in [Feel::Click, Feel::Tick, Feel::Alert] {
            let (d, i, c) = feel.shape();
            // Constructing them as u16 already guarantees this; the test is here so that a
            // future edit to microsecond values gets a compile-time home rather than a
            // silently truncated pulse.
            assert!(d > 0 && i > 0 && c > 0, "{feel:?} has a zero field");
        }
    }
}
