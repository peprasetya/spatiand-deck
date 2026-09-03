//! Which of the desktop's own settings panels this system has, and how to open them.
//!
//! A HUD row that opens nothing is worse than a missing row: the wearer concludes the whole
//! HUD is broken rather than that one panel is absent. So availability is a fact to be
//! checked — and checked *per panel*, because "a desktop is installed" and "this particular
//! panel is installed" are different questions. Bluetooth is the case that forces the
//! distinction: the settings runner comes with Plasma, the Bluetooth pieces come with
//! bluedevil, and a machine can easily have the first without the second.
//!
//! Three ways to open one, preferred in order:
//!
//! * **A standalone program**, where the panel has one. Named per panel rather than assumed,
//!   because most do not.
//! * **The panel applet, in its own window** (`plasmawindowed`). The same controls as the
//!   desktop's task bar — a list you pick from, sized like a menu. This is what someone means
//!   when they say they want the one from the tray.
//! * **The settings module** (`kcmshell6`). The full configuration dialog. A fallback, not a
//!   preference: it is a window full of options for a task that is usually "join this one",
//!   and on a headset that is a lot of surface to read through a laser pointer.
//!
//! ## Why an applet is not always the answer
//!
//! An applet is a piece of a desktop shell, and this session is not running one. Whether that
//! matters depends on where the applet gets its information. The network applet asks
//! NetworkManager directly and works anywhere. **The Bluetooth applet does not**: its list of
//! devices comes from KDE's `bluedevil` background module, which a Plasma session starts and
//! this session does not — so it opened, looked exactly right, and listed nothing, on a
//! machine whose adapter was powered, discovering, and had already found five devices.
//!
//! So an applet is offered only where it is known to stand on its own, and Bluetooth names a
//! standalone program instead.
//!
//! The panel names are KDE's, but the mechanism is not special-cased anywhere else: the shell
//! asks for "wifi", this decides what that means here, and on a system with neither the answer
//! is simply "nothing" and the row is hidden.

use std::path::{Path, PathBuf};

/// Programs that can open a settings module in its own window.
const KCM_RUNNERS: [&str; 2] = ["/usr/bin/kcmshell6", "/usr/bin/systemsettings"];
/// The program that runs a desktop panel applet as an ordinary window.
const APPLET_RUNNER: &str = "/usr/bin/plasmawindowed";
/// Where applets live. Two directories because a locally installed one shadows a packaged one.
const APPLET_DIRS: [&str; 2] = [
    "/usr/share/plasma/plasmoids",
    "/usr/local/share/plasma/plasmoids",
];

/// One thing the shell can ask to open.
struct Panel {
    /// What the shell asks for. Deliberately not KDE's name — that is this module's business.
    name: &'static str,
    /// A program that opens this on its own, if one exists. Tried first.
    tool: Option<&'static str>,
    /// A desktop applet to run in its own window, if that applet works without a desktop shell.
    applet: Option<&'static str>,
    /// The settings module, as a last resort.
    module: &'static str,
}

const PANELS: [Panel; 2] = [
    Panel {
        name: "wifi",
        tool: None,
        // Talks to NetworkManager itself, so it needs nothing a desktop session would start.
        applet: Some("org.kde.plasma.networkmanagement"),
        module: "kcm_networkmanagement",
    },
    Panel {
        name: "bluetooth",
        // The wizard is the pairing flow, it discovers devices itself, and it does not depend
        // on anything a Plasma session would have started. It is what the row promises.
        tool: Some("/usr/bin/bluedevil-wizard"),
        // Deliberately none: this applet reads its devices from the `bluedevil` background
        // module, which is not running here. See the note at the top of this file.
        applet: None,
        module: "kcm_bluetooth",
    },
];

/// Is there anything here that can open a settings panel at all?
pub fn has_desktop_settings() -> bool {
    KCM_RUNNERS.iter().any(|p| Path::new(p).exists()) || Path::new(APPLET_RUNNER).exists()
}

