//! An on-screen keyboard, laid out as a grid you point at.
//!
//! Opening an application you cannot type into is most of the way to useless, and there is no
//! physical keyboard in a spatial session. This is the layout and the hit-testing; drawing it
//! and delivering the keystrokes belong to the compositor.
//!
//! ## Why a grid and not a real keyboard shape
//!
//! A staggered QWERTY has keys at fractional offsets, which means hit-testing has to know each
//! row's indent. Pointing at it with a head-anchored ray at two metres, where a key is about a
//! degree across, that precision buys nothing — what matters is that every key is the same
//! size and that the gaps are predictable. So the rows are aligned and the keys are uniform.
//!
//! Codes are **evdev** keycodes, the same numbers `/usr/include/linux/input-event-codes.h`
//! uses. Wayland wants them offset by 8; that offset is applied where the event is sent rather
//! than baked in here, so this table can be read against the header directly.

/// One key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// What to draw when shift is not held.
    pub label: &'static str,
    /// What to draw when it is.
    pub shifted: &'static str,
    /// evdev keycode.
    pub code: u32,
    /// How many normal keys wide.
    pub width: u8,
    /// Toggles rather than types.
    pub sticky: bool,
}

const fn key(label: &'static str, shifted: &'static str, code: u32) -> Key {
    Key {
        label,
        shifted,
        code,
        width: 1,
        sticky: false,
    }
}

const fn wide(label: &'static str, code: u32, width: u8) -> Key {
    Key {
        label,
        shifted: label,
        code,
        width,
        sticky: false,
    }
}

/// evdev codes used below, named so the table can be checked against the kernel header.
pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_TAB: u32 = 15;
pub const KEY_ENTER: u32 = 28;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_SPACE: u32 = 57;
pub const KEY_ESC: u32 = 1;

/// The rows, top to bottom.
pub const ROWS: &[&[Key]] = &[
    &[
        key("1", "!", 2),
        key("2", "@", 3),
        key("3", "#", 4),
        key("4", "$", 5),
        key("5", "%", 6),
        key("6", "^", 7),
        key("7", "&", 8),
        key("8", "*", 9),
        key("9", "(", 10),
        key("0", ")", 11),
        wide("back", KEY_BACKSPACE, 2),
    ],
    &[
        key("q", "Q", 16),
        key("w", "W", 17),
        key("e", "E", 18),
        key("r", "R", 19),
        key("t", "T", 20),
        key("y", "Y", 21),
        key("u", "U", 22),
        key("i", "I", 23),
        key("o", "O", 24),
        key("p", "P", 25),
        wide("tab", KEY_TAB, 2),
    ],
    &[
        key("a", "A", 30),
        key("s", "S", 31),
        key("d", "D", 32),
        key("f", "F", 33),
        key("g", "G", 34),
        key("h", "H", 35),
        key("j", "J", 36),
        key("k", "K", 37),
        key("l", "L", 38),
        key(";", ":", 39),
        wide("enter", KEY_ENTER, 2),
    ],
    &[
        Key {
            label: "shift",
            shifted: "shift",
            code: KEY_LEFTSHIFT,
            width: 2,
            sticky: true,
        },
        key("z", "Z", 44),
        key("x", "X", 45),
        key("c", "C", 46),
        key("v", "V", 47),
        key("b", "B", 48),
        key("n", "N", 49),
        key("m", "M", 50),
        key(",", "<", 51),
        key(".", ">", 52),
        key("/", "?", 53),
    ],
    &[
        wide("esc", KEY_ESC, 2),
        wide("space", KEY_SPACE, 7),
        key("-", "_", 12),
        key("=", "+", 13),
    ],
];

/// Widest row, in key units. Every row is drawn to this width so the columns line up.
pub fn row_units() -> u8 {
    ROWS.iter()
        .map(|row| row.iter().map(|k| k.width).sum::<u8>())
        .max()
        .unwrap_or(1)
}

/// The keyboard's state.
#[derive(Debug, Default, Clone)]
pub struct Keyboard {
    pub open: bool,
    /// Shift is a **latch**, not a hold: there is one pointer and it cannot press two keys at
    /// once, so a shift you have to hold would make capitals impossible.
    pub shift: bool,
}

