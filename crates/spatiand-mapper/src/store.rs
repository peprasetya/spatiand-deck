//! Where layouts live: one file per application, like Game Mode's per-game layouts.

use std::path::{Path, PathBuf};

use crate::layout::Layout;
use crate::templates;

/// Which application a layout belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AppKey {
    /// A Steam game, by app id — the number Steam itself files layouts under. The same game
    /// keeps its layout whichever desktop entry or window it came through.
    Steam(u32),
    /// Anything else, by its desktop-entry or Wayland app id.
    App(String),
}

impl AppKey {
    /// The Steam game a desktop entry's `Exec` line launches, if it launches one.
    pub fn from_exec(exec: &str) -> Option<AppKey> {
        let (_, rest) = exec.split_once("steam://rungameid/")?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok().map(AppKey::Steam)
    }

    /// From a process's `SteamAppId` environment variable, which Steam sets on every game it
    /// starts and which every process of the game inherits.
    pub fn from_steam_app_id(value: &str) -> Option<AppKey> {
        match value.trim().parse::<u32>() {
            // Steam sets 0 for things that are not a game, such as its own tools.
            Ok(0) | Err(_) => None,
            Ok(id) => Some(AppKey::Steam(id)),
        }
    }

    pub fn is_steam_game(&self) -> bool {
        matches!(self, AppKey::Steam(_))
    }

    /// A file name that is safe whatever an app id contains.
    pub fn file_stem(&self) -> String {
        match self {
            AppKey::Steam(id) => format!("steam-{id}"),
            AppKey::App(id) => {
                let safe: String = id
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                format!("app-{safe}")
            }
        }
    }

