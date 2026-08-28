//! The Deck's own screen, while the glasses have the world.
//!
//! A spatial session leaves the built-in panel doing nothing, which is a waste of the one
//! display you can see without putting anything on your face. It shows what is awkward to
//! check *inside* the headset: how hard the machine is working, how much battery is left, and
//! the controls that are fiddly to reach through a laser pointer.
//!
//! Drawn in plain 2D, in the panel's own pixels, and then rolled a quarter turn — the Deck's
//! screen is physically mounted in portrait, 800x1280 with the top of the image along the long
//! edge. Everything below works in landscape coordinates and the rotation happens once, at the
//! transform.
//!
//! Deliberately not a mirror of the 3D world. A second view of the same thing is useless to
//! anyone wearing the glasses and unreadable to anyone who is not.

use glam::{Mat4, Vec3, Vec4};

use crate::gl::QuadPipeline;
use crate::system::{AudioDevice, Direction, Monitors, Series};

use smithay::backend::renderer::gles::ffi;

/// Colours.
///
/// A near-black ground with cards lifted off it by a few percent of white, one accent, and
/// two weights of text. The previous set painted every row a solid mid-blue plate, which is
/// why the panel read as an instrument rather than as something anyone would want to look at:
/// with no dark ground there is nothing for a bright value to be bright *against*.
const INK: [f32; 4] = [0.94, 0.96, 1.0, 1.0];
const DIM: [f32; 4] = [0.62, 0.67, 0.78, 1.0];
const GROUND: [f32; 4] = [0.035, 0.04, 0.055, 1.0];
/// Cards, and the unfilled part of a slider track. Barely there on purpose — enough to say
/// where a thing begins and ends, not enough to compete with its contents.
const CARD: [f32; 4] = [1.0, 1.0, 1.0, 0.055];
const TRACK: [f32; 4] = [1.0, 1.0, 1.0, 0.13];
const ACCENT: [f32; 4] = [0.42, 0.68, 1.0, 1.0];
/// The graph fill, and the same colour again at a fraction of its alpha for the area under it.
const GRAPH_INK: [f32; 4] = [0.42, 0.68, 1.0, 0.85];

/// Corner radii. Cards are gently rounded; anything that takes a touch is a full capsule,
/// which is the difference the eye reads as "this one is a control".
const CARD_RADIUS: f32 = 18.0;

/// Layout, in landscape pixels. Named because the hit test has to agree with the drawing
/// exactly, and two copies of `34.0` do not stay equal.
const MARGIN: f32 = 40.0;
const HEADER_HEIGHT: f32 = 96.0;
const HEADER_GAP: f32 = 28.0;
/// The gutter between the two columns.
const COLUMN_GAP: f32 = 28.0;
/// Padding inside a card, between its edge and its contents.
const CARD_PAD: f32 = 22.0;
const CARD_GAP: f32 = 18.0;

/// How tall a touchable bar is drawn.
///
/// The panel is 1280x800 landscape across roughly 151x94 mm, so a landscape pixel is about
/// 0.118 mm and there are ~8.5 to the millimetre. A fingertip contact patch is 8–10 mm wide.
/// The bars were 30 px — 3.5 mm — when nothing could touch them, which was fine for something
/// only being read and far too thin for something being aimed at.
const BAR_HEIGHT: f32 = 26.0;
/// Text sizes. A card's reading is deliberately about twice its name: at arm's length on an
/// 800-pixel panel the number is the only part anyone actually reads, and setting the two at
/// the same weight is what made the old panel look like a log file.
const HEADER_TEXT: f32 = 46.0;
const LABEL_TEXT: f32 = 22.0;
const VALUE_TEXT: f32 = 36.0;
/// One entry in a device list. Smaller than a reading, because there are several of them and
/// they are read once when choosing rather than glanced at continually.
const DEVICE_TEXT: f32 = 22.0;
/// How tall one device row is. A finger has to land on it, so this is the same order as a
/// slider card rather than the size the text needs.
const DEVICE_ROW: f32 = 54.0;
/// The page-swap button in the header.
const PAGE_BUTTON_HEIGHT: f32 = 64.0;
/// The header buttons carry a symbol rather than a word, so they are sized for the finger
/// rather than for the longest label. A capsule half again as wide as it is tall still reads
/// as a button and still gives a fingertip about 100 x 64 px to land in.
const HEADER_BUTTON_WIDTH: f32 = PAGE_BUTTON_HEIGHT * 1.6;
/// How large to set each header symbol, as a multiple of the button's height.
///
/// A size per symbol rather than one for all of them, and not a taste decision. A font size is
/// the em box, and these glyphs fill very different fractions of theirs: measured on the
/// Deck's own fonts, set at a nominal 33 px in a 64 px capsule, the power symbol inks 12 px
/// while the keyboard inks 17 px. Setting both from one number is what made the power symbol
/// look like a speck beside a full-size keyboard. These bring all three to roughly 28 px of
/// ink -- a little under half the capsule, which is what a button's icon wants to be.
///
/// Re-measure these if the symbols change or the font stack does. `tools/`'s snapshot backend
/// renders the sidecar headlessly, which is how the numbers above were arrived at.
const POWER_SYMBOL: f32 = PAGE_BUTTON_HEIGHT * 1.20;
const KEYBOARD_SYMBOL: f32 = PAGE_BUTTON_HEIGHT * 0.85;
const DASHBOARD_SYMBOL: f32 = PAGE_BUTTON_HEIGHT * 0.80;
/// How long the exit button has to be held before the session ends.
///
/// A tap will not do it. Leaving closes every window in the session, and this control sits on
/// a bare touchscreen that spends the whole session face-up on a table with the wearer unable
/// to see it -- the one surface in the machine most likely to be brushed by accident. Holding
/// is the cheapest thing that cannot happen by accident, and the button fills while held so
/// the requirement explains itself rather than needing a caption.
const HOLD_TO_EXIT: std::time::Duration = std::time::Duration::from_millis(1200);
/// The exit button while it is filling. Red, because this is the one control on the panel that
/// throws work away.
const DANGER: [f32; 4] = [0.90, 0.32, 0.30, 1.0];
/// Gap between neighbouring keycaps on the panel, as a fraction of a cell's shorter side.
///
/// Larger than the 3D keyboard's, because this one is hit with a fingertip about 9 mm across
/// rather than with a ray: the gaps are what stop a thumb landing on two keys at once from
/// looking like the wrong one was chosen.
const KEY_GAP: f32 = 0.16;


/// The lowest the brightness slider will go.
///
/// Not a taste decision. At zero the panel is dark, and the control you need in order to
/// undo that is drawn on it.
const MINIMUM_BRIGHTNESS: f32 = 0.05;

/// One piece of text, positioned and rasterised, ready to draw.
///
/// A struct rather than the tuple this used to be. It grew a colour when labels and readings
/// stopped being the same weight, and at six positional fields the two `f32` coordinates and
/// the `f32` height were one careless reorder away from silently swapping.
pub struct Label {
    /// Texture id and aspect ratio.
    pub texture: (u32, f32),
    pub x: f32,
    pub y: f32,
    pub height: f32,
    pub colour: [f32; 4],
}

/// A rectangle in landscape pixels, origin top left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// Where `x` sits across the rectangle, 0..1.
    fn fraction(&self, x: f32) -> f32 {
        if self.w <= 0.0 {
            return 0.0;
        }
        ((x - self.x) / self.w).clamp(0.0, 1.0)
    }
}

/// What the panel is showing.
///
/// Two pages rather than one crowded screen. The Deck's panel is a *touchscreen* the wearer can
/// reach without aiming anything, which makes it far and away the best keyboard in the session —
/// a finger on glass beats a head-anchored ray at a key a degree across. But a keyboard needs
/// the whole panel to have keys worth hitting, so it replaces the readouts rather than squeezing
/// in beside them. The 3D keyboard stays available either way; they are not exclusive, and
/// nothing here turns the other one off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Dashboard,
    Keyboard,
}

impl Page {
    /// What the button offers, which is the page you are *not* on.
    ///
    /// A symbol rather than the word, because the word was doing no work. There are exactly
    /// two pages and the button always shows the other one, so nothing has to be read -- and
    /// on a panel glanced down at from inside a headset, a shape is recognised faster than a
    /// word is. Both of these are in DejaVu Sans, which is on every system that has fontconfig
    /// at all; see the sidecar's glyph test.
    fn button_label(self) -> (&'static str, f32) {
        match self {
            // U+2328 KEYBOARD.
            Page::Dashboard => ("\u{2328}", KEYBOARD_SYMBOL),
            // U+25A6 SQUARE WITH ORTHOGONAL CROSSHATCH FILL -- a grid of panes, which is what
            // the dashboard is.
            Page::Keyboard => ("\u{25A6}", DASHBOARD_SYMBOL),
        }
    }

    fn other(self) -> Self {
        match self {
            Page::Dashboard => Page::Keyboard,
            Page::Keyboard => Page::Dashboard,
        }
    }
}

/// Something on the sidecar a finger can change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Knob {
    /// The headset's panel. The glasses have their own temple buttons for this, but nobody
    /// can see a temple button while wearing them, whereas the Deck is right there to look
    /// down at.
    Glasses,
    /// The Deck's own backlight.
    Screen,
    Volume,
}

impl Knob {
    /// In the order they are drawn: the glasses first, because in a spatial session they are
    /// the display being looked *through*, and the Deck's panel is the one glanced down at.
    pub const ALL: [Knob; 3] = [Knob::Glasses, Knob::Screen, Knob::Volume];

    pub fn label(self) -> &'static str {
        match self {
            // Named for the thing, not for the setting. "Screen" and "Glasses" side by side
            // left it ambiguous which screen was which.
            Knob::Glasses => "Glass Brightness",
            Knob::Screen => "Deck Brightness",
            Knob::Volume => "Volume",
        }
    }
}

/// Everything the sidecar can both show and change, as it stands right now.
///
/// One struct rather than a parameter each, because `rows`, `touch`, `knob_value`, `prepare`
/// and `draw` all need the same set and all have to agree about it. Three positional
/// `Option<f32>` arguments threaded through five call sites is a swap waiting to happen, and
/// the symptom would be the volume slider moving the backlight.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Levels {
    pub screen: Option<f32>,
    pub glasses: Option<f32>,
    pub volume: Option<f32>,
}

impl Levels {
    pub fn get(&self, knob: Knob) -> Option<f32> {
        match knob {
            Knob::Screen => self.screen,
            Knob::Glasses => self.glasses,
            Knob::Volume => self.volume,
        }
    }
}

/// What is available to listen to and speak into, as it stands right now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Audio {
    pub outputs: Vec<AudioDevice>,
    pub inputs: Vec<AudioDevice>,
}

impl Audio {
    pub fn list(&self, direction: Direction) -> &[AudioDevice] {
        match direction {
            Direction::Output => &self.outputs,
            Direction::Input => &self.inputs,
        }
    }
}

