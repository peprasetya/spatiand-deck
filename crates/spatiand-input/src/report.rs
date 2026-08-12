//! Decoding one 64-byte vendor report into something the shell can reason about.
//!
//! Everything here is a pure function of a byte slice, which is the point: the parsing is the
//! part most likely to be subtly wrong, and it is also the part that needs no hardware to test.
//! `docs/steam-deck-controller.md` §5 is the layout this implements.
//!
//! Scale factors are chosen so that the units mean something physical — g and deg/s, matching
//! [`spatiand_hmd::ImuSample`] — rather than raw counts. A resting controller must read
//! `|accel| ≈ 1.0`; that single invariant is what caught the accelerometer offset during
//! bring-up, and the test below keeps it honest.

use crate::layout::{Control, BITS};

/// Header of a Deck input report: version `0x0001`, type `0x09`, length `0x40`.
pub const REPORT_HEADER: [u8; 4] = [0x01, 0x00, 0x09, 0x40];
pub const REPORT_LEN: usize = 64;

/// Counts per g. Verified: at rest the raw vector read `(-716, 298, 16664)`, magnitude 1.018 g.
const ACCEL_COUNTS_PER_G: f32 = 16384.0;
/// Full-scale gyro range, deg/s, spread over a signed 16-bit field.
const GYRO_FULL_SCALE_DPS: f32 = 2000.0;

/// A set of pressed controls.
///
/// Wraps the raw u64 rather than exposing it, so nothing downstream can accidentally start
/// depending on a bit position and quietly bypass `layout.rs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Buttons(u64);

impl Buttons {
    pub fn from_raw(bits: u64) -> Self {
        Self(bits)
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    pub fn is_down(self, control: Control) -> bool {
        match control.bit() {
            Some(bit) => self.0 & (1u64 << bit) != 0,
            None => false,
        }
    }

    pub fn any(self) -> bool {
        self.0 != 0
    }

    /// Controls that went down between `previous` and `self`.
    ///
    /// Edge detection has to happen here rather than in the shell, because reports arrive at
    /// 250 Hz: a shell that reacted to the *level* would see a single button press as a couple
    /// of hundred activations and the launcher would scroll away from you.
    pub fn pressed_since(self, previous: Buttons) -> impl Iterator<Item = Control> {
        let newly = self.0 & !previous.0;
        Control::ALL
            .into_iter()
            .filter(move |c| match c.bit() {
                Some(bit) => newly & (1u64 << bit) != 0,
                None => false,
            })
    }

    pub fn released_since(self, previous: Buttons) -> impl Iterator<Item = Control> {
        let gone = previous.0 & !self.0;
        Control::ALL
            .into_iter()
            .filter(move |c| match c.bit() {
                Some(bit) => gone & (1u64 << bit) != 0,
                None => false,
            })
    }

    /// Every named control currently down. Used by the probe and by debug overlays.
    pub fn iter(self) -> impl Iterator<Item = Control> {
        BITS.iter()
            .filter(move |(_, bit, _)| self.0 & (1u64 << bit) != 0)
            .map(|(c, _, _)| *c)
    }
}

/// One touchpad.
///
/// `x` and `y` are normalised to −1..1 with **+x right and +y up**, matching the world frame's
/// handedness rather than the report's. The raw report has +y pointing down, which is a screen
/// convention and would silently invert every vertical gesture if it leaked out of here.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pad {
    pub x: f32,
    pub y: f32,
    pub touched: bool,
    pub clicked: bool,
}

