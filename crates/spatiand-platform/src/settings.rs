//! Which of the desktop's own settings panels this system actually has.
//!
//! A HUD row that opens nothing is worse than a missing row: the wearer concludes the whole
//! HUD is broken rather than that one panel is absent. So availability is a fact to be
//! checked — and checked *per panel*, because "a desktop is installed" and "this particular
//! module is installed" are different questions. Bluetooth is the case that forces the
//! distinction: `kcmshell6` comes with Plasma, `kcm_bluetooth` comes with bluedevil, and a
//! machine can easily have the first without the second.
//!
//! Portable by construction. The module names are KDE's, but the search path is the ordinary
//! freedesktop one, so on a system with neither this simply answers "no" and the rows are
//! hidden — which is the whole contract.

use std::path::{Path, PathBuf};

/// Programs that can open a settings module as its own window.
const RUNNERS: [&str; 2] = ["/usr/bin/kcmshell6", "/usr/bin/systemsettings"];

/// Is there anything here that can open a settings panel at all?
pub fn has_desktop_settings() -> bool {
    RUNNERS.iter().any(|p| Path::new(p).exists())
}

/// Is this particular settings module installed?
///
/// Plasma 6 ships each module's own `.desktop` alongside ordinary applications, so this is the
/// same search path the launcher already walks. `kservices6` is checked too: that is where
/// Plasma 5 put them, and some modules still land there.
pub fn panel_available(module: &str) -> bool {
    has_desktop_settings()
        && candidates_in(&crate::desktop_entry::search_directories(), module)
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
        assert!(paths.contains(&PathBuf::from("/usr/share/applications/kcm_bluetooth.desktop")));
        assert!(paths.contains(&PathBuf::from("/usr/share/kservices6/kcm_bluetooth.desktop")));
    }

    #[test]
    fn a_module_nobody_ships_is_not_offered() {
        assert!(!panel_available("kcm_no_such_module_exists_anywhere"));
    }
}
