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
        }
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
                    if let Some((slot, knob)) = self.held {
                        if slot == c.slot {
                            changed.push(Action::Moved(knob));
                        }
                    }
                }
                TouchEvent::Up { slot } => {
                    self.touches.retain(|(s, _, _)| *s != slot);
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

        // Wherever a finger is. This is the only confirmation the panel gives that a touch
        // arrived at all, and it is what tells a wrong quarter turn from a dead digitiser:
        // a dot that mirrors the finger is a sign error, a dot that never appears is not.
        //
        // Round, because a fingertip is, and a square one looked like a rendering fault.
        for (_, x, y) in &self.touches {
            let size = 56.0;
            round(
                Rect { x: x - size * 0.5, y: y - size * 0.5, w: size, h: size },
                [1.0, 1.0, 1.0, 0.22],
                size * 0.5,
            );
        }

        // Text last, so it is never behind a card.
        for label in prepared {
            let width = label.height * label.texture.1.max(0.01);
            quads.draw(
                gl,
                label.texture.0,
                &(projection * layout.rect(label.x, label.y, width, label.height)),
                label.colour,
                (0.0, 1.0),
            );
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
    ) -> Vec<Label> {
        let rows = self.rows(levels);
        let mut out = Vec::new();
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
        push(
            self,
            renderer,
            text,
            status.to_string(),
            header.x + 4.0,
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
