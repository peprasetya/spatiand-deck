//! Choosing and loading the 360° environment.
//!
//! Spatiand ships with no image and works anyway: [`spatiand_render::Sky::studio`] generates
//! one. That is not a placeholder to be replaced later — it is what makes a first run on a
//! machine with no assets and no network look deliberate rather than broken, and it is the
//! only environment guaranteed to carry a key light for the glass bubbles to catch.
//!
//! Dropping equirectangular JPEGs or PNGs into `~/.local/share/spatiand/environments` adds to
//! the rotation. `tools/fetch-environments.sh` will fetch some CC0 ones.
//!
//! ## Guessing the layout
//!
//! A panorama file does not say whether it is mono, stereo, 360 or 180, so it is inferred from
//! the name and then the shape:
//!
//! | clue | read as |
//! |---|---|
//! | name contains `_ou`, `_tb`, `over-under`, `top-bottom` | over/under stereo |
//! | name contains `_sbs`, `side-by-side` | side-by-side stereo |
//! | name contains `180`, `vr180` | front hemisphere only |
//! | aspect 2:1 | mono 360 — the overwhelmingly common case |
//! | aspect 1:1 | over/under stereo 360 (two 2:1 images stacked) |
//! | aspect 4:1 | side-by-side stereo 360 |
//!
//! Guessing wrong is not fatal and is obvious to look at: a stereo image read as mono shows
//! both eyes' views stacked, which nobody could mistake for correct.

use std::path::{Path, PathBuf};

use spatiand_render::sky::{Sky, SkyProjection, SkySource, SkyStereo};

/// Resolution of the generated environment.
///
/// 2048x1024 is about 5.7 pixels per degree, against the ~48 the display can resolve. That is
/// deliberately soft: it is a background, it costs 8 MB, and a sharp one would draw the eye to
/// exactly where nothing is happening.
const GENERATED_SIZE: (u32, u32) = (2048, 1024);

/// The environments available this session.
pub struct Environments {
    files: Vec<PathBuf>,
    /// `None` means the generated one, which always sits at the front of the rotation.
    index: Option<usize>,
}

impl Environments {
    pub fn discover() -> Self {
        let mut files = Vec::new();
        for dir in search_directories() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if is_image(&path) {
                    files.push(path);
                }
            }
        }
        // Stable order, so "the third one" means the same thing between runs.
        files.sort();
        if files.is_empty() {
            log::info!("no environment images found; using the generated one");
        } else {
            log::info!("{} environment image(s) available", files.len());
        }
        Self { files, index: None }
    }

    /// Load whatever is currently selected.
    pub fn current(&self) -> Sky {
        match self.index.and_then(|i| self.files.get(i)) {
            Some(path) => match load_image(path) {
                Ok(sky) => sky,
                Err(e) => {
                    // Falling back rather than failing: an unreadable file should not leave
                    // the wearer in a black void with no way to change it.
                    log::warn!("could not load {}: {e}", path.display());
                    Sky::studio(GENERATED_SIZE.0, GENERATED_SIZE.1)
                }
            },
            None => Sky::studio(GENERATED_SIZE.0, GENERATED_SIZE.1),
        }
    }

    /// Move to the next environment, wrapping back to the generated one.
    pub fn advance(&mut self) {
        self.index = match self.index {
            None if self.files.is_empty() => None,
            None => Some(0),
            Some(i) if i + 1 < self.files.len() => Some(i + 1),
            Some(_) => None,
        };
        log::info!("environment -> {}", self.describe());
    }

    pub fn describe(&self) -> String {
        match self.index.and_then(|i| self.files.get(i)) {
            Some(p) => p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image".into()),
            None => "Studio (generated)".into(),
        }
    }
}

fn search_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(explicit) = std::env::var("SPATIAND_ENVIRONMENTS") {
        dirs.push(PathBuf::from(explicit));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".local/share/spatiand/environments"));
    }
    dirs
}

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "png")
    )
}

fn load_image(path: &Path) -> Result<Sky, String> {
    let decoded = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
    let (width, height) = decoded.dimensions();
    let source = guess_source(path, width, height);
    log::info!(
        "environment {} — {width}x{height}, {:?} {:?}",
        path.display(),
        source.projection,
        source.stereo
    );
    Sky::from_rgba(width, height, decoded.into_raw(), source)
        .ok_or_else(|| "decoded image had an implausible size".to_string())
}

