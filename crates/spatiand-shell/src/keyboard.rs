//! An on-screen keyboard: the layout, the latching modifiers, and the hit-testing.
//!
//! Opening an application you cannot type into is most of the way to useless, and there is no
//! physical keyboard in a spatial session. This module owns the shape of the thing and what a
//! point on it means; drawing it and delivering the keystrokes belong to the compositor.
//!
//! ## Why a uniform grid and not a staggered one
//!
//! A staggered QWERTY has keys at fractional offsets, which means hit-testing has to know each
//! row's indent. Pointing at it with a head-anchored ray at two metres, where a key is about a
//! degree across, that precision buys nothing — what matters is that every key is the same
//! height and that the columns are predictable. So the rows are aligned and widths are whole
//! numbers of a small unit.
//!
//! Widths are in **quarter-keys**: an ordinary letter is [`UNIT`] = 4, so a 1.5-wide tab is 6
//! and a 2.25-wide enter is 9. Integers rather than floats because every row has to add up to
//! exactly [`ROW_UNITS`], and that is an equality worth being able to assert.
//!
//! ## The strip above the keys
//!
//! The face is not only keys: a strip along the top carries the sound toggle. It is part of
//! the face rather than of the resize border because it has to be *hittable* — see
//! [`CHROME_UNITS`] — and part of the face rather than a thing beside it because the face is
//! rasterised as one image and hit-tested in one coordinate system, and splitting either of
//! those is how a control comes to be drawn in one place and pressed in another.
//!
//! Codes are **evdev** keycodes, the same numbers `/usr/include/linux/input-event-codes.h`
//! uses. Wayland wants them offset by 8; that offset is applied where the event is sent rather
//! than baked in here, so this table can be read against the header directly.

/// What pressing a key does besides typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Sends its code.
    Normal,
    /// Latches a modifier instead of sending anything.
    Modifier(Modifier),
}

/// The modifiers this keyboard can hold.
///
/// Ctrl and Alt earn their place: `Ctrl+L` is how you reach a browser's address bar, and
/// without them the keyboard can type into a page but cannot drive the application around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Shift,
    Ctrl,
    Alt,
}

/// One key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// What to draw when shift is not latched.
    pub label: &'static str,
    /// What to draw when it is.
    pub shifted: &'static str,
    /// evdev keycode.
    pub code: u32,
    /// Width in quarter-keys. An ordinary key is [`UNIT`].
    pub width: u8,
    pub role: Role,
}

impl Key {
    /// What this key shows, given whether shift is latched.
    ///
    /// One definition, because two places need it: the keyboard itself, for the baked face, and
    /// the renderer, for redrawing a raised key's legend on top of it. A second copy of this
    /// would drift, and the symptom would be a key that changes what it says when you point at
    /// it — which reads as the wrong key being under the pointer.
    pub fn face(&self, shift: bool) -> &'static str {
        if shift {
            self.shifted
        } else {
            self.label
        }
    }
}

/// Width of an ordinary key, in the units [`Key::width`] is expressed in.
pub const UNIT: u8 = 4;

/// Every row is exactly this many units wide, so the columns line up and the face is a
/// rectangle rather than a ragged stack.
pub const ROW_UNITS: u16 = 60;

const fn key(label: &'static str, shifted: &'static str, code: u32) -> Key {
    Key {
        label,
        shifted,
        code,
        width: UNIT,
        role: Role::Normal,
    }
}

const fn wide(label: &'static str, code: u32, width: u8) -> Key {
    Key {
        label,
        shifted: label,
        code,
        width,
        role: Role::Normal,
    }
}

const fn modifier(label: &'static str, code: u32, width: u8, which: Modifier) -> Key {
    Key {
        label,
        shifted: label,
        code,
        width,
        role: Role::Modifier(which),
    }
}

/// evdev codes used below, named so the table can be checked against the kernel header.
pub const KEY_ESC: u32 = 1;
pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_TAB: u32 = 15;
pub const KEY_ENTER: u32 = 28;
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_RIGHTSHIFT: u32 = 54;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_RIGHTALT: u32 = 100;
pub const KEY_SPACE: u32 = 57;