/// What a touch asked the machine to do.
///
/// The sidecar decides *what was meant* and the backend decides *how to do it*. Keeping
/// `wpctl` and sysfs out of this file is what lets the whole interaction -- where a finger
/// landed, what it grabbed, what it chose -- be tested with no panel, no mixer and no
/// backlight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// A slider is being dragged; ask [`Sidecar::knob_value`] where it is now.
    Moved(Knob),
    /// Make this PipeWire node the default for its direction.
    ChooseDevice(Direction, u32),
    /// A key on the panel's keyboard was touched.
    ///
    /// The key rather than the keystroke: latching and releasing modifiers is the keyboard's
    /// own business, and it is shared with the one hanging in the world. Deciding it twice
    /// would let shift be on in one place and off in the other.
    PressKey(&'static spatiand_shell::keyboard::Key),
    /// The speaker above the keys was tapped. Flip the click and remember the choice.
    ///
    /// The panel does not flip it itself, for the same reason it does not latch its own
    /// modifiers: there is one keyboard state and one preferences file, and both live with the
    /// backend. A panel that kept its own copy could disagree with the keyboard in the world.
    ToggleKeyClick,
    /// The exit button was held long enough. End the session and go back to the desktop.
    LeaveSession,
}

/// Every row of the sidecar, worked out once.
///
/// Drawing, laying out text and hit testing all need these numbers, and before this they each
/// walked the same `y += …` sequence separately. Three copies of a layout is three chances for
/// a touch to land on the row above the one under the finger — the kind of bug that reads as a
/// broken digitiser rather than as arithmetic.
pub struct Rows {
    pub header: Rect,
    /// The left column, one card per measurement.
    pub graphs: [Rect; 3],
    /// The middle column, one card per knob, indexed by [`Knob::ALL`]. `None` where the machine
    /// has no reading to show — a slider that moves nothing is worse than an absent one.
    pub sliders: [Option<Rect>; 3],
    /// The right column, one card per direction, indexed by [`Direction::ALL`].
    pub devices: [Rect; 2],
}

impl Rows {
    /// The capsule inside a slider card that shows the value.
    ///
    /// Smaller than the card it sits in, and deliberately so: the *card* is the touch target,
    /// which makes the thing a finger has to hit about four times the height of the thing the
    /// eye has to read. Sizing a control by what a fingertip needs makes for a panel of chunky
    /// slabs; sizing it by what it has to say makes for one nobody can hit.
    pub fn track(card: Rect) -> Rect {
        Rect {
            x: card.x + CARD_PAD,
            y: card.y + card.h - CARD_PAD - BAR_HEIGHT,
            w: (card.w - CARD_PAD * 2.0).max(1.0),
            h: BAR_HEIGHT,
        }
    }

    /// Where each entry in a device card is drawn, top to bottom.
    ///
    /// Only as many as fit. A list that overflowed its card would draw over the one below and
    /// take touches meant for it, so the overflow is dropped rather than clipped — and
    /// [`Rows::device_capacity`] is what the caller uses to say so out loud.
    pub fn device_rows(card: Rect, count: usize) -> impl Iterator<Item = Rect> {
        let top = card.y + CARD_PAD + LABEL_TEXT + CARD_PAD * 0.6;
        let shown = count.min(Self::device_capacity(card));
        (0..shown).map(move |i| Rect {
            x: card.x + CARD_PAD * 0.5,
            y: top + DEVICE_ROW * i as f32,
            w: (card.w - CARD_PAD).max(1.0),
            h: DEVICE_ROW,
        })
    }

    /// How many entries a device card has room for.
    pub fn device_capacity(card: Rect) -> usize {
        let top = card.y + CARD_PAD + LABEL_TEXT + CARD_PAD * 0.6;
        let room = (card.y + card.h - CARD_PAD * 0.5) - top;
        (room / DEVICE_ROW).floor().max(0.0) as usize
    }

    pub fn device_card(&self, direction: Direction) -> Rect {
        match direction {
            Direction::Output => self.devices[0],
            Direction::Input => self.devices[1],
        }
    }

    /// Which device entry is under a point.
    pub fn device_at(&self, x: f32, y: f32, audio: &Audio) -> Option<(Direction, usize)> {
        for direction in Direction::ALL {
            let card = self.device_card(direction);
            let count = audio.list(direction).len();
            for (index, row) in Self::device_rows(card, count).enumerate() {
                if row.contains(x, y) {
                    return Some((direction, index));
                }
            }
        }
        None
    }

    pub fn slider(&self, knob: Knob) -> Option<Rect> {
        let index = Knob::ALL.iter().position(|k| *k == knob)?;
        self.sliders[index]
    }

    /// Which knob is under a point, and where along it, in landscape pixels.
    pub fn knob_at(&self, x: f32, y: f32) -> Option<(Knob, f32)> {
        for (knob, card) in Knob::ALL.into_iter().zip(self.sliders) {
            if let Some(card) = card {
                if card.contains(x, y) {
                    return Some((knob, Self::track(card).fraction(x)));
                }
            }
        }
        None
    }
}

/// Where a row of content sits, in landscape pixels.
struct Layout {
    width: f32,
    height: f32,
}

impl Layout {
    /// A rectangle in landscape pixels, as a model matrix for the unit quad.
    ///
    /// The quad spans −0.5..0.5, so a rectangle is a scale and a translate. Pixel coordinates
    /// have their origin at the top left, which is what every layout number below assumes;
    /// [`Sidecar::projection`] is where that becomes GL's bottom-left world.
    ///
    /// Note the **negated height**, which is not a typo. `QUAD_VERT` derives the texture
    /// coordinate from the vertex position as `v_uv.y = 0.5 - a_pos.y`, so a quad's texture
    /// arrives already flipped relative to its geometry. Flipping the quad about its own
    /// centre puts it back. Solid fills do not care — they are one colour — so this is
    /// invisible everywhere except text, which is exactly where it matters.
    fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> Mat4 {
        Mat4::from_cols(
            Vec4::new(w, 0.0, 0.0, 0.0),
            Vec4::new(0.0, -h, 0.0, 0.0),
            Vec4::new(0.0, 0.0, 1.0, 0.0),
            Vec4::new(x + w * 0.5, y + h * 0.5, 0.0, 1.0),
        )
    }

    fn of(&self, r: Rect) -> Mat4 {
        self.rect(r.x, r.y, r.w, r.h)
    }
}

/// The sidecar's own textures.
pub struct Sidecar {
    /// One texture per label, rebuilt only when the text changes.
    labels: std::collections::HashMap<String, (u32, f32)>,
    white: u32,
    /// Landscape size, i.e. the panel's dimensions swapped if it is mounted portrait.
    size: (f32, f32),
    portrait: bool,
    /// Which finger is holding which knob, and where it currently is.
    ///
    /// A slider has to keep following the finger that grabbed it even once that finger has
    /// wandered off the bar, which is how every other slider on every other machine behaves.
    /// Re-testing the position each frame instead would drop the drag the moment a thumb
    /// strayed a few millimetres up, on a control 9 mm tall.
    held: Option<(usize, Knob)>,
    /// Live contacts in landscape pixels, drawn so a touch is visibly registered.
    touches: Vec<(usize, f32, f32)>,
    page: Page,
    /// The key under each live finger, so a pressed key can be drawn as pressed. Without it a
    /// touch keyboard gives no sign it registered anything, which on a panel with no travel is
    /// the whole of the feedback.
    pressed: Vec<(usize, &'static spatiand_shell::keyboard::Key)>,
    /// Which finger is holding the exit button, and since when. Cleared the moment it lifts or
    /// slides off, so backing out is simply a matter of moving away before it fills.
    exit_hold: Option<(usize, std::time::Instant)>,
}

impl Sidecar {
    pub fn new(white: u32, panel: (u32, u32)) -> Self {
        // The Deck's panel reports 800x1280. Everything here is laid out landscape and rotated
        // at the end, so the working size is the panel's dimensions the other way round.
        let portrait = panel.1 > panel.0;
        let size = if portrait {
            (panel.1 as f32, panel.0 as f32)
        } else {
            (panel.0 as f32, panel.1 as f32)
        };
        Self {
            labels: std::collections::HashMap::new(),
            white,
            size,
            portrait,
            held: None,
            touches: Vec::new(),
            page: Page::default(),
            pressed: Vec::new(),
            exit_hold: None,
        }
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn show(&mut self, page: Page) {
        self.page = page;
    }

    /// The button that swaps pages, at the right-hand end of the header.
    ///
    /// In the header rather than on a card of its own, because it is the one control that is
    /// not about a reading — putting it in the grid would make it look like a fourth setting.
    pub fn page_button(&self) -> Rect {
        let header = self.rows(Levels::default()).header;
        let w = HEADER_BUTTON_WIDTH.min(header.w * 0.42);
        Rect {
            x: header.x + header.w - w,
            y: header.y + (header.h - PAGE_BUTTON_HEIGHT) * 0.5,
            w,
            h: PAGE_BUTTON_HEIGHT,
        }
    }

    /// The button that ends the session, at the left-hand end of the header.
    ///
    /// The far end from the page button on purpose. These are the only two controls in the
    /// header, one is harmless and the other throws the session away, and putting them within
    /// a thumb's width of each other is how the harmless one gets mis-hit into the other. The
    /// clock is set to the right of this rather than at the margin, so the two never overlap.
    pub fn exit_button(&self) -> Rect {
        let header = self.rows(Levels::default()).header;
        Rect {
            x: header.x,
            y: header.y + (header.h - PAGE_BUTTON_HEIGHT) * 0.5,
            w: HEADER_BUTTON_WIDTH.min(header.w * 0.42),
            h: PAGE_BUTTON_HEIGHT,
        }
    }

    /// Where the clock starts: clear of the exit button.
    fn clock_x(&self) -> f32 {
        let button = self.exit_button();
        button.x + button.w + CARD_GAP
    }

    /// Pose the exit hold at a given fraction, for the snapshot backend.
    ///
    /// The fill is the only thing that tells anyone the button wants a hold rather than a tap,
    /// so it has to be *looked at*, and a headless render is the only way to look at it
    /// without a Deck and a spare finger. Same reasoning as `SPATIAND_VIEW_HOVER` on the
    /// keyboard: state that only exists mid-gesture is state no still frame can otherwise show.
    pub fn pose_exit_hold(&mut self, progress: f32) {
        let progress = progress.clamp(0.0, 1.0);
        let elapsed = HOLD_TO_EXIT.mul_f32(progress);
        self.exit_hold = std::time::Instant::now()
            .checked_sub(elapsed)
            .map(|since| (0, since));
    }

    /// How far through the exit hold we are, 0..1. The button fills by this much.
    pub fn exit_progress(&self) -> f32 {
        match self.exit_hold {
            Some((_, since)) => {
                (since.elapsed().as_secs_f32() / HOLD_TO_EXIT.as_secs_f32()).clamp(0.0, 1.0)
            }
            None => 0.0,
        }
    }

    /// Call once a frame, whether or not any touch arrived.
    ///
    /// The hold completes on the clock, not on an event: a finger resting perfectly still
    /// generates no motion, so waiting for one to notice would mean the button only fired if
    /// you wobbled. `touch` cannot answer this for the same reason -- it is not called on a
    /// frame with no events.
    pub fn settle(&mut self) -> Option<Action> {
        let (_, since) = self.exit_hold?;
        if since.elapsed() >= HOLD_TO_EXIT {
            self.exit_hold = None;
            return Some(Action::LeaveSession);
        }
        None
    }

    /// Where the keyboard is drawn, in landscape pixels.
    ///
    /// The largest rectangle of the keyboard's own aspect that fits under the header, centred
    /// in what is left. Sized from [`spatiand_shell::keyboard::face_aspect`] rather than from a
    /// constant, so a change to the rows cannot quietly restretch the keys.
    pub fn keyboard_area(&self) -> Rect {
        let (width, height) = self.size;
        let top = MARGIN + HEADER_HEIGHT + HEADER_GAP;
        let room_w = width - MARGIN * 2.0;
        let room_h = (height - MARGIN - top).max(1.0);
        let aspect = spatiand_shell::keyboard::face_aspect() as f32;
        let w = room_w.min(room_h * aspect);
        let h = w / aspect;
        Rect {
            x: MARGIN + (room_w - w) * 0.5,
            y: top + (room_h - h) * 0.5,
            w,
            h,
        }
    }

    /// Which key is under a point in landscape pixels.
    pub fn key_at(&self, x: f32, y: f32) -> Option<&'static spatiand_shell::keyboard::Key> {
        let (u, v) = self.face_at(x, y)?;
        spatiand_shell::Keyboard::default().key_at(u, v)
    }

