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
//! ## Coming back to the same world
//!
//! Whichever environment is chosen is written to `environment` in the same folder, and the next
//! session starts in it. Choosing one is a deliberate act that takes several presses through a
//! headset, and having to repeat it every launch is the kind of small tax that makes a thing
//! feel unfinished.
//!
//! What is stored is the **path**, not the position in the list, because that list changes
//! whenever a file is added or removed — a remembered index would eventually name a different
//! image, and being surrounded by the wrong one looks exactly like a bug. A path that has since
//! gone falls back to the generated environment and says so in the log.
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
    /// The data directory this session writes to: the chosen environment, and the paths added
    /// through the browser.
    ///
    /// Injected rather than looked up, so a test can point it at a temporary directory. A test
    /// that wrote to the real one would quietly change the wearer's environment and add rows to
    /// their picker, which is exactly the sort of thing nobody suspects until it happens.
    data: Option<PathBuf>,
}

impl Environments {
    pub fn discover() -> Self {
        let files = scan();
        if files.is_empty() {
            log::info!("no environment images found; using the generated one");
        } else {
            log::info!("{} environment image(s) available", files.len());
        }
        Self::opening(files, data_dir())
    }

    /// Build from a known list and a known data directory, choosing what to start on.
    ///
    /// Precedence, most explicit first: the environment variable, then what was in use when the
    /// last session ended, then the generated one. The variable wins because it exists to force
    /// a particular world — for a snapshot, or for anyone who wants the same one every time —
    /// and a remembered choice quietly overriding it would make it useless.
    fn opening(files: Vec<PathBuf>, data: Option<PathBuf>) -> Self {
        let named = std::env::var("SPATIAND_ENVIRONMENT")
            .ok()
            .and_then(|wanted| {
                let wanted = wanted.to_lowercase();
                files
                    .iter()
                    .position(|p| p.to_string_lossy().to_lowercase().contains(&wanted))
                    .map(EnvironmentChoice::File)
            });
        let choice = named
            .or_else(|| {
                data.as_ref()
                    .and_then(|d| restore(&d.join(STATE_FILE), &files))
            })
            .unwrap_or(EnvironmentChoice::Studio);
        if let EnvironmentChoice::File(i) = choice {
            log::info!("starting on {}", files[i].display());
        }
        let opened = Self {
            files,
            choice,
            data,
        };
        // Said for every kind, not only for files. Restoring "blank" is the case where the log
        // is the only way to tell a remembered choice from a session that failed to draw
        // anything, since both look like an empty room.
        log::info!("environment: {}", opened.describe());
        opened
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
                    log::info!(
                        "{} is gone; falling back to the generated one",
                        path.display()
                    );
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

    /// Select one, and remember it for next time.
    ///
    /// A file index that no longer exists falls back to the generated one rather than to a black
    /// void the wearer did not ask for.
    ///
    /// Everything that changes the environment comes through here — the picker and the browser
    /// both — so this is the one place that has to record anything. `refresh` deliberately does
    /// not: re-reading the folder is not a decision, and a panorama on a drive that happens to
    /// be unplugged should not lose its place permanently.
    pub fn select(&mut self, choice: EnvironmentChoice) {
        self.choice = match choice {
            EnvironmentChoice::File(i) if i >= self.files.len() => {
                log::warn!("environment {i} no longer exists; using the generated one");
                EnvironmentChoice::Studio
            }
            other => other,
        };
        log::info!("environment -> {}", self.describe());
        self.save();
    }

    /// Write the current choice down, if there is anywhere to write it.
    ///
    /// Failure is logged and otherwise ignored: not being able to remember the environment is a
    /// worse next session, not a broken one, and refusing to change it because a file could not
    /// be written would be the wrong trade.
    fn save(&self) {
        let Some(path) = self.data.as_ref().map(|d| d.join(STATE_FILE)) else {
            return;
        };
        let line = match self.choice {
            EnvironmentChoice::Blank => BLANK.to_string(),
            EnvironmentChoice::Studio => STUDIO.to_string(),
            // The path, not the index. Indices are positions in a listing that changes whenever
            // a file is added or removed, so a remembered index eventually names a different
            // image — and being surrounded by the wrong one is indistinguishable from a bug.
            EnvironmentChoice::File(i) => match self.files.get(i) {
                Some(p) => p.display().to_string(),
                None => return,
            },
        };
        let write = |path: &Path| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, format!("{STATE_HEADER}{line}\n"))
        };
        if let Err(e) = write(&path) {
            log::warn!(
                "could not record the environment in {}: {e}",
                path.display()
            );
        }
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
        if let Err(e) = self.remember(&path) {
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

    /// Append a browser-chosen path to the list, so it is on offer next time.
    fn remember(&self, path: &Path) -> std::io::Result<()> {
        let Some(list) = self.data.as_ref().map(|d| d.join(LIST_FILE)) else {
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

/// The two files kept in the data directory.
///
/// `environments.list` holds paths added through the browser — one per line, `#` for comments,
/// so it can be edited by hand on a machine where the headset is what is broken. `environment`
/// holds the one currently chosen.
const LIST_FILE: &str = "environments.list";
const STATE_FILE: &str = "environment";

/// Where paths added through the browser are recorded.
fn remembered_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join(LIST_FILE))
}

/// What the two generated choices are called in the state file.
const BLANK: &str = "blank";
const STUDIO: &str = "studio";
/// Written above the value, since a bare path in a file called `environment` tells whoever
/// finds it nothing about what it does or that deleting it is safe.
const STATE_HEADER: &str = "# What Spatiand was last set to. Delete this to start on the \
                            generated one.\n";

/// Read back what [`Environments::save`] wrote, as an index into *this* session's list.
///
/// `None` for anything that cannot be honoured — no file, an unreadable one, or a path that is
/// no longer on offer — and the caller falls back to the generated environment.
fn restore(path: &Path, files: &[PathBuf]) -> Option<EnvironmentChoice> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    match line {
        BLANK => Some(EnvironmentChoice::Blank),
        STUDIO => Some(EnvironmentChoice::Studio),
        path => match files.iter().position(|p| p.as_os_str() == path) {
            Some(i) => Some(EnvironmentChoice::File(i)),
            None => {
                // Deleted, renamed, or on a drive that is not plugged in. Worth saying out
                // loud: the wearer asked for that image and is about to get a different one.
                log::info!("last environment {path} is no longer available");
                None
            }
        },
    }
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
    let stereo = if has(&[
        "_ou",
        "_tb",
        "over-under",
        "over_under",
        "top-bottom",
        "top_bottom",
    ]) {
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
            // No data directory: these tests are about the list, and none of them should be
            // able to write to the machine they run on.
            data: None,
        }
    }

