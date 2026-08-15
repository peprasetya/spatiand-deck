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
use crate::system::{Monitors, Series};

use smithay::backend::renderer::gles::ffi;

/// Colours, in the same family as the world's glass.
const INK: [f32; 4] = [0.88, 0.92, 1.0, 1.0];
const DIM: [f32; 4] = [0.55, 0.62, 0.78, 1.0];
const PLATE: [f32; 4] = [0.06, 0.08, 0.13, 0.92];
const ACCENT: [f32; 4] = [0.45, 0.72, 1.0, 1.0];

/// Row heights and gaps, in landscape pixels. Named because the hit test has to agree with
/// the drawing exactly, and two copies of `34.0` do not stay equal.
const MARGIN: f32 = 34.0;
const HEADER_HEIGHT: f32 = 74.0;
const HEADER_GAP: f32 = 26.0;
const GRAPH_HEIGHT: f32 = 132.0;
const GRAPH_GAP: f32 = 18.0;

/// How tall a touchable bar is drawn.
///
/// The panel is 1280x800 landscape across roughly 151x94 mm, so a landscape pixel is about
/// 0.118 mm and there are ~8.5 to the millimetre. A fingertip contact patch is 8–10 mm wide.
/// The bars were 30 px — 3.5 mm — when nothing could touch them, which was fine for something
/// only being read and far too thin for something being aimed at.
const BAR_HEIGHT: f32 = 56.0;
const BAR_GAP: f32 = 14.0;

/// Extra height, above and below, that counts as a hit but is not drawn.
///
/// 10 px each side takes the target to 76 px ≈ 9 mm, which is a finger. Growing the *drawn*
/// bar to that instead would make two chunky slabs the eye reads as the main content, when
/// they are the least important thing on the screen.
const BAR_TOUCH_SLOP: f32 = 10.0;

/// The lowest the brightness slider will go.
///
/// Not a taste decision. At zero the panel is dark, and the control you need in order to
/// undo that is drawn on it.
const MINIMUM_BRIGHTNESS: f32 = 0.05;

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

    /// The same rectangle, grown by `pad` top and bottom.
    fn taller(&self, pad: f32) -> Self {
        Self {
            y: self.y - pad,
            h: self.h + pad * 2.0,
            ..*self
        }
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
    Volume,
    Brightness,
}

/// Every row of the sidecar, worked out once.
///
/// Drawing, laying out text and hit testing all need these numbers, and before this they each
/// walked the same `y += …` sequence separately. Three copies of a layout is three chances for
/// a touch to land on the row above the one under the finger — the kind of bug that reads as a
/// broken digitiser rather than as arithmetic.
pub struct Rows {
    pub header: Rect,
    pub graphs: [Rect; 3],
    pub volume: Option<Rect>,
    pub brightness: Option<Rect>,
}

