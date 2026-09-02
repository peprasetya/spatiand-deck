//! Which bit is which in the Deck's vendor report.
//!
//! This is a **data table on purpose**. The offsets in `docs/steam-deck-controller.md` come
//! from three different places — some observed on this hardware, some read out of
//! `hid-steam.c`, some inferred — and mixing those confidence levels into `if` statements
//! scattered through the shell would make a wrong bit impossible to find. Here, correcting one
//! is a one-line edit and every consumer picks it up.
//!
//! `examples/probe.rs` is the other half of this: it prints the *named* control for every bit
//! that changes, so five minutes of pressing buttons turns [`Confidence::Kernel`] entries into
//! [`Confidence::Verified`] ones without touching any code that uses them.

/// A named control on the Deck's body.
///
/// Named for what the wearer calls it, not for what the kernel calls it: the shell should read
/// `Control::Steam`, not `BTN_MODE`. Anything that needs the evdev name can look it up here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Control {
    A,
    B,
    X,
    Y,
    Up,
    Down,
    Left,
    Right,
    /// Shoulder bumpers.
    L1,
    R1,
    /// Triggers, as a digital full-pull. The analog value is separate.
    L2,
    R2,
    /// Back paddles, upper pair.
    L4,
    R4,
    /// Back paddles, lower pair.
    L5,
    R5,
    /// The STEAM button. Opens the HUD.
    Steam,
    /// The `⋯` button below STEAM — Valve calls it QAM. Opens the launcher.
    Quick,
    /// `☰`, right of the right stick.
    Menu,
    /// `⧉`, left of the left stick.
    View,
    LPadClick,
    RPadClick,
    /// Finger resting on the pad, with no click. This is what drives the pointer.
    LPadTouch,
    RPadTouch,
    LStickClick,
    RStickClick,
}

impl Control {
    /// Every control, in a stable order. Used by the probe and by tests that must not silently
    /// skip a control added later.
    pub const ALL: [Control; 26] = [
        Control::A,
        Control::B,
        Control::X,
        Control::Y,
        Control::Up,
        Control::Down,
        Control::Left,
        Control::Right,
        Control::L1,
        Control::R1,
        Control::L2,
        Control::R2,
        Control::L4,
        Control::R4,
        Control::L5,
        Control::R5,
        Control::Steam,
        Control::Quick,
        Control::Menu,
        Control::View,
        Control::LPadClick,
        Control::RPadClick,
        Control::LPadTouch,
        Control::RPadTouch,
        Control::LStickClick,
        Control::RStickClick,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Control::A => "A",
            Control::B => "B",
            Control::X => "X",
            Control::Y => "Y",
            Control::Up => "D-pad up",
            Control::Down => "D-pad down",
            Control::Left => "D-pad left",
            Control::Right => "D-pad right",
            Control::L1 => "L1",
            Control::R1 => "R1",
            Control::L2 => "L2",
            Control::R2 => "R2",
            Control::L4 => "L4",
            Control::R4 => "R4",
            Control::L5 => "L5",
            Control::R5 => "R5",
            Control::Steam => "STEAM",
            Control::Quick => "quick access (...)",
            Control::Menu => "menu",
            Control::View => "view",
            Control::LPadClick => "left pad click",
            Control::RPadClick => "right pad click",
            Control::LPadTouch => "left pad touch",
            Control::RPadTouch => "right pad touch",
            Control::LStickClick => "left stick click",
            Control::RStickClick => "right stick click",
        }
    }

    /// Bit position within the 64-bit button field, or `None` if we do not know it yet.
    pub fn bit(self) -> Option<u8> {
        BITS.iter().find(|(c, _, _)| *c == self).map(|(_, b, _)| *b)
    }

    pub fn confidence(self) -> Confidence {
        BITS.iter()
            .find(|(c, _, _)| *c == self)
            .map(|(_, _, k)| *k)
            .unwrap_or(Confidence::Unknown)
    }
}

/// How much to trust a table entry. Kept in the table rather than in a comment so the probe can
/// report it and so tests can assert that nothing the shell depends on is still a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Seen change on this hardware when the control was operated.
    Verified,
    /// Read out of `drivers/hid/hid-steam.c`. Very likely right, never exercised here.
    Kernel,
    /// Reasoning from surrounding entries. Treat as a starting point for the probe.
    Guess,
    Unknown,
}

/// Bit index within the little-endian u64 at report offset 8, i.e. `byte * 8 + bit`.
///
/// Byte 8 carries the face buttons and shoulders; byte 9 the D-pad and the three system
/// buttons; byte 10 the paddles and pad contacts. Beyond that the kernel's mapping is sparse
/// and the entries below are correspondingly less certain — which is exactly why the
/// confidence column exists.
type Entry = (Control, u8, Confidence);

