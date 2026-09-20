//! Finding a picture for an application, and making it small enough to send.
//!
//! In this order, stopping at the first that gives an image:
//!
//! 1. **What the owner chose** (`icon` in the catalogue): a path, or a name in the icon theme.
//!    Always wins, which is what makes an icon replaceable.
//! 2. **The application's desktop entry.** Matched by the program it runs, *after following
//!    links*: Chrome's catalogue entry says `/usr/bin/google-chrome` and its desktop entry says
//!    `/usr/bin/google-chrome-stable`, and both are links to `/opt/google/chrome/google-chrome`.
//!    Comparing the names as written would never match them.
//! 3. **An image beside the program**, or in a folder beside it. Applications installed by
//!    unpacking an archive have no desktop entry at all — Firestorm is one, and ships
//!    `firestorm_icon.png` and `res-sdl/firestorm_icon128.png` next to its launcher. Images
//!    named after the program, or called an icon or a logo, are preferred, and the largest of
//!    those wins.
//!
//! Whatever is found is rendered to a [`SIZE`]-pixel PNG, which is what crosses the network:
//! an SVG or an ICO means nothing to the other end, and a 1024-pixel PNG is 30 times the bytes
//! of what a launcher bubble can show.

use std::path::{Path, PathBuf};

use spatiand_stream::App;

/// Edge of the icon as sent, in pixels. A launcher bubble shows it at about a hundred.
pub const SIZE: u32 = 128;

const IMAGE_EXTENSIONS: &[&str] = &["png", "svg", "ico", "jpg", "jpeg", "bmp"];

/// Where an application's icon comes from, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    /// Which rule found it, for the settings app to say.
    pub source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Chosen,
    DesktopEntry,
    BesideProgram,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Chosen => "chosen",
            Source::DesktopEntry => "from its desktop entry",
            Source::BesideProgram => "found beside the program",
        }
    }
}

/// Find an icon for an application. See the module notes for the order.
pub fn find(app: &App, entries: &[spatiand_platform::DesktopEntry]) -> Option<Found> {
    if let Some(chosen) = app.icon.as_deref().filter(|s| !s.is_empty()) {
        if let Some(path) = spatiand_platform::resolve_icon(chosen) {
            return Some(Found {
                path,
                source: Source::Chosen,
            });
        }
        log::info!("{}: the chosen icon {chosen:?} was not found; looking for another", app.id);
    }
    let program = resolve_program(&app.exec);
    if let Some(program) = &program {
        for entry in entries {
            let Some(icon) = entry.icon.as_deref() else { continue };
            let command = spatiand_platform::launch::split_command(&entry.exec);
            let Some(first) = command.first() else { continue };
            if resolve_program(first).as_ref() == Some(program) {
                if let Some(path) = spatiand_platform::resolve_icon(icon) {
                    return Some(Found {
                        path,
                        source: Source::DesktopEntry,
                    });
                }
            }
        }
    }
    // Beside the program as written *and* as resolved: a launcher script in an unpacked
    // archive is usually the thing with pictures next to it, and a link in /usr/bin is not.
    let mut places: Vec<PathBuf> = Vec::new();
    for p in [Some(PathBuf::from(&app.exec)), program].into_iter().flatten() {
        if let Some(dir) = p.parent() {
            if !places.contains(&dir.to_path_buf()) {
                places.push(dir.to_path_buf());
            }
        }
    }
    let stem = Path::new(&app.exec)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    beside(&places, &[stem, app.id.to_lowercase(), app.name.to_lowercase()]).map(|path| Found {
        path,
        source: Source::BesideProgram,
    })
}

/// A program as the operating system would find it: through `PATH` if it is a bare name, and
/// with every link followed.
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    let program = program.trim();
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        return std::fs::canonicalize(program).ok();
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::canonicalize(p).ok())
}

/// The best-looking image in these folders and the folders directly inside them.
fn beside(places: &[PathBuf], names: &[String]) -> Option<PathBuf> {
    // System folders are full of other programs' pictures; an image in /usr/bin is never
    // this application's.
    const TOO_SHARED: &[&str] = &["/usr/bin", "/bin", "/usr/local/bin", "/usr/sbin", "/sbin"];
    let mut best: Option<(i64, PathBuf)> = None;
    for place in places {
        if TOO_SHARED.iter().any(|s| place == Path::new(s)) {
            continue;
        }
        let mut files = images_in(place);
        if let Ok(entries) = std::fs::read_dir(place) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    files.extend(images_in(&entry.path()));
                }
            }
        }
        for file in files {
            let score = score(&file, names);
            if score > 0 && best.as_ref().is_none_or(|(b, _)| score > *b) {
                best = Some((score, file));
            }
        }
    }
    best.map(|(_, path)| path)
}