    /// Whether a point in landscape pixels is on the sound toggle.
    pub fn on_sound_toggle(&self, x: f32, y: f32) -> bool {
        match self.face_at(x, y) {
            Some((u, v)) => spatiand_shell::keyboard::toggle_rect().contains(u, v),
            None => false,
        }
    }

    /// A point in landscape pixels, in the keyboard face's own 0..1 coordinates.
    ///
    /// Straight into the face: the panel draws the face alone, with no resize border, because
    /// there is nothing to resize — it fills the screen it is on. It is the same face the 3D
    /// keyboard shows, strip included, so the toggle is in the same place on both.
    fn face_at(&self, x: f32, y: f32) -> Option<(f64, f64)> {
        let area = self.keyboard_area();
        if !area.contains(x, y) {
            return None;
        }
        Some((((x - area.x) / area.w) as f64, ((y - area.y) / area.h) as f64))
    }

    /// Where every row sits. See [`Rows`].
    ///
    /// Two columns of three cards, measurements on the left and controls on the right, under
    /// a full-width header. The wearer's objection to the previous arrangement was that
    /// everything ran the whole way across, and that is not only a matter of taste: a slider
    /// 1200 px wide gives roughly twelve pixels per percent, so a thumb cannot place it to
    /// better than a percent or two and the extra width buys nothing but the appearance of
    /// technical seriousness. Half the width is still far finer than the ear or the eye can
    /// tell apart, and it leaves room for the graphs to sit beside rather than below.
    pub fn rows(&self, levels: Levels) -> Rows {
        let (width, height) = self.size;
        let full = width - MARGIN * 2.0;

        let header = Rect { x: MARGIN, y: MARGIN, w: full, h: HEADER_HEIGHT };
        let top = MARGIN + HEADER_HEIGHT + HEADER_GAP;

        // Three columns: what the machine is doing, what the wearer can change, and where the
        // sound goes.
        let column = (full - COLUMN_GAP * 2.0) / 3.0;
        let middle = MARGIN + column + COLUMN_GAP;
        let right = middle + column + COLUMN_GAP;
        // Both columns take the same three rows, so the two line up across the gutter. A grid
        // that agrees with itself is most of what separates this from the version that looked
        // like a readout.
        let available = (height - MARGIN - top).max(1.0);
        let card = ((available - CARD_GAP * 2.0) / 3.0).max(1.0);
        let slot = |x: f32, index: usize| Rect {
            x,
            y: top + (card + CARD_GAP) * index as f32,
            w: column,
            h: card,
        };
        // The device lists get the same column split two ways instead of three, because a list
        // needs height and there are only two of them.
        let tall = ((available - CARD_GAP) / 2.0).max(1.0);
        let devices = std::array::from_fn(|i| Rect {
            x: right,
            y: top + (tall + CARD_GAP) * i as f32,
            w: column,
            h: tall,
        });

        let graphs = std::array::from_fn(|i| slot(MARGIN, i));

        // Absent readings close up rather than leaving their slot empty: a gap in the middle
        // of a column of three reads as something having failed to draw.
        let mut next = 0usize;
        let sliders = Knob::ALL.map(|knob| {
            levels.get(knob).map(|_| {
                let r = slot(middle, next);
                next += 1;
                r
            })
        });

        Rows { header, graphs, sliders, devices }
    }

    /// A touch, in the digitiser's 0..1, as a point in landscape pixels.
    ///
    /// The panel is mounted portrait and [`Sidecar::projection`] turns the image a quarter
    /// turn to suit; a touch has to make the same turn or it lands somewhere plausible and
    /// wrong. Working it through: the projection maps landscape (lx, ly) to clip
    /// `(2·ly/h − 1, 2·lx/w − 1)`, and the viewport maps that to panel pixels
    /// `(u·W, (1 − v)·H)` — so `lx = w·(1 − v)` and `ly = h·u`.
    ///
    /// It is the exact inverse of [`Sidecar::projection`] followed by the viewport, and
    /// [`tests::a_touch_lands_on_what_was_drawn_there`] is what holds it to that. An earlier
    /// version was tuned by hand against a panel until touches felt right, which made it the
    /// inverse of a projection that was itself wrong — see [`Sidecar::projection`]. Two
    /// compensating errors agree with each other and with nothing else.
    pub fn touch_to_layout(&self, u: f32, v: f32) -> (f32, f32) {
        let (w, h) = self.size;
        if self.portrait {
            (w * v, h * (1.0 - u))
        } else {
            (w * u, h * v)
        }
    }

    /// Landscape pixels to clip space, with a quarter turn for a portrait panel.
    ///
    /// **This must be a rotation, and for a long time it was a reflection.** The mistake is
    /// worth describing, because the screen gave no sign of it.
    ///
    /// The landscape path flips Y, which is right: the layout reads top-down, as anyone
    /// writing it thinks, and GL's origin is at the bottom. But the portrait path was built as
    /// *that matrix composed with a quarter turn*, and a rotation preserves the sign of a
    /// determinant — so folding a flip into it leaves a mirror. Every position on the panel was
    /// reflected: the rows drew bottom-to-top, so the clock sat under the graphs and the
    /// sliders came out at the top, and each graph grew downward from its ceiling instead of
    /// upward from its floor.
    ///
    /// None of which looked like a transform bug, because **the text was perfect**. Text is a
    /// texture, and `QUAD_VERT` flips texture coordinates relative to vertex positions, so
    /// glyphs picked up a second reflection that cancelled the first. A panel of upright,
    /// correctly-spaced text in the wrong order reads as a layout someone chose.
    ///
    /// So the flip is gone from here and moved into [`Layout::rect`], where it applies to a
    /// quad's own contents rather than to where the quad sits.
    fn projection(&self) -> Mat4 {
        let (w, h) = self.size;
        let centre = Mat4::from_translation(Vec3::new(-w * 0.5, -h * 0.5, 0.0));
        if self.portrait {
            // A quarter turn and nothing else, hence the positive Y scale. The panel's top
            // edge runs along its long side, so the whole image turns to suit.
            Mat4::from_rotation_z(std::f32::consts::FRAC_PI_2)
                * Mat4::from_scale(Vec3::new(2.0 / w, 2.0 / h, 1.0))
                * centre
        } else {
            Mat4::from_scale(Vec3::new(2.0 / w, -2.0 / h, 1.0)) * centre
        }
    }

    /// Feed the touchscreen, and say what the machine should now do.
    ///
    /// Returning actions rather than calling `wpctl` and writing to sysfs from here keeps this
    /// file about layout. It also means the whole interaction — where a finger landed, what it
    /// grabbed, what it dragged — is testable without a panel, a mixer or a backlight.
    pub fn touch(
        &mut self,
        events: &[spatiand_input::TouchEvent],
        levels: Levels,
        audio: &Audio,
    ) -> Vec<Action> {
        use spatiand_input::TouchEvent;
        let rows = self.rows(levels);
        let mut changed = Vec::new();
        for event in events {
            match *event {
                TouchEvent::Down(c) => {
                    let (x, y) = self.touch_to_layout(c.x, c.y);
                    self.touches.retain(|(slot, _, _)| *slot != c.slot);
                    self.touches.push((c.slot, x, y));

                    // The page button is tested before anything else and on either page, so
                    // there is always a way back. A control that only exists on one of two
                    // pages is a way to get stranded on the other.
                    if self.page_button().contains(x, y) {
                        self.page = self.page.other();
                        continue;
                    }

                    // Exit starts a hold rather than doing anything. Nothing is emitted here;
                    // `settle` decides, once the finger has stayed put long enough.
                    if self.exit_button().contains(x, y) {
                        self.exit_hold = Some((c.slot, std::time::Instant::now()));
                        continue;
                    }

                    if self.page == Page::Keyboard {
                        if self.on_sound_toggle(x, y) {
                            changed.push(Action::ToggleKeyClick);
                            continue;
                        }
                        // Typed on the way down, like every other touch keyboard. Waiting for
                        // the lift puts a visible delay between the tap and the character.
                        if let Some(key) = self.key_at(x, y) {
                            self.pressed.retain(|(slot, _)| *slot != c.slot);
                            self.pressed.push((c.slot, key));
                            changed.push(Action::PressKey(key));
                        }
                        continue;
                    }

                    // First finger down on a knob owns it. A second one arriving on the same
                    // bar must not steal it, or resting a palm mid-drag jumps the value.
                    if self.held.is_none() {
                        if let Some((knob, _)) = rows.knob_at(x, y) {
                            self.held = Some((c.slot, knob));
                            changed.push(Action::Moved(knob));
                        } else if let Some((direction, index)) = rows.device_at(x, y, audio) {
                            // Chosen on the way down rather than on release. A device list is
                            // a set of buttons, not a slider, so there is nothing to drag and
                            // waiting for the lift only adds a delay before the sound moves.
                            if let Some(device) = audio.list(direction).get(index) {
                                changed.push(Action::ChooseDevice(direction, device.id));
                            }
                        }
                    }
                }
                TouchEvent::Motion(c) => {
                    let (x, y) = self.touch_to_layout(c.x, c.y);
                    for entry in self.touches.iter_mut() {
                        if entry.0 == c.slot {
                            *entry = (c.slot, x, y);
                        }
                    }
                    // Sliding off cancels. That is the way out of a hold begun by accident,
                    // and it is the one every press-and-hold control on a phone already has.
                    if self.exit_hold.map(|(slot, _)| slot) == Some(c.slot)
                        && !self.exit_button().contains(x, y)
                    {
                        self.exit_hold = None;
                    }
                    if let Some((slot, knob)) = self.held {
                        if slot == c.slot {
                            changed.push(Action::Moved(knob));
                        }
                    }
                }
                TouchEvent::Up { slot } => {
                    self.touches.retain(|(s, _, _)| *s != slot);
                    self.pressed.retain(|(s, _)| *s != slot);
                    if self.exit_hold.map(|(s, _)| s) == Some(slot) {
                        self.exit_hold = None;
                    }
                    if self.held.map(|(s, _)| s) == Some(slot) {
                        self.held = None;
                    }
                }
            }
        }
        changed.dedup();
        changed
    }

