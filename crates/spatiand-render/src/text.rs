//! Text rasterisation.
//!
//! Produces plain RGBA images on the CPU; uploading them is the GL layer's job. Keeping the
//! split there means text can be tested without a GPU, a display or a headset — and the
//! things worth testing about text (does it wrap, does it fit, is it actually opaque where
//! glyphs are) are all CPU-side properties.
//!
//! Sizing is in **degrees of field of view**, not pixels. Each eye gets 1920 px across about
//! 40°, so roughly 48 px per degree — but that ratio changes with the headset, and a UI
//! specified in pixels would silently become unreadable on different optics. Asking for "1.5°
//! tall" is a statement about legibility that survives a hardware change.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};

/// An RGBA8 image, straight (non-premultiplied) alpha.
#[derive(Debug, Clone, PartialEq)]
pub struct TextImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl TextImage {
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Fraction of pixels with any coverage. Used by tests to tell "drew something" from
    /// "silently drew nothing", which is otherwise an easy failure to miss.
    pub fn ink_fraction(&self) -> f32 {
        if self.rgba.is_empty() {
            return 0.0;
        }
        let inked = self.rgba.chunks_exact(4).filter(|p| p[3] > 8).count();
        inked as f32 / (self.rgba.len() / 4) as f32
    }
}

pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl TextRenderer {
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        }
    }

    /// Pixels per degree for a given eye. See the module note on why sizing is angular.
    pub fn px_per_degree(eye_width_px: u32, h_fov_deg: f64) -> f32 {
        (eye_width_px as f64 / h_fov_deg) as f32
    }

    /// Rasterise `text` into an image `max_width` px wide, wrapping as needed.
    ///
    /// `size_px` is the em size; `color` is straight RGBA.
    pub fn render(&mut self, text: &str, size_px: f32, max_width: u32, color: [u8; 4]) -> TextImage {
        // Generous line spacing: at a 40 degree field the eye travels a long way between
        // lines, and tight leading reads as cramped in a way it does not on a monitor.
        let metrics = Metrics::new(size_px, size_px * 1.4);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        let attrs = Attrs::new().family(Family::SansSerif);
        {
            // 0.19 moved Buffer's mutating methods behind `borrow_with`, which pairs the
            // buffer with its font system for the duration rather than threading it through
            // every call.
            let mut b = buffer.borrow_with(&mut self.font_system);
            b.set_size(Some(max_width as f32), None);
            b.set_text(text, &attrs, Shaping::Advanced, None);
            b.shape_until_scroll(false);
        }

        // Measure before allocating, so a short string does not carry a full-width bitmap
        // around with it.
        let mut used_width = 0.0f32;
        let mut lines = 0usize;
        for run in buffer.layout_runs() {
            used_width = used_width.max(run.line_w);
            lines += 1;
        }
        let width = (used_width.ceil() as u32).clamp(1, max_width);
        let height = ((lines.max(1) as f32) * metrics.line_height).ceil() as u32;

        let mut rgba = vec![0u8; (width * height * 4) as usize];
        let text_color = cosmic_text::Color::rgba(color[0], color[1], color[2], color[3]);
        let mut drawable = buffer.borrow_with(&mut self.font_system);
        drawable.draw(
            &mut self.swash_cache,
            text_color,
            |x, y, w, h, c| {
                let a = c.a();
                if a == 0 {
                    return;
                }
                for dy in 0..h as i32 {
                    for dx in 0..w as i32 {
                        let (px, py) = (x + dx, y + dy);
                        if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                            continue;
                        }
                        let i = ((py as u32 * width + px as u32) * 4) as usize;
                        // Source-over against what is already there, so overlapping glyphs
                        // (accents, tight scripts) composite instead of punching holes.
                        let sa = a as u32;
                        let da = rgba[i + 3] as u32;
                        let out_a = sa + da * (255 - sa) / 255;
                        if out_a == 0 {
                            continue;
                        }
                        for k in 0..3 {
                            let sc = [c.r(), c.g(), c.b()][k] as u32;
                            let dc = rgba[i + k] as u32;
                            rgba[i + k] =
                                ((sc * sa + dc * da * (255 - sa) / 255) / out_a).min(255) as u8;
                        }
                        rgba[i + 3] = out_a.min(255) as u8;
                    }
                }
            },
        );

        TextImage {
            width,
            height,
            rgba,
        }
    }
}

impl Default for TextRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px_per_degree_matches_the_air() {
        // 1920 px across 40 degrees is the number every UI size is derived from.
        let ppd = TextRenderer::px_per_degree(1920, 40.0);
        assert!((ppd - 48.0).abs() < 0.1, "expected ~48 px/deg, got {ppd}");
    }

    #[test]
    fn renders_visible_glyphs() {
        let mut r = TextRenderer::new();
        let img = r.render("Turn LEFT", 48.0, 800, [255, 255, 255, 255]);
        assert!(!img.is_empty(), "produced no image");
        assert_eq!(img.rgba.len(), (img.width * img.height * 4) as usize);
        // The failure this guards against is a correct-looking image full of zeros, which
        // renders as an invisible prompt and looks like the compositor having hung.
        assert!(
            img.ink_fraction() > 0.02,
            "almost nothing was drawn (ink {:.4})",
            img.ink_fraction()
        );
    }

    #[test]
    fn empty_text_is_survivable() {
        let mut r = TextRenderer::new();
        let img = r.render("", 48.0, 800, [255, 255, 255, 255]);
        assert_eq!(img.ink_fraction(), 0.0);
        // Must still be a valid 1x1-or-larger buffer rather than a zero-sized texture, which
        // GL rejects.
        assert!(img.width >= 1 && img.height >= 1);
    }

    #[test]
    fn narrow_width_wraps_into_more_lines() {
        let mut r = TextRenderer::new();
        let text = "Slowly turn your head to the left, like saying no, and hold.";
        let wide = r.render(text, 32.0, 1600, [255, 255, 255, 255]);
        let narrow = r.render(text, 32.0, 400, [255, 255, 255, 255]);
        assert!(
            narrow.height > wide.height,
            "wrapping should add lines: {} vs {}",
            narrow.height,
            wide.height
        );
        assert!(narrow.width <= 400);
    }

    #[test]
    fn colour_is_respected() {
        let mut r = TextRenderer::new();
        let img = r.render("X", 64.0, 200, [255, 0, 0, 255]);
        let reddest = img
            .rgba
            .chunks_exact(4)
            .filter(|p| p[3] > 200)
            .map(|p| (p[0], p[1], p[2]))
            .next()
            .expect("should have an opaque pixel");
        assert!(
            reddest.0 > 200 && reddest.1 < 80 && reddest.2 < 80,
            "expected red, got {reddest:?}"
        );
    }
}
