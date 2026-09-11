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
//!
//! ## Not on the frame loop
//!
//! Finding an icon means walking the theme's directories, and for a window, possibly reading
//! every desktop file on the machine too: 1 to 25 ms, measured on the Deck. Rasterising an
//! SVG took another 22 ms, most of it spent filling a font database, until [`fonts`] started
//! filling one for the whole session. Both used to happen on the render thread, all at once,
//! every time the launcher changed level. Opening a group of ten apps stopped the world for a
//! quarter of a second, and it happened again on every visit. [`Loader`] does the work on a
//! thread of its own, and the frame loop only uploads what comes back.

use std::path::Path;
use std::sync::{mpsc, Arc, OnceLock};

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
    options.fontdb = fonts();

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

/// Every font on the system, found once for the whole session.
///
/// usvg needs a font database for the odd icon with text in it, and filling one means reading
/// every font file on the machine: 751 of them on the Deck, 16 to 23 ms, measured. That used
/// to be done again for every SVG. Rasterising all forty launcher icons took 870 ms; with one
/// database shared between them it takes 95.
fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut fonts = usvg::fontdb::Database::new();
            fonts.load_system_fonts();
            Arc::new(fonts)
        })
        .clone()
}

/// Which icon. The two places that show icons name them differently.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Wanted {
    /// A name from a desktop entry's `Icon=` line, looked up in the theme.
    Named(String),
    /// A window's application id, which has to be matched to an application first. See
    /// [`spatiand_platform::icon_for_app`].
    App(String),
}

impl Wanted {
    fn load(&self, size: u32) -> Option<TextImage> {
        let path = match self {
            Wanted::Named(name) => spatiand_platform::resolve_icon(name),
            Wanted::App(id) => spatiand_platform::icon_for_app(id),
        }?;
        load(&path, size)
    }
}

/// Icons found and rasterised on a thread of their own. See the module notes for why.
///
/// In the order asked for, so whatever is on screen can be asked for first and arrive first.
pub struct Loader {
    to: Option<mpsc::Sender<(Wanted, u32)>>,
    from: mpsc::Receiver<(Wanted, Option<TextImage>)>,
    /// Icons loaded here and now, with no thread to hand them to.
    ready: Vec<(Wanted, Option<TextImage>)>,
}

impl Loader {
    pub fn start() -> Loader {
        let (to, jobs) = mpsc::channel::<(Wanted, u32)>();
        let (done, from) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("icons".into())
            .spawn(move || {
                while let Ok((wanted, size)) = jobs.recv() {
                    let image = wanted.load(size);
                    if done.send((wanted, image)).is_err() {
                        return;
                    }
                }
            });
        let to = match spawned {
            Ok(_) => Some(to),
            Err(e) => {
                log::warn!("no icon thread ({e}); icons will be loaded on the frame loop");
                None
            }
        };
        Loader {
            to,
            from,
            ready: Vec::new(),
        }
    }

    /// Ask for an icon at `size` x `size`. Never waits, unless there is no thread.
    pub fn request(&mut self, wanted: Wanted, size: u32) {
        let unsent = match &self.to {
            Some(to) => to.send((wanted, size)).err().map(|e| e.0),
            None => Some((wanted, size)),
        };
        // A thread that has gone -- resvg panicked on a malformed file -- still leaves every
        // app with its icon, just the slow way.
        if let Some((wanted, size)) = unsent {
            let image = wanted.load(size);
            self.ready.push((wanted, image));
        }
    }

    /// Everything finished since the last call. `None` for an icon means there is none to
    /// be had, which is an answer rather than a failure.
    pub fn finished(&mut self) -> Vec<(Wanted, Option<TextImage>)> {
        let mut finished = std::mem::take(&mut self.ready);
        finished.extend(self.from.try_iter());
        finished
    }
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

    /// Everything the loader hands back within a generous deadline, until `count` have come.
    fn collect(loader: &mut Loader, count: usize) -> Vec<(Wanted, Option<TextImage>)> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut out = Vec::new();
        while out.len() < count && std::time::Instant::now() < deadline {
            out.extend(loader.finished());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        out
    }

    #[test]
    fn the_loader_answers_every_request_in_order_including_the_ones_with_no_icon() {
        let dir = std::env::temp_dir().join("spatiand-icon-loader-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("square.svg");
        std::fs::write(
            &path,
            br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10"/></svg>"##,
        )
        .unwrap();
        let found = Wanted::Named(path.display().to_string());
        let absent = Wanted::Named("/nonexistent/icon.svg".into());

        let mut loader = Loader::start();
        loader.request(found.clone(), 16);
        loader.request(absent.clone(), 16);
        let answers = collect(&mut loader, 2);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(answers.len(), 2, "an icon was never answered");
        assert_eq!(answers[0].0, found);
        let image = answers[0].1.as_ref().expect("the file was there");
        assert_eq!((image.width, image.height), (16, 16));
        // Answered as absent rather than left unanswered: the launcher shows a letter for it
        // for good, instead of waiting for it for the rest of the session.
        assert_eq!(answers[1].0, absent);
        assert!(answers[1].1.is_none());
    }

    #[test]
    fn the_fonts_are_found_once() {
        assert!(Arc::ptr_eq(&fonts(), &fonts()));
    }
}