    /// The value a held knob should now take, from where its finger is.
    ///
    /// Read from the finger's own position rather than passed along with the event, so that a
    /// drag which has wandered off the bar still tracks horizontally.
    pub fn knob_value(&self, knob: Knob, levels: Levels) -> Option<f32> {
        let (slot, held) = self.held?;
        if held != knob {
            return None;
        }
        let (_, x, _) = self.touches.iter().find(|(s, _, _)| *s == slot)?;
        let fraction = Rows::track(self.rows(levels).slider(knob)?).fraction(*x);
        Some(match knob {
            // A backlight dragged to zero turns off the screen the slider is drawn on, and
            // there is then nothing to see in order to drag it back. The floor is what makes
            // the control safe to explore.
            //
            // The glasses get the same floor for the same reason turned around: the panel the
            // wearer is actually looking through is the one that goes dark, and the control to
            // undo it is on a screen behind their eyes.
            Knob::Screen | Knob::Glasses => fraction.max(MINIMUM_BRIGHTNESS),
            Knob::Volume => fraction,
        })
    }

    /// Get or build a text texture.
    fn label(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text_renderer: &mut spatiand_render::TextRenderer,
        text: &str,
        size_px: f32,
    ) -> Option<(u32, f32)> {
        // Size is part of the key: the same word at two sizes is two textures, and sharing
        // them would render one of the two blurry.
        let key = format!("{size_px:.0}|{text}");
        if let Some(existing) = self.labels.get(&key) {
            return Some(*existing);
        }
        if self.labels.len() > 128 {
            let ids: Vec<u32> = self.labels.values().map(|(id, _)| *id).collect();
            let _ = renderer.with_context(|gl| unsafe {
                for id in ids {
                    gl.DeleteTextures(1, &id);
                }
            });
            self.labels.clear();
        }
        let image = text_renderer.render(text, size_px, 1600, [235, 240, 255, 255]);
        let aspect = image.width as f32 / image.height.max(1) as f32;
        let id = renderer
            .with_context(|gl| unsafe { crate::gl::upload_rgba(gl, &image) })
            .ok()?;
        self.labels.insert(key, (id, aspect));
        Some((id, aspect))
    }

    /// Draw the whole sidecar.
    ///
    /// # Safety
    /// Context must be current and the target framebuffer bound.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn draw(
        &mut self,
        gl: &ffi::Gles2,
        quads: &QuadPipeline,
        rounded: &crate::gl::RoundedPipeline,
        monitors: &Monitors,
        levels: Levels,
        audio: &Audio,
        keyboard: &spatiand_shell::Keyboard,
        prepared: &[Label],
    ) {
        let layout = Layout {
            width: self.size.0,
            height: self.size.1,
        };
        let projection = self.projection();
        // Every rounded shape goes through here, so the radius is always expressed in the same
        // pixels as the rectangle it belongs to.
        let round = |r: Rect, colour: [f32; 4], radius: f32| {
            // SAFETY: same context as the rest of this function -- the caller has made the GL
            // context current and bound the target. A closure inside an `unsafe fn` does not
            // inherit that, so it is restated rather than assumed.
            unsafe {
                rounded.draw(gl, &(projection * layout.of(r)), colour, (r.w, r.h), radius);
            }
        };

        // Backdrop. Flat and nearly black, so everything above it is what carries the light.
        quads.draw(
            gl,
            self.white,
            &(projection * layout.rect(0.0, 0.0, layout.width, layout.height)),
            GROUND,
            (0.0, 1.0),
        );

        let rows = self.rows(levels);

        // The header carries no card of its own. Text on the ground reads as a title; text on
        // a plate reads as another row of data, and the clock is not data.

        // The page button, on both pages, so there is always a way back.
        let button = self.page_button();
        round(button, TRACK, button.h * 0.5);

        // The exit button, on both pages for the same reason. Its plate fills red from the
        // left as it is held, so the hold is visibly doing something and its length is
        // visible rather than guessed at. A tap paints a sliver and stops, which reads as
        // "not yet" -- the right answer for a control that must not fire by accident.
        let exit = self.exit_button();
        round(exit, TRACK, exit.h * 0.5);
        // A fill inside the capsule rather than one the same size as it. Two earlier shapes
        // were wrong for instructive reasons: a fill floored at the capsule's own height could
        // not be drawn narrower than 64 px in a 102 px button, so the first two thirds of the
        // hold all painted the same shape and it looked committed the moment it was touched;
        // and a fill sharing the capsule's bounds but not its corner radius poked its square
        // corners out through the rounded ends. Inset, it is the same track-and-fill the
        // sliders below already use, and it grows from a dot to a pill across the whole hold.
        let progress = self.exit_progress();
        let inset = 5.0f32;
        let track_w = (exit.w - inset * 2.0).max(0.0);
        let w = track_w * progress;
        if w >= 1.0 {
            let full = (exit.h - inset * 2.0).max(1.0);
            // Round while it is narrower than it is tall, so the fill begins as a dot and
            // becomes a pill rather than starting as a full-height hairline. A hairline is not
            // only ugly -- at one pixel wide it stands taller than the capsule's own rounded
            // end is at that x, so it visibly pokes out through the curve.
            let h = full.min(w);
            round(
                Rect { x: exit.x + inset, y: exit.y + (exit.h - h) * 0.5, w, h },
                DANGER,
                h * 0.5,
            );
        }

        if self.page == Page::Keyboard {
            self.draw_keys(gl, &round, keyboard);
            self.draw_touches(&round);
            self.draw_labels(gl, quads, &projection, &layout, prepared);
            return;
        }

        for (series, card) in [&monitors.cpu, &monitors.gpu, &monitors.memory]
            .into_iter()
            .zip(rows.graphs)
        {
            round(card, CARD, CARD_RADIUS);
            // Leave the upper part of the card for the label and the reading; the history
            // occupies the lower two thirds.
            let plot = Rect {
                x: card.x + CARD_PAD,
                y: card.y + card.h * 0.42,
                w: card.w - CARD_PAD * 2.0,
                h: card.h * 0.58 - CARD_PAD,
            };
            self.draw_series(gl, quads, &projection, &layout, series, plot);
        }

        for (knob, card) in Knob::ALL.into_iter().zip(rows.sliders) {
            let (Some(card), Some(value)) = (card, levels.get(knob)) else {
                continue;
            };
            round(card, CARD, CARD_RADIUS);

            let track = Rows::track(card);
            let radius = track.h * 0.5;
            round(track, TRACK, radius);

            // The filled part keeps the full capsule's radius so its left end stays round even
            // when it is short, and never narrower than a full circle so that zero still shows
            // a handle to grab rather than nothing at all.
            let filled = Rect {
                w: (track.w * value.clamp(0.0, 1.0)).max(track.h),
                ..track
            };
            round(filled, ACCENT, radius);

            // A handle, because a filled bar alone says "this is how loud it is" where a
            // handle says "this is how loud it is, and you may move it".
            let knob_size = track.h * 1.7;
            round(
                Rect {
                    x: filled.x + filled.w - knob_size * 0.5,
                    y: track.y + track.h * 0.5 - knob_size * 0.5,
                    w: knob_size,
                    h: knob_size,
                },
                INK,
                knob_size * 0.5,
            );
        }

        for direction in Direction::ALL {
            let card = rows.device_card(direction);
            round(card, CARD, CARD_RADIUS);
            let devices = audio.list(direction);
            for (device, row) in devices.iter().zip(Rows::device_rows(card, devices.len())) {
                if !device.is_default {
                    continue;
                }
                // Only the chosen one is drawn. An unselected row is its text and nothing
                // else, so the eye finds the current device by looking for the one thing on
                // the card that is lit rather than by comparing five similar rows.
                round(row, [ACCENT[0], ACCENT[1], ACCENT[2], 0.22], row.h * 0.5);
                let dot = DEVICE_TEXT * 0.5;
                round(
                    Rect {
                        x: row.x + CARD_PAD * 0.5,
                        y: row.y + (row.h - dot) * 0.5,
                        w: dot,
                        h: dot,
                    },
                    ACCENT,
                    dot * 0.5,
                );
            }
        }

        self.draw_touches(&round);
        self.draw_labels(gl, quads, &projection, &layout, prepared);
    }

    /// Wherever a finger is.
    ///
    /// This is the only confirmation the panel gives that a touch arrived at all, and it is
    /// what tells a wrong quarter turn from a dead digitiser: a dot that mirrors the finger is
    /// a sign error, a dot that never appears is not.
    ///
    /// Round, because a fingertip is, and a square one looked like a rendering fault.
    fn draw_touches(&self, round: &impl Fn(Rect, [f32; 4], f32)) {
        for (_, x, y) in &self.touches {
            let size = 56.0;
            round(
                Rect { x: x - size * 0.5, y: y - size * 0.5, w: size, h: size },
                [1.0, 1.0, 1.0, 0.22],
                size * 0.5,
            );
        }
    }

    /// Text last, so it is never behind a card.
    ///
    /// # Safety
    /// Context must be current and the target framebuffer bound.
    unsafe fn draw_labels(
        &self,
        gl: &ffi::Gles2,
        quads: &QuadPipeline,
        projection: &Mat4,
        layout: &Layout,
        prepared: &[Label],
    ) {
        for label in prepared {
            let width = label.height * label.texture.1.max(0.01);
            quads.draw(
                gl,
                label.texture.0,
                &(*projection * layout.rect(label.x, label.y, width, label.height)),
                label.colour,
                (0.0, 1.0),
            );
        }
    }

    /// The keycaps, drawn straight onto the panel.
    ///
    /// One rounded rectangle per key rather than a rasterised face like the 3D keyboard uses.
    /// The panel draws in its own pixels with no perspective and no texture upload, so there is
    /// nothing to gain from baking it into an image — and the pressed and latched states change
    /// on every touch, which would mean rebuilding that image on every touch.
    fn draw_keys(
        &self,
        _gl: &ffi::Gles2,
        round: &impl Fn(Rect, [f32; 4], f32),
        keyboard: &spatiand_shell::Keyboard,
    ) {
        use spatiand_shell::keyboard::Role;
        let area = self.keyboard_area();
        for (key, rect) in spatiand_shell::keyboard::layout() {
            let cap = self.cap_rect(area, &rect);
            let held = self.pressed.iter().any(|(_, k)| k.code == key.code);
            let colour = if held {
                // Brighter than a latch, because it lasts only as long as the finger does and
                // has to be visible in that time.
                [1.0, 1.0, 1.0, 0.55]
            } else if keyboard.is_latched(key) {
                [ACCENT[0], ACCENT[1], ACCENT[2], 0.85]
            } else if matches!(key.role, Role::Modifier(_)) || key.label.len() > 1 {
                CARD
            } else {
                TRACK
            };
            round(cap, colour, cap.h.min(cap.w) * 0.22);
        }

        // The sound toggle, in the strip above the keys. Drawn as a cap rather than as bare
        // chrome, because it is something to press — and held back rather than accented, so
        // that on a keyboard whose default is "on" it is not the brightest thing on the panel.
        // The speaker itself is what says which way it is; see `prepare`.
        let cap = self.cap_rect(area, &spatiand_shell::keyboard::toggle_rect());
        round(cap, if keyboard.click { TRACK } else { CARD }, cap.h.min(cap.w) * 0.22);
    }

