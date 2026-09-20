//! What a remote application host keeps on disk, and the pictures that go with it.
//!
//! Two files, both in `~/.config/spatiand-host/`:
//!
//! * `apps.toml` — the catalogue, [`spatiand_stream::Catalog`]: what the host offers.
//! * `host.toml` — [`Settings`]: how much of the network it may use.
//!
//! Both are written by the host's settings app and read by the host, which notices when they
//! change. Nothing here needs a GPU or a network, which is why the settings app can use it
//! without bringing the host along, and why it is tested on a laptop.

pub mod icons;
pub mod import;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use spatiand_stream::{App, Bandwidth, Catalog};

/// The directory both files live in, honouring `XDG_CONFIG_HOME`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("spatiand-host")
}

pub fn catalog_path() -> PathBuf {
    config_dir().join("apps.toml")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("host.toml")
}

/// Where a running host listens for commands: pairing, and starting an application.
/// `$XDG_RUNTIME_DIR` is private to the user and gone at logout, so reaching it is already
/// proof of being the account the host runs as.
pub fn control_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("spatiand-host")
        .join("control")
}

/// The id the settings app always has. It is not in the file — it cannot be removed, because
/// it is the only way to put anything back.
pub const SETTINGS_APP_ID: &str = "settings";

/// Read the catalogue, or an empty one if there is not a file yet.
///
/// A missing file is not an error: a host that has never been configured should start, listen,
/// and offer its settings app, which is the thing that fills this in. A file that does not
/// parse is kept rather than replaced — somebody's list of applications is not something to
/// overwrite because one line in it is wrong — and the error is returned to be shown.
pub fn load_catalog(path: &Path) -> Result<Catalog, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Catalog::default()),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    let mut catalog: Catalog =
        toml::from_str(&text).map_err(|e| format!("{} does not parse: {e}", path.display()))?;
    // The rendered icon is never the file's business; one that got in is ignored.
    for app in &mut catalog.apps {
        app.icon_png = None;
    }
    catalog.apps.retain(|a| a.id != SETTINGS_APP_ID);
    Ok(catalog)
}

/// Write the catalogue. Atomically — written beside and renamed over — because the host reads
/// this file whenever it changes, and must never read half of one.
pub fn save_catalog(path: &Path, catalog: &Catalog) -> std::io::Result<()> {
    let mut clean = catalog.clone();
    for app in &mut clean.apps {
        app.icon_png = None;
    }
    clean.apps.retain(|a| a.id != SETTINGS_APP_ID);
    write_atomically(path, &to_toml(&clean)?)
}

/// How the host uses the network. Every field falls back to its default on its own, so a file
/// written by an older settings app still opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The most every window together may use, in kbit/s. 25 Mbit/s is comfortable on 5 GHz
    /// Wi-Fi and on Tailscale across town; a wired link can take several times that.
    pub max_kbit: u32,
    /// The most any one window is encoded at. The glasses show 72.
    pub max_fps: u8,
    /// What a window that has stopped changing drops to. Zero sends nothing at all.
    pub idle_fps: u8,
    /// How long a window must be unchanged to count as still.
    pub idle_after_ms: u32,
    /// Where the host listens.
    pub port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_kbit: 25_000,
            max_fps: 72,
            idle_fps: 0,
            idle_after_ms: 250,
            port: spatiand_stream::link::DEFAULT_PORT,
        }
    }
}

impl Settings {
    pub fn bandwidth(&self) -> Bandwidth {
        Bandwidth {
            max_kbit: self.max_kbit,
            max_fps: self.max_fps,
            idle_fps: self.idle_fps,
            idle_after_ms: self.idle_after_ms,
        }
    }
}

pub fn load_settings(path: &Path) -> Result<Settings, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str(&text).map_err(|e| format!("{} does not parse: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

pub fn save_settings(path: &Path, settings: &Settings) -> std::io::Result<()> {
    write_atomically(path, &to_toml(settings)?)
}

/// The settings app's own entry, pointing at the program beside the host's.
///
/// Beside, because that is where a tarball puts it and where `cargo build` puts it, and it
/// means nothing has to be configured for the one app that does the configuring.
pub fn settings_app(host_program: &Path) -> Option<App> {
    let program = host_program.parent()?.join("spatiand-host-config");
    if !program.exists() {
        return None;
    }
    // No `icon`: it brings its own, see `icons::settings_png`.
    Some(App::new(SETTINGS_APP_ID, "Host settings", program.to_string_lossy()))
}

fn to_toml<T: Serialize>(value: &T) -> std::io::Result<String> {
    toml::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let beside = path.with_extension("toml.new");
    std::fs::write(&beside, text)?;
    std::fs::rename(&beside, path)
}

/// A file's modification time, for noticing that it changed.
pub fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spatiand-catalog-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_catalogue_round_trips_without_its_rendered_icons_or_the_settings_app() {
        let dir = temp("round");
        let path = dir.join("apps.toml");
        let mut app = App::new("chrome", "Chrome", "/usr/bin/google-chrome");
        app.icon = Some("google-chrome".into());
        app.icon_png = Some(vec![1, 2, 3]);
        let settings = App::new(SETTINGS_APP_ID, "Host settings", "/x");
        save_catalog(
            &path,
            &Catalog {
                apps: vec![settings, app],
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("icon_png"), "{text}");
        let back = load_catalog(&path).unwrap();
        assert_eq!(back.apps.len(), 1);
        assert_eq!(back.apps[0].icon.as_deref(), Some("google-chrome"));
        assert!(back.apps[0].icon_png.is_none());
    }

    #[test]
    fn a_missing_file_is_an_empty_catalogue_and_a_broken_one_is_an_error() {
        let dir = temp("missing");
        assert!(load_catalog(&dir.join("nope.toml")).unwrap().apps.is_empty());
        std::fs::write(dir.join("bad.toml"), "[[app]\nid=").unwrap();
        assert!(load_catalog(&dir.join("bad.toml")).is_err());
    }

    #[test]
    fn settings_fill_in_whatever_the_file_leaves_out() {
        let dir = temp("settings");
        let path = dir.join("host.toml");
        std::fs::write(&path, "max_kbit = 60000\n").unwrap();
        let s = load_settings(&path).unwrap();
        assert_eq!(s.max_kbit, 60_000);
        assert_eq!(s.max_fps, Settings::default().max_fps);
        save_settings(&path, &s).unwrap();
        assert_eq!(load_settings(&path).unwrap(), s);
    }
}