fn images_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()))
        })
        .collect()
}

/// How likely a picture is to be this application's icon. Zero means "not at all".
fn score(file: &Path, names: &[String]) -> i64 {
    let name = file
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let named = names.iter().any(|n| !n.is_empty() && name.contains(n.as_str()));
    let iconish = ["icon", "logo"].iter().any(|w| name.contains(w));
    if !named && !iconish {
        return 0;
    }
    let mut score = if named && iconish {
        3_000_000
    } else if named {
        2_000_000
    } else {
        1_000_000
    };
    // Bigger is better, up to what is sent; a vector is as big as anything.
    let edge = if file.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) {
        SIZE
    } else {
        image::image_dimensions(file)
            .map(|(w, h)| w.min(h))
            .unwrap_or(0)
    };
    score += i64::from(edge.min(SIZE * 2));
    score
}

/// Render an image file as a [`SIZE`]-pixel square PNG, keeping its proportions.
pub fn render_png(path: &Path) -> Result<Vec<u8>, String> {
    let is_svg = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"));
    let rgba = if is_svg {
        render_svg(path)?
    } else {
        let image = image::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let fitted = image.resize(SIZE, SIZE, image::imageops::FilterType::Lanczos3);
        let mut square = image::RgbaImage::new(SIZE, SIZE);
        let x = (SIZE - fitted.width()) / 2;
        let y = (SIZE - fitted.height()) / 2;
        image::imageops::overlay(&mut square, &fitted.to_rgba8(), x.into(), y.into());
        square
    };
    let mut out = std::io::Cursor::new(Vec::new());
    rgba.write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("could not encode an icon: {e}"))?;
    Ok(out.into_inner())
}

/// The settings app's own icon, carried in the program rather than looked up: it is the one
/// app every host has, on machines whose icon themes have nothing in common.
const SETTINGS_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
<circle cx="32" cy="32" r="30" fill="#3b6fd8"/>
<g fill="#ffffff" transform="translate(32 32)">
<rect x="-4" y="-22" width="8" height="10" rx="2"/>
<rect x="-4" y="-22" width="8" height="10" rx="2" transform="rotate(45)"/>
<rect x="-4" y="-22" width="8" height="10" rx="2" transform="rotate(90)"/>
<rect x="-4" y="-22" width="8" height="10" rx="2" transform="rotate(135)"/>
<rect x="-4" y="-22" width="8" height="10" rx="2" transform="rotate(180)"/>
<rect x="-4" y="-22" width="8" height="10" rx="2" transform="rotate(225)"/>
<rect x="-4" y="-22" width="8" height="10" rx="2" transform="rotate(270)"/>
<rect x="-4" y="-22" width="8" height="10" rx="2" transform="rotate(315)"/>
<circle r="14"/>
</g>
<circle cx="32" cy="32" r="6" fill="#3b6fd8"/>
</svg>"##;

/// The settings app's icon, as sent.
pub fn settings_png() -> Option<Vec<u8>> {
    let rgba = render_svg_data(SETTINGS_SVG.as_bytes(), "the settings icon").ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    rgba.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

fn render_svg(path: &Path) -> Result<image::RgbaImage, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    render_svg_data(&data, &path.display().to_string())
}

fn render_svg_data(data: &[u8], what: &str) -> Result<image::RgbaImage, String> {
    let tree = resvg::usvg::Tree::from_data(data, &resvg::usvg::Options::default())
        .map_err(|e| format!("{what}: {e}"))?;
    let size = tree.size();
    let scale = SIZE as f32 / size.width().max(size.height());
    let mut pixmap = resvg::tiny_skia::Pixmap::new(SIZE, SIZE)
        .ok_or_else(|| "could not make a canvas".to_string())?;
    let dx = (SIZE as f32 - size.width() * scale) / 2.0;
    let dy = (SIZE as f32 - size.height() * scale) / 2.0;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy),
        &mut pixmap.as_mut(),
    );
    // tiny-skia holds premultiplied alpha; a PNG wants it straight.
    let mut pixels = pixmap.take();
    for px in pixels.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    image::RgbaImage::from_raw(SIZE, SIZE, pixels).ok_or_else(|| "a canvas of the wrong size".into())
}

