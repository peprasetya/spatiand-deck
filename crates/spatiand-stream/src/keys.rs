//! Keys by name, for anything that has to press them without a keyboard.
//!
//! A test that copies in one application and pastes in another has to say "Control and C"
//! somehow, and so does a person at a terminal asking a running compositor to do the same.
//! Both ends of the link understand the same names, which is why they are here and not in
//! either of them.
//!
//! Every code is a Linux evdev code, the numbers in `linux/input-event-codes.h`. Each end adds
//! the eight XKB wants where it hands a key to the seat, as it already does for real keys.
//! Typing assumes a US layout, which is the layout both compositors give their seat.

/// A key and the modifiers held while it is pressed, in the order they go down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stroke {
    pub held: Vec<u32>,
    pub key: u32,
}

const LEFTCTRL: u32 = 29;
const LEFTSHIFT: u32 = 42;
const LEFTALT: u32 = 56;
const LEFTMETA: u32 = 125;

/// Letters and digits in evdev order, which follows the keyboard's rows, not the alphabet.
const LETTERS: [(char, u32); 26] = [
    ('q', 16), ('w', 17), ('e', 18), ('r', 19), ('t', 20), ('y', 21), ('u', 22), ('i', 23),
    ('o', 24), ('p', 25), ('a', 30), ('s', 31), ('d', 32), ('f', 33), ('g', 34), ('h', 35),
    ('j', 36), ('k', 37), ('l', 38), ('z', 44), ('x', 45), ('c', 46), ('v', 47), ('b', 48),
    ('n', 49), ('m', 50),
];
const DIGITS: [u32; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];

/// Punctuation a US keyboard types without Shift, and what it types with it.
const PUNCTUATION: [(char, char, u32); 11] = [
    ('-', '_', 12), ('=', '+', 13), ('[', '{', 26), (']', '}', 27), (';', ':', 39),
    ('\'', '"', 40), ('`', '~', 41), ('\\', '|', 43), (',', '<', 51), ('.', '>', 52),
    ('/', '?', 53),
];
const SHIFTED_DIGITS: [char; 10] = [')', '!', '@', '#', '$', '%', '^', '&', '*', '('];

/// A key named on its own: `a`, `5`, `return`, `f5`, `-`.
fn named(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let mut chars = lower.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if let Some(&(_, code)) = LETTERS.iter().find(|(l, _)| *l == c) {
            return Some(code);
        }
        if let Some(d) = c.to_digit(10) {
            return Some(DIGITS[d as usize]);
        }
        if let Some(&(_, _, code)) = PUNCTUATION.iter().find(|(p, _, _)| *p == c) {
            return Some(code);
        }
    }
    Some(match lower.as_str() {
        "escape" | "esc" => 1,
        "backspace" => 14,
        "tab" => 15,
        "return" | "enter" => 28,
        "space" => 57,
        "home" => 102,
        "up" => 103,
        "pageup" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "pagedown" => 109,
        "insert" => 110,
        "delete" | "del" => 111,
        f if f.starts_with('f') => match f[1..].parse::<u32>() {
            Ok(n @ 1..=10) => 58 + n,
            Ok(11) => 87,
            Ok(12) => 88,
            _ => return None,
        },
        _ => return None,
    })
}

fn modifier(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => LEFTCTRL,
        "shift" => LEFTSHIFT,
        "alt" => LEFTALT,
        "super" | "meta" | "logo" => LEFTMETA,
        _ => return None,
    })
}

/// One chord, as a person writes it: `ctrl+shift+v`, `return`, `a`.
pub fn chord(text: &str) -> Result<Stroke, String> {
    let parts: Vec<&str> = text.split('+').collect();
    // A chord ending in "+" is the plus key itself: `ctrl++` is Control and plus.
    let (mods, key) = match parts.as_slice() {
        [rest @ .., "", ""] if !rest.is_empty() => (rest, "+"),
        [rest @ .., key] => (rest, *key),
        [] => return Err("an empty chord".into()),
    };
    let mut held = Vec::new();
    for m in mods {
        held.push(modifier(m).ok_or_else(|| format!("{m:?} is not a modifier"))?);
    }
    if key == "+" {
        held.push(LEFTSHIFT);
        return Ok(Stroke { held, key: 13 });
    }
    let key = named(key).ok_or_else(|| format!("{key:?} is not a key this knows"))?;
    Ok(Stroke { held, key })
}

