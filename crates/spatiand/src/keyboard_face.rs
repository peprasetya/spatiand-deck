//! Rasterising the on-screen keyboard's face.
//!
//! One texture for the whole keyboard rather than a quad per key. Sixty-odd quads with sixty-odd
//! labels would be sixty-odd uploads every time a modifier latched, and at the size a key is
//! actually drawn — about a degree across — the difference is invisible. The hit-testing is
//! arithmetic on the pointer's UV either way, and it lives in [`spatiand_shell::keyboard`] so
//! that what is drawn here and what can be pressed come from one set of numbers.
//!
//! The keycaps are drawn rather than written. The first version set the whole face as a single
//! block of padded monospaced text, which is legible and reads as a terminal: there is nothing
//! to press, only characters arranged in a grid.

use spatiand_render::{TextImage, TextRenderer};
use spatiand_shell::keyboard::{layout, Keyboard, KeyRect, Role};

/// Gap between neighbouring keycaps, as a fraction of a cell's shorter side.
///
/// The gap belongs to the *drawing* only — the cells themselves tile the face exactly, so a
/// press landing in a gap still counts as the nearer key. A gap that ate touches would be a
/// dead strip nobody could see, and with a head-anchored ray it would read as bad aim.
const GAP: f32 = 0.13;

/// Keycap corner radius, as a fraction of the cap's shorter side.
const RADIUS: f32 = 0.24;

/// The face's own ground, behind the caps.
///
/// Opaque, like everything else here. A keyboard is a thing you read glyphs off while typing
/// into something else, and a translucent one has whatever is behind it — a bright web page,
/// most likely — showing through the gaps between the keys and competing with the labels. The
/// rest of the shell is glass because glass is furniture; this is an instrument.
const GROUND: [f32; 4] = [0.055, 0.065, 0.095, 1.0];

/// An ordinary keycap.
const CAP: [f32; 4] = [0.20, 0.23, 0.30, 1.0];
/// Modifiers and the named keys, held back so the letters are what the eye lands on.
const CAP_MODIFIER: [f32; 4] = [0.13, 0.15, 0.21, 1.0];
/// A latched modifier. The one saturated thing on the face, because it is the one piece of
/// state the wearer cannot otherwise see — a shift that is on and looks off types the wrong
/// character and looks like a broken keymap.
const CAP_LATCHED: [f32; 4] = [0.42, 0.68, 1.0, 1.0];

const INK: [u8; 4] = [236, 242, 255, 255];
const INK_MODIFIER: [u8; 4] = [186, 199, 224, 255];
const INK_LATCHED: [u8; 4] = [8, 14, 28, 255];

/// Rasterise the keyboard's face at `width_px` across.
///
/// The height follows from the layout's own aspect rather than from the image, so the face is
/// the shape the layout says it is and a change to the rows cannot quietly restretch the keys.
pub fn face(text: &mut TextRenderer, keyboard: &Keyboard, width_px: u32) -> TextImage {
    let aspect = spatiand_shell::keyboard::face_aspect();
    let width = width_px.max(64);
    let height = ((width as f64 / aspect).round() as u32).max(32);
    // Filled rather than cleared to transparent: the face is opaque, so the gaps between the
    // caps are its own ground rather than a window onto whatever is behind the keyboard.
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for _ in 0..(width as usize) * (height as usize) {
        rgba.extend_from_slice(&[
            (GROUND[0] * 255.0) as u8,
            (GROUND[1] * 255.0) as u8,
            (GROUND[2] * 255.0) as u8,
            255,
        ]);
    }

    for (key, rect) in layout() {
        let cap = cap_rect(&rect, width, height);
        let latched = keyboard.is_latched(key);
        let (fill, ink) = match (latched, key.role) {
            (true, _) => (CAP_LATCHED, INK_LATCHED),
            (false, Role::Modifier(_)) => (CAP_MODIFIER, INK_MODIFIER),
            (false, Role::Normal) => (CAP, if key.label.len() > 1 { INK_MODIFIER } else { INK }),
        };
        let radius = cap.2.min(cap.3) * RADIUS;
        rounded_rect(&mut rgba, width, height, cap, radius, fill);

        let label = keyboard.label(key);
        if !label.is_empty() {
            let glyphs = fit_label(text, label, cap.2, cap.3, ink);
            blit_centred(&mut rgba, width, height, &glyphs, cap);
        }
    }

    TextImage { width, height, rgba }
}