    /// A directory of this test's own, removed on the way out.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("spatiand-env-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("could not make a scratch directory");
            Self(dir)
        }

        fn state(&self) -> PathBuf {
            self.0.join(STATE_FILE)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn reopen(scratch: &Scratch, files: &[&str]) -> Environments {
        Environments::opening(
            files.iter().map(PathBuf::from).collect(),
            Some(scratch.0.clone()),
        )
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
    fn the_environment_you_chose_is_the_one_you_come_back_to() {
        // The whole point: choosing a world takes several presses through a headset, and doing
        // it again on every launch is what makes a thing feel unfinished.
        let scratch = Scratch::new("comeback");
        let files = ["/a/one.jpg", "/a/two.jpg"];
        let mut session = reopen(&scratch, &files);
        assert_eq!(session.choice(), EnvironmentChoice::Studio, "first run");
        session.select(EnvironmentChoice::File(1));

        let next = reopen(&scratch, &files);
        assert_eq!(next.choice(), EnvironmentChoice::File(1));
        assert_eq!(next.describe(), "two");
    }

    #[test]
    fn what_is_remembered_is_the_file_rather_than_its_place_in_the_list() {
        // The failure this prevents: an image is added, every index after it shifts by one, and
        // the next session silently surrounds you with a different photograph. Nothing errors,
        // which is what makes it hard to recognise as a bug.
        let scratch = Scratch::new("by-path");
        let mut session = reopen(&scratch, &["/a/two.jpg"]);
        session.select(EnvironmentChoice::File(0));

        // Someone drops a file in that sorts before it.
        let next = reopen(&scratch, &["/a/one.jpg", "/a/two.jpg"]);
        assert_eq!(next.choice(), EnvironmentChoice::File(1));
        assert_eq!(next.describe(), "two", "same image, different index");
    }

    #[test]
    fn blank_is_remembered_like_any_other_choice() {
        // Easy to get wrong by treating blank as "nothing chosen" and starting on the studio,
        // which would make the one choice that is hardest to re-find also the one that never
        // sticks.
        let scratch = Scratch::new("blank");
        let mut session = reopen(&scratch, &["/a/one.jpg"]);
        session.select(EnvironmentChoice::Blank);
        assert_eq!(
            reopen(&scratch, &["/a/one.jpg"]).choice(),
            EnvironmentChoice::Blank
        );
    }

    #[test]
    fn an_environment_that_has_gone_falls_back_instead_of_failing_to_start() {
        // Deleted, renamed, or on a drive that is not plugged in. Starting on the generated one
        // is recoverable; anything that refuses to start is not, on a machine whose only screen
        // is the thing being debugged.
        let scratch = Scratch::new("gone");
        let mut session = reopen(&scratch, &["/a/one.jpg"]);
        session.select(EnvironmentChoice::File(0));
        assert_eq!(reopen(&scratch, &[]).choice(), EnvironmentChoice::Studio);
    }

    #[test]
    fn a_state_file_nobody_can_parse_is_survivable() {
        // It is a plain text file in the wearer's own data directory, so it can be edited, and
        // half-written by a machine that lost power mid-save.
        let scratch = Scratch::new("junk");
        std::fs::write(scratch.state(), "\u{0}not a choice at all").unwrap();
        assert_eq!(
            reopen(&scratch, &["/a/one.jpg"]).choice(),
            EnvironmentChoice::Studio
        );
    }

    #[test]
    fn the_state_file_survives_being_read_by_a_person() {
        // It carries a comment saying what it is and that deleting it is safe, which the reader
        // has to skip past to find the value.
        let scratch = Scratch::new("readable");
        let mut session = reopen(&scratch, &["/a/one.jpg"]);
        session.select(EnvironmentChoice::File(0));
        let text = std::fs::read_to_string(scratch.state()).unwrap();
        assert!(text.starts_with('#'), "no explanation for whoever finds it");
        assert!(text.contains("/a/one.jpg"));
        assert_eq!(
            reopen(&scratch, &["/a/one.jpg"]).choice(),
            EnvironmentChoice::File(0)
        );
    }

    #[test]
    fn adding_an_image_through_the_browser_also_remembers_it() {
        // Adding selects, so it must persist too — otherwise the one environment the wearer
        // went furthest out of their way to get is the one that does not come back.
        let scratch = Scratch::new("added");
        let mut session = reopen(&scratch, &["/a/one.jpg"]);
        let choice = session.add(Path::new("/a/zebra.jpg"));
        session.select(choice);
        let next = reopen(&scratch, &["/a/one.jpg", "/a/zebra.jpg"]);
        assert_eq!(next.describe(), "zebra");
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