/// The rows, top to bottom.
pub const ROWS: &[&[Key]] = &[
    &[
        key("`", "~", 41),
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
        key("-", "_", 12),
        key("=", "+", 13),
        wide("back", KEY_BACKSPACE, 8),
    ],
    &[
        wide("tab", KEY_TAB, 6),
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
        key("[", "{", 26),
        key("]", "}", 27),
        wide("\\", 43, 6),
    ],
    &[
        wide("esc", KEY_ESC, 7),
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
        key("'", "\"", 40),
        wide("enter", KEY_ENTER, 9),
    ],
    &[
        modifier("shift", KEY_LEFTSHIFT, 11, Modifier::Shift),
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
        modifier("shift", KEY_RIGHTSHIFT, 9, Modifier::Shift),
    ],
    &[
        modifier("ctrl", KEY_LEFTCTRL, 6, Modifier::Ctrl),
        modifier("alt", KEY_LEFTALT, 6, Modifier::Alt),
        wide("space", KEY_SPACE, 26),
        modifier("alt", KEY_RIGHTALT, 6, Modifier::Alt),
        wide("←", 105, 4),
        wide("↑", 103, 4),
        wide("↓", 108, 4),
        wide("→", 106, 4),
    ],
];

/// How thick the resize border is, as a fraction of the **face's** height.
///
/// Deliberately thinner than a window's frame, which is 10% of its content. A window is
/// furniture you arrange; the keyboard is a tool you point at, and a border heavy enough to
/// look like a window's would compete with the keys for both attention and aim. At roughly 5%
/// of a face about 9° tall this still lands near half a degree, which is grabbable because the
/// border runs the whole way round rather than being a small target.
pub const BORDER_FRACTION: f64 = 0.055;

/// The smallest and largest the wearer may drag the keyboard, as a multiple of its natural
/// size. Not unbounded: dragged to nothing it takes its own resize border with it, and there
/// is then no way to get it back.
pub const MIN_SCALE: f32 = 0.6;
pub const MAX_SCALE: f32 = 1.9;

/// A key's cell on the face, in the face's own 0..1 coordinates.
///
/// The cell, not the drawn keycap: the keycap is inset inside it so the keys have gaps between
/// them, while the cells tile the face exactly. Hit-testing against the cells rather than the
/// caps is what stops there being dead strips between keys that read as a missed click.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyRect {
    pub u: f64,
    pub v: f64,
    pub half_u: f64,
    pub half_v: f64,
}

impl KeyRect {
    /// Whether a point in the same coordinates falls inside the cell.
    pub fn contains(&self, u: f64, v: f64) -> bool {
        (u - self.u).abs() <= self.half_u && (v - self.v).abs() <= self.half_v
    }
}

/// Height of the strip above the keys, in the quarter-keys [`Key::width`] uses.
///
/// The strip carries the sound toggle, and its height is the whole reason that toggle is
/// hittable. Putting the control in the resize border would have cost no room at all, but the
/// border is about half a degree tall at the size the keyboard is really drawn — it works as a
/// grab handle only because it runs the whole way round, and a small button inside it would be
/// a target nobody could hit with a head-anchored ray. Three quarters of a key row is a little
/// over a degree, the same order as a key, and costs the keyboard 15% of its height rather
/// than the 20% a full row would.
pub const CHROME_UNITS: u16 = 3;

/// Width of the sound toggle, in quarter-keys: one and a half ordinary keys.
pub const TOGGLE_UNITS: u16 = 6;

fn face_units() -> f64 {
    // Rows are as tall as an ordinary key is wide, so the key cells are square.
    ROWS.len() as f64 * UNIT as f64 + CHROME_UNITS as f64
}

/// How much of the face's height the chrome strip takes.
pub fn chrome_fraction() -> f64 {
    CHROME_UNITS as f64 / face_units()
}

/// Aspect ratio of the face — width over height.
///
/// The face is the strip *and* the keys: they are rasterised as one image and hit-tested in
/// one set of coordinates, so that a toggle drawn in one place and pressed in another is not a
/// thing that can happen.
pub fn face_aspect() -> f64 {
    ROW_UNITS as f64 / face_units()
}

/// Where the sound toggle sits, in the face's own 0..1 coordinates.
///
/// Flush with the face's **left** edge, which is a choice about what a near miss costs rather
/// than about symmetry. The right end sits directly above `back`, and backspace is the key a
/// keyboard you have to aim at gets pressed over and over — overshooting it by a third of a
/// row would mute the session. Above the left end is the backtick, which is the least-pressed
/// key on the board.
pub fn toggle_rect() -> KeyRect {
    let w = TOGGLE_UNITS as f64 / ROW_UNITS as f64;
    let h = chrome_fraction();
    KeyRect { u: w * 0.5, v: h * 0.5, half_u: w * 0.5, half_v: h * 0.5 }
}