/// Work out what a panorama file contains. See the table in the module docs.
pub fn guess_source(path: &Path, width: u32, height: u32) -> SkySource {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let has = |needles: &[&str]| needles.iter().any(|n| name.contains(n));

    let aspect = width as f32 / height.max(1) as f32;

    // The name wins where it says anything, because it is the author's own statement about the
    // file; shape is only a fallback.
    let stereo = if has(&["_ou", "_tb", "over-under", "over_under", "top-bottom", "top_bottom"]) {
        SkyStereo::OverUnder
    } else if has(&["_sbs", "side-by-side", "side_by_side"]) {
        SkyStereo::SideBySide
    } else if aspect < 1.4 {
        // Two 2:1 images stacked come out square.
        SkyStereo::OverUnder
    } else if aspect > 3.0 {
        SkyStereo::SideBySide
    } else {
        SkyStereo::Mono
    };

    let projection = if has(&["180", "vr180"]) {
        SkyProjection::Equirect180
    } else {
        SkyProjection::Equirect360
    };

    SkySource {
        projection,
        stereo,
        yaw_offset_millideg: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guess(name: &str, w: u32, h: u32) -> SkySource {
        guess_source(&PathBuf::from(name), w, h)
    }

    #[test]
    fn an_ordinary_two_to_one_panorama_is_mono_360() {
        // By far the most common thing anyone will drop in, so it has to be right without any
        // naming convention at all.
        let s = guess("sunset.jpg", 4096, 2048);
        assert_eq!(s.stereo, SkyStereo::Mono);
        assert_eq!(s.projection, SkyProjection::Equirect360);
    }

    #[test]
    fn a_square_image_is_read_as_stacked_stereo() {
        let s = guess("room.jpg", 4096, 4096);
        assert_eq!(s.stereo, SkyStereo::OverUnder);
    }

    #[test]
    fn a_very_wide_image_is_read_as_side_by_side() {
        let s = guess("hall.jpg", 8192, 2048);
        assert_eq!(s.stereo, SkyStereo::SideBySide);
    }

    #[test]
    fn the_filename_overrides_the_shape() {
        // A stereo file that has been letterboxed, or a mono one padded square, would
        // otherwise be misread — and the author naming it is better evidence than its shape.
        let s = guess("forest_ou.jpg", 4096, 2048);
        assert_eq!(s.stereo, SkyStereo::OverUnder, "name should win over 2:1");
        let s = guess("beach_sbs.png", 4096, 2048);
        assert_eq!(s.stereo, SkyStereo::SideBySide);
    }

    #[test]
    fn vr180_is_recognised_from_the_name() {
        let s = guess("concert_vr180_ou.jpg", 4096, 4096);
        assert_eq!(s.projection, SkyProjection::Equirect180);
        assert_eq!(s.stereo, SkyStereo::OverUnder);
    }

    #[test]
    fn only_image_extensions_are_offered() {
        assert!(is_image(Path::new("a.jpg")));
        assert!(is_image(Path::new("a.JPEG")));
        assert!(is_image(Path::new("a.png")));
        assert!(!is_image(Path::new("a.txt")));
        assert!(!is_image(Path::new("readme")));
    }

    #[test]
    fn the_rotation_always_includes_the_generated_environment() {
        // With no files at all, advancing must be a no-op rather than leaving `index` pointing
        // at a file that does not exist.
        let mut e = Environments {
            files: vec![],
            index: None,
        };
        e.advance();
        assert!(e.index.is_none());
        assert_eq!(e.describe(), "Studio (generated)");
    }

    #[test]
    fn advancing_cycles_through_the_files_and_back_to_generated() {
        let mut e = Environments {
            files: vec![PathBuf::from("/a/one.jpg"), PathBuf::from("/a/two.jpg")],
            index: None,
        };
        e.advance();
        assert_eq!(e.describe(), "one");
        e.advance();
        assert_eq!(e.describe(), "two");
        e.advance();
        assert_eq!(e.describe(), "Studio (generated)");
    }
}