pub const BITS: &[Entry] = &[
    // --- byte 8: face buttons, shoulders, triggers-as-digital ---
    // Every entry in this byte was pressed on hardware and matched `hid-steam.c` exactly,
    // which is also the strongest evidence that the whole report layout in
    // `docs/steam-deck-controller.md` §5 is right.
    (Control::R2, 0, Confidence::Verified),
    (Control::L2, 1, Confidence::Verified),
    (Control::R1, 2, Confidence::Verified),
    (Control::L1, 3, Confidence::Verified),
    (Control::Y, 4, Confidence::Verified),
    (Control::B, 5, Confidence::Verified),
    (Control::X, 6, Confidence::Verified),
    (Control::A, 7, Confidence::Verified),
    // --- byte 9: d-pad and system buttons ---
    (Control::Up, 8, Confidence::Verified),
    (Control::Right, 9, Confidence::Verified),
    (Control::Left, 10, Confidence::Verified),
    (Control::Down, 11, Confidence::Verified),
    // View and Menu are the one pair worth a note. An early run appeared to show them
    // swapped, which turned out to be the order they were pressed in and not the mapping; a
    // second, cleaner run put them exactly where the kernel says. Trusting the first reading
    // would have quietly transposed two buttons.
    (Control::View, 12, Confidence::Verified),
    (Control::Steam, 13, Confidence::Verified),
    (Control::Menu, 14, Confidence::Verified),
    (Control::L5, 15, Confidence::Verified),
    // --- byte 10: paddles, pad contact, stick click ---
    (Control::R5, 16, Confidence::Verified),
    // Touch and click are separate bits, and clicking raises both — a click is a firm touch.
    // Anything that treats them as alternatives will drop the pointer the moment you press.
    (Control::LPadClick, 17, Confidence::Verified),
    (Control::RPadClick, 18, Confidence::Verified),
    (Control::LPadTouch, 19, Confidence::Verified),
    (Control::RPadTouch, 20, Confidence::Verified),
    (Control::LStickClick, 22, Confidence::Verified),
    // --- byte 11 onwards (bit index is relative to byte 8, so byte 13 starts at 40) ---
    (Control::RStickClick, 26, Confidence::Verified),
    (Control::L4, 41, Confidence::Verified),
    (Control::R4, 42, Confidence::Verified),
    // Bits 46 and 47 (byte 13, bits 6 and 7) were seen going high while a stick was merely
    // being held, and 47 sits high at rest often enough to be filtered as noise. Almost
    // certainly capacitive stick-touch sensing. Nothing needs them yet.
    // The QAM button is the one control with no evdev equivalent on a normal gamepad, and the
    // kernel reports it as BTN_BASE out of byte 14. Spatiand's launcher hangs off it, so it
    // was the single most important entry here to confirm — and it did, at byte 14 bit 2.
    (Control::Quick, 50, Confidence::Verified),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_control_has_a_bit() {
        // A control the shell can name but the parser can never report is worse than one that
        // does not exist: it looks wired up and silently never fires.
        for c in Control::ALL {
            assert!(c.bit().is_some(), "{} has no bit assigned", c.name());
        }
    }

    #[test]
    fn no_two_controls_share_a_bit() {
        // A duplicate makes two buttons indistinguishable, which presents as "the launcher
        // opens when I press B" and is maddening to trace back to a table.
        for (i, (a, bit_a, _)) in BITS.iter().enumerate() {
            for (b, bit_b, _) in &BITS[i + 1..] {
                assert_ne!(
                    bit_a,
                    bit_b,
                    "{} and {} share bit {bit_a}",
                    a.name(),
                    b.name()
                );
            }
        }
    }

    #[test]
    fn bits_fit_the_eight_byte_button_field() {
        for (c, bit, _) in BITS {
            assert!(
                *bit < 64,
                "{} is at bit {bit}, past the button field",
                c.name()
            );
        }
    }

    #[test]
    fn every_control_the_shell_binds_was_confirmed_on_hardware() {
        // Navigation, the two menu buttons and the pad contacts are what make Spatiand usable
        // at all. All of them were pressed on a real Deck and matched; a future edit that
        // introduces a guess here should have to argue with this test first.
        for c in [
            Control::A,
            Control::B,
            Control::Up,
            Control::Down,
            Control::Left,
            Control::Right,
            Control::Steam,
            Control::Quick,
            Control::LPadTouch,
            Control::RPadTouch,
            Control::RPadClick,
        ] {
            assert_eq!(
                c.confidence(),
                Confidence::Verified,
                "{} is not verified",
                c.name()
            );
        }
    }

    #[test]
    fn pad_touch_and_pad_click_are_distinct_bits() {
        // Clicking a pad raises both, so treating them as one control loses the ability to
        // tell "pointing" from "selecting" — the pointer would vanish on every click.
        assert_ne!(Control::RPadTouch.bit(), Control::RPadClick.bit());
        assert_ne!(Control::LPadTouch.bit(), Control::LPadClick.bit());
    }
}
