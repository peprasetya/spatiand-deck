//! Choosing and loading the 360° environment.
//!
//! Spatiand ships with no image and works anyway: [`spatiand_render::Sky::studio`] generates
//! one. That is not a placeholder to be replaced later — it is what makes a first run on a
//! machine with no assets and no network look deliberate rather than broken, and it is the
//! only environment guaranteed to carry a key light for the glass bubbles to catch.
//!
//! There are three ways an image gets into the list, in increasing order of effort:
//!
//! * drop it into `~/.local/share/spatiand/environments` (`tools/fetch-environments.sh` will
//!   fetch some CC0 ones);
//! * point `SPATIAND_ENVIRONMENTS` at a folder of your own;
//! * pick it with the in-world browser, which records the path in `environments.list` beside
//!   that folder.
//!
//! The third of those records a **path** rather than copying the file. A panorama is tens of
//! megabytes and copying it would mean two of them, with no way to tell later which was the
//! original. The cost is that moving or deleting the file breaks the entry — handled the same
//! way as any other unreadable image, by falling back rather than by leaving a black void.
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
use spatiand_shell::{EnvironmentChoice, EnvironmentEntry, FileEntry};

/// Resolution of the generated environment.
///
/// 2048x1024 is about 5.7 pixels per degree, against the ~48 the display can resolve. That is
/// deliberately soft: it is a background, it costs 8 MB, and a sharp one would draw the eye to
/// exactly where nothing is happening.
const GENERATED_SIZE: (u32, u32) = (2048, 1024);

/// The environments available this session.
pub struct Environments {
    files: Vec<PathBuf>,
    choice: EnvironmentChoice,
}

impl Environments {
    pub fn discover() -> Self {
        let files = scan();
        if files.is_empty() {
            log::info!("no environment images found; using the generated one");
        } else {
            log::info!("{} environment image(s) available", files.len());
        }
        // Start on a named one if asked. Useful for a snapshot of a particular background, and
        // for anyone who wants the same world every time rather than whatever they left it on.
        let choice = std::env::var("SPATIAND_ENVIRONMENT")
            .ok()
            .and_then(|wanted| {
                let wanted = wanted.to_lowercase();
                files
                    .iter()
                    .position(|p| p.to_string_lossy().to_lowercase().contains(&wanted))
            })
            .map(EnvironmentChoice::File)
            .unwrap_or(EnvironmentChoice::Studio);
        if let EnvironmentChoice::File(i) = choice {
            log::info!("starting on {}", files[i].display());
        }
        Self { files, choice }
    }

    /// Re-read the folders, keeping whatever is currently in use selected.
    ///
    /// The selection is re-found **by path**, not by index. Indices shift when a file is added
    /// or removed, and silently swapping the world out from under someone who only opened a
    /// menu is exactly the kind of thing nobody thinks to test for.
    pub fn refresh(&mut self) {
        let selected = self.selected_path().map(PathBuf::from);
        self.files = scan();
        self.choice = match selected {
            Some(path) => match self.files.iter().position(|p| *p == path) {
                Some(i) => EnvironmentChoice::File(i),
                None => {
                    log::info!("{} is gone; falling back to the generated one", path.display());
                    EnvironmentChoice::Studio
                }
            },
            None => self.choice,
        };
    }

    pub fn choice(&self) -> EnvironmentChoice {
        self.choice
    }

    fn selected_path(&self) -> Option<&Path> {
        match self.choice {
            EnvironmentChoice::File(i) => self.files.get(i).map(PathBuf::as_path),
            _ => None,
        }
    }

    /// The list as the picker should show it.
    ///
    /// Blank first, then the generated one, then the files. Blank leads because it is the one
    /// choice whose whole point is to be found quickly when the room is in the way.
    pub fn entries(&self) -> Vec<EnvironmentEntry> {
        let mut entries = vec![
            EnvironmentEntry {
                label: "Blank (black)".into(),
                choice: EnvironmentChoice::Blank,
            },
            EnvironmentEntry {
                label: "Studio (generated)".into(),
                choice: EnvironmentChoice::Studio,
            },
        ];
        for (i, path) in self.files.iter().enumerate() {
            entries.push(EnvironmentEntry {
                label: stem(path),
                choice: EnvironmentChoice::File(i),
            });
        }
        entries
    }

