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
use spatiand_shell::keyboard::{layout, Key, KeyRect, Keyboard, Role};

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

/// The speaker in the strip above the keys, sounding and silent.
///
/// U+1F50A SPEAKER WITH THREE SOUND WAVES and U+1F507 SPEAKER WITH CANCELLATION STROKE. Both
/// come out of the same font stack the rest of the shell sets its symbols from, monochrome
/// rather than as colour emoji -- checked by `the_speaker_symbols_are_really_in_the_font`
/// below, because a missing glyph here is a tofu box on a control with no caption to fall back
/// on. This is the pair every phone and desktop uses, so it needs no caption.
pub const SOUND_ON: &str = "\u{1F50A}";
pub const SOUND_OFF: &str = "\u{1F507}";

const INK: [u8; 4] = [236, 242, 255, 255];
const INK_MODIFIER: [u8; 4] = [186, 199, 224, 255];
const INK_LATCHED: [u8; 4] = [8, 14, 28, 255];

/// How far a raised cap's picture extends past its cell on every side, as a fraction of the
/// cell, to leave room for the shadow. The scene grows the quad by the same amount, so the cap
/// drawn inside the image lands exactly on the key it stands for.
pub const CAP_MARGIN: f32 = 0.20;

/// How much brighter a raised cap is than one lying flat.
///
/// The *only* colour that changes: the legend stays the shade it is on the face. A raised key
/// that also restyled its lettering read as a second key appearing rather than as one key
/// coming up to meet the thumb.
const RAISE: f32 = 1.85;

/// The shadow under a raised cap: how far it falls and how far it fades, as fractions of the
/// cap's shorter side.
const SHADOW_DROP: f32 = 0.10;
const SHADOW_BLUR: f32 = 0.26;
const SHADOW: [f32; 4] = [0.0, 0.0, 0.02, 0.66];

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
        let (fill, ink) = appearance(keyboard, key);
        let radius = cap.2.min(cap.3) * RADIUS;
        rounded_rect(&mut rgba, width, height, cap, radius, fill);

        let label = keyboard.label(key);
        if !label.is_empty() {
            let glyphs = fit_label(text, label, cap.2, cap.3, ink);
            blit_centred(&mut rgba, width, height, &glyphs, cap);
        }
    }

    // The sound toggle, in the strip above the keys. Drawn by the same code as a keycap, at
    // the same inset, so it reads as something to press rather than as a decal on the frame.
    //
    // Not given a latched modifier's colour, though it is a piece of state. The saturated blue
    // is spent on shift and ctrl because those change what the *next* key does and are
    // otherwise invisible; this one announces itself the moment you type, and the glyph itself
    // says which way it is. Making it the brightest thing on a keyboard whose default state is
    // "on" would have it competing with a latched shift for the eye.
    let toggle = spatiand_shell::keyboard::toggle_rect();
    let cap = cap_rect(&toggle, width, height);
    let (fill, ink) = if keyboard.click {
        (CAP, INK)
    } else {
        (CAP_MODIFIER, INK_MODIFIER)
    };
    rounded_rect(
        &mut rgba,
        width,
        height,
        cap,
        cap.2.min(cap.3) * RADIUS,
        fill,
    );
    let glyphs = fit_label(text, sound_symbol(keyboard), cap.2, cap.3, ink);
    blit_centred(&mut rgba, width, height, &glyphs, cap);

    TextImage {
        width,
        height,
        rgba,
    }
}

/// Whether a label is an **icon** rather than type.
///
/// The distinction decides how the label is centred, and it is a real one rather than a
/// convenience. Type has to keep the baseline it shares with the labels beside it: `q` and `w`
/// are only legible as a row because their bowls sit on one line, and the thing that guarantees
/// that is the font's own line box, which is the same height for every label at a given size.
/// Centring each glyph on its own ink instead would push `q` up by half its descender and pull
/// `t` down by half its ascender -- measured on the Deck at a keycap's size, about four pixels
/// each way, which reads as a keyboard whose letters have come loose.
///
/// An icon has no baseline to share. It is alone in its cap and the only thing it can be
/// aligned to is the cap, so it is centred on its own ink. Left in the line box it inherits
/// room for a descender it does not have and sits visibly high -- which is what happened to the
/// arrow keys and the sound toggle.
///
/// Single, and not ASCII: every legend on this keyboard that is *type* is ASCII, and every
/// symbol used as an icon is not. The sidecar holds its header symbols to the same rule.
pub fn is_icon(label: &str) -> bool {
    let mut chars = label.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_ascii())
}

