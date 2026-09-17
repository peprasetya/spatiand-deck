//! Keyboard keys a layout can press, by evdev code, with the names a picker shows.

/// In the order a picker lists them: the keys games use most first.
pub const KEYS: &[(u16, &str)] = &[
    (17, "W"),
    (30, "A"),
    (31, "S"),
    (32, "D"),
    (57, "Space"),
    (42, "Left Shift"),
    (29, "Left Ctrl"),
    (56, "Left Alt"),
    (15, "Tab"),
    (1, "Esc"),
    (28, "Enter"),
    (14, "Backspace"),
    (16, "Q"),
    (18, "E"),
    (19, "R"),
    (33, "F"),
    (34, "G"),
    (35, "H"),
    (44, "Z"),
    (45, "X"),
    (46, "C"),
    (47, "V"),
    (48, "B"),
    (20, "T"),
    (21, "Y"),
    (22, "U"),
    (23, "I"),
    (24, "O"),
    (25, "P"),
    (36, "J"),
    (37, "K"),
    (38, "L"),
    (49, "N"),
    (50, "M"),
    (2, "1"),
    (3, "2"),
    (4, "3"),
    (5, "4"),
    (6, "5"),
    (7, "6"),
    (8, "7"),
    (9, "8"),
    (10, "9"),
    (11, "0"),
    (103, "Up"),
    (108, "Down"),
    (105, "Left"),
    (106, "Right"),
    (59, "F1"),
    (60, "F2"),
    (61, "F3"),
    (62, "F4"),
    (63, "F5"),
    (64, "F6"),
    (65, "F7"),
    (66, "F8"),
    (67, "F9"),
    (68, "F10"),
    (87, "F11"),
    (88, "F12"),
    (54, "Right Shift"),
    (97, "Right Ctrl"),
    (100, "Right Alt"),
    (125, "Super"),
    (58, "Caps Lock"),
    (102, "Home"),
    (107, "End"),
    (104, "Page Up"),
    (109, "Page Down"),
    (110, "Insert"),
    (111, "Delete"),
    (12, "-"),
    (13, "="),
    (26, "["),
    (27, "]"),
    (39, ";"),
    (40, "'"),
    (41, "`"),
    (43, "\\"),
    (51, ","),
    (52, "."),
    (53, "/"),
    (82, "Keypad 0"),
    (79, "Keypad 1"),
    (80, "Keypad 2"),
    (81, "Keypad 3"),
    (75, "Keypad 4"),
    (76, "Keypad 5"),
    (77, "Keypad 6"),
    (71, "Keypad 7"),
    (72, "Keypad 8"),
    (73, "Keypad 9"),
    (96, "Keypad Enter"),
    (113, "Mute"),
    (114, "Volume down"),
    (115, "Volume up"),
    (164, "Play/Pause"),
    (163, "Next track"),
    (165, "Previous track"),
];

pub fn name(code: u16) -> Option<&'static str> {
    KEYS.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_key_is_listed_twice() {
        let mut seen = std::collections::HashSet::new();
        for (code, name) in KEYS {
            assert!(seen.insert(*code), "{name} ({code}) listed twice");
        }
    }

    #[test]
    fn the_movement_keys_have_their_evdev_codes() {
        // A transposition here is a game that walks backwards.
        assert_eq!(name(17), Some("W"));
        assert_eq!(name(30), Some("A"));
        assert_eq!(name(31), Some("S"));
        assert_eq!(name(32), Some("D"));
    }
}