    /// A key's drawn cap: its cell inset by the gap.
    fn cap_rect(&self, area: Rect, rect: &spatiand_shell::keyboard::KeyRect) -> Rect {
        let cell_w = (rect.half_u * 2.0) as f32 * area.w;
        let cell_h = (rect.half_v * 2.0) as f32 * area.h;
        let inset = cell_w.min(cell_h) * KEY_GAP * 0.5;
        Rect {
            x: area.x + (rect.u as f32) * area.w - cell_w * 0.5 + inset,
            y: area.y + (rect.v as f32) * area.h - cell_h * 0.5 + inset,
            w: (cell_w - inset * 2.0).max(1.0),
            h: (cell_h - inset * 2.0).max(1.0),
        }
    }

    /// A filled area graph, oldest sample on the left.
    ///
    /// Drawn as one column per sample rather than a line: at this size a line is a single pixel
    /// that disappears against the plate, and a filled area reads as a shape from across a
    /// room, which is the whole point of putting it on a screen you glance at.
    unsafe fn draw_series(
        &self,
        gl: &ffi::Gles2,
        quads: &QuadPipeline,
        projection: &Mat4,
        layout: &Layout,
        series: &Series,
        plot: Rect,
    ) {
        let count = crate::system::HISTORY as f32;
        let column = plot.w / count;
        for (i, value) in series.samples().enumerate() {
            // Right-aligned, so the newest sample is always at the same edge even before the
            // history has filled up. A left-aligned graph appears to scroll while filling and
            // then stops, which looks like it has frozen.
            let offset = count - series.len() as f32 + i as f32;
            // Grows up from the floor of the plot. This looked inverted on the panel for a
            // while -- 8% memory drawing tall bars and a 99% GPU drawing a hairline -- but the
            // arithmetic here was right all along and the projection was reflected. See
            // [`Sidecar::projection`].
            let bar = (value.clamp(0.0, 1.0) * plot.h).max(1.5);
            // A hairline gap between columns, so a flat reading is a texture rather than a
            // solid block. Below about two pixels per column there is no room for one, and
            // running them together is better than dropping every other bar.
            let width = (column - 1.0).max(column * 0.6);
            quads.draw(
                gl,
                self.white,
                &(*projection
                    * layout.rect(
                        plot.x + offset * column,
                        plot.y + plot.h - bar,
                        width,
                        bar,
                    )),
                GRAPH_INK,
                (0.0, 1.0),
            );
        }
    }