/// Find and render, for serving: what the host puts in `icon_png`.
pub fn icon_png(app: &App, entries: &[spatiand_platform::DesktopEntry]) -> Option<Vec<u8>> {
    if app.id == crate::SETTINGS_APP_ID && app.icon.is_none() {
        return settings_png();
    }
    let found = find(app, entries)?;
    match render_png(&found.path) {
        Ok(png) => Some(png),
        Err(e) => {
            log::warn!("{}: {e}", app.id);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spatiand-icons-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn png(path: &Path, edge: u32) {
        image::RgbaImage::from_pixel(edge, edge, image::Rgba([200, 30, 30, 255]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn an_unpacked_app_gets_the_biggest_picture_named_after_it_even_one_folder_down() {
        // Firestorm's layout, in miniature.
        let dir = temp("firestorm");
        std::fs::write(dir.join("firestorm"), "#!/bin/sh\n").unwrap();
        png(&dir.join("firestorm_48.png"), 48);
        png(&dir.join("firestorm_icon.png"), 64);
        std::fs::create_dir(dir.join("res-sdl")).unwrap();
        png(&dir.join("res-sdl/firestorm_icon128.png"), 128);
        png(&dir.join("unrelated.png"), 512);
        let app = App::new("firestorm", "Firestorm", dir.join("firestorm").to_string_lossy());
        let found = find(&app, &[]).expect("finds one");
        assert_eq!(found.source, Source::BesideProgram);
        assert_eq!(found.path, dir.join("res-sdl/firestorm_icon128.png"));
    }

    #[test]
    fn a_chosen_icon_always_wins() {
        let dir = temp("chosen");
        std::fs::write(dir.join("tool"), "").unwrap();
        png(&dir.join("tool_icon.png"), 128);
        png(&dir.join("mine.png"), 16);
        let mut app = App::new("tool", "Tool", dir.join("tool").to_string_lossy());
        app.icon = Some(dir.join("mine.png").to_string_lossy().into());
        let found = find(&app, &[]).unwrap();
        assert_eq!(found.source, Source::Chosen);
        assert_eq!(found.path, dir.join("mine.png"));
    }

    #[test]
    fn a_desktop_entry_is_matched_by_the_program_behind_the_link() {
        let dir = temp("links");
        let real = dir.join("real-program");
        std::fs::write(&real, "").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("name-a")).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("name-b")).unwrap();
        let icon = dir.join("pic.png");
        png(&icon, 32);
        let entry = spatiand_platform::DesktopEntry {
            name: "Thing".into(),
            exec: format!("{} --new-window", dir.join("name-b").display()),
            icon: Some(icon.to_string_lossy().into()),
            categories: vec![],
            path: dir.join("thing.desktop"),
        };
        let app = App::new("thing", "Thing", dir.join("name-a").to_string_lossy());
        let found = find(&app, &[entry]).unwrap();
        assert_eq!(found.source, Source::DesktopEntry);
        assert_eq!(found.path, icon);
    }

    #[test]
    fn the_settings_app_brings_its_own_icon() {
        let png = settings_png().expect("renders");
        let back = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(back.dimensions(), (SIZE, SIZE));
        assert_eq!(back.get_pixel(64, 64).0[3], 255, "opaque in the middle");
    }

    #[test]
    fn whatever_is_found_is_sent_as_a_small_square_png() {
        let dir = temp("render");
        let big = dir.join("wide.png");
        image::RgbaImage::from_pixel(400, 200, image::Rgba([1, 2, 3, 255]))
            .save(&big)
            .unwrap();
        let bytes = render_png(&big).unwrap();
        let back = image::load_from_memory(&bytes).unwrap();
        assert_eq!((back.width(), back.height()), (SIZE, SIZE));

        let svg = dir.join("v.svg");
        std::fs::write(
            &svg,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="red"/></svg>"#,
        )
        .unwrap();
        let back = image::load_from_memory(&render_png(&svg).unwrap()).unwrap().to_rgba8();
        assert_eq!(back.get_pixel(64, 64).0, [255, 0, 0, 255]);
    }
}