impl Pad {
    /// Distance of the contact from the centre of the pad, 0..~1.41 at the corners.
    pub fn radius(&self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

/// Everything one report carries.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ControllerState {
    /// Monotonic frame counter. The only field that keeps moving while Steam owns the device,
    /// which is how bring-up told "we are not reading it" from "Steam has zeroed it".
    pub sequence: u32,
    pub buttons: Buttons,
    pub left_pad: Pad,
    pub right_pad: Pad,
    /// Analog sticks, −1..1, +y up.
    pub left_stick: (f32, f32),
    pub right_stick: (f32, f32),
    /// Analog triggers, 0..1.
    pub left_trigger: f32,
    pub right_trigger: f32,
    /// g, device frame.
    pub accel: [f32; 3],
    /// deg/s, device frame.
    pub gyro: [f32; 3],
}

impl ControllerState {
    /// Decode a report, or `None` if it is not one.
    ///
    /// Rejecting on the header matters: the same hidraw node also carries replies to the
    /// feature reports we send while taking the device, and decoding one of those as input
    /// produces a burst of nonsense button presses at exactly the moment the shell starts.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < REPORT_LEN || data[..4] != REPORT_HEADER {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
        let i16_at = |o: usize| i16::from_le_bytes([data[o], data[o + 1]]);
        let axis = |o: usize| i16_at(o) as f32 / i16::MAX as f32;