/// A key's drawn cap, as `(x, y, w, h)` in pixels — its cell inset by the gap.
fn cap_rect(rect: &KeyRect, width: u32, height: u32) -> (f32, f32, f32, f32) {
    let (w, h) = (width as f32, height as f32);
    let cell_w = (rect.half_u * 2.0) as f32 * w;
    let cell_h = (rect.half_v * 2.0) as f32 * h;
    let inset = cell_w.min(cell_h) * GAP * 0.5;
    (
        (rect.u as f32) * w - cell_w * 0.5 + inset,
        (rect.v as f32) * h - cell_h * 0.5 + inset,
        (cell_w - inset * 2.0).max(1.0),
        (cell_h - inset * 2.0).max(1.0),
    )
}

/// Render a label at the largest size that still fits inside a keycap.
///
/// Two passes at most. Guessing the size from the character count alone is wrong for the cases
/// that matter — "space" against "w" — and measuring is one extra rasterisation of a string
/// that is never more than five characters long.
fn fit_label(
    text: &mut TextRenderer,
    label: &str,
    cap_w: f32,
    cap_h: f32,
    ink: [u8; 4],
) -> TextImage {
    // Room inside the cap. Letters get most of the height; a word has to leave side padding or
    // it touches the cap's rounded corners.
    let room_w = cap_w * 0.82;
    let room_h = cap_h * 0.54;
    let first = text.render(label, room_h, room_w.max(8.0) as u32, ink);
    if first.width as f32 <= room_w || first.width == 0 {
        return first;
    }
    // Too wide: shrink by exactly the overshoot rather than by a guess.
    let shrink = (room_w / first.width as f32).clamp(0.25, 1.0);
    text.render(label, room_h * shrink, room_w.max(8.0) as u32, ink)
}

/// Fill a rounded rectangle, compositing over whatever is already there.
fn rounded_rect(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rect: (f32, f32, f32, f32),
    radius: f32,
    colour: [f32; 4],
) {
    let (rx, ry, rw, rh) = rect;
    let radius = radius.max(0.0).min(rw.min(rh) * 0.5);
    let x0 = (rx.floor().max(0.0)) as u32;
    let y0 = (ry.floor().max(0.0)) as u32;
    let x1 = ((rx + rw).ceil().min(width as f32)) as u32;
    let y1 = ((ry + rh).ceil().min(height as f32)) as u32;

    for y in y0..y1 {
        for x in x0..x1 {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Distance outside the rounded rectangle, in pixels.
            let dx = (rx + radius - fx).max(fx - (rx + rw - radius)).max(0.0);
            let dy = (ry + radius - fy).max(fy - (ry + rh - radius)).max(0.0);
            let outside = (dx * dx + dy * dy).sqrt() - radius;
            // One pixel of feathering, enough to kill the jaggies at this angular size.
            let coverage = (0.5 - outside).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            // A gentle vertical lift, so a cap reads as a surface catching light rather than a
            // flat swatch. Subtle on purpose: at a degree across, a strong gradient just looks
            // like the texture is dirty.
            let t = ((fy - ry) / rh.max(1.0)).clamp(0.0, 1.0);
            let lift = 1.0 + (0.18 - 0.30 * t);
            let src = [
                (colour[0] * lift).min(1.0),
                (colour[1] * lift).min(1.0),
                (colour[2] * lift).min(1.0),
                colour[3] * coverage,
            ];
            over(rgba, ((y * width + x) * 4) as usize, src);
        }
    }
}

/// Composite a rasterised label into the middle of a rectangle.
fn blit_centred(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    src: &TextImage,
    rect: (f32, f32, f32, f32),
) {
    if src.is_empty() {
        return;
    }
    let (rx, ry, rw, rh) = rect;
    let left = (rx + (rw - src.width as f32) * 0.5).round() as i64;
    let top = (ry + (rh - src.height as f32) * 0.5).round() as i64;
    for sy in 0..src.height {
        let dy = top + sy as i64;
        if dy < 0 || dy >= height as i64 {
            continue;
        }
        for sx in 0..src.width {
            let dx = left + sx as i64;
            if dx < 0 || dx >= width as i64 {
                continue;
            }
            let s = ((sy * src.width + sx) * 4) as usize;
            let a = src.rgba[s + 3] as f32 / 255.0;
            if a <= 0.0 {
                continue;
            }
            over(
                rgba,
                ((dy as u32 * width + dx as u32) * 4) as usize,
                [
                    src.rgba[s] as f32 / 255.0,
                    src.rgba[s + 1] as f32 / 255.0,
                    src.rgba[s + 2] as f32 / 255.0,
                    a,
                ],
            );
        }
    }
}