impl Rows {
    /// Which knob is under a point, and where along it, in landscape pixels.
    pub fn knob_at(&self, x: f32, y: f32) -> Option<(Knob, f32)> {
        for (knob, rect) in [(Knob::Volume, self.volume), (Knob::Brightness, self.brightness)] {
            if let Some(rect) = rect {
                if rect.taller(BAR_TOUCH_SLOP).contains(x, y) {
                    return Some((knob, rect.fraction(x)));
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
    pub fn rows(&self, volume: Option<f32>, brightness: Option<f32>) -> Rows {
        let (width, _) = self.size;
        let full = width - MARGIN * 2.0;
        let mut y = MARGIN;

        let header = Rect { x: MARGIN, y, w: full, h: HEADER_HEIGHT };
        y += HEADER_HEIGHT + HEADER_GAP;

        let graphs = std::array::from_fn(|_| {
            let r = Rect { x: MARGIN, y, w: full, h: GRAPH_HEIGHT };
            y += GRAPH_HEIGHT + GRAPH_GAP;
            r
        });

        // A bar exists only if there is a value for it: a brightness slider on a machine with
        // no backlight control would be a control that does nothing, which is worse than an
        // absent one.
        let mut bar = |present: bool| -> Option<Rect> {
            present.then(|| {
                let r = Rect { x: MARGIN, y, w: full, h: BAR_HEIGHT };
                y += BAR_HEIGHT + BAR_GAP;
                r
            })
        };
        let volume = bar(volume.is_some());
        let brightness = bar(brightness.is_some());

        Rows { header, graphs, volume, brightness }
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
        volume: Option<f32>,
        brightness: Option<f32>,
    ) -> Vec<Knob> {
        use spatiand_input::TouchEvent;
        let rows = self.rows(volume, brightness);
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
                            changed.push(knob);
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
                            changed.push(knob);
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
    pub fn knob_value(&self, knob: Knob, volume: Option<f32>, brightness: Option<f32>) -> Option<f32> {
        let (slot, held) = self.held?;
        if held != knob {
            return None;
        }
        let (_, x, _) = self.touches.iter().find(|(s, _, _)| *s == slot)?;
        let rows = self.rows(volume, brightness);
        let rect = match knob {
            Knob::Volume => rows.volume?,
            Knob::Brightness => rows.brightness?,
        };
        let fraction = rect.fraction(*x);
        Some(match knob {
            // A backlight dragged to zero turns off the screen the slider is drawn on, and
            // there is then nothing to see in order to drag it back. The floor is what makes
            // the control safe to explore.
            Knob::Brightness => fraction.max(MINIMUM_BRIGHTNESS),
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
        monitors: &Monitors,
        status: &str,
        volume: Option<f32>,
        brightness: Option<f32>,
        prepared: &[(String, (u32, f32), f32, f32, f32)],
    ) {
        let layout = Layout {
            width: self.size.0,
            height: self.size.1,
        };
        let projection = self.projection();

        // Backdrop.
        quads.draw(
            gl,
            self.white,
            &(projection * layout.rect(0.0, 0.0, layout.width, layout.height)),
            [0.02, 0.03, 0.05, 1.0],
            (0.0, 1.0),
        );

        let rows = self.rows(volume, brightness);

        // Header plate.
        quads.draw(gl, self.white, &(projection * layout.of(rows.header)), PLATE, (0.0, 1.0));
        let _ = status;

        // Graphs.
        for (series, rect) in [&monitors.cpu, &monitors.gpu, &monitors.memory]
            .into_iter()
            .zip(rows.graphs)
        {
            quads.draw(gl, self.white, &(projection * layout.of(rect)), PLATE, (0.0, 1.0));
            self.draw_series(
                gl,
                quads,
                &projection,
                &layout,
                series,
                rect.x + 16.0,
                rect.y + 10.0,
                rect.w - 32.0,
                rect.h - 20.0,
            );
        }

        // Volume and brightness. These are sliders now rather than readouts, so they are drawn
        // with a handle: a filled bar alone says "this is how loud it is" where a handle says
        // "this is how loud it is, and you may move it".
        for (value, rect, colour) in [
            (volume, rows.volume, ACCENT),
            (brightness, rows.brightness, DIM),
        ] {
            let (Some(value), Some(rect)) = (value, rect) else {
                continue;
            };
            let value = value.clamp(0.0, 1.0);
            quads.draw(gl, self.white, &(projection * layout.of(rect)), PLATE, (0.0, 1.0));
            let inner = (rect.w - 8.0) * value;
            quads.draw(
                gl,
                self.white,
                &(projection * layout.rect(rect.x + 4.0, rect.y + 4.0, inner, rect.h - 8.0)),
                colour,
                (0.0, 1.0),
            );
            let handle = 10.0;
            quads.draw(
                gl,
                self.white,
                &(projection
                    * layout.rect(
                        rect.x + 4.0 + (inner - handle).max(0.0),
                        rect.y + 2.0,
                        handle,
                        rect.h - 4.0,
                    )),
                INK,
                (0.0, 1.0),
            );
        }

        // Wherever a finger is. This is the only confirmation the panel gives that a touch
        // arrived at all, and it is what tells a wrong quarter turn from a dead digitiser:
        // a dot that mirrors the finger is a sign error, a dot that never appears is not.
        for (_, x, y) in &self.touches {
            let size = 40.0;
            quads.draw(
                gl,
                self.white,
                &(projection * layout.rect(x - size * 0.5, y - size * 0.5, size, size)),
                [1.0, 1.0, 1.0, 0.28],
                (0.0, 1.0),
            );
        }

        // Text last, so it is never behind a plate.
        for (_, (id, aspect), x, ty, height) in prepared {
            let width = height * aspect.max(0.01);
            quads.draw(
                gl,
                *id,
                &(projection * layout.rect(*x, *ty, width, *height)),
                INK,
                (0.0, 1.0),
            );
        }
    }

    /// A filled area graph, oldest sample on the left.
    ///
    /// Drawn as one column per sample rather than a line: at this size a line is a single pixel
    /// that disappears against the plate, and a filled area reads as a shape from across a
    /// room, which is the whole point of putting it on a screen you glance at.
    #[allow(clippy::too_many_arguments)]
    unsafe fn draw_series(
        &self,
        gl: &ffi::Gles2,
        quads: &QuadPipeline,
        projection: &Mat4,
        layout: &Layout,
        series: &Series,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) {
        let count = crate::system::HISTORY as f32;
        let column = width / count;
        for (i, value) in series.samples().enumerate() {
            // Right-aligned, so the newest sample is always at the same edge even before the
            // history has filled up. A left-aligned graph appears to scroll while filling and
            // then stops, which looks like it has frozen.
            let offset = count - series.len() as f32 + i as f32;
            let bar = (value * height).max(1.0);
            quads.draw(
                gl,
                self.white,
                &(*projection
                    * layout.rect(
                        x + offset * column,
                        y + height - bar,
                        column.max(1.0),
                        bar,
                    )),
                [0.36, 0.62, 0.95, 0.85],
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
        volume: Option<f32>,
        brightness: Option<f32>,
    ) -> Vec<(String, (u32, f32), f32, f32, f32)> {
        let rows = self.rows(volume, brightness);
        let mut out = Vec::new();
        let mut push = |this: &mut Self,
                        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
                        text: &mut spatiand_render::TextRenderer,
                        s: String,
                        x: f32,
                        y: f32,
                        h: f32| {
            if let Some(entry) = this.label(renderer, text, &s, h * 1.35) {
                out.push((s, entry, x, y, h));
            }
        };

        let header = rows.header;
        push(self, renderer, text, status.to_string(), header.x + 18.0, header.y + 20.0, 34.0);

        for (series, rect) in [&monitors.cpu, &monitors.gpu, &monitors.memory]
            .into_iter()
            .zip(rows.graphs)
        {
            push(
                self,
                renderer,
                text,
                format!("{}  {:.0}%", series.label, series.latest() * 100.0),
                rect.x + 18.0,
                rect.y + 8.0,
                26.0,
            );
        }
        for (label, rect) in [
            (volume.map(|v| format!("VOL {:.0}%", v * 100.0)), rows.volume),
            (brightness.map(|b| format!("BRIGHT {:.0}%", b * 100.0)), rows.brightness),
        ] {
            let (Some(label), Some(rect)) = (label, rect) else {
                continue;
            };
            // Centred in the bar rather than at its top: the bar is tall enough to touch now,
            // and text pinned to the top edge of a 56 px slab looks like it belongs to the row
            // above it.
            push(self, renderer, text, label, rect.x + 14.0, rect.y + (rect.h - 22.0) * 0.5, 22.0);
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
        let row = s.rows(Some(0.5), Some(0.5)).graphs[0];
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
        let rows = s.rows(Some(0.5), Some(0.5));
        let bottom = rows.brightness.expect("brightness row").y + BAR_HEIGHT;
        assert!(bottom + MARGIN <= s.size.1, "content runs to {bottom} of {}", s.size.1);
    }

    #[test]
    fn rows_do_not_overlap() {
        // knob_at walks them in order and returns the first hit, so an overlap would make one
        // control permanently unreachable.
        let s = sidecar((800, 1280));
        let rows = s.rows(Some(0.5), Some(0.5));
        let volume = rows.volume.expect("volume row");
        let brightness = rows.brightness.expect("brightness row");
        assert!(rows.graphs[2].y + rows.graphs[2].h <= volume.y);
        assert!(volume.taller(BAR_TOUCH_SLOP).y + volume.taller(BAR_TOUCH_SLOP).h <= brightness.y);
    }

    #[test]
    fn a_touch_target_is_a_fingers_width() {
        // ~8.5 landscape pixels to the millimetre on this panel; a fingertip is 8-10 mm. The
        // original 30 px bar was 3.5 mm, which is a stylus target, not a thumb one.
        let millimetre = 800.0 / 94.1;
        let height = BAR_HEIGHT + BAR_TOUCH_SLOP * 2.0;
        assert!(height / millimetre >= 8.0, "target is {} mm", height / millimetre);
    }

    #[test]
    fn an_absent_reading_leaves_out_the_row_it_would_control() {
        // A machine with no backlight control should not offer a brightness slider that does
        // nothing -- and the volume bar must not shift when it is absent.
        let s = sidecar((800, 1280));
        let both = s.rows(Some(0.5), Some(0.5));
        let volume_only = s.rows(Some(0.5), None);
        assert!(volume_only.brightness.is_none());
        assert_eq!(volume_only.volume, both.volume);
        let neither = s.rows(None, None);
        assert!(neither.volume.is_none() && neither.brightness.is_none());
        assert!(neither.knob_at(400.0, 700.0).is_none());
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
        let volume = s.rows(Some(0.5), Some(0.5)).volume.expect("volume row");
        let quarter = volume.x + volume.w * 0.25;
        let event = press(&s, 0, quarter, volume.y + volume.h * 0.5);
        assert_eq!(s.touch(&[event], Some(0.5), Some(0.5)), vec![Knob::Volume]);
        let value = s.knob_value(Knob::Volume, Some(0.5), Some(0.5)).expect("a value");
        assert!((value - 0.25).abs() < 0.02, "got {value}");
    }

    #[test]
    fn a_drag_that_leaves_the_bar_still_controls_it() {
        // Every slider anywhere behaves this way, and on a 9 mm target a thumb drifts off it
        // constantly. Re-testing the position each frame would drop the drag mid-gesture.
        let mut s = sidecar((800, 1280));
        let volume = s.rows(Some(0.5), Some(0.5)).volume.expect("volume row");
        let down = press(&s, 0, volume.x + 10.0, volume.y + volume.h * 0.5);
        s.touch(&[down], Some(0.5), Some(0.5));
        // Well above the bar, and three quarters of the way across.
        let away = drag(&s, 0, volume.x + volume.w * 0.75, volume.y - 120.0);
        assert_eq!(s.touch(&[away], Some(0.5), Some(0.5)), vec![Knob::Volume]);
        let value = s.knob_value(Knob::Volume, Some(0.5), Some(0.5)).expect("still held");
        assert!((value - 0.75).abs() < 0.02, "got {value}");
    }

    #[test]
    fn lifting_releases_the_knob() {
        let mut s = sidecar((800, 1280));
        let volume = s.rows(Some(0.5), Some(0.5)).volume.expect("volume row");
        s.touch(&[press(&s, 0, volume.x + 40.0, volume.y + 20.0)], Some(0.5), Some(0.5));
        s.touch(&[spatiand_input::TouchEvent::Up { slot: 0 }], Some(0.5), Some(0.5));
        assert!(s.knob_value(Knob::Volume, Some(0.5), Some(0.5)).is_none());
        assert!(s.touches.is_empty(), "the dot should go with the finger");
    }

    #[test]
    fn a_second_finger_cannot_steal_a_knob_mid_drag() {
        // A palm or a second thumb landing on the same bar would otherwise take the slider
        // and jump the value to wherever it touched.
        let mut s = sidecar((800, 1280));
        let volume = s.rows(Some(0.5), Some(0.5)).volume.expect("volume row");
        s.touch(&[press(&s, 0, volume.x + 10.0, volume.y + 20.0)], Some(0.5), Some(0.5));
        let intruder = press(&s, 1, volume.x + volume.w - 10.0, volume.y + 20.0);
        s.touch(&[intruder], Some(0.5), Some(0.5));
        let value = s.knob_value(Knob::Volume, Some(0.5), Some(0.5)).expect("still ours");
        assert!(value < 0.1, "the first finger should still own it, got {value}");
    }

    #[test]
    fn touching_a_graph_changes_nothing() {
        let mut s = sidecar((800, 1280));
        let graph = s.rows(Some(0.5), Some(0.5)).graphs[1];
        let event = press(&s, 0, graph.x + graph.w * 0.5, graph.y + graph.h * 0.5);
        assert!(s.touch(&[event], Some(0.5), Some(0.5)).is_empty());
        assert_eq!(s.touches.len(), 1, "but it should still show a dot");
    }

    #[test]
    fn brightness_cannot_be_dragged_to_black() {
        // At zero the panel goes dark, and the slider you need in order to undo it is drawn
        // on that panel.
        let mut s = sidecar((800, 1280));
        let bar = s.rows(Some(0.5), Some(0.5)).brightness.expect("brightness row");
        s.touch(&[press(&s, 0, bar.x - 200.0, bar.y + 20.0)], Some(0.5), Some(0.5));
        // Pressing left of the bar still grabs nothing; press on it, then drag off the left.
        s.touch(&[press(&s, 1, bar.x + 40.0, bar.y + 20.0)], Some(0.5), Some(0.5));
        s.touch(&[drag(&s, 1, bar.x - 500.0, bar.y + 20.0)], Some(0.5), Some(0.5));
        let value = s.knob_value(Knob::Brightness, Some(0.5), Some(0.5)).expect("held");
        assert!(value >= MINIMUM_BRIGHTNESS, "got {value}");
    }

    #[test]
    fn a_portrait_projection_actually_rotates() {
        let portrait = sidecar((800, 1280)).projection();
        let landscape = sidecar((1280, 800)).projection();
        assert_ne!(portrait, landscape, "the quarter turn was not applied");
    }
}
