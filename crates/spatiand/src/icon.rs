//! Turning an icon file into pixels.
//!
//! Two formats, because that is what is installed: SVG for essentially everything KDE ships
//! (19,978 of them against 176 PNGs), and PNG for the rest. Both are rasterised to a square
//! RGBA image at whatever size the bubble wants, which is why SVG is worth the dependency —
//! the icon is drawn at ~220 px inside the glass, and no fixed-size PNG on the system is that
//! large.
//!
//! Failure is always `None`, never an error. A missing or corrupt icon is a cosmetic problem
//! and the launcher already has a fallback; refusing to open because one app out of forty has
//! a bad icon would be absurd.

use std::path::Path;

use spatiand_render::text::TextImage;

/// Rasterise an icon to `size` x `size` RGBA.
pub fn load(path: &Path, size: u32) -> Option<TextImage> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("svg") => load_svg(path, size),
        Some("png") => load_bitmap(path, size),
        // .xpm and anything else: not worth a decoder for the handful that use them.
        _ => None,
    }
}

fn load_svg(path: &Path, size: u32) -> Option<TextImage> {
    let data = std::fs::read(path).ok()?;
    let mut options = usvg::Options::default();
    // Some icons reference their own directory for embedded images.
    options.resources_dir = path.parent().map(|p| p.to_path_buf());
    options.fontdb_mut().load_system_fonts();

    let tree = usvg::Tree::from_data(&data, &options).ok()?;
    let tree_size = tree.size();
    if tree_size.width() <= 0.0 || tree_size.height() <= 0.0 {
        return None;
    }

    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    // Fit the drawing into the square without distorting it, then centre it. Icons are
    // usually square already; the ones that are not look wrong stretched.
    let scale = (size as f32 / tree_size.width()).min(size as f32 / tree_size.height());
    let dx = (size as f32 - tree_size.width() * scale) * 0.5;
    let dy = (size as f32 - tree_size.height() * scale) * 0.5;
    let transform = tiny_skia::Transform::from_translate(dx, dy).pre_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    Some(TextImage {
        width: size,
        height: size,
        // tiny-skia works in premultiplied alpha; the quad and bubble shaders both expect
        // straight alpha, and the difference shows up as dark fringing around every icon.
        rgba: demultiply(pixmap.data()),
    })
}

fn load_bitmap(path: &Path, size: u32) -> Option<TextImage> {
    let decoded = image::open(path).ok()?;
    let scaled = decoded.resize(size, size, image::imageops::FilterType::CatmullRom);
    let rgba = scaled.to_rgba8();
    let (w, h) = rgba.dimensions();
    // Centre a non-square icon rather than stretching it.
    let mut out = vec![0u8; (size * size * 4) as usize];
    let ox = (size - w.min(size)) / 2;
    let oy = (size - h.min(size)) / 2;
    for y in 0..h.min(size) {
        for x in 0..w.min(size) {
            let src = ((y * w + x) * 4) as usize;
            let dst = (((y + oy) * size + x + ox) * 4) as usize;
            out[dst..dst + 4].copy_from_slice(&rgba.as_raw()[src..src + 4]);
        }
    }
    Some(TextImage {
        width: size,
        height: size,
        rgba: out,
    })
}

fn demultiply(premultiplied: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(premultiplied.len());
    for p in premultiplied.chunks_exact(4) {
        let a = p[3];
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        let un = |c: u8| ((c as u32 * 255) / a as u32).min(255) as u8;
        out.extend_from_slice(&[un(p[0]), un(p[1]), un(p[2]), a]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demultiplying_recovers_full_brightness_from_a_faded_pixel() {
        // Premultiplied half-alpha white is (128,128,128,128). Left as-is it composites to a
        // grey ghost, which is exactly the dark fringe that appears around every icon.
        let out = demultiply(&[128, 128, 128, 128]);
        assert!(out[0] > 250 && out[1] > 250 && out[2] > 250, "got {out:?}");
        assert_eq!(out[3], 128, "alpha must be left alone");
    }

    #[test]
    fn fully_transparent_pixels_do_not_divide_by_zero() {
        assert_eq!(demultiply(&[0, 0, 0, 0]), vec![0, 0, 0, 0]);
    }

    #[test]
    fn an_opaque_pixel_is_unchanged() {
        assert_eq!(demultiply(&[10, 200, 30, 255]), vec![10, 200, 30, 255]);
    }

    #[test]
    fn unknown_formats_are_declined_rather_than_guessed() {
        assert!(load(Path::new("/tmp/whatever.xpm"), 64).is_none());
        assert!(load(Path::new("/tmp/whatever"), 64).is_none());
    }

    #[test]
    fn a_missing_file_is_none_not_a_panic() {
        assert!(load(Path::new("/nonexistent/icon.svg"), 64).is_none());
        assert!(load(Path::new("/nonexistent/icon.png"), 64).is_none());
    }

    #[test]
    fn a_simple_svg_rasterises_to_the_requested_size() {
        let dir = std::env::temp_dir().join("spatiand-svg-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("square.svg");
        std::fs::write(
            &path,
            br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#ff0000"/></svg>"##,
        )
        .unwrap();
        let img = load(&path, 32).expect("should rasterise");
        assert_eq!((img.width, img.height), (32, 32));
        assert!(
            img.ink_fraction() > 0.9,
            "a filled square should cover the image"
        );
        let centre = ((16 * 32 + 16) * 4) as usize;
        assert!(
            img.rgba[centre] > 200,
            "should be red, got {:?}",
            &img.rgba[centre..centre + 4]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
