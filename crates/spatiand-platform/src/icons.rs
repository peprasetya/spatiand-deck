//! Finding the file behind a desktop entry's `Icon=` value.
//!
//! An `Icon=` key is usually a *name*, not a path — `google-chrome`, `org.kde.dolphin` — and
//! turning it into a file means walking the icon-theme directories the way every toolkit does.
//! The freedesktop icon theme spec describes an elaborate inheritance and cache mechanism;
//! this implements the part that matters and skips the rest, because a launcher that shows the
//! wrong-sized icon is fine and one that shows no icon is not.
//!
//! On this machine the format split is stark: KDE ships **19,978 SVGs and 176 PNGs**. Any
//! approach that only reads PNG produces a launcher with almost no icons on it, which is why
//! scalable is searched first and why the caller needs an SVG rasteriser.
//!
//! Portable by construction — the same directories exist on any freedesktop system.

use std::path::{Path, PathBuf};

/// Themes to search, in order.
///
/// The user's configured theme should really be read from `kdeglobals`; searching Breeze first
/// and falling through to hicolor gets the same answer for almost every icon and costs nothing
/// when it does not.
const THEMES: &[&str] = &["breeze", "Breeze_Light", "Adwaita", "hicolor"];

/// Preferred pixel sizes, largest first.
///
/// Largest wins because the icon is going onto a texture and being scaled down: a 48 px icon
/// blown up to 220 px inside a bubble looks like a mistake, whereas 256 px scaled down does
/// not. `scalable` beats all of them.
///
/// **Both naming conventions.** hicolor writes `48x48`; Breeze writes plain `32`. Searching
/// only for `NxN` finds every application icon on this machine and none of the *category*
/// icons, because those live exclusively in Breeze — which presented as the launcher's group
/// bubbles all falling back to a letter while the app bubbles were fine.
const SIZES: &[&str] = &[
    "scalable", "512x512", "512", "256x256", "256", "128x128", "128", "96x96", "96", "64x64", "64",
    "48x48", "48", "32x32", "32", "24", "22",
];

fn icon_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(&home).join(".local/share/icons"));
        roots.push(PathBuf::from(&home).join(".icons"));
    }
    for dir in std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
        .split(':')
        .filter(|d| !d.is_empty())
    {
        roots.push(PathBuf::from(dir).join("icons"));
    }
    roots
}

/// Resolve an `Icon=` value to a file on disk.
///
/// Returns `None` when nothing matches, which is normal and not an error — plenty of entries
/// name an icon that is not installed. The caller falls back to drawing the app's initial.
pub fn resolve(icon: &str) -> Option<PathBuf> {
    if icon.is_empty() {
        return None;
    }
    // An absolute path is the easy case, and the spec explicitly allows it.
    let direct = Path::new(icon);
    if direct.is_absolute() {
        return direct.exists().then(|| direct.to_path_buf());
    }

    let roots = icon_roots();
    for theme in THEMES {
        for size in SIZES {
            // Categories vary by theme (`apps`, `devices`, `places`...), so scan the size
            // directory's children rather than guessing which one an icon lives in.
            for root in &roots {
                let base = root.join(theme).join(size);
                if let Some(found) = search_categories(&base, icon) {
                    return Some(found);
                }
                // Breeze inverts the order: <theme>/<category>/<size>/.
                if let Some(found) = search_inverted(&root.join(theme), size, icon) {
                    return Some(found);
                }
            }
        }
    }

    // Last resort: the flat legacy directory. Old applications still ship here only.
    for dir in ["/usr/share/pixmaps", "/usr/local/share/pixmaps"] {
        for ext in ["svg", "png", "xpm"] {
            let candidate = Path::new(dir).join(format!("{icon}.{ext}"));
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn search_categories(base: &Path, icon: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(base).ok()?;
    for entry in entries.flatten() {
        if let Some(found) = matching_file(&entry.path(), icon) {
            return Some(found);
        }
    }
    None
}

fn search_inverted(theme_root: &Path, size: &str, icon: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(theme_root).ok()?;
    for entry in entries.flatten() {
        if let Some(found) = matching_file(&entry.path().join(size), icon) {
            return Some(found);
        }
    }
    None
}

fn matching_file(dir: &Path, icon: &str) -> Option<PathBuf> {
    // SVG before PNG: scalable renders crisply at whatever size the bubble needs.
    for ext in ["svg", "png"] {
        let candidate = dir.join(format!("{icon}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_path_that_exists_is_returned_unchanged() {
        // Desktop entries are allowed to give a full path, and rewriting one into a theme
        // lookup would lose icons that are not in any theme.
        let existing = std::env::current_exe().expect("test binary exists");
        let as_str = existing.to_string_lossy().into_owned();
        assert_eq!(resolve(&as_str), Some(existing));
    }

    #[test]
    fn an_absolute_path_that_does_not_exist_is_not_invented() {
        assert!(resolve("/nonexistent/icon/xyzzy.png").is_none());
    }

    #[test]
    fn an_empty_name_resolves_to_nothing() {
        assert!(resolve("").is_none());
    }

    #[test]
    fn an_unknown_name_resolves_to_nothing_rather_than_panicking() {
        // Most of the search path does not exist on any given machine.
        assert!(resolve("definitely-not-an-installed-icon-9f3a").is_none());
    }

    #[test]
    fn scalable_is_searched_before_any_fixed_size() {
        // The format split on this machine is 19,978 SVGs to 176 PNGs. Searching fixed sizes
        // first would find almost nothing.
        assert_eq!(SIZES[0], "scalable");
    }

    #[test]
    fn larger_fixed_sizes_are_preferred_to_smaller() {
        // A 32 px icon scaled up to fill a bubble looks broken; 256 px scaled down does not.
        let numeric: Vec<u32> = SIZES
            .iter()
            .filter(|s| **s != "scalable")
            .filter_map(|s| {
                s.split_once('x')
                    .map_or_else(|| s.parse().ok(), |(w, _)| w.parse().ok())
            })
            .collect();
        assert!(
            numeric.windows(2).all(|w| w[0] >= w[1]),
            "sizes must not ascend: {numeric:?}"
        );
    }

    #[test]
    fn both_size_naming_conventions_are_searched() {
        // hicolor writes 48x48, Breeze writes 32. Knowing only one finds every application
        // icon and no category icon at all, since those live only in Breeze.
        assert!(SIZES.contains(&"48x48"), "hicolor convention missing");
        assert!(SIZES.contains(&"32"), "breeze convention missing");
    }

    #[test]
    fn svg_is_preferred_to_png_within_a_directory() {
        let dir = std::env::temp_dir().join("spatiand-icon-test");
        let _ = std::fs::create_dir_all(&dir);
        let png = dir.join("thing.png");
        let svg = dir.join("thing.svg");
        std::fs::write(&png, b"x").unwrap();
        std::fs::write(&svg, b"x").unwrap();
        assert_eq!(matching_file(&dir, "thing"), Some(svg));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