        let buttons = Buttons::from_raw(u64::from_le_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]));

        let pad = |x_at: usize, touch: Control, click: Control| Pad {
            x: axis(x_at),
            // Report y grows downward; the world's grows up.
            y: -axis(x_at + 2),
            touched: buttons.is_down(touch),
            clicked: buttons.is_down(click),
        };

        Some(Self {
            sequence: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
            buttons,
            left_pad: pad(16, Control::LPadTouch, Control::LPadClick),
            right_pad: pad(20, Control::RPadTouch, Control::RPadClick),
            left_stick: (axis(48), -axis(50)),
            right_stick: (axis(52), -axis(54)),
            left_trigger: u16_at(44) as f32 / i16::MAX as f32,
            right_trigger: u16_at(46) as f32 / i16::MAX as f32,
            accel: [
                i16_at(24) as f32 / ACCEL_COUNTS_PER_G,
                i16_at(26) as f32 / ACCEL_COUNTS_PER_G,
                i16_at(28) as f32 / ACCEL_COUNTS_PER_G,
            ],
            gyro: [
                i16_at(30) as f32 * GYRO_FULL_SCALE_DPS / 32768.0,
                i16_at(32) as f32 * GYRO_FULL_SCALE_DPS / 32768.0,
                i16_at(34) as f32 * GYRO_FULL_SCALE_DPS / 32768.0,
            ],
        })
    }

    /// Magnitude of the measured acceleration, in g.
    ///
    /// A cheap health check with a known answer: a controller that is not being thrown about
    /// reads 1.0. Anything else means the offsets are wrong, and every symptom downstream will
    /// look like a filter or gesture bug instead.
    pub fn accel_magnitude(&self) -> f32 {
        let [x, y, z] = self.accel;
        (x * x + y * y + z * z).sqrt()
    }

    /// True while Steam still owns the device.
    ///
    /// Steam configures the controller for itself and turns gyro reporting off, so every
    /// payload field reads zero while the sequence counter keeps advancing. Without this check
    /// the symptom is a spatial desktop where nothing responds and no error is ever logged.
    pub fn looks_silenced(&self) -> bool {
        self.sequence != 0 && self.accel == [0.0; 3] && self.gyro == [0.0; 3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report with the correct header and everything else zero.
    fn blank() -> [u8; REPORT_LEN] {
        let mut r = [0u8; REPORT_LEN];
        r[..4].copy_from_slice(&REPORT_HEADER);
        r
    }

    fn put_i16(r: &mut [u8], at: usize, v: i16) {
        r[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn rejects_anything_that_is_not_an_input_report() {
        assert!(ControllerState::parse(&[]).is_none());
        assert!(ControllerState::parse(&[0u8; REPORT_LEN]).is_none());
        // Right header but truncated — a short read must not be decoded from stale bytes.
        let short = blank();
        assert!(ControllerState::parse(&short[..32]).is_none());
    }

    #[test]
    fn decodes_the_resting_accelerometer_capture() {
        // The exact raw vector observed on this Deck lying on a desk. Magnitude is a physical
        // constant, so this fails loudly if the offsets or the scale ever move.
        let mut r = blank();
        put_i16(&mut r, 24, -716);
        put_i16(&mut r, 26, 298);
        put_i16(&mut r, 28, 16664);
        let s = ControllerState::parse(&r).expect("valid report");
        assert!(
            (s.accel_magnitude() - 1.018).abs() < 0.01,
            "expected ~1.018 g, got {}",
            s.accel_magnitude()
        );
        assert!(s.accel[2] > 0.9, "gravity should be on +Z when flat");
    }

    #[test]
    fn pad_y_points_up() {
        // The report's +y is down. If this inversion is ever dropped, dragging a window up
        // sends it down and scrolling runs backwards — both of which read as "the gesture code
        // is wrong" rather than as a decode bug.
        let mut r = blank();
        put_i16(&mut r, 22, i16::MAX); // right pad Y at full deflection
        let s = ControllerState::parse(&r).expect("valid report");
        assert!(s.right_pad.y < -0.99, "got {}", s.right_pad.y);
    }

    #[test]
    fn pads_are_normalised_to_the_unit_square() {
        let mut r = blank();
        put_i16(&mut r, 16, i16::MAX);
        put_i16(&mut r, 18, i16::MIN);
        let s = ControllerState::parse(&r).expect("valid report");
        assert!((s.left_pad.x - 1.0).abs() < 1e-3);
        assert!(s.left_pad.y <= 1.001 && s.left_pad.y >= 0.999);
    }

    #[test]
    fn an_untouched_pad_is_not_reported_as_a_contact_at_the_origin() {
        // Pads read (0,0) when nobody is touching them, so contact has to come from the touch
        // bit. Inferring it from "coordinates are non-zero" would park a phantom pointer dead
        // centre for as long as nobody touches anything.
        let s = ControllerState::parse(&blank()).expect("valid report");
        assert!(!s.left_pad.touched && !s.right_pad.touched);
        assert_eq!((s.left_pad.x, s.left_pad.y), (0.0, 0.0));
    }

    #[test]
    fn buttons_decode_through_the_layout_table() {
        let mut r = blank();
        let bit = Control::A.bit().expect("A has a bit");
        r[8 + (bit / 8) as usize] |= 1 << (bit % 8);
        let s = ControllerState::parse(&r).expect("valid report");
        assert!(s.buttons.is_down(Control::A));
        assert!(!s.buttons.is_down(Control::B));
    }

    #[test]
    fn edges_fire_once_not_every_frame() {
        // The failure this prevents: holding D-pad down at 250 Hz scrolling a menu past the
        // end before you have let go.
        let none = Buttons::default();
        let a = Buttons::from_raw(1 << Control::A.bit().unwrap());
        assert_eq!(a.pressed_since(none).collect::<Vec<_>>(), vec![Control::A]);
        assert_eq!(a.pressed_since(a).count(), 0, "a held button is not a press");
        assert_eq!(none.released_since(a).collect::<Vec<_>>(), vec![Control::A]);
    }

    #[test]
    fn steam_owning_the_device_is_detectable() {
        let mut r = blank();
        r[4..8].copy_from_slice(&12345u32.to_le_bytes());
        let s = ControllerState::parse(&r).expect("valid report");
        assert!(s.looks_silenced(), "all-zero payload with a live sequence");

        put_i16(&mut r, 28, 16384);
        let live = ControllerState::parse(&r).expect("valid report");
        assert!(!live.looks_silenced());
    }

    #[test]
    fn gyro_full_scale_is_two_thousand_degrees_per_second() {
        let mut r = blank();
        put_i16(&mut r, 30, i16::MAX);
        let s = ControllerState::parse(&r).expect("valid report");
        assert!((s.gyro[0] - 2000.0).abs() < 1.0, "got {}", s.gyro[0]);
    }
}