/// Aspect ratio of the whole plate, border included.
pub fn outer_aspect() -> f64 {
    let face_h = 1.0;
    let face_w = face_aspect();
    let border = face_h * BORDER_FRACTION;
    (face_w + border * 2.0) / (face_h + border * 2.0)
}

/// The face's size as a fraction of the plate's, as `(width, height)`.
///
/// The one place the border's thickness turns into a coordinate change. Both the hit test and
/// the drawing go through this, so a border drawn at one thickness and aimed at another is not
/// a thing that can happen.
pub fn face_fraction() -> (f64, f64) {
    let face_w = face_aspect();
    let border = BORDER_FRACTION;
    (
        face_w / (face_w + border * 2.0),
        1.0 / (1.0 + border * 2.0),
    )
}

/// Every key with the cell it occupies.
pub fn layout() -> Vec<(&'static Key, KeyRect)> {
    let rows = ROWS.len() as f64;
    let total = ROW_UNITS as f64;
    let top = chrome_fraction();
    let row_h = (1.0 - top) / rows;
    let mut out = Vec::new();
    for (row_index, row) in ROWS.iter().enumerate() {
        let mut x = 0.0f64;
        for k in row.iter() {
            let w = k.width as f64 / total;
            out.push((
                k,
                KeyRect {
                    u: x + w * 0.5,
                    v: top + (row_index as f64 + 0.5) * row_h,
                    half_u: w * 0.5,
                    half_v: row_h * 0.5,
                },
            ));
            x += w;
        }
    }
    out
}

/// What a point on the plate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Key(&'static Key),
    /// The speaker in the strip above the keys. Turns the click on and off.
    SoundToggle,
    /// The frame. Grab to resize.
    Border,
}

/// One keystroke to deliver, with whatever modifiers were latched when it was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke {
    pub code: u32,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// The keyboard's state.
#[derive(Debug, Clone)]
pub struct Keyboard {
    pub open: bool,
    /// Modifiers **latch** rather than being held: there is one pointer and it cannot press two
    /// keys at once, so a shift you have to hold would make a capital letter impossible.
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// How much the wearer has grown or shrunk it, as a multiple of its natural size.
    pub scale: f32,
    /// Whether pressing a key should make a sound.
    ///
    /// The shell has no idea how to make one — it only carries the choice, because both
    /// keyboards share this one state. Two copies would let the click be on in the world and
    /// off on the panel, which is the same class of bug as a shift that latches in one place
    /// and not the other.
    ///
    /// On by default. Neither keyboard has any travel, so without a sound the only thing
    /// confirming a press is the cap coming up under the pointer — which the panel does not
    /// even draw, and which you are not looking at while typing anyway.
    pub click: bool,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            open: false,
            shift: false,
            ctrl: false,
            alt: false,
            scale: 1.0,
            click: true,
        }
    }
}

