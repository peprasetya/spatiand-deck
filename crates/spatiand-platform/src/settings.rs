//! Which of the desktop's own settings panels this system has, and how to open them.
//!
//! A HUD row that opens nothing is worse than a missing row: the wearer concludes the whole
//! HUD is broken rather than that one panel is absent. So availability is a fact to be
//! checked — and checked *per panel*, because "a desktop is installed" and "this particular
//! panel is installed" are different questions. Bluetooth is the case that forces the
//! distinction: the settings runner comes with Plasma, the Bluetooth pieces come with
//! bluedevil, and a machine can easily have the first without the second.
//!
//! Two ways to open each, preferred in order:
//!
//! * **The panel applet, in its own window** (`plasmawindowed`). The same Wi-Fi and Bluetooth
//!   controls as the desktop's task bar — a list you pick from, sized like a menu. This is
//!   what someone means when they say they want the one from the tray.
//! * **The settings module** (`kcmshell6`). The full configuration dialog. A fallback, not a
//!   preference: it is a window full of options for a task that is usually "join this one",
//!   and on a headset that is a lot of surface to read through a laser pointer.
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

/// What the shell can ask for. Deliberately not KDE's names — those are this module's business.
const PANELS: [(&str, &str, &str); 2] = [
    // (what the shell asks for, the applet, the settings module)
    (
        "wifi",
        "org.kde.plasma.networkmanagement",
        "kcm_networkmanagement",
    ),
    ("bluetooth", "org.kde.plasma.bluetooth", "kcm_bluetooth"),
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
    let (_, applet, module) = PANELS.iter().find(|(name, _, _)| *name == panel)?;
    if Path::new(APPLET_RUNNER).exists() && applet_installed(applet) {
        return Some(format!("{APPLET_RUNNER} {applet}"));
    }
    let runner = KCM_RUNNERS.iter().find(|p| Path::new(p).exists())?;
    if !module_installed(module) {
        return None;
    }
    Some(format!("{runner} {module}"))
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
    fn every_panel_the_shell_can_ask_for_has_both_a_way_to_open_it() {
        // A panel with an applet but no module, or the reverse, would work on one machine and
        // silently vanish on the next.
        for (name, applet, module) in PANELS {
            assert!(!name.is_empty());
            assert!(
                applet.starts_with("org.kde.plasma."),
                "{applet} is not an applet id"
            );
            assert!(
                module.starts_with("kcm_"),
                "{module} is not a settings module"
            );
        }
    }

    #[test]
    fn the_applet_is_preferred_to_the_settings_module() {
        // The whole point of the change: the task bar's own control, not a configuration
        // dialog. If both exist on this machine, the command must be the applet.
        if !Path::new(APPLET_RUNNER).exists() || !applet_installed(PANELS[0].1) {
            return; // not a Plasma machine; nothing to assert
        }
        let command = settings_command("wifi").expect("wifi should be available here");
        assert!(command.starts_with(APPLET_RUNNER), "got {command}");
    }
}