    /// Work out the text to draw and make sure every label exists.
    ///
    /// Split from [`Sidecar::draw`] because building a texture needs `&mut renderer` while
    /// drawing holds the GL context — the same split as everywhere else here.
    pub fn prepare(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text: &mut spatiand_render::TextRenderer,
        monitors: &Monitors,
        status: &str,
        levels: Levels,
        audio: &Audio,
        keyboard: &spatiand_shell::Keyboard,
    ) -> Vec<Label> {
        let rows = self.rows(levels);
        let mut out = Vec::new();

        // Both header buttons carry a symbol, centred in its capsule.
        let (page_glyph, page_size) = self.page.button_label();
        for (rect, glyph, size) in [
            (self.page_button(), page_glyph, page_size),
            // U+23FB POWER SYMBOL. Not a word: "Exit Spatiand" in a capsule this size would
            // have to be set small enough to stop being glanceable, and the power symbol is
            // the one piece of iconography everyone already reads without being taught.
            (self.exit_button(), "\u{23FB}", POWER_SYMBOL),
        ] {
            let Some(entry) = self.label(renderer, text, glyph, size * 1.35) else {
                continue;
            };
            let width = size * entry.1.max(0.01);
            out.push(Label {
                texture: entry,
                x: rect.x + (rect.w - width) * 0.5,
                y: rect.y + (rect.h - size) * 0.5,
                height: size,
                colour: INK,
            });
        }

        if self.page == Page::Keyboard {
            // The clock comes along, so the header is not an empty strip with one button in
            // it -- and it is the thing most worth glancing at while typing anyway.
            if let Some(entry) = self.label(renderer, text, status, HEADER_TEXT * 1.35) {
                out.push(Label {
                    texture: entry,
                    x: self.clock_x(),
                    y: rows.header.y + (rows.header.h - HEADER_TEXT) * 0.5,
                    height: HEADER_TEXT,
                    colour: INK,
                });
            }
            let area = self.keyboard_area();
            for (key, rect) in spatiand_shell::keyboard::layout() {
                let cap = self.cap_rect(area, &rect);
                let label = keyboard.label(key);
                // Words are set smaller than letters, so "space" fits its cap without the
                // letters shrinking to match the longest label on the board.
                let size = if label.chars().count() > 1 {
                    cap.h * 0.34
                } else {
                    cap.h * 0.48
                };
                let Some(entry) = self.label(renderer, text, label, size * 1.35) else {
                    continue;
                };
                let width = size * entry.1.max(0.01);
                out.push(Label {
                    texture: entry,
                    x: cap.x + (cap.w - width) * 0.5,
                    y: cap.y + (cap.h - size) * 0.5,
                    height: size,
                    // A latched key is drawn on a bright plate, so its label has to go dark to
                    // stay readable.
                    colour: if keyboard.is_latched(key) { GROUND } else { INK },
                });
            }
            let cap = self.cap_rect(area, &spatiand_shell::keyboard::toggle_rect());
            // Set from the cap's height rather than fitted, and larger than the number alone
            // suggests: `render` crops a label to its ink horizontally but keeps the full line
            // box vertically, so a glyph asked for at 62% of the cap inked at barely a third of
            // it and read as a speck in a long pill. Measured on the Deck's own fonts through
            // the snapshot backend, the same way the header symbols above were.
            let size = cap.h * 1.05;
            let symbol = crate::keyboard_face::sound_symbol(keyboard);
            if let Some(entry) = self.label(renderer, text, symbol, size * 1.35) {
                let width = size * entry.1.max(0.01);
                out.push(Label {
                    texture: entry,
                    x: cap.x + (cap.w - width) * 0.5,
                    y: cap.y + (cap.h - size) * 0.5,
                    height: size,
                    // Dimmed when off, so the control reads as inactive at a glance rather
                    // than only once the crossed-out speaker has been made out.
                    colour: if keyboard.click { INK } else { DIM },
                });
            }
            return out;
        }

        // `x` is the left edge, or the right edge when `from_right`. Right alignment has to
        // happen here rather than in `draw`, because the width of a piece of text is not known
        // until it has been rasterised and its aspect measured.
        let mut push = |this: &mut Self,
                        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
                        text: &mut spatiand_render::TextRenderer,
                        s: String,
                        x: f32,
                        y: f32,
                        h: f32,
                        from_right: bool,
                        colour: [f32; 4]| {
            // `s` is consumed here as the texture cache's key; nothing downstream needs the
            // characters again, only the pixels they were rasterised into.
            if let Some(entry) = this.label(renderer, text, &s, h * 1.35) {
                let x = if from_right { x - h * entry.1.max(0.01) } else { x };
                out.push(Label { texture: entry, x, y, height: h, colour });
            }
        };

        // The clock, large, on the ground rather than on a plate.
        let header = rows.header;
        let clock_x = self.clock_x();
        push(
            self,
            renderer,
            text,
            status.to_string(),
            clock_x,
            header.y + (header.h - HEADER_TEXT) * 0.5,
            HEADER_TEXT,
            false,
            INK,
        );

        // Every card reads the same way: what it is on the left, what it says on the right,
        // the reading twice the size of its name. That consistency is most of what makes a
        // panel scannable -- the eye learns one shape and then only has to find the numbers.
        let mut card_heading = |this: &mut Self,
                                renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
                                text: &mut spatiand_render::TextRenderer,
                                card: Rect,
                                name: String,
                                reading: String| {
            // The name is quieter than the reading. Two weights of the same colour family is
            // what tells the eye which half of a card it is meant to land on.
            push(
                this,
                renderer,
                text,
                name,
                card.x + CARD_PAD,
                card.y + CARD_PAD,
                LABEL_TEXT,
                false,
                DIM,
            );
            push(
                this,
                renderer,
                text,
                reading,
                card.x + card.w - CARD_PAD,
                card.y + CARD_PAD - (VALUE_TEXT - LABEL_TEXT) * 0.5,
                VALUE_TEXT,
                true,
                INK,
            );
        };

        for (series, card) in [&monitors.cpu, &monitors.gpu, &monitors.memory]
            .into_iter()
            .zip(rows.graphs)
        {
            card_heading(
                self,
                renderer,
                text,
                card,
                series.label.to_string(),
                format!("{:.0}%", series.latest() * 100.0),
            );
        }

        for (knob, card) in Knob::ALL.into_iter().zip(rows.sliders) {
            let (Some(card), Some(value)) = (card, levels.get(knob)) else {
                continue;
            };
            card_heading(
                self,
                renderer,
                text,
                card,
                knob.label().to_string(),
                format!("{:.0}%", value * 100.0),
            );
        }

        for direction in Direction::ALL {
            let card = rows.device_card(direction);
            push(
                self,
                renderer,
                text,
                direction.label().to_string(),
                card.x + CARD_PAD,
                card.y + CARD_PAD,
                LABEL_TEXT,
                false,
                DIM,
            );
            let devices = audio.list(direction);
            if devices.is_empty() {
                // Say so rather than leaving an empty card, which reads as something still
                // loading.
                push(
                    self,
                    renderer,
                    text,
                    "None found".to_string(),
                    card.x + CARD_PAD,
                    card.y + CARD_PAD * 2.0 + LABEL_TEXT,
                    DEVICE_TEXT,
                    false,
                    DIM,
                );
                continue;
            }
            for (device, row) in devices.iter().zip(Rows::device_rows(card, devices.len())) {
                push(
                    self,
                    renderer,
                    text,
                    device.name.clone(),
                    // Clear of the dot that marks the current one, and by the same amount
                    // whether or not this row has one, so the list does not step sideways.
                    row.x + CARD_PAD * 0.5 + DEVICE_TEXT + CARD_PAD * 0.5,
                    row.y + (row.h - DEVICE_TEXT) * 0.5,
                    DEVICE_TEXT,
                    false,
                    if device.is_default { INK } else { DIM },
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidecar(panel: (u32, u32)) -> Sidecar {
        Sidecar::new(1, panel)
    }

    fn down(slot: usize, x: f32, y: f32) -> spatiand_input::TouchEvent {
        spatiand_input::TouchEvent::Down(spatiand_input::Contact {
            slot,
            id: slot as i32,
            x,
            y,
        })
    }

    /// A touch aimed at a point in landscape pixels, expressed the way the digitiser would
    /// report it — so these tests go through the same quarter turn the real panel does.
    fn touch_at(s: &Sidecar, lx: f32, ly: f32) -> (f32, f32) {
        let (w, h) = s.size;
        if s.portrait {
            (1.0 - ly / h, lx / w)
        } else {
            (lx / w, ly / h)
        }
    }

    fn up(slot: usize) -> spatiand_input::TouchEvent {
        spatiand_input::TouchEvent::Up { slot }
    }

    fn motion(slot: usize, x: f32, y: f32) -> spatiand_input::TouchEvent {
        spatiand_input::TouchEvent::Motion(spatiand_input::Contact { slot, id: slot as i32, x, y })
    }

    fn centre(r: Rect) -> (f32, f32) {
        (r.x + r.w * 0.5, r.y + r.h * 0.5)
    }

    #[test]
    fn tapping_exit_does_not_end_the_session() {
        // The whole reason this is a hold. The panel lies face-up on a table for the length of
        // a session while the wearer cannot see it, so a tap is something that happens to it,
        // not something someone meant.
        let mut s = sidecar((800, 1280));
        let (cx, cy) = centre(s.exit_button());
        let (u, v) = touch_at(&s, cx, cy);
        assert!(s.touch(&[down(0, u, v)], all(), &Audio::default()).is_empty());
        assert_eq!(s.settle(), None, "a hold that has just begun has not finished");
        s.touch(&[up(0)], all(), &Audio::default());
        assert_eq!(s.settle(), None, "lifting abandons the hold");
        assert_eq!(s.exit_progress(), 0.0);
    }

    #[test]
    fn sliding_off_exit_abandons_the_hold() {
        // The way out of a hold begun by accident, and the one every press-and-hold control on
        // a phone already has. Without it the only escape from a brushed button is to be quick.
        let mut s = sidecar((800, 1280));
        let exit = s.exit_button();
        let (cx, cy) = centre(exit);
        let (u, v) = touch_at(&s, cx, cy);
        s.touch(&[down(0, u, v)], all(), &Audio::default());
        assert!(s.exit_hold.is_some());

        let (fu, fv) = touch_at(&s, exit.x + exit.w * 2.0, cy);
        s.touch(&[motion(0, fu, fv)], all(), &Audio::default());
        assert!(s.exit_hold.is_none(), "moving off the button should cancel");
        assert_eq!(s.settle(), None);
    }

    #[test]
    fn a_completed_hold_asks_to_leave_exactly_once() {
        let mut s = sidecar((800, 1280));
        let (cx, cy) = centre(s.exit_button());
        let (u, v) = touch_at(&s, cx, cy);
        s.touch(&[down(0, u, v)], all(), &Audio::default());
        // Reach back in time rather than sleeping for the hold: a test that takes more than a
        // second to say one thing is a test people start skipping.
        s.exit_hold = Some((0, std::time::Instant::now() - HOLD_TO_EXIT));
        assert_eq!(s.exit_progress(), 1.0);
        assert_eq!(s.settle(), Some(Action::LeaveSession));
        assert_eq!(s.settle(), None, "leaving twice would run the teardown twice");
    }

    #[test]
    fn the_exit_and_page_buttons_are_at_opposite_ends_and_do_not_touch() {
        // They are the only two controls in the header; one is harmless and the other throws
        // the session away. A thumb aiming at either must not be able to reach the other.
        for panel in [(800u32, 1280u32), (1280, 800), (1920, 1080)] {
            let s = sidecar(panel);
            let exit = s.exit_button();
            let page = s.page_button();
            let header = s.rows(all()).header;
            assert_eq!(exit.x, header.x, "exit sits at the left edge of the header");
            assert_eq!(page.x + page.w, header.x + header.w, "the page button sits at the right");
            assert!(
                exit.x + exit.w < page.x,
                "{panel:?}: the header buttons overlap"
            );
        }
    }

    #[test]
    fn the_clock_starts_clear_of_the_exit_button() {
        // The clock used to start at the header's left margin, which is now where the button
        // is. Text drawn under a button is unreadable and the button takes the touch.
        let s = sidecar((800, 1280));
        let exit = s.exit_button();
        assert!(s.clock_x() >= exit.x + exit.w, "the clock would overlap the exit button");
    }

    #[test]
    fn the_header_buttons_are_still_big_enough_for_a_finger() {
        // Swapping words for symbols is not a licence to shrink the target. A fingertip is
        // about 9 mm, which on this panel is a little over 40 px.
        let s = sidecar((800, 1280));
        for r in [s.exit_button(), s.page_button()] {
            assert!(r.w >= 60.0 && r.h >= 60.0, "{r:?} is too small to hit");
        }
    }

    #[test]
    fn every_header_symbol_is_one_character_and_not_a_word() {
        // The point of the change. A regression here means a caption crept back in, which at
        // this size would have to be set too small to glance at.
        for page in [Page::Dashboard, Page::Keyboard] {
            let (label, size) = page.button_label();
            assert_eq!(label.chars().count(), 1, "{label:?} should be a single symbol");
            assert!(!label.is_ascii(), "{label:?} should be a symbol, not a letter");
            assert!(size > 0.0, "{label:?} has no size");
        }
    }

    #[test]
    fn the_page_button_swaps_pages_and_swaps_back() {
        // A control that only exists on one of two pages is a way to get stranded on the other.
        let mut s = sidecar((800, 1280));
        let button = s.page_button();
        let (cx, cy) = (button.x + button.w * 0.5, button.y + button.h * 0.5);
        assert_eq!(s.page(), Page::Dashboard);

        let (u, v) = touch_at(&s, cx, cy);
        s.touch(&[down(0, u, v)], all(), &Audio::default());
        assert_eq!(s.page(), Page::Keyboard);

        // The button does not move between pages, so the same place takes you back.
        s.touch(
            &[spatiand_input::TouchEvent::Up { slot: 0 }, down(1, u, v)],
            all(),
            &Audio::default(),
        );
        assert_eq!(s.page(), Page::Dashboard);
    }

    #[test]
    fn touching_a_key_asks_for_that_key() {
        let mut s = sidecar((800, 1280));
        s.page = Page::Keyboard;
        let area = s.keyboard_area();
        // The middle of the "a" cell: second row of five, second cell in.
        let (key, rect) = spatiand_shell::keyboard::layout()
            .into_iter()
            .find(|(k, _)| k.label == "a")
            .expect("there must be an a");
        let lx = area.x + rect.u as f32 * area.w;
        let ly = area.y + rect.v as f32 * area.h;
        let (u, v) = touch_at(&s, lx, ly);
        let actions = s.touch(&[down(0, u, v)], all(), &Audio::default());
        assert_eq!(actions, vec![Action::PressKey(key)]);
    }

    #[test]
    fn the_keys_land_where_they_are_drawn() {
        // The panel draws each cap from `cap_rect` and decides what was pressed from `key_at`.
        // If those disagree every key is subtly the wrong one, which reads as a broken keymap
        // rather than as a layout that is a few pixels out.
        let s = sidecar((800, 1280));
        let area = s.keyboard_area();
        for (key, rect) in spatiand_shell::keyboard::layout() {
            let cap = s.cap_rect(area, &rect);
            let hit = s
                .key_at(cap.x + cap.w * 0.5, cap.y + cap.h * 0.5)
                .expect("the middle of a cap must be a key");
            assert_eq!(hit.code, key.code, "cap for {:?} hits {:?}", key.label, hit.label);
        }
    }

    #[test]
    fn the_sound_toggle_is_reachable_and_does_not_type() {
        // The control has to be on the panel's keyboard as well as on the one in the world:
        // the panel is where most typing actually happens, and a setting you can only change
        // by putting the headset on is not a setting you change while it is annoying you.
        let mut s = sidecar((800, 1280));
        s.page = Page::Keyboard;
        let area = s.keyboard_area();
        let t = spatiand_shell::keyboard::toggle_rect();
        let (cx, cy) = (
            area.x + t.u as f32 * area.w,
            area.y + t.v as f32 * area.h,
        );
        assert!(s.on_sound_toggle(cx, cy));
        assert!(s.key_at(cx, cy).is_none(), "the toggle is being read as a key");

        let (u, v) = touch_at(&s, cx, cy);
        let actions = s.touch(&[down(0, u, v)], all(), &Audio::default());
        assert!(
            actions.iter().any(|a| matches!(a, Action::ToggleKeyClick)),
            "tapping the speaker did nothing: {actions:?}"
        );
        assert!(
            !actions.iter().any(|a| matches!(a, Action::PressKey(_))),
            "tapping the speaker also typed something: {actions:?}"
        );
    }

    #[test]
    fn the_toggle_sits_clear_of_every_key_on_the_panel() {
        // A thumb is about nine millimetres across. The toggle overlapping a keycap would mean
        // a mis-tap deletes a character instead of muting, which is the expensive direction.
        let mut s = sidecar((800, 1280));
        s.page = Page::Keyboard;
        let area = s.keyboard_area();
        let toggle = s.cap_rect(area, &spatiand_shell::keyboard::toggle_rect());
        for (key, rect) in spatiand_shell::keyboard::layout() {
            let cap = s.cap_rect(area, &rect);
            let overlaps = toggle.x < cap.x + cap.w
                && cap.x < toggle.x + toggle.w
                && toggle.y < cap.y + cap.h
                && cap.y < toggle.y + toggle.h;
            assert!(!overlaps, "the toggle overlaps {:?}", key.label);
        }
        // And it is inside the keyboard's own area rather than floating over the header.
        assert!(toggle.y >= area.y - 1.0, "the toggle is above the keyboard");
    }

    #[test]
    fn a_touch_off_the_keyboard_types_nothing() {
        // The area is letterboxed inside the panel, and the margin around it must not be a
        // strip that types whatever key is nearest.
        let s = sidecar((800, 1280));
        let area = s.keyboard_area();
        assert!(s.key_at(area.x - 8.0, area.y + area.h * 0.5).is_none());
        assert!(s.key_at(area.x + area.w * 0.5, area.y - 8.0).is_none());
    }

    #[test]
    fn the_keyboard_keeps_its_shape_on_the_panel() {
        let s = sidecar((800, 1280));
        let area = s.keyboard_area();
        let aspect = (area.w / area.h) as f64;
        assert!(
            (aspect - spatiand_shell::keyboard::face_aspect()).abs() < 1e-3,
            "drew at {aspect}"
        );
        // And fits inside the panel with its margins.
        assert!(area.x >= MARGIN - 0.01 && area.y >= MARGIN);
        assert!(area.x + area.w <= s.size.0 - MARGIN + 0.01);
        assert!(area.y + area.h <= s.size.1 - MARGIN + 0.01);
    }

    #[test]
    fn a_key_on_the_panel_is_big_enough_for_a_finger() {
        // The whole reason the keyboard gets the panel to itself. A landscape pixel is about
        // 0.118 mm on the Deck, and a fingertip contact patch is 8-10 mm — so an ordinary key
        // has to be comfortably over 60 px to be aimed at rather than guessed.
        let s = sidecar((800, 1280));
        let area = s.keyboard_area();
        let (_, rect) = spatiand_shell::keyboard::layout()
            .into_iter()
            .find(|(k, _)| k.label == "g")
            .unwrap();
        let cap = s.cap_rect(area, &rect);
        assert!(cap.w > 60.0, "an ordinary key is only {} px wide", cap.w);
        assert!(cap.h > 60.0, "an ordinary key is only {} px tall", cap.h);
    }

    #[test]
    fn the_dashboard_still_answers_a_slider_when_the_keyboard_is_shut() {
        // The page split must not have taken the original panel with it.
        let mut s = sidecar((800, 1280));
        let card = s.rows(all()).slider(Knob::Volume).expect("volume has a reading");
        let (u, v) = touch_at(&s, card.x + card.w * 0.5, card.y + card.h * 0.5);
        let actions = s.touch(&[down(0, u, v)], all(), &Audio::default());
        assert_eq!(actions, vec![Action::Moved(Knob::Volume)]);
    }

    #[test]
    fn a_portrait_panel_is_laid_out_landscape() {
        // The Deck reports 800x1280 with its top edge along the long side. Laying out in the
        // reported dimensions would produce a tall narrow column of text down a wide screen.
        let s = sidecar((800, 1280));
        assert!(s.portrait);
        assert_eq!(s.size, (1280.0, 800.0));
    }

    #[test]
    fn a_landscape_panel_is_left_alone() {
        let s = sidecar((1920, 1080));
        assert!(!s.portrait);
        assert_eq!(s.size, (1920.0, 1080.0));
    }

    #[test]
    fn the_top_left_of_the_layout_maps_to_the_top_left_of_the_screen() {
        // Y is flipped in the projection so the layout can read top-down. Getting that
        // backwards puts the header at the bottom, which looks like a layout choice.
        let s = sidecar((1920, 1080));
        let p = s.projection();
        let top_left = p * Vec4::new(0.0, 0.0, 0.0, 1.0);
        assert!(top_left.x < -0.99, "x = {}", top_left.x);
        assert!(top_left.y > 0.99, "y = {} (should be the TOP)", top_left.y);
    }

    #[test]
    fn a_rectangle_is_centred_on_its_own_area() {
        // `rect` takes a top-left corner and a size, but the unit quad is centred, so the
        // translation has to carry half the size. An error here shifts everything by half a
        // widget and looks like bad margins.
        let layout = Layout {
            width: 100.0,
            height: 100.0,
        };
        let m = layout.rect(10.0, 20.0, 30.0, 40.0);
        let centre = m * Vec4::new(0.0, 0.0, 0.0, 1.0);
        assert_eq!((centre.x, centre.y), (25.0, 40.0));
    }

    /// Every reading present, which is the ordinary case in a session with glasses on.
    fn all() -> Levels {
        Levels {
            screen: Some(0.5),
            glasses: Some(0.5),
            volume: Some(0.5),
        }
    }

    /// Where the digitiser reports a given panel pixel, as normalised `(u, v)`.
    ///
    /// The one measured fact about this hardware, written down once. Everything else about
    /// touch is arithmetic derived from it and from [`Sidecar::projection`], which is the
    /// point: the previous code had this fact and the projection disagreeing, each tuned
    /// separately until the pair happened to behave.
    fn digitiser_reports(panel: (u32, u32), px: f32, py: f32) -> (f32, f32) {
        let (pw, ph) = (panel.0 as f32, panel.1 as f32);
        (px / pw, 1.0 - py / ph)
    }

    /// Push a layout point through the real projection and the viewport, to a panel pixel.
    fn draws_at(s: &Sidecar, panel: (u32, u32), lx: f32, ly: f32) -> (f32, f32) {
        let clip = s.projection() * glam::Vec4::new(lx, ly, 0.0, 1.0);
        let (pw, ph) = (panel.0 as f32, panel.1 as f32);
        ((clip.x + 1.0) * 0.5 * pw, (1.0 - clip.y) * 0.5 * ph)
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn a_touch_lands_on_what_was_drawn_there() {
        // The test this file was missing, and the reason a reflected projection survived for
        // so long. Drawing and touching were each checked on their own, against the panel, by
        // eye -- and two compensating errors pass that check every time. This closes the loop
        // instead: take a point, draw it, ask the digitiser where that pixel is, and require
        // touch_to_layout to hand back the point we started with.
        let panel = (800u32, 1280u32);
        let s = sidecar(panel);
        let (w, h) = s.size;
        for (lx, ly) in [
            (0.0, 0.0),
            (w, 0.0),
            (0.0, h),
            (w, h),
            (w * 0.5, h * 0.5),
            (MARGIN, MARGIN),
            (w * 0.25, h * 0.75),
        ] {
            let (px, py) = draws_at(&s, panel, lx, ly);
            let (u, v) = digitiser_reports(panel, px, py);
            let (bx, by) = s.touch_to_layout(u, v);
            assert!(
                (bx - lx).abs() < 0.01 && (by - ly).abs() < 0.01,
                "drew ({lx}, {ly}) at panel ({px}, {py}), which reads back as ({bx}, {by})"
            );
        }
    }

    #[test]
    fn the_layout_stacks_downward_on_the_panel() {
        // Stated in the terms the wearer reported it in: the status line is the first row and
        // the sliders are the last, and on the panel the first row was coming out *underneath*
        // the last one, with each graph growing down from its ceiling instead of up from its
        // floor.
        //
        // The direction below is an observation, not a derivation. The panel is mounted
        // portrait, so the layout's vertical runs along the panel's X, and a photograph of the
        // running screen is what says which end of that axis the wearer sees as the top: the
        // high end. That fact cannot be recovered from the framebuffer alone -- the scanout
        // sits between this matrix and anybody's eyes, and it is not in the matrix.
        //
        // Which is why there is no assertion here about the projection's determinant. A
        // reflection in the matrix is not a reflection on the panel unless the scanout is
        // known to be a rotation, and on this hardware it demonstrably is not.
        let panel = (800u32, 1280u32);
        let s = sidecar(panel);
        let top = draws_at(&s, panel, s.size.0 * 0.5, MARGIN);
        let bottom = draws_at(&s, panel, s.size.0 * 0.5, s.size.1 - MARGIN);
        assert!(
            top.0 > bottom.0,
            "the first row must draw above the last: top at {top:?}, bottom at {bottom:?}"
        );
    }

    #[test]
    fn a_graph_column_grows_up_from_its_floor() {
        // The symptom the wearer photographed: memory at 8% drew tall bars and the GPU at 99%
        // drew a hairline, because every column hung from the ceiling of its row.
        let panel = (800u32, 1280u32);
        let s = sidecar(panel);
        let row = s.rows(all()).graphs[0];
        let floor = draws_at(&s, panel, row.x, row.y + row.h);
        let small = draws_at(&s, panel, row.x, row.y + row.h - row.h * 0.1);
        let large = draws_at(&s, panel, row.x, row.y + row.h - row.h * 0.9);
        let rise = |p: (f32, f32)| (p.0 - floor.0).abs();
        assert!(
            rise(large) > rise(small),
            "a bigger reading must reach further from the floor: 10% at {small:?}, 90% at {large:?}"
        );
    }

    #[test]
    fn nothing_but_the_header_runs_the_full_width() {
        // The wearer's objection to the old panel, as an assertion. Every graph and every
        // slider used to span the whole content width, which is what made it read as a
        // readout: six full-width bands stacked down the screen with nowhere for the eye to
        // rest.
        let s = sidecar((800, 1280));
        let rows = s.rows(all());
        let full = s.size.0 - MARGIN * 2.0;
        for card in rows
            .graphs
            .into_iter()
            .chain(rows.sliders.into_iter().flatten())
            .chain(rows.devices)
        {
            assert!(
                card.w < full * 0.4,
                "a card is {} wide of a possible {full}",
                card.w
            );
        }
        assert_eq!(rows.header.w, full, "the header is the one thing that should span");
    }

    #[test]
    fn the_two_columns_line_up() {
        // Graphs on the left, controls on the right, sharing three rows. A grid that agrees
        // with itself across the gutter is most of the difference between this and the
        // version that looked assembled out of whatever fitted.
        let s = sidecar((800, 1280));
        let rows = s.rows(all());
        for (graph, slider) in rows.graphs.into_iter().zip(rows.sliders.into_iter().flatten()) {
            assert_eq!(graph.y, slider.y, "rows should share a baseline");
            assert_eq!(graph.h, slider.h);
            assert!(graph.x + graph.w < slider.x, "the columns should not touch");
        }
    }

    #[test]
    fn a_slider_at_zero_still_shows_something_to_grab() {
        // A filled bar of width zero is invisible, and the control then looks broken at
        // exactly the moment someone wants to turn it back up.
        let s = sidecar((800, 1280));
        let card = s
            .rows(Levels { volume: Some(0.0), ..all() })
            .slider(Knob::Volume)
            .expect("volume card");
        let track = Rows::track(card);
        assert!(track.h > 0.0 && track.w > track.h, "a track should be a capsule, not a dot");
    }

    fn some_audio() -> Audio {
        let device = |id: u32, name: &str, is_default: bool| AudioDevice {
            id,
            name: name.into(),
            is_default,
        };
        Audio {
            outputs: vec![
                device(62, "Deck Headphones", false),
                device(66, "Deck Speaker", false),
                device(81, "Glasses", true),
            ],
            inputs: vec![
                device(50, "Glasses Microphone", true),
                device(72, "Deck Microphone", false),
            ],
        }
    }

    #[test]
    fn tapping_a_device_chooses_it() {
        let mut s = sidecar((800, 1280));
        let audio = some_audio();
        let rows = s.rows(all());
        let card = rows.device_card(Direction::Output);
        // The second entry: Deck Speaker, id 66.
        let row = Rows::device_rows(card, audio.outputs.len()).nth(1).expect("a second row");
        let event = press(&s, 0, row.x + row.w * 0.5, row.y + row.h * 0.5);
        assert_eq!(
            s.touch(&[event], all(), &audio),
            vec![Action::ChooseDevice(Direction::Output, 66)]
        );
    }

    #[test]
    fn the_two_device_lists_do_not_take_each_others_taps() {
        // Output and Input sit in one column, one above the other, and their ids overlap with
        // nothing to distinguish them but position. A row bleeding into the card below would
        // silently change the microphone when the wearer asked for a speaker.
        let s = sidecar((800, 1280));
        let audio = some_audio();
        let rows = s.rows(all());
        for direction in Direction::ALL {
            let card = rows.device_card(direction);
            for row in Rows::device_rows(card, audio.list(direction).len()) {
                assert!(
                    row.y >= card.y && row.y + row.h <= card.y + card.h,
                    "a {direction:?} row escapes its card"
                );
                let found = rows.device_at(row.x + 5.0, row.y + row.h * 0.5, &audio);
                assert_eq!(found.map(|(d, _)| d), Some(direction));
            }
        }
    }

    #[test]
    fn a_device_list_never_draws_past_its_card() {
        // A machine with a dock, a headset and two Bluetooth speakers can offer more than
        // there is room for. Overflow has to be dropped rather than drawn over the card
        // below, which would also steal its touches.
        let s = sidecar((800, 1280));
        let card = s.rows(all()).device_card(Direction::Output);
        let capacity = Rows::device_capacity(card);
        assert!(capacity >= 3, "only room for {capacity} devices");
        assert_eq!(Rows::device_rows(card, 99).count(), capacity);
        for row in Rows::device_rows(card, 99) {
            assert!(row.y + row.h <= card.y + card.h + 0.5);
        }
    }

    #[test]
    fn a_slider_and_a_device_are_never_both_under_one_finger() {
        // The two live in adjacent columns and are hit-tested by different methods, so an
        // overlap would fire both and set a volume while switching a speaker.
        let s = sidecar((800, 1280));
        let audio = some_audio();
        let rows = s.rows(all());
        for direction in Direction::ALL {
            let card = rows.device_card(direction);
            for row in Rows::device_rows(card, audio.list(direction).len()) {
                let point = (row.x + row.w * 0.5, row.y + row.h * 0.5);
                assert!(
                    rows.knob_at(point.0, point.1).is_none(),
                    "a device row at {point:?} also reads as a slider"
                );
            }
        }
    }

    #[test]
    fn an_empty_list_is_not_a_panic() {
        // No sound server, or a poll that happened before wireplumber was up.
        let s = sidecar((800, 1280));
        let rows = s.rows(all());
        let empty = Audio::default();
        assert!(rows.device_at(400.0, 400.0, &empty).is_none());
        assert_eq!(Rows::device_rows(rows.device_card(Direction::Input), 0).count(), 0);
    }

    #[test]
    fn a_landscape_panel_needs_no_turn() {
        let s = sidecar((1920, 1080));
        assert_eq!(s.touch_to_layout(0.0, 0.0), (0.0, 0.0));
        assert_eq!(s.touch_to_layout(1.0, 1.0), (1920.0, 1080.0));
    }

    #[test]
    fn every_row_fits_on_the_panel() {
        // The bars grew from 30 px to 56 px to be touchable. There is no scrolling here, so a
        // row past the bottom edge is simply invisible.
        let s = sidecar((800, 1280));
        let rows = s.rows(all());
        for card in rows
            .graphs
            .into_iter()
            .chain(rows.sliders.into_iter().flatten())
            .chain(rows.devices)
        {
            assert!(
                card.y + card.h <= s.size.1 - MARGIN + 0.5,
                "a card runs to {} of {}",
                card.y + card.h,
                s.size.1
            );
            assert!(card.x + card.w <= s.size.0 - MARGIN + 0.5, "a card runs off the side");
        }
    }

    #[test]
    fn rows_do_not_overlap() {
        // knob_at walks them in order and returns the first hit, so an overlap would make one
        // control permanently unreachable.
        let s = sidecar((800, 1280));
        let rows = s.rows(all());
        let cards: Vec<Rect> = rows
            .graphs
            .into_iter()
            .chain(rows.sliders.into_iter().flatten())
            .chain(rows.devices)
            .collect();
        for (i, a) in cards.iter().enumerate() {
            for b in &cards[i + 1..] {
                let apart = a.x + a.w <= b.x
                    || b.x + b.w <= a.x
                    || a.y + a.h <= b.y
                    || b.y + b.h <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_touch_target_is_a_fingers_width() {
        // ~8.5 landscape pixels to the millimetre on this panel; a fingertip is 8-10 mm. The
        // original 30 px bar was 3.5 mm, which is a stylus target, not a thumb one.
        //
        // The drawn capsule is smaller than that now, and deliberately: the whole card takes
        // the touch, so the thing being aimed at and the thing being read no longer have to be
        // the same size. Measure the card, which is what a finger actually has to find.
        let s = sidecar((800, 1280));
        let millimetre = 800.0 / 94.1;
        for card in s.rows(all()).sliders.into_iter().flatten() {
            let mm = card.h / millimetre;
            assert!(mm >= 8.0, "{} is only {mm} mm tall", card.h);
        }
    }

    #[test]
    fn an_absent_reading_leaves_out_the_row_it_would_control() {
        // A machine with no backlight, or a session with no glasses, should not offer a
        // slider that moves nothing.
        let s = sidecar((800, 1280));
        let no_glasses = s.rows(Levels { glasses: None, ..all() });
        assert!(no_glasses.slider(Knob::Glasses).is_none());
        // The two that remain close up rather than leaving a hole where the third was. The
        // glasses are drawn first, so with them gone everything below shifts up one slot.
        let full = s.rows(all());
        assert_eq!(
            no_glasses.slider(Knob::Screen),
            full.slider(Knob::Glasses),
            "the deck slider should take the empty top slot"
        );
        assert_eq!(
            no_glasses.slider(Knob::Volume),
            full.slider(Knob::Screen),
            "and volume should follow it up"
        );

        let none = s.rows(Levels::default());
        assert!(Knob::ALL.into_iter().all(|k| none.slider(k).is_none()));
        // Graphs do not depend on any of this, so they must be untouched.
        assert_eq!(none.graphs, full.graphs);
        // A point in the middle of the column the sliders would have occupied.
        let where_sliders_were = full.slider(Knob::Screen).expect("a screen slider");
        assert!(none
            .knob_at(
                where_sliders_were.x + where_sliders_were.w * 0.5,
                where_sliders_were.y + where_sliders_were.h * 0.5
            )
            .is_none());
    }

    /// A press at a landscape point, as the decoder would report it.
    fn press(s: &Sidecar, slot: usize, lx: f32, ly: f32) -> spatiand_input::TouchEvent {
        contact_at(s, slot, lx, ly, spatiand_input::TouchEvent::Down)
    }

    fn drag(s: &Sidecar, slot: usize, lx: f32, ly: f32) -> spatiand_input::TouchEvent {
        contact_at(s, slot, lx, ly, spatiand_input::TouchEvent::Motion)
    }

    fn contact_at(
        s: &Sidecar,
        slot: usize,
        lx: f32,
        ly: f32,
        make: fn(spatiand_input::Contact) -> spatiand_input::TouchEvent,
    ) -> spatiand_input::TouchEvent {
        // Go the long way round -- draw the point through the real projection, then ask the
        // digitiser where that pixel is -- rather than inverting `touch_to_layout` by hand.
        // A hand-written inverse here would have to be updated in step with the real one, and
        // a test that is kept in step with the code it checks stops checking anything.
        let panel = if s.portrait {
            (s.size.1 as u32, s.size.0 as u32)
        } else {
            (s.size.0 as u32, s.size.1 as u32)
        };
        let (px, py) = draws_at(s, panel, lx, ly);
        let (u, v) = digitiser_reports(panel, px, py);
        make(spatiand_input::Contact { slot, id: slot as i32, x: u, y: v })
    }

    #[test]
    fn touching_a_bar_sets_it_to_where_the_finger_is() {
        let mut s = sidecar((800, 1280));
        let card = s.rows(all()).slider(Knob::Volume).expect("volume card");
        // A quarter of the way along the *track*, not the card. The card is what the finger
        // has to hit; the track is what the value is measured across, and they are not the
        // same rectangle.
        let track = Rows::track(card);
        let event = press(&s, 0, track.x + track.w * 0.25, card.y + card.h * 0.5);
        assert_eq!(s.touch(&[event], all(), &Audio::default()), vec![Action::Moved(Knob::Volume)]);
        let value = s.knob_value(Knob::Volume, all()).expect("a value");
        assert!((value - 0.25).abs() < 0.02, "got {value}");
    }

    #[test]
    fn a_drag_that_leaves_the_bar_still_controls_it() {
        // Every slider anywhere behaves this way, and on a 9 mm target a thumb drifts off it
        // constantly. Re-testing the position each frame would drop the drag mid-gesture.
        let mut s = sidecar((800, 1280));
        let card = s.rows(all()).slider(Knob::Volume).expect("volume card");
        let track = Rows::track(card);
        let down = press(&s, 0, track.x + 10.0, card.y + card.h * 0.5);
        s.touch(&[down], all(), &Audio::default());
        // Well clear of the card, and three quarters of the way across the track.
        let away = drag(&s, 0, track.x + track.w * 0.75, card.y - 120.0);
        assert_eq!(s.touch(&[away], all(), &Audio::default()), vec![Action::Moved(Knob::Volume)]);
        let value = s.knob_value(Knob::Volume, all()).expect("still held");
        assert!((value - 0.75).abs() < 0.02, "got {value}");
    }

    #[test]
    fn lifting_releases_the_knob() {
        let mut s = sidecar((800, 1280));
        let volume = s.rows(all()).slider(Knob::Volume).expect("volume card");
        s.touch(&[press(&s, 0, volume.x + 40.0, volume.y + 20.0)], all(), &Audio::default());
        s.touch(&[spatiand_input::TouchEvent::Up { slot: 0 }], all(), &Audio::default());
        assert!(s.knob_value(Knob::Volume, all()).is_none());
        assert!(s.touches.is_empty(), "the dot should go with the finger");
    }

    #[test]
    fn a_second_finger_cannot_steal_a_knob_mid_drag() {
        // A palm or a second thumb landing on the same bar would otherwise take the slider
        // and jump the value to wherever it touched.
        let mut s = sidecar((800, 1280));
        let volume = s.rows(all()).slider(Knob::Volume).expect("volume card");
        s.touch(&[press(&s, 0, volume.x + 10.0, volume.y + 20.0)], all(), &Audio::default());
        let intruder = press(&s, 1, volume.x + volume.w - 10.0, volume.y + 20.0);
        s.touch(&[intruder], all(), &Audio::default());
        let value = s.knob_value(Knob::Volume, all()).expect("still ours");
        assert!(value < 0.1, "the first finger should still own it, got {value}");
    }

    #[test]
    fn touching_a_graph_changes_nothing() {
        let mut s = sidecar((800, 1280));
        let graph = s.rows(all()).graphs[1];
        let event = press(&s, 0, graph.x + graph.w * 0.5, graph.y + graph.h * 0.5);
        assert!(s.touch(&[event], all(), &Audio::default()).is_empty());
        assert_eq!(s.touches.len(), 1, "but it should still show a dot");
    }

    #[test]
    fn brightness_cannot_be_dragged_to_black() {
        // At zero the panel goes dark, and the slider you need in order to undo it is drawn
        // on that panel.
        let mut s = sidecar((800, 1280));
        let bar = s.rows(all()).slider(Knob::Screen).expect("screen card");
        s.touch(&[press(&s, 0, bar.x - 200.0, bar.y + 20.0)], all(), &Audio::default());
        // Pressing left of the bar still grabs nothing; press on it, then drag off the left.
        s.touch(&[press(&s, 1, bar.x + 40.0, bar.y + 20.0)], all(), &Audio::default());
        s.touch(&[drag(&s, 1, bar.x - 500.0, bar.y + 20.0)], all(), &Audio::default());
        let value = s.knob_value(Knob::Screen, all()).expect("held");
        assert!(value >= MINIMUM_BRIGHTNESS, "got {value}");
    }

    #[test]
    fn a_portrait_projection_actually_rotates() {
        let portrait = sidecar((800, 1280)).projection();
        let landscape = sidecar((1280, 800)).projection();
        assert_ne!(portrait, landscape, "the quarter turn was not applied");
    }
}