/// Source-over, on straight (non-premultiplied) alpha.
fn over(rgba: &mut [u8], at: usize, src: [f32; 4]) {
    let dst_a = rgba[at + 3] as f32 / 255.0;
    let out_a = src[3] + dst_a * (1.0 - src[3]);
    if out_a <= 0.0 {
        rgba[at..at + 4].fill(0);
        return;
    }
    for c in 0..3 {
        let d = rgba[at + c] as f32 / 255.0;
        let value = (src[c] * src[3] + d * dst_a * (1.0 - src[3])) / out_a;
        rgba[at + c] = (value.clamp(0.0, 1.0) * 255.0) as u8;
    }
    rgba[at + 3] = (out_a.clamp(0.0, 1.0) * 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use spatiand_shell::keyboard::ROWS;

    fn ink_fraction(image: &TextImage) -> f32 {
        image.ink_fraction()
    }

    #[test]
    fn the_face_is_the_shape_the_layout_says_it_is() {
        let mut text = TextRenderer::new();
        let image = face(&mut text, &Keyboard::default(), 1200);
        let aspect = image.width as f64 / image.height as f64;
        assert!(
            (aspect - spatiand_shell::keyboard::face_aspect()).abs() < 0.02,
            "face drew at {aspect}"
        );
    }

    #[test]
    fn the_face_is_opaque_everywhere() {
        // Nothing behind the keyboard may show through it. A translucent face puts whatever is
        // being typed into -- a bright page, usually -- in among the labels.
        let mut text = TextRenderer::new();
        let image = face(&mut text, &Keyboard::default(), 900);
        assert_eq!(ink_fraction(&image), 1.0, "part of the face is see-through");
    }

    #[test]
    fn the_caps_stand_out_from_the_ground_between_them() {
        // With everything opaque, "are there keys on this" is a question about colour rather
        // than about alpha. The failure it guards is a face that rasterises to a flat slab,
        // which in the world reads as a placement bug rather than as an empty texture.
        let mut text = TextRenderer::new();
        let image = face(&mut text, &Keyboard::default(), 900);
        let ground = [
            (GROUND[0] * 255.0) as u8,
            (GROUND[1] * 255.0) as u8,
            (GROUND[2] * 255.0) as u8,
        ];
        let on_ground = image
            .rgba
            .chunks_exact(4)
            .filter(|p| p[0] == ground[0] && p[1] == ground[1] && p[2] == ground[2])
            .count() as f32
            / (image.rgba.len() / 4) as f32;
        // Gaps exist, so it reads as keys...
        assert!(on_ground > 0.05, "only {:.1}% is gap; the caps have run together", on_ground * 100.0);
        // ...but the caps are most of it.
        assert!(on_ground < 0.6, "{:.1}% is bare ground; the caps are too small", on_ground * 100.0);
    }

    #[test]
    fn a_latched_modifier_looks_different_from_a_loose_one() {
        // The only way to see that shift is on. If these two images match, the latch is
        // invisible and the wearer types the wrong case with no way to tell why.
        let mut text = TextRenderer::new();
        let loose = face(&mut text, &Keyboard::default(), 900);
        let mut latched_kb = Keyboard::default();
        latched_kb.press(&ROWS[3][0]);
        assert!(latched_kb.shift);
        let latched = face(&mut text, &latched_kb, 900);
        assert_ne!(loose.rgba, latched.rgba, "a latched shift must be visible");
    }

    #[test]
    fn shifted_labels_are_drawn_when_shift_is_latched() {
        let mut text = TextRenderer::new();
        let plain = face(&mut text, &Keyboard::default(), 900);
        let mut shifted_kb = Keyboard::default();
        shifted_kb.shift = true;
        let shifted = face(&mut text, &shifted_kb, 900);
        assert_ne!(plain.rgba, shifted.rgba);
    }

    #[test]
    fn a_wide_key_gets_a_label_that_fits_inside_it() {
        // "space" in a letter-sized type would run off both ends of its cap. The check is that
        // the fitted label is narrower than the room it was given.
        let mut text = TextRenderer::new();
        let fitted = fit_label(&mut text, "space", 200.0, 40.0, INK);
        assert!(fitted.width as f32 <= 200.0 * 0.82 + 1.0, "label is {} wide", fitted.width);
        assert!(fitted.width > 0, "the label rasterised to nothing");
    }

    #[test]
    fn compositing_a_transparent_pixel_leaves_the_ground_alone() {
        let mut rgba = vec![10u8, 20, 30, 255];
        over(&mut rgba, 0, [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(rgba, vec![10, 20, 30, 255]);
    }

    #[test]
    fn compositing_an_opaque_pixel_replaces_the_ground() {
        let mut rgba = vec![10u8, 20, 30, 255];
        over(&mut rgba, 0, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(rgba, vec![255, 0, 0, 255]);
    }
}