impl Keyboard {
    /// Which key is at a point on the **face**, in 0..1 face coordinates.
    pub fn key_at(&self, u: f64, v: f64) -> Option<&'static Key> {
        if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
            return None;
        }
        // The strip above the keys is the one part of the face that is not a key, which is why
        // `target_at` asks about the toggle before it asks about this.
        let top = chrome_fraction();
        if v < top {
            return None;
        }
        let v = (v - top) / (1.0 - top);
        let rows = ROWS.len();
        let row_index = ((v * rows as f64) as usize).min(rows - 1);
        let row = ROWS.get(row_index)?;
        let total = ROW_UNITS as f64;
        let mut x = 0.0;
        for k in row.iter() {
            x += k.width as f64 / total;
            if u < x {
                return Some(k);
            }
        }
        row.last()
    }

    /// What is at a point on the **plate**, border included, in 0..1 plate coordinates.
    pub fn target_at(&self, u: f64, v: f64) -> Option<Target> {
        if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
            return None;
        }
        let (fw, fh) = face_fraction();
        let fu = (u - (1.0 - fw) * 0.5) / fw;
        let fv = (v - (1.0 - fh) * 0.5) / fh;
        if toggle_rect().contains(fu, fv) {
            return Some(Target::SoundToggle);
        }
        match self.key_at(fu, fv) {
            Some(k) => Some(Target::Key(k)),
            None => Some(Target::Border),
        }
    }

    /// Press a key. Returns the keystroke to send, or `None` if it only latched a modifier.
    pub fn press(&mut self, k: &Key) -> Option<Stroke> {
        match k.role {
            Role::Modifier(m) => {
                let flag = match m {
                    Modifier::Shift => &mut self.shift,
                    Modifier::Ctrl => &mut self.ctrl,
                    Modifier::Alt => &mut self.alt,
                };
                *flag = !*flag;
                None
            }
            Role::Normal => Some(Stroke {
                code: k.code,
                shift: self.shift,
                ctrl: self.ctrl,
                alt: self.alt,
            }),
        }
    }

    /// Called after a key has been sent, to drop the one-shot latches.
    pub fn after_press(&mut self, k: &Key) {
        if k.role == Role::Normal {
            self.shift = false;
            self.ctrl = false;
            self.alt = false;
        }
    }

    /// Whether a modifier key is currently lit.
    pub fn is_latched(&self, k: &Key) -> bool {
        match k.role {
            Role::Modifier(Modifier::Shift) => self.shift,
            Role::Modifier(Modifier::Ctrl) => self.ctrl,
            Role::Modifier(Modifier::Alt) => self.alt,
            Role::Normal => false,
        }
    }

    /// The label a key should currently show.
    pub fn label(&self, k: &Key) -> &'static str {
        k.face(self.shift)
    }

    /// Turn the click on or off. Returns what it now is, which is what has to be stored.
    pub fn toggle_click(&mut self) -> bool {
        self.click = !self.click;
        self.click
    }

    /// Grow or shrink, clamped. Returns the scale actually adopted.
    pub fn rescale(&mut self, factor: f32) -> f32 {
        self.scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        self.scale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_is_exactly_the_same_width() {
        // Rows that do not add up leave keys hanging past the edge of the face, and the
        // hit-test and the drawing then disagree about where they are.
        for (i, row) in ROWS.iter().enumerate() {
            let sum: u16 = row.iter().map(|k| k.width as u16).sum();
            assert_eq!(sum, ROW_UNITS, "row {i} is {sum} units, not {ROW_UNITS}");
        }
    }

    #[test]
    fn the_corners_land_on_the_expected_keys() {
        let kb = Keyboard::default();
        // The top row starts below the strip, not at the top of the face.
        let first_row = chrome_fraction() + 0.01;
        assert_eq!(kb.key_at(0.01, first_row).unwrap().label, "`");
        assert_eq!(kb.key_at(0.99, first_row).unwrap().label, "back");
        assert_eq!(kb.key_at(0.01, 0.52).unwrap().label, "esc");
        assert_eq!(kb.key_at(0.01, 0.99).unwrap().label, "ctrl");
        assert_eq!(kb.key_at(0.99, 0.99).unwrap().label, "→");
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
        let top = chrome_fraction();
        for i in 0..120 {
            for j in 0..40 {
                let u = (i as f64 + 0.5) / 120.0;
                let v = top + (1.0 - top) * (j as f64 + 0.5) / 40.0;
                assert!(kb.key_at(u, v).is_some(), "nothing at ({u}, {v})");
            }
        }
    }

    #[test]
    fn the_cells_agree_with_the_hit_test() {
        // The layout draws the keys and `key_at` decides what was pressed. If they disagree,
        // every key is subtly the wrong one and it looks like a broken keymap.
        let kb = Keyboard::default();
        for (k, rect) in layout() {
            let hit = kb.key_at(rect.u, rect.v).expect("a cell centre must hit");
            assert_eq!(hit.code, k.code, "cell for {:?} hits {:?}", k.label, hit.label);
        }
    }

    #[test]
    fn the_cells_tile_the_face_without_gaps_or_overlap() {
        // Everything below the strip, and nothing above it.
        let want = 1.0 - chrome_fraction();
        let total: f64 = layout().iter().map(|(_, r)| r.half_u * 2.0 * r.half_v * 2.0).sum();
        assert!((total - want).abs() < 1e-9, "cells cover {total} of the face, wanted {want}");
    }

    #[test]
    fn the_border_is_outside_the_keys_and_the_keys_fill_the_rest() {
        let kb = Keyboard::default();
        // Dead centre is a key; the very edge of the plate is border.
        assert!(matches!(kb.target_at(0.5, 0.5), Some(Target::Key(_))));
        assert_eq!(kb.target_at(0.001, 0.5), Some(Target::Border));
        assert_eq!(kb.target_at(0.5, 0.999), Some(Target::Border));
        assert_eq!(kb.target_at(0.999, 0.001), Some(Target::Border));
        // And off the plate entirely is neither.
        assert!(kb.target_at(1.2, 0.5).is_none());
    }

    #[test]
    fn the_border_is_thinner_than_a_window_frame() {
        // The whole point of the keyboard's frame: grabbable, but not competing with the keys.
        // A window's frame is 10% of its content height; this must stay visibly under that.
        assert!(BORDER_FRACTION < 0.08, "border is {BORDER_FRACTION}");
        // And not so thin it stops being a target at all.
        assert!(BORDER_FRACTION > 0.02);
    }

    #[test]
    fn the_face_is_about_as_wide_as_a_real_keyboard_is() {
        // A real keyboard's proportions. Far from this and it stops reading as a keyboard.
        // The keys alone are 3:1; the strip above them makes the whole face a little squarer,
        // and this band is what says how much of that is affordable.
        let a = face_aspect();
        assert!((2.5..3.5).contains(&a), "aspect {a}");
        // The plate is a little squarer than the face, because the border is a bigger share of
        // the short side.
        assert!(outer_aspect() < a);
    }

    #[test]
    fn modifiers_latch_rather_than_needing_to_be_held() {
        // There is one pointer. A shift you have to hold makes a capital letter impossible.
        let mut kb = Keyboard::default();
        let shift = ROWS[3][0];
        assert!(matches!(shift.role, Role::Modifier(Modifier::Shift)));
        assert_eq!(kb.press(&shift), None, "shift types nothing on its own");
        assert!(kb.shift);
        assert!(kb.is_latched(&shift));

        let a = ROWS[2][1];
        assert_eq!(kb.label(&a), "A");
        let stroke = kb.press(&a).expect("a letter types");
        assert_eq!(stroke.code, 30);
        assert!(stroke.shift, "the latch must reach the keystroke, not just the label");
        kb.after_press(&a);
        assert!(!kb.shift, "a latch releases after one key");
        assert_eq!(kb.label(&a), "a");
    }

    #[test]
    fn ctrl_and_alt_latch_together_so_combinations_are_possible() {
        // Ctrl+L is how you reach a browser's address bar. Without carrying the latch into the
        // stroke the keyboard can fill in a page but cannot drive the application.
        let mut kb = Keyboard::default();
        let ctrl = ROWS[4][0];
        let alt = ROWS[4][1];
        kb.press(&ctrl);
        kb.press(&alt);
        assert!(kb.ctrl && kb.alt);
        let l = ROWS[2][10];
        let stroke = kb.press(&l).unwrap();
        assert!(stroke.ctrl && stroke.alt);
        kb.after_press(&l);
        assert!(!kb.ctrl && !kb.alt, "both latches release together");
    }

    #[test]
    fn pressing_a_latched_modifier_again_releases_it() {
        let mut kb = Keyboard::default();
        let shift = ROWS[3][0];
        kb.press(&shift);
        kb.press(&shift);
        assert!(!kb.shift, "a modifier must be escapable without typing something");
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
        assert_eq!(find("`"), Some(41));
        assert_eq!(find("←"), Some(105));
        assert_eq!(find("→"), Some(106));
        assert_eq!(find("↑"), Some(103));
        assert_eq!(find("↓"), Some(108));
    }

    #[test]
    fn no_two_keys_share_a_code() {
        let mut codes: Vec<u32> = ROWS.iter().flat_map(|r| r.iter()).map(|k| k.code).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(before, codes.len(), "two keys share an evdev code");
    }

    #[test]
    fn there_is_a_full_number_row() {
        // Typing a password or a URL without digits is not typing.
        let kb = Keyboard::default();
        for (i, d) in "1234567890".chars().enumerate() {
            let found = ROWS[0]
                .iter()
                .find(|k| k.label == d.to_string())
                .unwrap_or_else(|| panic!("no {d} key"));
            assert_eq!(found.code, 2 + i as u32);
        }
        // And the shifted symbols above them. The "1" is the second cell, so it starts one
        // key-width in — aiming at 0.03 lands on the backtick.
        let one = kb
            .key_at(UNIT as f64 * 1.5 / ROW_UNITS as f64, chrome_fraction() + 0.05)
            .unwrap();
        assert_eq!(one.label, "1");
        assert_eq!(one.shifted, "!");
    }

    #[test]
    fn resizing_is_bounded_at_both_ends() {
        // Dragged to nothing the keyboard takes its own resize border with it, and there is
        // then no way to get hold of it again.
        let mut kb = Keyboard::default();
        for _ in 0..40 {
            kb.rescale(0.5);
        }
        assert_eq!(kb.scale, MIN_SCALE);
        for _ in 0..40 {
            kb.rescale(2.0);
        }
        assert_eq!(kb.scale, MAX_SCALE);
    }

    #[test]
    fn two_thumbs_typing_at_the_same_key_do_not_confuse_the_latches() {
        // Both pads can press keys, so two presses can arrive in one frame. A latch set by one
        // thumb has to survive being read by the other, and clear exactly once.
        let mut kb = Keyboard::default();
        let shift = ROWS[3][0];
        let a = ROWS[2][1];
        let b = ROWS[3][5];

        kb.press(&shift);
        // Left thumb types A, which consumes the latch...
        let first = kb.press(&a).unwrap();
        kb.after_press(&a);
        assert!(first.shift);
        // ...so the right thumb, arriving in the same frame, gets a lower-case one.
        let second = kb.press(&b).unwrap();
        kb.after_press(&b);
        assert!(!second.shift, "the latch must not apply twice");
    }

    #[test]
    fn a_new_keyboard_is_its_natural_size_and_shut() {
        let kb = Keyboard::default();
        assert!(!kb.open);
        assert_eq!(kb.scale, 1.0);
        assert!(kb.click, "a keyboard with no travel should confirm a press somehow");
    }

    #[test]
    fn the_strip_sits_above_every_key_and_takes_none_of_their_room() {
        let top = chrome_fraction();
        assert!(top > 0.0, "there is no strip to put the toggle in");
        for (k, rect) in layout() {
            assert!(
                rect.v - rect.half_v >= top - 1e-9,
                "{:?} reaches up into the strip",
                k.label
            );
        }
        // And the toggle is inside it, not hanging down into the top row of keys.
        let t = toggle_rect();
        assert!(t.v + t.half_v <= top + 1e-9, "the toggle overlaps the keys");
    }

    #[test]
    fn the_toggle_is_a_target_worth_aiming_at() {
        // The reason it is not in the resize border. The border is BORDER_FRACTION of the face
        // height; a control has to beat that by enough to be a different kind of thing.
        let t = toggle_rect();
        assert!(
            t.half_v * 2.0 > BORDER_FRACTION * 2.0,
            "the toggle is no easier to hit than the frame it was moved out of"
        );
        // Wider than an ordinary key, so it reads as a button rather than as a stray keycap.
        let key_w = UNIT as f64 / ROW_UNITS as f64;
        assert!(t.half_u * 2.0 > key_w, "the toggle is narrower than a key");
    }

    #[test]
    fn the_toggle_is_pressable_and_is_not_a_key() {
        let kb = Keyboard::default();
        let t = toggle_rect();
        // On the face it is not a key...
        assert!(kb.key_at(t.u, t.v).is_none(), "the toggle is being read as a key");
        // ...and on the plate it is the toggle rather than the frame.
        let (fw, fh) = face_fraction();
        let plate = |u: f64, v: f64| (u * fw + (1.0 - fw) * 0.5, v * fh + (1.0 - fh) * 0.5);
        let (pu, pv) = plate(t.u, t.v);
        assert_eq!(kb.target_at(pu, pv), Some(Target::SoundToggle));
        // Just below it is the top row of keys, not the toggle again.
        let (ku, kv) = plate(t.u, chrome_fraction() + 0.02);
        assert!(matches!(kb.target_at(ku, kv), Some(Target::Key(_))));
    }

    #[test]
    fn the_click_can_be_turned_off_and_back_on() {
        // The point of the control: it has to be escapable, or it is a one-way door to silence.
        let mut kb = Keyboard::default();
        assert!(kb.click);
        assert!(!kb.toggle_click());
        assert!(!kb.click);
        assert!(kb.toggle_click());
        assert!(kb.click);
    }

    #[test]
    fn turning_the_sound_off_does_not_disturb_the_typing() {
        // The toggle sits on the keyboard so it can be reached mid-sentence. Reaching it must
        // not cost a latched shift or a resize.
        let mut kb = Keyboard::default();
        kb.press(&ROWS[3][0]);
        kb.rescale(1.3);
        let scale = kb.scale;
        kb.toggle_click();
        assert!(kb.shift, "the toggle dropped a latched modifier");
        assert_eq!(kb.scale, scale);
    }
}