/// Which speaker the toggle is showing.
pub fn sound_symbol(keyboard: &Keyboard) -> &'static str {
    if keyboard.click {
        SOUND_ON
    } else {
        SOUND_OFF
    }
}

/// Rasterise one key on its own, raised: brighter, with a shadow beneath it, and nothing but
/// transparency around it.
///
/// Drawn by the same code as the face's own caps, at the same size within its cell and with the
/// same legend, so the key under the pointer is *that* key standing up rather than a second
/// drawing of it laid on top. The first attempt filled a plain quad grown past the cell and
/// re-rendered the legend over it at its own size; what that produced was a square slab behind
/// the key and a legend fractionally off the one underneath, which reads as a shadow on the
/// lettering. Nothing here is drawn twice, so neither can happen.
pub fn cap(text: &mut TextRenderer, keyboard: &Keyboard, key: &Key, width_px: u32) -> TextImage {
    let Some((_, cell)) = layout().into_iter().find(|(k, _)| k.code == key.code) else {
        return TextImage {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
    };
    // The cell's own shape, in pixels rather than in the face's units. Growing it by the margin
    // on all four sides leaves the aspect alone, so this is the image's aspect too.
    let cell_aspect = (cell.half_u / cell.half_v) * spatiand_shell::keyboard::face_aspect();
    let width = width_px.max(16);
    let height = ((width as f64 / cell_aspect).round() as u32).max(16);

    // Where the cell sits inside its own image, and the cap inside the cell -- inset by the
    // same gap the face uses, which is what makes the two line up.
    let margin = CAP_MARGIN / (1.0 + 2.0 * CAP_MARGIN);
    let cell_w = width as f32 * (1.0 - 2.0 * margin);
    let cell_h = height as f32 * (1.0 - 2.0 * margin);
    let inset = cell_w.min(cell_h) * GAP * 0.5;
    let rect = (
        width as f32 * margin + inset,
        height as f32 * margin + inset,
        (cell_w - inset * 2.0).max(1.0),
        (cell_h - inset * 2.0).max(1.0),
    );

    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    let radius = rect.2.min(rect.3) * RADIUS;
    let short = rect.2.min(rect.3);

    // The shadow first, so the cap composites over it. It travels with the cap rather than
    // being cast onto the face: at arm's length the two planes are six millimetres apart and
    // the parallax between them is far below what the eye can pick out.
    let under = (rect.0, rect.1 + short * SHADOW_DROP, rect.2, rect.3);
    let (fill, ink) = appearance(keyboard, key);
    rounded_rect_with(
        &mut rgba,
        width,
        height,
        under,
        radius,
        SHADOW,
        short * SHADOW_BLUR,
        false,
    );

    let raised = [
        (fill[0] * RAISE).min(1.0),
        (fill[1] * RAISE).min(1.0),
        (fill[2] * RAISE).min(1.0),
        fill[3],
    ];
    rounded_rect_with(&mut rgba, width, height, rect, radius, raised, 1.0, true);

    let label = keyboard.label(key);
    if !label.is_empty() {
        let glyphs = fit_label(text, label, rect.2, rect.3, ink);
        blit_centred(&mut rgba, width, height, &glyphs, rect);
    }
    TextImage {
        width,
        height,
        rgba,
    }
}

/// What a raised cap's picture depends on: which key it is, what it currently reads, and
/// whether it is latched. Anything agreeing on all three draws identically, so one texture
/// serves both — and the cache and the drawing ask the same question, so a stale cap is not a
/// thing that can happen.
pub fn cap_id(keyboard: &Keyboard, key: &Key) -> String {
    // The toggle is not a key and never rises, so it is deliberately not part of this. What
    // *does* depend on it is the face, whose cache lives in `Scene::sync_keyboard`.
    format!(
        "{}|{}|{}",
        key.code,
        keyboard.label(key),
        keyboard.is_latched(key)
    )
}

/// How a key is coloured: its cap and its ink.
///
/// Shared by the face and by a raised cap so that standing a key up cannot change what it is,
/// only how high it sits.
fn appearance(keyboard: &Keyboard, key: &Key) -> ([f32; 4], [u8; 4]) {
    match (keyboard.is_latched(key), key.role) {
        (true, _) => (CAP_LATCHED, INK_LATCHED),
        (false, Role::Modifier(_)) => (CAP_MODIFIER, INK_MODIFIER),
        (false, Role::Normal) => (
            CAP,
            if key.label.len() > 1 {
                INK_MODIFIER
            } else {
                INK
            },
        ),
    }
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
    let fitted = if first.width as f32 <= room_w || first.width == 0 {
        first
    } else {
        // Too wide: shrink by exactly the overshoot rather than by a guess.
        let shrink = (room_w / first.width as f32).clamp(0.25, 1.0);
        text.render(label, room_h * shrink, room_w.max(8.0) as u32, ink)
    };
    // `render` crops to the ink horizontally but keeps the full line-height box vertically.
    // For type that box is exactly what is wanted -- it is the same height for every label, so
    // every label lands on one baseline. For an icon it is room for a descender that does not
    // exist, and the glyph rides high in its cap. See [`is_icon`].
    //
    // Cropping is free here: `blit_centred` places this image by its own pixel dimensions
    // rather than stretching it into the cap, so tightening the box moves where the pixels
    // land without touching their size.
    if is_icon(label) {
        fitted.crop_to_ink_vertically()
    } else {
        fitted
    }
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
    // One pixel of edge, enough to kill the jaggies at this angular size, and lit like a cap.
    rounded_rect_with(rgba, width, height, rect, radius, colour, 1.0, true);
}

/// The same, with the edge and the shading spelled out.
///
/// `feather` is how many pixels the edge fades over — one for a keycap, many for the shadow a
/// raised one casts, which is the whole difference between the two. `lit` adds the vertical
/// gradient that makes a cap read as a surface; a shadow is not a surface and must not have it.
#[allow(clippy::too_many_arguments)]
fn rounded_rect_with(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rect: (f32, f32, f32, f32),
    radius: f32,
    colour: [f32; 4],
    feather: f32,
    lit: bool,
) {
    let (rx, ry, rw, rh) = rect;
    let radius = radius.max(0.0).min(rw.min(rh) * 0.5);
    let feather = feather.max(0.01);
    // A soft edge reaches beyond the rectangle, so the pixels it touches have to be visited.
    let reach = feather * 0.5 + 1.0;
    let x0 = ((rx - reach).floor().max(0.0)) as u32;
    let y0 = ((ry - reach).floor().max(0.0)) as u32;
    let x1 = ((rx + rw + reach).ceil().clamp(0.0, width as f32)) as u32;
    let y1 = ((ry + rh + reach).ceil().clamp(0.0, height as f32)) as u32;

    for y in y0..y1 {
        for x in x0..x1 {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Distance outside the rounded rectangle, in pixels.
            let dx = (rx + radius - fx).max(fx - (rx + rw - radius)).max(0.0);
            let dy = (ry + radius - fy).max(fy - (ry + rh - radius)).max(0.0);
            let outside = (dx * dx + dy * dy).sqrt() - radius;
            let coverage = ((feather * 0.5 - outside) / feather).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            // A gentle vertical lift, so a cap reads as a surface catching light rather than a
            // flat swatch. Subtle on purpose: at a degree across, a strong gradient just looks
            // like the texture is dirty.
            let lift = if lit {
                let t = ((fy - ry) / rh.max(1.0)).clamp(0.0, 1.0);
                1.0 + (0.18 - 0.30 * t)
            } else {
                1.0
            };
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
    fn the_speaker_symbols_are_really_in_the_font() {
        // The failure this exists to catch is specific: a font stack with neither glyph draws
        // the same tofu box for both, and the toggle then has two states that look identical
        // on a control with no caption to fall back on.
        let mut text = TextRenderer::new();
        let on = text.render(SOUND_ON, 64.0, 256, [255, 255, 255, 255]);
        let off = text.render(SOUND_OFF, 64.0, 256, [255, 255, 255, 255]);
        for (s, image) in [(SOUND_ON, &on), (SOUND_OFF, &off)] {
            assert_eq!(s.chars().count(), 1, "{s:?} is not a single symbol");
            assert!(!s.is_ascii(), "{s:?} should be a symbol, not a letter");
            assert!(
                image.width > 0 && image.height > 0,
                "{s:?} rendered nothing at all"
            );
        }
        assert!(
            on.width != off.width || on.rgba != off.rgba,
            "the two speakers draw identically — the font has neither of them"
        );
    }

    #[test]
    fn the_sound_toggle_says_which_way_it_is_and_says_it_in_its_own_corner() {
        let mut text = TextRenderer::new();
        let mut keyboard = Keyboard::default();
        let on = face(&mut text, &keyboard, 900);
        keyboard.click = false;
        let off = face(&mut text, &keyboard, 900);
        assert_ne!(
            on.rgba, off.rgba,
            "the face looks the same with the sound off"
        );

        // Everything that changed has to be inside the toggle's own cell. This is the check
        // that a change to the toggle has not quietly restyled the keys as well — which would
        // not show up in "the images differ" and is exactly the kind of thing that only gets
        // noticed once it is in a headset.
        let (w, h) = (on.width as usize, on.height as usize);
        let cell = cap_rect(
            &spatiand_shell::keyboard::toggle_rect(),
            on.width,
            on.height,
        );
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                if on.rgba[i..i + 4] == off.rgba[i..i + 4] {
                    continue;
                }
                let inside = (x as f32) >= cell.0 - 2.0
                    && (x as f32) <= cell.0 + cell.2 + 2.0
                    && (y as f32) >= cell.1 - 2.0
                    && (y as f32) <= cell.1 + cell.3 + 2.0;
                assert!(
                    inside,
                    "turning the sound off changed the face at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn an_icon_is_cropped_to_its_ink_so_that_centring_it_actually_centres_it() {
        // The complaint this fixes: the speaker and the arrow keys sat visibly high in their
        // caps. `blit_centred` places a label by its image's own dimensions, so an icon is
        // centred exactly when its image has no blank rows left on it.
        let mut text = TextRenderer::new();
        for symbol in [
            SOUND_ON, SOUND_OFF, "\u{2190}", "\u{2191}", "\u{2193}", "\u{2192}",
        ] {
            let image = fit_label(&mut text, symbol, 120.0, 45.0, INK);
            let (top, bottom) = image.ink_vertical_extent();
            assert!(
                top < 0.02 && bottom > 0.98,
                "{symbol:?} keeps blank rows ({top}..{bottom}), so it will sit off centre"
            );
        }
    }

    #[test]
    fn letters_keep_their_shared_baseline_instead_of_each_finding_its_own_middle() {
        // The correction to the fix above, and the reason it is not applied to everything.
        // Cropping a letter to its ink centres *that letter* and so moves it off the line the
        // letters beside it are standing on: a descender pushes `q` up, an ascender pulls `t`
        // down. Measured on the Deck before this was narrowed, that was about four pixels each
        // way at a keycap's size, which reads as lettering that has come loose from the board.
        //
        // What holds the row together is that every label keeps the same font-derived box, so
        // the assertion is that the boxes are all the same height -- descender or not.
        let mut text = TextRenderer::new();
        let heights: Vec<u32> = ["q", "w", "t", "a", "y", "space"]
            .iter()
            .map(|l| fit_label(&mut text, l, 120.0, 60.0, INK).height)
            .collect();
        assert!(
            heights.windows(2).all(|w| w[0] == w[1]),
            "labels came out at different heights, so they cannot share a baseline: {heights:?}"
        );
    }

    #[test]
    fn what_counts_as_an_icon_is_exactly_the_symbols_and_none_of_the_type() {
        let keyboard = Keyboard::default();
        for (key, _) in layout() {
            let label = keyboard.label(key);
            let expected = matches!(label, "\u{2190}" | "\u{2191}" | "\u{2193}" | "\u{2192}");
            assert_eq!(is_icon(label), expected, "is_icon({label:?}) is wrong");
        }
        assert!(is_icon(SOUND_ON) && is_icon(SOUND_OFF));
        assert!(!is_icon("space"), "a word is type, however short");
        assert!(!is_icon(""), "nothing at all is not an icon");
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
        assert!(
            on_ground > 0.05,
            "only {:.1}% is gap; the caps have run together",
            on_ground * 100.0
        );
        // ...but the caps are most of it.
        assert!(
            on_ground < 0.6,
            "{:.1}% is bare ground; the caps are too small",
            on_ground * 100.0
        );
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
        assert!(
            fitted.width as f32 <= 200.0 * 0.82 + 1.0,
            "label is {} wide",
            fitted.width
        );
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