/// The command that opens a panel, or `None` if this system cannot.
///
/// The applet first. It is the control people already know from the task bar, and it is the
/// right *size*: a list of networks rather than a configuration dialog.
pub fn settings_command(panel: &str) -> Option<String> {
    let panel = PANELS.iter().find(|p| p.name == panel)?;
    if let Some(tool) = panel.tool {
        if Path::new(tool).exists() {
            return Some(tool.to_string());
        }
    }
    if let Some(applet) = panel.applet {
        if Path::new(APPLET_RUNNER).exists() && applet_installed(applet) {
            return Some(format!("{APPLET_RUNNER} {applet}"));
        }
    }
    let runner = KCM_RUNNERS.iter().find(|p| Path::new(p).exists())?;
    if !module_installed(panel.module) {
        return None;
    }
    Some(format!("{runner} {}", panel.module))
}

/// Can this panel be opened at all?
pub fn panel_available(panel: &str) -> bool {
    settings_command(panel).is_some()
}

fn applet_installed(applet: &str) -> bool {
    APPLET_DIRS
        .iter()
        .any(|dir| Path::new(dir).join(applet).join("metadata.json").exists())
}

/// Is this settings module installed?
///
/// Plasma 6 ships each module's own `.desktop` alongside ordinary applications, so this is the
/// same search path the launcher already walks. `kservices6` is checked too: that is where
/// Plasma 5 put them, and some modules still land there.
fn module_installed(module: &str) -> bool {
    candidates_in(&crate::desktop_entry::search_directories(), module)
        .iter()
        .any(|p| p.exists())
}

fn candidates_in(dirs: &[PathBuf], module: &str) -> Vec<PathBuf> {
    let file = format!("{module}.desktop");
    let mut out = Vec::new();
    for apps in dirs {
        out.push(apps.join(&file));
        // `search_directories` yields `<data>/applications`; the older location is its sibling.
        if let Some(data) = apps.parent() {
            out.push(data.join("kservices6").join(&file));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_the_new_and_the_old_module_locations_are_looked_in() {
        // Plasma moved KCMs from `kservices6` into `applications` between 5 and 6, and a
        // system mid-migration has some of each. Looking in only one place would hide a panel
        // that is installed, which presents as the feature having been dropped.
        let dirs = vec![PathBuf::from("/usr/share/applications")];
        let paths = candidates_in(&dirs, "kcm_bluetooth");
        assert!(paths.contains(&PathBuf::from(
            "/usr/share/applications/kcm_bluetooth.desktop"
        )));
        assert!(paths.contains(&PathBuf::from(
            "/usr/share/kservices6/kcm_bluetooth.desktop"
        )));
    }

    #[test]
    fn a_panel_nobody_ships_is_not_offered() {
        assert!(!panel_available("teleporter"));
        assert_eq!(settings_command("teleporter"), None);
    }

    #[test]
    fn every_panel_the_shell_can_ask_for_has_a_way_to_open_it() {
        // A settings module is the floor: whatever else a panel names, there is always one
        // more thing to fall back to, or the row works on one machine and vanishes on the next.
        for panel in PANELS {
            assert!(!panel.name.is_empty());
            assert!(
                panel.module.starts_with("kcm_"),
                "{} is not a settings module",
                panel.module
            );
            if let Some(applet) = panel.applet {
                assert!(
                    applet.starts_with("org.kde.plasma."),
                    "{applet} is not an applet id"
                );
            }
            if let Some(tool) = panel.tool {
                assert!(tool.starts_with('/'), "{tool} is not a program path");
            }
        }
    }

    #[test]
    fn bluetooth_does_not_offer_the_applet_that_needs_a_desktop_shell() {
        // It looks right and lists nothing, because its devices come from a background module
        // a Plasma session starts and this one does not. Found on a machine whose adapter was
        // powered, discovering, and had already found five devices.
        let bluetooth = PANELS
            .iter()
            .find(|p| p.name == "bluetooth")
            .expect("bluetooth is a panel");
        assert_eq!(bluetooth.applet, None);
        assert!(
            bluetooth.tool.is_some(),
            "then it needs something standalone"
        );
    }

    #[test]
    fn the_applet_is_preferred_to_the_settings_module() {
        // The whole point of the change: the task bar's own control, not a configuration
        // dialog. If both exist on this machine, the command must be the applet.
        let Some(applet) = PANELS[0].applet else {
            return; // this panel does not offer one
        };
        if !Path::new(APPLET_RUNNER).exists() || !applet_installed(applet) {
            return; // not a Plasma machine; nothing to assert
        }
        let command = settings_command("wifi").expect("wifi should be available here");
        assert!(command.starts_with(APPLET_RUNNER), "got {command}");
    }
}