/// The strokes that type `text` on a US layout.
///
/// Printable ASCII only. Anything else is refused rather than dropped, so a test cannot pass
/// by typing less than it was asked to.
pub fn typing(text: &str) -> Result<Vec<Stroke>, String> {
    text.chars()
        .map(|c| {
            let plain = |key| Stroke { held: vec![], key };
            let shifted = |key| Stroke { held: vec![LEFTSHIFT], key };
            if c == ' ' {
                return Ok(plain(57));
            }
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                return Ok(plain(named(&c.to_string()).expect("letters and digits are named")));
            }
            if c.is_ascii_uppercase() {
                let lower = c.to_ascii_lowercase().to_string();
                return Ok(shifted(named(&lower).expect("letters are named")));
            }
            if let Some(d) = SHIFTED_DIGITS.iter().position(|&s| s == c) {
                return Ok(shifted(DIGITS[d]));
            }
            if let Some(&(_, _, code)) = PUNCTUATION.iter().find(|(p, _, _)| *p == c) {
                return Ok(plain(code));
            }
            if let Some(&(_, _, code)) = PUNCTUATION.iter().find(|(_, s, _)| *s == c) {
                return Ok(shifted(code));
            }
            Err(format!("{c:?} cannot be typed"))
        })
        .collect()
}

/// Every transition a stroke makes, in order: modifiers down, the key down and up, modifiers
/// up in reverse. `(code, pressed)`.
pub fn transitions(stroke: &Stroke) -> Vec<(u32, bool)> {
    let mut out: Vec<(u32, bool)> = stroke.held.iter().map(|&c| (c, true)).collect();
    out.push((stroke.key, true));
    out.push((stroke.key, false));
    out.extend(stroke.held.iter().rev().map(|&c| (c, false)));
    out
}

/// Bytes as a line of text that survives any terminal: printable ASCII as itself, everything
/// else — and `%` — as `%XX`. What a compositor's control socket answers a clipboard with.
pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x21..0x7f).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_and_paste_are_the_chords_everyone_means() {
        assert_eq!(chord("ctrl+c").unwrap(), Stroke { held: vec![29], key: 46 });
        assert_eq!(chord("Ctrl+Shift+V").unwrap(), Stroke { held: vec![29, 42], key: 47 });
        assert_eq!(chord("return").unwrap(), Stroke { held: vec![], key: 28 });
    }

    #[test]
    fn a_misspelt_key_is_refused_rather_than_skipped() {
        assert!(chord("ctrl+cc").is_err());
        assert!(chord("ctl+c").is_err());
    }

    #[test]
    fn the_rows_are_in_evdev_order_not_the_alphabets() {
        // Q is the first letter key and M the last, the way a keyboard is wired.
        assert_eq!(chord("q").unwrap().key, 16);
        assert_eq!(chord("a").unwrap().key, 30);
        assert_eq!(chord("m").unwrap().key, 50);
        assert_eq!(chord("0").unwrap().key, 11);
        assert_eq!(chord("1").unwrap().key, 2);
    }

    #[test]
    fn capitals_and_symbols_are_typed_with_shift() {
        let typed = typing("aA_!").unwrap();
        assert_eq!(typed[0], Stroke { held: vec![], key: 30 });
        assert_eq!(typed[1], Stroke { held: vec![42], key: 30 });
        assert_eq!(typed[2], Stroke { held: vec![42], key: 12 });
        assert_eq!(typed[3], Stroke { held: vec![42], key: 2 });
    }

    #[test]
    fn what_cannot_be_typed_is_refused() {
        assert!(typing("café").is_err());
    }

    #[test]
    fn a_stroke_lets_go_in_the_reverse_order_it_took_hold() {
        let t = transitions(&chord("ctrl+shift+v").unwrap());
        assert_eq!(t, vec![(29, true), (42, true), (47, true), (47, false), (42, false), (29, false)]);
    }

    #[test]
    fn escaped_bytes_read_back_as_themselves() {
        assert_eq!(escape(b"clip-42"), "clip-42");
        assert_eq!(escape(b"a b%\n\xff"), "a%20b%25%0A%FF");
    }
}