    /// What an application uses until someone gives it a layout of its own.
    ///
    /// A game starts as a gamepad, which is what Game Mode gives a game with controller
    /// support. Everything else starts as the desktop Spatiand has always had, so opening a
    /// browser behaves exactly as it did before layouts existed.
    pub fn default_layout(&self) -> Layout {
        match self {
            AppKey::Steam(_) => templates::gamepad(),
            AppKey::App(_) => templates::desktop(),
        }
    }
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, app: &AppKey) -> PathBuf {
        self.dir.join(format!("{}.toml", app.file_stem()))
    }

    /// This application's own layout, if it has one that can be read.
    ///
    /// A file that does not parse is reported and treated as absent rather than as fatal: the
    /// fallback is a working layout, and a typo in a hand edit must not leave a game with no
    /// controls at all.
    pub fn load(&self, app: &AppKey) -> Option<Layout> {
        let path = self.path(app);
        let text = std::fs::read_to_string(&path).ok()?;
        match toml::from_str::<Layout>(&text) {
            Ok(layout) => Some(layout.repaired()),
            Err(e) => {
                log::warn!("ignoring the layout in {}: {e}", path.display());
                None
            }
        }
    }

    /// The layout in effect for an application, and whether it is the app's own.
    pub fn layout_for(&self, app: &AppKey) -> (Layout, bool) {
        match self.load(app) {
            Some(layout) => (layout, true),
            None => (app.default_layout(), false),
        }
    }

    /// Written beside the destination and renamed over it, so a crash mid-write leaves the old
    /// layout rather than half a file.
    pub fn save(&self, app: &AppKey, layout: &Layout) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let text = toml::to_string_pretty(layout)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let path = self.path(app);
        let temporary = path.with_extension("toml.new");
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, &path)
    }

    /// Every layout saved here, other than `except`'s, each with a name a person would know it
    /// by. For copying one application's layout to another: a new application usually wants the
    /// same controls as one already set up — a second viewer, a second browser — and making the
    /// wearer rebuild them by hand is the worst way to get there. Unreadable files are left out
    /// rather than failing the list.
    pub fn saved_except(&self, except: &AppKey) -> Vec<(String, Layout)> {
        let mine = except.file_stem();
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut found: Vec<(String, String, Layout)> = dir
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    return None;
                }
                let stem = path.file_stem()?.to_str()?.to_string();
                if stem == mine {
                    return None;
                }
                let text = std::fs::read_to_string(&path).ok()?;
                let layout: Layout = toml::from_str(&text).ok()?;
                Some((stem.clone(), describe(&stem), layout))
            })
            .collect();
        found.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        found.into_iter().map(|(_, name, layout)| (name, layout)).collect()
    }

    /// Go back to the default for this application.
    pub fn forget(&self, app: &AppKey) -> std::io::Result<()> {
        match std::fs::remove_file(self.path(app)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// A stored layout's file name, as the application a person would recognise.
///
/// The store keeps no record of which application a file was made for beyond its name, so the
/// name is read back: `steam-1677740` is a Steam game, `app-remote.<host>_<port>.<id>` is an
/// application on another machine, anything else is its own id.
pub fn describe(stem: &str) -> String {
    if let Some(id) = stem.strip_prefix("steam-") {
        return format!("Steam game {id}");
    }
    let app = stem.strip_prefix("app-").unwrap_or(stem);
    if let Some(rest) = app.strip_prefix("remote.") {
        if let Some((host, id)) = rest.rsplit_once('.') {
            // The port is in the name only to keep two hosts on one address apart.
            let host = host.rsplit_once('_').map(|(h, _)| h).unwrap_or(host);
            return format!("{id} on {host}");
        }
    }
    app.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spatiand-mapper-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_stored_layout_is_named_the_way_a_person_would_name_it() {
        assert_eq!(describe("app-remote.192.0.2.7_47600.firestorm"), "firestorm on 192.0.2.7");
        assert_eq!(describe("steam-1677740"), "Steam game 1677740");
        assert_eq!(describe("app-org.mozilla.firefox"), "org.mozilla.firefox");
    }

    #[test]
    fn every_other_saved_layout_is_offered_but_not_the_applications_own() {
        let dir = scratch("saved-except");
        let store = Store::new(&dir);
        let viewer = AppKey::App("remote.192.0.2.7:47600.firestorm".into());
        let world = AppKey::App("remote.192.0.2.7:47600.spatiworld".into());
        store.save(&viewer, &templates::gamepad()).expect("save");
        store.save(&world, &templates::desktop()).expect("save");
        // Something that is not a layout at all is passed over, not fatal.
        std::fs::write(dir.join("app-broken.toml"), "this is not = [a layout").unwrap();
        let offered = store.saved_except(&world);
        let names: Vec<&str> = offered.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["firestorm on 192.0.2.7"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_steam_desktop_entry_names_its_game() {
        assert_eq!(
            AppKey::from_exec("steam steam://rungameid/1677740"),
            Some(AppKey::Steam(1677740))
        );
        assert_eq!(AppKey::from_exec("/usr/bin/firefox %u"), None);
    }

    #[test]
    fn steam_app_id_zero_is_not_a_game() {
        assert_eq!(AppKey::from_steam_app_id("0"), None);
        assert_eq!(
            AppKey::from_steam_app_id("1677740\n"),
            Some(AppKey::Steam(1677740))
        );
    }

    #[test]
    fn an_app_id_cannot_escape_the_layouts_folder() {
        let stem = AppKey::App("../../etc/passwd".into()).file_stem();
        assert!(!stem.contains('/'));
    }

    #[test]
    fn games_start_as_a_gamepad_and_everything_else_as_the_desktop() {
        assert_eq!(AppKey::Steam(1).default_layout().name, "Gamepad");
        assert_eq!(
            AppKey::App("org.kde.dolphin".into()).default_layout().name,
            "Spatial desktop"
        );
    }

    #[test]
    fn a_saved_layout_comes_back_and_forgetting_it_restores_the_default() {
        let store = Store::new(scratch("roundtrip"));
        let app = AppKey::Steam(42);
        let mut layout = templates::keyboard_and_mouse();
        layout.name = "Mine".into();
        store.save(&app, &layout).unwrap();
        assert_eq!(store.layout_for(&app), (layout, true));
        store.forget(&app).unwrap();
        assert_eq!(store.layout_for(&app).1, false);
        // Forgetting twice is not an error.
        store.forget(&app).unwrap();
    }

    #[test]
    fn a_broken_file_falls_back_to_the_default_instead_of_failing() {
        let store = Store::new(scratch("broken"));
        let app = AppKey::Steam(7);
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.path(&app), "name = = =").unwrap();
        let (layout, own) = store.layout_for(&app);
        assert!(!own);
        assert_eq!(layout.name, "Gamepad");
    }
}
