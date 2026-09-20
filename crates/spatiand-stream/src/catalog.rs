//! What a host can run, written down once.
//!
//! The host reads this from `~/.config/spatiand-host/apps.toml`, the configurator writes it,
//! and the session is sent it as a list. All three use the types here, so "what a remote
//! application is" has exactly one definition.
//!
//! An entry is deliberately close to a desktop entry — a name, a command, an icon — because
//! most of them come from one. What a desktop entry cannot say is everything a headset needs
//! to know before the first frame arrives: whether this thing is a window or takes the whole
//! view, how its two eyes are packed, what its sound is shaped like, and what it should be
//! told a gamepad looks like.

use serde::{Deserialize, Serialize};

/// Everything a host offers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default, rename = "app")]
    pub apps: Vec<App>,
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&App> {
        self.apps.iter().find(|a| a.id == id)
    }

    /// Ids have to be unique, because everything else refers to an application by one.
    ///
    /// A duplicate is a configuration mistake rather than a protocol error, so it is reported
    /// rather than refused: the host logs it and keeps the first.
    pub fn duplicate_ids(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut twice = Vec::new();
        for app in &self.apps {
            if !seen.insert(&app.id) && !twice.contains(&app.id) {
                twice.push(app.id.clone());
            }
        }
        twice
    }
}

/// One application the host can start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct App {
    /// Stable, and never shown: it names the layout file, the window's app id and the
    /// application's place in the catalogue. Renaming an application must not lose its
    /// controller layout, which is why the name is not this.
    pub id: String,
    pub name: String,
    /// An icon, as PNG bytes, sent to the session with the catalogue.
    ///
    /// Sent rather than named. The session is a different machine and cannot look in the
    /// host's icon theme, and a launcher bubble with no picture is the one thing people
    /// notice immediately.
    ///
    /// Absent rather than empty when there is none: postcard is not self-describing, so a
    /// field that is sometimes written and sometimes not could not be read back at all.
    /// The icon the owner chose: an absolute path to an image, or a name to look up in the
    /// icon theme. Left out, the host finds one itself — from the application's desktop entry,
    /// or from an image beside the program. See `spatiand-host-catalog`.
    #[serde(default)]
    pub icon: Option<String>,
    /// The icon as it is sent: a small PNG the host renders from whatever `icon` resolved to.
    /// Filled in by the host when it serves the catalogue, and never written to the file.
    #[serde(default)]
    pub icon_png: Option<Vec<u8>>,
    pub exec: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment, on top of what the host sets for routing and for the pad.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub kind: AppKind,
    #[serde(default)]
    pub eyes: Eyes,
    #[serde(default)]
    pub audio: AudioMode,
    #[serde(default)]
    pub pad: PadProfile,
    #[serde(default)]
    pub detach: Detach,
}

impl App {
    /// A minimal entry, the way the configurator's "add" button starts one.
    pub fn new(id: impl Into<String>, name: impl Into<String>, exec: impl Into<String>) -> App {
        App {
            id: id.into(),
            name: name.into(),
            icon: None,
            icon_png: None,
            exec: exec.into(),
            args: Vec::new(),
            env: Vec::new(),
            workdir: None,
            kind: AppKind::default(),
            eyes: Eyes::default(),
            audio: AudioMode::default(),
            pad: PadProfile::default(),
            detach: Detach::default(),
        }
    }
}

/// What the application is in the room.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    /// A window among the others. The wearer moves it, resizes it and puts things beside it.
    #[default]
    Window,
    /// It takes the view. The host draws what the head is looking at, and the session keeps
    /// its own windows and menus on top of it.
    Vr,
}

/// How the two eyes are packed into the pictures the host sends.
///
/// The same three values `spatiand_xr_v1` uses, because a remote application's frames end up
/// on a surface that speaks that protocol.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eyes {
    #[default]
    Mono,
    SideBySide,
    TopBottom,
}

/// What shape the application's sound is.
///
/// `Auto` is right for almost everything: the host opens a stereo sink and the session places
/// it at the window. The rest exist because a few applications will only produce what they are
/// given a sink for — a viewer's engine asks the device how many channels it has and mixes to
/// that, so a 7.1 sink is the only way to be sent the surrounds it already computed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioMode {
    #[default]
    Auto,
    Stereo,
    Surround51,
    Surround71,
    /// 7.1.4 — surrounds plus four overhead.
    Surround714,
    /// Third-order ambisonics from OpenAL Soft, rotated by the head and decoded here.
    Ambisonic,
}

/// What the application is told a gamepad is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadProfile {
    /// The ordinary one: six axes, a hat for the D-pad, Steam's virtual-gamepad identity.
    #[default]
    Xbox,
    /// Eight axes and the D-pad as buttons, for applications that read a joystick through
    /// libndofdev — Second Life viewers do. That library copies every axis plus two per hat
    /// into an array of eight without checking, so a pad with more axes than that loses the
    /// end of the list, the D-pad included. Eight axes and no hat is the shape that survives,
    /// and it is what lets the head reach an application that has never heard of a head.
    Ndof8,
}

/// What the host does with the application when nobody is watching.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detach {
    /// Keep running at full speed. A game mid-match, a download, a world that other people are
    /// standing in.
    #[default]
    Run,
    /// Keep running, but give it frame callbacks slowly, so it stops spending a GPU on
    /// pictures nobody will see.
    Throttle { fps: u8 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Catalog {
        Catalog {
            apps: vec![
                App::new("firestorm", "Firestorm", "/opt/firestorm/firestorm"),
                App {
                    kind: AppKind::Vr,
                    eyes: Eyes::SideBySide,
                    audio: AudioMode::Surround71,
                    pad: PadProfile::Ndof8,
                    detach: Detach::Throttle { fps: 5 },
                    args: vec!["--login".into()],
                    env: vec![("LANG".into(), "C".into())],
                    ..App::new("secondlife", "Second Life", "/opt/sl/sl")
                },
            ],
        }
    }

    #[test]
    fn a_catalogue_survives_a_trip_through_a_file() {
        let text = toml::to_string_pretty(&sample()).expect("serialises");
        let back: Catalog = toml::from_str(&text).expect("parses back");
        assert_eq!(back, sample(), "changed on the way through:\n{text}");
    }

    #[test]
    fn a_catalogue_survives_the_wire() {
        let bytes = crate::to_bytes(&sample()).expect("encodes");
        let back: Catalog = crate::from_bytes(&bytes).expect("decodes");
        assert_eq!(back, sample());
    }

    #[test]
    fn an_entry_needs_only_a_name_and_a_command() {
        let text = r#"
            [[app]]
            id = "chrome"
            name = "Chrome"
            exec = "/usr/bin/google-chrome"
        "#;
        let catalog: Catalog = toml::from_str(text).expect("parses");
        let app = catalog.get("chrome").expect("is there");
        assert_eq!(app.kind, AppKind::Window);
        assert_eq!(app.eyes, Eyes::Mono);
        assert_eq!(app.pad, PadProfile::Xbox);
        assert_eq!(app.detach, Detach::Run);
    }

    #[test]
    fn two_entries_with_one_id_are_reported() {
        let catalog = Catalog {
            apps: vec![
                App::new("same", "One", "/one"),
                App::new("same", "Two", "/two"),
                App::new("other", "Three", "/three"),
            ],
        };
        assert_eq!(catalog.duplicate_ids(), vec!["same".to_string()]);
    }
}
