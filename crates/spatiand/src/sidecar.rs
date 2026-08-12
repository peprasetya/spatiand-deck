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
    fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> Mat4 {
        Mat4::from_cols(
            Vec4::new(w, 0.0, 0.0, 0.0),
            Vec4::new(0.0, h, 0.0, 0.0),
            Vec4::new(0.0, 0.0, 1.0, 0.0),
            Vec4::new(x + w * 0.5, y + h * 0.5, 0.0, 1.0),
        )
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
        }
    }

    /// Landscape pixels to clip space, with a quarter turn for a portrait panel.
    ///
    /// Y is flipped here rather than in every rectangle: the layout reads top-down, which is
    /// how anyone writing it thinks, and GL's origin is at the bottom.
    fn projection(&self) -> Mat4 {
        let (w, h) = self.size;
        let to_clip = Mat4::from_scale(Vec3::new(2.0 / w, -2.0 / h, 1.0))
            * Mat4::from_translation(Vec3::new(-w * 0.5, -h * 0.5, 0.0));
        if self.portrait {
            // The panel's top edge is along its long side, so the whole image turns a quarter
            // turn. Which way matters: the wrong sign puts the text upside down, which reads
            // as a mounting problem rather than a sign.
            Mat4::from_rotation_z(std::f32::consts::FRAC_PI_2) * to_clip
        } else {
            to_clip
        }
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

        let margin = 34.0f32;
        let mut y = margin;

        // Header plate.
        quads.draw(
            gl,
            self.white,
            &(projection * layout.rect(margin, y, layout.width - margin * 2.0, 74.0)),
            PLATE,
            (0.0, 1.0),
        );
        let _ = status;
        y += 74.0 + 26.0;

        // Graphs.
        let graph_height = 132.0;
        for series in [&monitors.cpu, &monitors.gpu, &monitors.memory] {
            quads.draw(
                gl,
                self.white,
                &(projection * layout.rect(margin, y, layout.width - margin * 2.0, graph_height)),
                PLATE,
                (0.0, 1.0),
            );
            self.draw_series(
                gl,
                quads,
                &projection,
                &layout,
                series,
                margin + 16.0,
                y + 10.0,
                layout.width - margin * 2.0 - 32.0,
                graph_height - 20.0,
            );
            y += graph_height + 18.0;
        }

        // Volume and brightness as plain bars: they are values to glance at, and a slider you
        // cannot touch is a lie about what the control does.
        for (value, colour) in [(volume, ACCENT), (brightness, DIM)] {
            let Some(value) = value else { continue };
            let bar_height = 30.0;
            quads.draw(
                gl,
                self.white,
                &(projection * layout.rect(margin, y, layout.width - margin * 2.0, bar_height)),
                PLATE,
                (0.0, 1.0),
            );
            let inner = (layout.width - margin * 2.0 - 8.0) * value.clamp(0.0, 1.0);
            quads.draw(
                gl,
                self.white,
                &(projection * layout.rect(margin + 4.0, y + 4.0, inner, bar_height - 8.0)),
                colour,
                (0.0, 1.0),
            );
            y += bar_height + 14.0;
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
        let margin = 34.0f32;
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

        push(self, renderer, text, status.to_string(), margin + 18.0, margin + 20.0, 34.0);

        let mut y = margin + 74.0 + 26.0;
        for series in [&monitors.cpu, &monitors.gpu, &monitors.memory] {
            push(
                self,
                renderer,
                text,
                format!("{}  {:.0}%", series.label, series.latest() * 100.0),
                margin + 18.0,
                y + 8.0,
                26.0,
            );
            y += 132.0 + 18.0;
        }
        if let Some(v) = volume {
            push(self, renderer, text, format!("VOL {:.0}%", v * 100.0), margin + 12.0, y + 4.0, 22.0);
            y += 44.0;
        }
        if let Some(b) = brightness {
            push(self, renderer, text, format!("BRIGHT {:.0}%", b * 100.0), margin + 12.0, y + 4.0, 22.0);
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

    #[test]
    fn a_portrait_projection_actually_rotates() {
        let portrait = sidecar((800, 1280)).projection();
        let landscape = sidecar((1280, 800)).projection();
        assert_ne!(portrait, landscape, "the quarter turn was not applied");
    }
}