    /// Select one. A file index that no longer exists falls back to the generated one rather
    /// than to a black void the wearer did not ask for.
    pub fn select(&mut self, choice: EnvironmentChoice) {
        self.choice = match choice {
            EnvironmentChoice::File(i) if i >= self.files.len() => {
                log::warn!("environment {i} no longer exists; using the generated one");
                EnvironmentChoice::Studio
            }
            other => other,
        };
        log::info!("environment -> {}", self.describe());
    }

    /// Add a file the browser found, and return the choice that now points at it.
    ///
    /// Adding an image already in the list selects the existing entry rather than duplicating
    /// it — picking the same file twice is a plausible thing to do and should be harmless.
    pub fn add(&mut self, path: &Path) -> EnvironmentChoice {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(i) = self.files.iter().position(|p| *p == path) {
            return EnvironmentChoice::File(i);
        }
        if let Err(e) = remember(&path) {
            // Worth using this session even if it will not survive a restart, and worth
            // saying so rather than appearing to have worked.
            log::warn!("could not record {} for next time: {e}", path.display());
        }
        self.files.push(path.clone());
        // Sorted, to keep the same stable order `scan` produces — so the row sits where it
        // will sit after a restart rather than jumping the next time Spatiand starts. That
        // moves it away from the end, so its index has to be looked up rather than assumed.
        self.files.sort();
        match self.files.iter().position(|p| *p == path) {
            Some(i) => EnvironmentChoice::File(i),
            None => EnvironmentChoice::Studio,
        }
    }

    /// Load whatever is currently selected.
    pub fn current(&self) -> Sky {
        match self.choice {
            EnvironmentChoice::Blank => Sky::blank(),
            EnvironmentChoice::Studio => Sky::studio(GENERATED_SIZE.0, GENERATED_SIZE.1),
            EnvironmentChoice::File(i) => match self.files.get(i) {
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
            },
        }
    }

    pub fn describe(&self) -> String {
        match self.choice {
            EnvironmentChoice::Blank => "Blank (black)".into(),
            EnvironmentChoice::Studio => "Studio (generated)".into(),
            EnvironmentChoice::File(i) => match self.files.get(i) {
                Some(p) => stem(p),
                None => "Studio (generated)".into(),
            },
        }
    }
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into())
}

/// Where the in-world file browser is looking.
///
/// Owned by the compositor rather than the shell for the same reason the environment list is:
/// the shell has no filesystem and no paths, only names.
pub struct Browser {
    directory: PathBuf,
}

impl Default for Browser {
    fn default() -> Self {
        Self::new()
    }
}

impl Browser {
    /// Start in Pictures if there is one, since that is where a downloaded panorama lands, and
    /// in the home directory otherwise. Never at `/`: a browser that opens on `bin`, `boot`,
    /// `dev` is technically correct and useless.
    pub fn new() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let directory = home
            .as_ref()
            .map(|h| h.join("Pictures"))
            .filter(|p| p.is_dir())
            .or(home)
            .unwrap_or_else(|| PathBuf::from("/"));
        Self { directory }
    }

    pub fn label(&self) -> String {
        self.directory.display().to_string()
    }

    /// What to show: directories first, then images, each sorted.
    ///
    /// Files that are not images are left out entirely. This browser exists to find a
    /// panorama, and a listing of every `.txt` in a home directory is noise you have to read
    /// past on a screen with about 640 usable pixels across it.
    pub fn entries(&self) -> Vec<FileEntry> {
        let Ok(read) = std::fs::read_dir(&self.directory) else {
            log::warn!("cannot read {}", self.directory.display());
            return Vec::new();
        };
        let mut directories = Vec::new();
        let mut images = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Hidden entries are skipped. `~/.cache`, `~/.local`, `~/.steam` between you and
            // `Pictures` is a wall of things nobody is browsing for.
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                directories.push(FileEntry {
                    name,
                    is_directory: true,
                });
            } else if is_image(&path) {
                images.push(FileEntry {
                    name,
                    is_directory: false,
                });
            }
        }
        directories.sort_by(|a, b| a.name.cmp(&b.name));
        images.sort_by(|a, b| a.name.cmp(&b.name));
        directories.extend(images);
        directories
    }

    /// Move, following the name the shell reported. `..` climbs, and climbing from the root
    /// stays at the root rather than failing.
    pub fn enter(&mut self, name: &str) {
        if name == spatiand_shell::files::PARENT_LABEL {
            if let Some(parent) = self.directory.parent() {
                self.directory = parent.to_path_buf();
            }
            return;
        }
        let candidate = self.directory.join(name);
        if candidate.is_dir() {
            self.directory = candidate;
        }
    }

    pub fn resolve(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }
}

