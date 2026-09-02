//! What the wearer has chosen, kept between sessions.
//!
//! Stored beside the axis map, in `$XDG_CONFIG_HOME/spatiand/prefs.toml` — the session's one
//! config directory, which happens to be defined in [`spatiand_track::config`] because the
//! calibration was the first thing that needed somewhere to live. On SteamOS that is on
//! `/home`, so a preference survives an OS update.
//!
//! Unlike the axis map, this file is **forgiving**. A missing field falls back to its default
//! and an unrecognised one is ignored, so a file written by a newer build still opens in an
//! older one and vice versa. The axis map refuses an old file because reinterpreting a stale
//! measurement produces a mirrored world that looks like filter drift; a stale preference is
//! at worst a keyboard that clicks when you did not want it to, and you can see the control
//! that turns it off.
//!
//! Written whenever something changes, which is a few bytes on a control the wearer had to
//! deliberately press. There is no flush-on-exit path on purpose: a session that ends by the
//! headset being unplugged should not be the one that loses the setting.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Whether the on-screen keyboards make a sound. See [`crate::click`].
    pub keyboard_click: bool,
    /// Whether each window's sound is placed where the window is.
    ///
    /// On by default, because it is the point. Off is a real choice rather than a fallback:
    /// someone listening on the Deck's own speakers with the glasses pushed up does not want
    /// their music moving about as they look around the room.
    #[serde(default = "yes")]
    pub spatial_audio: bool,

    /// How much of the plain stereo fold to keep when a window is straight ahead, 0 to 1.
    ///
    /// The one setting here that cannot be decided by anyone but the listener. A measured head
    /// is somebody else's, and where their ears disagree with yours the difference arrives as
    /// a mild colouring. More of this is less colour and less placement; less is the reverse.
    /// See `spatiand_audio::render::Directness`.
    #[serde(default = "default_directness_centred")]
    pub audio_directness_centred: f32,

    /// The same, once the window is well round to one side, where the placement is doing real
    /// work and is worth more than the tone.
    #[serde(default = "default_directness_off_axis")]
    pub audio_directness_off_axis: f32,
}

impl Default for Prefs {
    fn default() -> Self {
        // Deliberately the same default the shell's own keyboard carries, so a session with no
        // file behaves exactly like one whose file says what the defaults are.
        Self {
            keyboard_click: spatiand_shell::Keyboard::default().click,
            spatial_audio: true,
            audio_directness_centred: DEFAULT_DIRECTNESS.centred,
            audio_directness_off_axis: DEFAULT_DIRECTNESS.off_axis,
        }
    }
}

pub fn path() -> PathBuf {
    spatiand_track::config::config_dir().join("prefs.toml")
}

impl Prefs {
    /// Read the stored preferences, falling back to the defaults for anything missing.
    pub fn load() -> Self {
        let path = path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str::<Self>(&text) {
            Ok(prefs) => {
                log::info!("preferences from {}: {prefs:?}", path.display());
                prefs
            }
            Err(e) => {
                // Kept, not replaced. Overwriting a file we could not read is how a typo in it
                // turns into silently losing every setting in it.
                log::warn!("could not parse {} ({e}); using defaults", path.display());
                Self::default()
            }
        }
    }

    /// Write them back. Best-effort: a preference that could not be saved is worth a line in
    /// the log, not an error the wearer has to deal with mid-session.
    pub fn save(&self) {
        if let Err(e) = self.write() {
            log::warn!("could not save preferences to {}: {e}", path().display());
        }
    }

    fn write(&self) -> std::io::Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the config directory at somewhere disposable. Serialised, because the environment
    /// is process-wide and these tests all write to it.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spatiand-prefs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        dir
    }

    #[test]
    fn a_choice_survives_the_session_that_made_it() {
        let _lock = crate::prefs::tests::LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = scratch("roundtrip");
        let mut prefs = Prefs::default();
        prefs.keyboard_click = false;
        prefs.save();
        assert_eq!(Prefs::load(), prefs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_with_no_file_gets_the_defaults() {
        let _lock = crate::prefs::tests::LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = scratch("missing");
        assert_eq!(Prefs::load(), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_from_another_build_still_opens() {
        // Both directions at once: a setting this build has never heard of, and one of ours
        // that is not in the file. Refusing either would mean the first upgrade silently
        // resets everything the wearer had chosen.
        let _lock = crate::prefs::tests::LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = scratch("compat");
        std::fs::create_dir_all(&dir.join("spatiand")).unwrap();
        std::fs::write(path(), "something_from_the_future = 3\n").unwrap();
        assert_eq!(Prefs::load(), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_file_is_kept_rather_than_replaced() {
        // Overwriting what we could not parse is how one bad character costs the wearer every
        // setting in the file.
        let _lock = crate::prefs::tests::LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = scratch("broken");
        std::fs::create_dir_all(&dir.join("spatiand")).unwrap();
        std::fs::write(path(), "this is not toml = = =").unwrap();
        let _ = Prefs::load();
        assert!(
            std::fs::read_to_string(path())
                .unwrap()
                .contains("not toml"),
            "loading destroyed the file it could not read"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    pub(super) static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

/// The blend the session starts with, before anyone has had an opinion about it.
const DEFAULT_DIRECTNESS: spatiand_audio::render::Directness = spatiand_audio::render::Directness {
    centred: 0.35,
    off_axis: 0.10,
    fade_by: 40.0 * std::f64::consts::PI / 180.0,
};

fn yes() -> bool {
    true
}

fn default_directness_centred() -> f32 {
    DEFAULT_DIRECTNESS.centred
}

fn default_directness_off_axis() -> f32 {
    DEFAULT_DIRECTNESS.off_axis
}

impl Prefs {
    /// The blend these preferences ask for.
    ///
    /// Clamped, because this file is edited by hand and a value outside nought to one is a
    /// gain, not a blend -- it would make the sound louder rather than plainer.
    pub fn directness(&self) -> spatiand_audio::render::Directness {
        spatiand_audio::render::Directness {
            centred: self.audio_directness_centred.clamp(0.0, 1.0),
            off_axis: self.audio_directness_off_axis.clamp(0.0, 1.0),
            ..DEFAULT_DIRECTNESS
        }
    }
}
