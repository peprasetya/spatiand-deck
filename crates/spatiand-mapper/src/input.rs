//! What the mapper reads: every physical control, merged into one controller-shaped frame.

use serde::{Deserialize, Serialize};

/// A physical button a layout can bind.
///
/// The Deck's own controls, named as Steam names them, and the glasses' two temple buttons. A
/// Bluetooth pad's buttons arrive as the Deck button in the same place — its A is [`Button::A`]
/// — which is what lets one layout cover both hands on either device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    A,
    B,
    X,
    Y,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
    L1,
    R1,
    /// The click at the end of the trigger's travel. The analogue pull is [`Group::LeftTrigger`].
    ///
    /// [`Group::LeftTrigger`]: crate::layout::Group::LeftTrigger
    L2,
    R2,
    L4,
    R4,
    L5,
    R5,
    Menu,
    View,
    LStick,
    RStick,
    /// A thumb resting on a trackpad. The pads themselves are never bound — they are Spatiand's
    /// pointer in every layout — but *touching* one is the natural switch for gyro aiming.
    LPadTouch,
    RPadTouch,
    GlassesUp,
    GlassesDown,
}

impl Button {
    pub const ALL: [Button; 24] = [
        Button::A,
        Button::B,
        Button::X,
        Button::Y,
        Button::DpadUp,
        Button::DpadDown,
        Button::DpadLeft,
        Button::DpadRight,
        Button::L1,
        Button::R1,
        Button::L2,
        Button::R2,
        Button::L4,
        Button::R4,
        Button::L5,
        Button::R5,
        Button::Menu,
        Button::View,
        Button::LStick,
        Button::RStick,
        Button::LPadTouch,
        Button::RPadTouch,
        Button::GlassesUp,
        Button::GlassesDown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Button::A => "A",
            Button::B => "B",
            Button::X => "X",
            Button::Y => "Y",
            Button::DpadUp => "D-pad up",
            Button::DpadDown => "D-pad down",
            Button::DpadLeft => "D-pad left",
            Button::DpadRight => "D-pad right",
            Button::L1 => "L1 bumper",
            Button::R1 => "R1 bumper",
            Button::L2 => "L2 full pull",
            Button::R2 => "R2 full pull",
            Button::L4 => "L4 back button",
            Button::R4 => "R4 back button",
            Button::L5 => "L5 back button",
            Button::R5 => "R5 back button",
            Button::Menu => "Menu",
            Button::View => "View",
            Button::LStick => "Left stick click",
            Button::RStick => "Right stick click",
            Button::LPadTouch => "Left trackpad touch",
            Button::RPadTouch => "Right trackpad touch",
            Button::GlassesUp => "Glasses button up",
            Button::GlassesDown => "Glasses button down",
        }
    }

    fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// A set of buttons that are down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Buttons(u32);

impl Buttons {
    pub fn is_down(self, button: Button) -> bool {
        self.0 & button.bit() != 0
    }

    pub fn set(&mut self, button: Button, down: bool) {
        if down {
            self.0 |= button.bit();
        } else {
            self.0 &= !button.bit();
        }
    }

    pub fn with(mut self, button: Button) -> Self {
        self.set(button, true);
        self
    }

    pub fn any(self) -> bool {
        self.0 != 0
    }

    pub fn union(self, other: Buttons) -> Self {
        Self(self.0 | other.0)
    }
}

/// A finger on a trackpad. `x` and `y` run −1..1 with +y up, as the Deck reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Touch {
    pub x: f32,
    pub y: f32,
    pub touched: bool,
}

/// Rotation rates, degrees per second, in the holder's terms rather than a sensor's.
///
/// `pitch` is positive tipping the top away and the front up (aiming up), `yaw` positive turning
/// left, `roll` positive with the right side going down (steering right). Each source converts
/// its own axes into these before the mapper sees them, so a layout means the same thing for
/// the Deck and for the glasses.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rates {
    pub pitch: f32,
    pub yaw: f32,
    pub roll: f32,
}

/// Everything physical, as of one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Snapshot {
    pub buttons: Buttons,
    /// −1..1, +y up.
    pub left_stick: (f32, f32),
    pub right_stick: (f32, f32),
    pub left_pad: Touch,
    pub right_pad: Touch,
    /// 0..1.
    pub left_trigger: f32,
    pub right_trigger: f32,
    /// The controller's own gyro.
    pub gyro: Option<Rates>,
    /// The glasses' gyro, bias removed, in head terms.
    pub glasses_gyro: Option<Rates>,
}

impl Snapshot {
    /// Fold a second device into this one, so both read as a single controller.
    ///
    /// Buttons are either-or. A stick or trigger takes whichever device is pushed further —
    /// summing would let two resting sticks' noise add up, and the Deck's own stick drifting a
    /// hair must not cancel a Bluetooth stick held hard over. A trackpad takes the device being
    /// touched, and a gyro the first one present.
    pub fn merge(&mut self, other: &Snapshot) {
        self.buttons = self.buttons.union(other.buttons);
        self.left_stick = further(self.left_stick, other.left_stick);
        self.right_stick = further(self.right_stick, other.right_stick);
        if other.left_pad.touched && !self.left_pad.touched {
            self.left_pad = other.left_pad;
        }
        if other.right_pad.touched && !self.right_pad.touched {
            self.right_pad = other.right_pad;
        }
        self.left_trigger = self.left_trigger.max(other.left_trigger);
        self.right_trigger = self.right_trigger.max(other.right_trigger);
        if self.gyro.is_none() {
            self.gyro = other.gyro;
        }
        if self.glasses_gyro.is_none() {
            self.glasses_gyro = other.glasses_gyro;
        }
    }
}

fn further(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    if b.0 * b.0 + b.1 * b.1 > a.0 * a.0 + a.1 * a.1 {
        b
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_button_has_its_own_bit() {
        let mut seen = 0u32;
        for b in Button::ALL {
            assert_eq!(seen & b.bit(), 0, "{b:?} shares a bit");
            seen |= b.bit();
        }
    }

    #[test]
    fn a_second_pad_adds_its_buttons_and_wins_only_when_pushed_further() {
        let mut deck = Snapshot {
            left_stick: (0.02, 0.01),
            ..Default::default()
        };
        deck.buttons.set(Button::A, true);
        let mut bluetooth = Snapshot {
            left_stick: (-0.9, 0.0),
            right_trigger: 0.7,
            ..Default::default()
        };
        bluetooth.buttons.set(Button::R1, true);
        deck.merge(&bluetooth);
        assert!(deck.buttons.is_down(Button::A));
        assert!(deck.buttons.is_down(Button::R1));
        assert_eq!(deck.left_stick, (-0.9, 0.0));
        assert_eq!(deck.right_trigger, 0.7);

        // And a resting second pad does not pull a held stick back to centre.
        let resting = Snapshot {
            left_stick: (0.01, 0.0),
            ..Default::default()
        };
        deck.merge(&resting);
        assert_eq!(deck.left_stick, (-0.9, 0.0));
    }
}