/// Every image path on offer, in a stable order so "the third one" means the same thing
/// between runs.
fn scan() -> Vec<PathBuf> {
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
    for path in remembered() {
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    files
}

fn data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("spatiand"))
}

/// Where paths added through the browser are recorded — one per line, `#` for comments, so it
/// can be edited by hand on a machine where the headset is what is broken.
fn remembered_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("environments.list"))
}

fn remembered() -> Vec<PathBuf> {
    let Some(path) = remembered_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(PathBuf::from)
        // A file that has since been deleted is dropped here rather than being offered as a
        // row that fails when chosen.
        .filter(|p| p.is_file())
        .collect()
}

fn remember(path: &Path) -> std::io::Result<()> {
    let Some(list) = remembered_path() else {
        return Err(std::io::Error::other("no data directory"));
    };
    if let Some(parent) = list.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = std::fs::read_to_string(&list).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&path.display().to_string());
    text.push('\n');
    std::fs::write(&list, text)
}

fn search_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(explicit) = std::env::var("SPATIAND_ENVIRONMENTS") {
        dirs.push(PathBuf::from(explicit));
    }
    if let Some(data) = data_dir() {
        dirs.push(data.join("environments"));
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

    fn with_files(files: &[&str], choice: EnvironmentChoice) -> Environments {
        Environments {
            files: files.iter().map(PathBuf::from).collect(),
            choice,
        }
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
    fn blank_and_generated_are_offered_even_with_no_files_at_all() {
        // The first-run case. A picker with nothing in it would be indistinguishable from a
        // broken one, and "blank" is the choice least able to depend on the filesystem.
        let e = with_files(&[], EnvironmentChoice::Studio);
        let entries = e.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].choice, EnvironmentChoice::Blank);
        assert_eq!(entries[1].choice, EnvironmentChoice::Studio);
    }

    #[test]
    fn every_file_gets_a_row_indexed_the_way_the_picker_reports_it() {
        // The picker sends back an index into this list, so a mismatch here surrounds the
        // wearer with the wrong image and nothing errors.
        let e = with_files(&["/a/one.jpg", "/a/two.jpg"], EnvironmentChoice::Studio);
        let entries = e.entries();
        assert_eq!(entries[2].label, "one");
        assert_eq!(entries[2].choice, EnvironmentChoice::File(0));
        assert_eq!(entries[3].label, "two");
        assert_eq!(entries[3].choice, EnvironmentChoice::File(1));
    }

    #[test]
    fn choosing_blank_is_black_rather_than_the_generated_studio() {
        // The whole point of the option, and easy to get wrong by treating Blank as "no
        // choice made" — which is what the old `Option<usize>` shape would have done.
        let mut e = with_files(&["/a/one.jpg"], EnvironmentChoice::Studio);
        e.select(EnvironmentChoice::Blank);
        let sky = e.current();
        assert!(sky.rgba.chunks_exact(4).all(|p| p[..3] == [0, 0, 0]));
        assert_eq!(e.describe(), "Blank (black)");
    }

    #[test]
    fn selecting_a_file_that_is_no_longer_there_falls_back_rather_than_going_dark() {
        // Stale indices are reachable: the picker is built from one listing and activated
        // against another. Leaving the wearer in a void with no visible menu is the one
        // outcome that cannot be recovered from inside the headset.
        let mut e = with_files(&["/a/one.jpg"], EnvironmentChoice::Studio);
        e.select(EnvironmentChoice::File(9));
        assert_eq!(e.choice(), EnvironmentChoice::Studio);
    }

    #[test]
    fn adding_a_file_that_is_already_listed_selects_it_instead_of_duplicating_it() {
        let mut e = with_files(&["/a/one.jpg", "/a/two.jpg"], EnvironmentChoice::Studio);
        let choice = e.add(Path::new("/a/two.jpg"));
        assert_eq!(choice, EnvironmentChoice::File(1));
        assert_eq!(e.entries().len(), 4, "no new row");
    }

    #[test]
    fn the_generated_environment_is_never_absent_from_the_list() {
        // It is the only one that cannot fail to load, so it is the fallback for everything
        // else. A list without it has no safe row.
        for choice in [
            EnvironmentChoice::Blank,
            EnvironmentChoice::Studio,
            EnvironmentChoice::File(0),
        ] {
            let e = with_files(&["/a/one.jpg"], choice);
            assert!(e
                .entries()
                .iter()
                .any(|entry| entry.choice == EnvironmentChoice::Studio));
        }
    }
}
