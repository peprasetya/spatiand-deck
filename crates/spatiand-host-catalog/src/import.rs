//! Turning the machine's installed applications into catalogue entries.
//!
//! Every desktop environment already keeps a list of what is installed, as `.desktop` files;
//! offering those is how "add Chrome" becomes one choice instead of typing
//! `/usr/bin/google-chrome` into a field. The parsing is `spatiand-platform`'s, shared with the
//! headset's own launcher, so a quirk fixed there is fixed here.

use std::collections::HashSet;

use spatiand_platform::DesktopEntry;
use spatiand_stream::{App, Catalog};

/// One installed application that could be added.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub app: App,
    pub categories: Vec<String>,
    /// Already in the catalogue — by id, or because it runs the same program.
    pub already: bool,
}

/// Everything installed, as entries ready to add, alphabetically.
pub fn candidates(entries: &[DesktopEntry], catalog: &Catalog) -> Vec<Candidate> {
    let taken_ids: HashSet<&str> = catalog.apps.iter().map(|a| a.id.as_str()).collect();
    let taken_programs: HashSet<std::path::PathBuf> = catalog
        .apps
        .iter()
        .filter_map(|a| crate::icons::resolve_program(&a.exec))
        .collect();
    let mut seen = HashSet::new();
    let mut out: Vec<Candidate> = entries
        .iter()
        .filter_map(|entry| {
            let app = from_entry(entry)?;
            // One entry per id: the search path puts the user's own entries first, and those
            // are the ones that should win.
            if !seen.insert(app.id.clone()) {
                return None;
            }
            let already = taken_ids.contains(app.id.as_str())
                || crate::icons::resolve_program(&app.exec)
                    .is_some_and(|p| taken_programs.contains(&p));
            Some(Candidate {
                app,
                categories: entry.categories.clone(),
                already,
            })
        })
        .collect();
    out.sort_by_key(|c| c.app.name.to_lowercase());
    out
}

/// A catalogue entry for a desktop entry, or `None` for one with nothing to run.
pub fn from_entry(entry: &DesktopEntry) -> Option<App> {
    let command = spatiand_platform::launch::split_command(&entry.exec);
    let (program, args) = command.split_first()?;
    let id = id_for(entry);
    let mut app = App::new(id, entry.name.clone(), program.clone());
    app.args = args.to_vec();
    app.icon = entry.icon.clone();
    Some(app)
}

/// An id from the desktop file's name: `google-chrome.desktop` is `google-chrome`,
/// `org.kde.kate.desktop` is `org.kde.kate`. Stable across renames of the display name, and
/// what the headset keys a controller layout on.
fn id_for(entry: &DesktopEntry) -> String {
    let stem = entry
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| entry.name.clone());
    make_id(&stem)
}

/// Letters, digits, dots and dashes, lower case. What an id may be.
pub fn make_id(text: &str) -> String {
    let mut id: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while id.contains("--") {
        id = id.replace("--", "-");
    }
    let id = id.trim_matches('-').to_string();
    if id.is_empty() {
        "app".into()
    } else {
        id
    }
}

/// An id not already in the catalogue, made from `base`.
pub fn unique_id(base: &str, catalog: &Catalog) -> String {
    let base = make_id(base);
    if catalog.get(&base).is_none() && base != crate::SETTINGS_APP_ID {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| catalog.get(id).is_none())
        .expect("some number is free")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(file: &str, name: &str, exec: &str) -> DesktopEntry {
        DesktopEntry {
            name: name.into(),
            exec: exec.into(),
            icon: Some("some-icon".into()),
            categories: vec!["Network".into()],
            path: PathBuf::from(format!("/usr/share/applications/{file}")),
        }
    }

    #[test]
    fn a_desktop_entry_becomes_an_entry_with_its_arguments_split_out() {
        let app = from_entry(&entry(
            "google-chrome.desktop",
            "Google Chrome",
            "/usr/bin/google-chrome-stable --incognito",
        ))
        .unwrap();
        assert_eq!(app.id, "google-chrome");
        assert_eq!(app.exec, "/usr/bin/google-chrome-stable");
        assert_eq!(app.args, vec!["--incognito"]);
        assert_eq!(app.icon.as_deref(), Some("some-icon"));
    }

    #[test]
    fn candidates_are_sorted_deduplicated_and_marked_when_already_added() {
        let entries = vec![
            entry("zed.desktop", "Zed", "zed"),
            entry("kate.desktop", "Kate", "kate"),
            entry("kate.desktop", "Kate (system)", "kate"),
        ];
        let catalog = Catalog {
            apps: vec![App::new("kate", "Kate", "kate")],
        };
        let found = candidates(&entries, &catalog);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].app.name, "Kate");
        assert!(found[0].already);
        assert!(!found[1].already);
    }

    #[test]
    fn ids_are_tidy_and_never_collide() {
        assert_eq!(make_id("My App (beta)!"), "my-app-beta");
        let catalog = Catalog {
            apps: vec![App::new("chrome", "Chrome", "x"), App::new("chrome-2", "C", "x")],
        };
        assert_eq!(unique_id("chrome", &catalog), "chrome-3");
        assert_eq!(unique_id("settings", &Catalog::default()), "settings-2");
    }
}