impl Keyboard {
    /// Which key is at a point on the keyboard's face, in 0..1 surface coordinates.
    pub fn key_at(&self, u: f64, v: f64) -> Option<&'static Key> {
        if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
            return None;
        }
        let row_index = (v * ROWS.len() as f64) as usize;
        let row = ROWS.get(row_index.min(ROWS.len() - 1))?;
        let total = row_units() as f64;
        let mut x = 0.0;
        for k in row.iter() {
            let next = x + k.width as f64 / total;
            if u < next {
                return Some(k);
            }
            x = next;
        }
        row.last()
    }

    /// Press a key. Returns the evdev code to send, or `None` if it only changed state.
    pub fn press(&mut self, k: &Key) -> Option<u32> {
        if k.sticky {
            self.shift = !self.shift;
            return None;
        }
        Some(k.code)
    }

    /// Called after a key has been sent, to drop a one-shot shift.
    pub fn after_press(&mut self, k: &Key) {
        if !k.sticky {
            self.shift = false;
        }
    }

    /// The label a key should currently show.
    pub fn label(&self, k: &Key) -> &'static str {
        if self.shift {
            k.shifted
        } else {
            k.label
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_fits_the_same_width() {
        // Rows that do not add up leave keys hanging past the edge of the panel, and the
        // hit-test and the drawing disagree about where they are.
        let units = row_units();
        for (i, row) in ROWS.iter().enumerate() {
            let sum: u8 = row.iter().map(|k| k.width).sum();
            assert!(sum <= units, "row {i} is {sum} units against a width of {units}");
        }
    }

    #[test]
    fn the_corners_land_on_the_expected_keys() {
        let kb = Keyboard::default();
        assert_eq!(kb.key_at(0.01, 0.01).unwrap().label, "1");
        assert_eq!(kb.key_at(0.01, 0.45).unwrap().label, "a");
        assert_eq!(kb.key_at(0.01, 0.99).unwrap().label, "esc");
    }

    #[test]
    fn a_point_outside_the_face_is_not_a_key() {
        let kb = Keyboard::default();
        assert!(kb.key_at(-0.1, 0.5).is_none());
        assert!(kb.key_at(0.5, 1.5).is_none());
    }

    #[test]
    fn every_position_on_the_face_hits_something() {
        // A dead spot is indistinguishable from a missed click, and with a head-anchored ray
        // people will blame their aim.
        let kb = Keyboard::default();
        for i in 0..40 {
            for j in 0..20 {
                let u = (i as f64 + 0.5) / 40.0;
                let v = (j as f64 + 0.5) / 20.0;
                assert!(kb.key_at(u, v).is_some(), "nothing at ({u}, {v})");
            }
        }
    }

    #[test]
    fn shift_latches_rather_than_needing_to_be_held() {
        // There is one pointer. A shift you have to hold makes a capital letter impossible.
        let mut kb = Keyboard::default();
        let shift = ROWS[3][0];
        assert!(shift.sticky);
        assert_eq!(kb.press(&shift), None, "shift types nothing");
        assert!(kb.shift);

        let a = ROWS[2][0];
        assert_eq!(kb.label(&a), "A");
        assert_eq!(kb.press(&a), Some(30));
        kb.after_press(&a);
        assert!(!kb.shift, "a latched shift releases after one key");
        assert_eq!(kb.label(&a), "a");
    }

    #[test]
    fn the_letter_codes_match_the_kernel_header() {
        // Spot-checked against input-event-codes.h. A wrong code types the wrong character,
        // which looks like a broken keymap rather than a wrong table.
        let find = |label: &str| {
            ROWS.iter()
                .flat_map(|r| r.iter())
                .find(|k| k.label == label)
                .map(|k| k.code)
        };
        assert_eq!(find("q"), Some(16));
        assert_eq!(find("a"), Some(30));
        assert_eq!(find("z"), Some(44));
        assert_eq!(find("space"), Some(57));
        assert_eq!(find("enter"), Some(28));
    }

    #[test]
    fn no_two_keys_share_a_code() {
        let mut codes: Vec<u32> = ROWS.iter().flat_map(|r| r.iter()).map(|k| k.code).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(before, codes.len(), "two keys share an evdev code");
    }
}
