//! Reading freedesktop `.desktop` files, so the launcher shows what is actually installed.
//!
//! Written by hand rather than pulled from a crate, for one reason that matters: the launcher
//! needs a *small, correct* subset — name, command, icon, and the several ways an entry can
//! ask not to be shown — and the failure mode of getting the last part wrong is a home screen
//! full of `.desktop` files for MIME handlers and screen readers. That is a spec-reading
//! problem, not a parsing problem, and vendoring a parser would not have helped.
//!
//! Portable by construction: this is the same mechanism on SteamOS, Fedora and Debian. Nothing
//! here knows what a Steam Deck is.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One application worth showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    pub name: String,
    /// `Exec`, with field codes removed.
    pub exec: String,
    /// The raw `Icon` value: either a name to look up in a theme, or an absolute path.
    pub icon: Option<String>,
    /// The freedesktop `Categories` list, semicolon-separated in the file.
    pub categories: Vec<String>,
    /// Where it came from, for logs.
    pub path: PathBuf,
}

/// The standard search path, most specific first.
///
/// `XDG_DATA_HOME` before the system directories, so a user's own entry shadows a packaged one
/// of the same id — which is what the spec requires and what makes a locally installed app
/// appear instead of a duplicate.
pub fn search_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(home).join("applications"));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for dir in system.split(':').filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir).join("applications"));
    }
    dirs
}

/// Scan the standard directories.
///
/// Entries are de-duplicated by file name, keeping the first — that is the shadowing rule
/// above. Sorted by name so the launcher grid is stable between runs; an app list that
/// reshuffles itself every launch makes muscle memory impossible.
pub fn scan() -> Vec<DesktopEntry> {
    scan_in(&search_directories())
}

pub fn scan_in(dirs: &[PathBuf]) -> Vec<DesktopEntry> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<DesktopEntry> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let id = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !seen.insert(id) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(parsed) = parse(&text, &path) {
                out.push(parsed);
            }
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Parse one file's contents, or `None` if it should not appear in a launcher.
pub fn parse(text: &str, path: &Path) -> Option<DesktopEntry> {
    let mut in_desktop_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut hidden = false;
    let mut kind = None;
    let mut categories = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // Only the main group describes the application. Action groups further down carry
            // their own Name and Exec, and reading those would launch the wrong thing — the
            // "New Window" action instead of the app.
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            // Localised keys look like `Name[de]`. Take the unlocalised one and ignore the
            // rest rather than showing whichever translation happened to come last in the file.
            "Name" => name = Some(value.to_string()),
            "Exec" => exec = Some(value.to_string()),
            "Icon" => icon = Some(value.to_string()),
            "Type" => kind = Some(value.to_string()),
            "Categories" => {
                categories = value
                    .split(';')
                    .filter(|c| !c.is_empty())
                    .map(|c| c.to_string())
                    .collect()
            }
            "NoDisplay" | "Hidden" => hidden |= value.eq_ignore_ascii_case("true"),
            // An entry that only makes sense inside a terminal has nowhere to run here: there
            // is no terminal in the spatial session, so launching it would appear to do
            // nothing at all.
            "Terminal" => hidden |= value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    if hidden || kind.as_deref() != Some("Application") {
        return None;
    }
    let exec = strip_field_codes(&exec?);
    if exec.is_empty() {
        return None;
    }
    Some(DesktopEntry {
        name: name?,
        exec,
        icon,
        categories,
        path: path.to_path_buf(),
    })
}

/// Remove the `%f`, `%U`, `%i`, `%c`, `%k` placeholders from an `Exec` line, and Flatpak's
/// file-forwarding markers.
///
/// Field codes expand to files and URLs being opened; with nothing to pass, the spec says to
/// drop them. Leaving them in means the app is launched with a literal `%U` as its first
/// argument, which some handle and others refuse to start over.
///
/// Flatpak wraps its forwarded arguments in `@@u … @@` — Google Chrome's entry on this machine
/// reads `… com.google.Chrome @@u %U @@`. Removing only the `%U` leaves `@@u` and `@@` behind
/// as literal arguments, which is not a syntax `flatpak run` accepts. The app never starts and
/// the launcher shows no error, because the failure is inside the child.
pub fn strip_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // `%%` is a literal percent sign.
            Some('%') => out.push('%'),
            Some(_) => {}
            None => {}
        }
    }
    out.split_whitespace()
        // `@@`, `@@u`, `@@f` and friends only ever delimit forwarded files, and there are none.
        .filter(|token| !(token.starts_with("@@") && token.len() <= 3))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The icon file for a window, given the application id it reports over Wayland.
///
/// An app id is not an icon name, and the relationship between them is a convention rather
/// than a rule, so this is a ladder of increasingly expensive guesses:
///
/// 1. the app id as an icon name — `org.kde.dolphin` is both, for most modern KDE apps;
/// 2. the desktop entry whose filename is the app id, and whatever `Icon=` it names — this is
///    the association the freedesktop spec actually endorses;
/// 3. the last dot-separated piece — `org.gnome.TextEditor` becomes `TextEditor` — which
///    catches older applications whose icon is named after the binary.
///
/// The order matters for cost as much as for correctness: step 2 reads every desktop file on
/// the machine, and doing that while a window is opening is a visible hitch. Most windows are
/// answered by step 1 without touching the disk beyond the icon theme.
///
/// `None` is an ordinary answer. A dialog, or anything launched from a terminal, may report an
/// app id that belongs to no installed application.
pub fn icon_for_app(app_id: &str) -> Option<PathBuf> {
    if app_id.is_empty() {
        return None;
    }
    if let Some(path) = crate::icons::resolve(app_id) {
        return Some(path);
    }
    let wanted = app_id.to_lowercase();
    let entry = scan().into_iter().find(|e| {
        e.path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase() == wanted)
            .unwrap_or(false)
    });
    if let Some(icon) = entry.and_then(|e| e.icon) {
        if let Some(path) = crate::icons::resolve(&icon) {
            return Some(path);
        }
    }
    app_id
        .rsplit('.')
        .next()
        .filter(|tail| *tail != app_id)
        .and_then(crate::icons::resolve)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> PathBuf {
        PathBuf::from("/tmp/test.desktop")
    }

    #[test]
    fn parses_an_ordinary_entry() {
        let text = "[Desktop Entry]\n\
                    Type=Application\n\
                    Name=Firefox\n\
                    Exec=/usr/lib/firefox/firefox %u\n\
                    Icon=firefox\n";
        let e = parse(text, &p()).expect("should parse");
        assert_eq!(e.name, "Firefox");
        assert_eq!(e.exec, "/usr/lib/firefox/firefox");
        assert_eq!(e.icon.as_deref(), Some("firefox"));
    }

    #[test]
    fn categories_are_split_on_semicolons() {
        // The trailing semicolon is idiomatic in these files and must not become an empty
        // category, which would show up in a launcher as a nameless group.
        let text = "[Desktop Entry]\nType=Application\nName=A\nExec=a\n\
                    Categories=Network;WebBrowser;\n";
        let e = parse(text, &p()).unwrap();
        assert_eq!(e.categories, vec!["Network", "WebBrowser"]);
    }

    #[test]
    fn an_entry_with_no_categories_is_still_valid() {
        let text = "[Desktop Entry]\nType=Application\nName=A\nExec=a\n";
        assert!(parse(text, &p()).unwrap().categories.is_empty());
    }

    #[test]
    fn field_codes_are_removed_but_literal_percents_survive() {
        assert_eq!(strip_field_codes("app %U"), "app");
        assert_eq!(strip_field_codes("app %f --flag"), "app --flag");
        assert_eq!(strip_field_codes("app -c 50%%"), "app -c 50%");
        assert_eq!(strip_field_codes("app %i %c %k"), "app");
        // A trailing bare % must not panic or swallow the command.
        assert_eq!(strip_field_codes("app %"), "app");
    }

    #[test]
    fn flatpak_file_forwarding_markers_are_removed() {
        // Verbatim from this machine's Google Chrome entry. Dropping only the %U leaves `@@u`
        // and `@@` as arguments, which `flatpak run` rejects -- and the launcher cannot see
        // that, because the failure happens inside the child after a successful spawn.
        let exec = "/usr/bin/flatpak run --branch=stable --arch=x86_64 \
--command=/app/bin/chrome --file-forwarding com.google.Chrome @@u %U @@";
        let cleaned = strip_field_codes(exec);
        assert!(!cleaned.contains("@@"), "markers survived: {cleaned}");
        assert!(cleaned.ends_with("com.google.Chrome"), "got {cleaned}");
    }

    #[test]
    fn an_argument_that_merely_starts_with_at_signs_is_kept() {
        // The markers are short and standalone; a real argument is not.
        assert_eq!(strip_field_codes("app @@userdata"), "app @@userdata");
    }

    #[test]
    fn entries_that_ask_not_to_be_shown_are_not_shown() {
        // This is the difference between a launcher and a directory listing. Without it the
        // home screen fills with MIME handlers, URL protocol stubs and settings modules.
        for flag in ["NoDisplay=true", "Hidden=true", "NoDisplay=True"] {
            let text = format!(
                "[Desktop Entry]\nType=Application\nName=Hidden\nExec=/bin/true\n{flag}\n"
            );
            assert!(parse(&text, &p()).is_none(), "{flag} should hide the entry");
        }
    }

    #[test]
    fn terminal_applications_are_left_out() {
        // There is no terminal in a spatial session, so a terminal app launches into nothing
        // and looks like a broken icon.
        let text = "[Desktop Entry]\nType=Application\nName=htop\nExec=htop\nTerminal=true\n";
        assert!(parse(text, &p()).is_none());
    }

    #[test]
    fn non_applications_are_left_out() {
        let text = "[Desktop Entry]\nType=Link\nName=Somewhere\nURL=https://example.invalid\n";
        assert!(parse(text, &p()).is_none());
    }

    #[test]
    fn an_entry_with_no_exec_is_not_launchable() {
        let text = "[Desktop Entry]\nType=Application\nName=Broken\n";
        assert!(parse(text, &p()).is_none());
    }

    #[test]
    fn only_the_main_group_is_read() {
        // Action groups carry their own Name and Exec. Reading them gives you the app's
        // "New Private Window" command under the app's own name, which launches the wrong
        // thing in a way nobody would suspect the parser for.
        let text = "[Desktop Entry]\n\
                    Type=Application\n\
                    Name=Firefox\n\
                    Exec=/usr/bin/firefox\n\
                    Actions=new-window;\n\
                    \n\
                    [Desktop Action new-window]\n\
                    Name=Open a New Window\n\
                    Exec=/usr/bin/firefox --new-window\n";
        let e = parse(text, &p()).expect("should parse");
        assert_eq!(e.name, "Firefox");
        assert_eq!(e.exec, "/usr/bin/firefox");
    }

    #[test]
    fn localised_names_do_not_override_the_plain_one() {
        // `Name[de]` sorts after `Name`, so a naive parser ends up showing German to everyone.
        let text = "[Desktop Entry]\n\
                    Type=Application\n\
                    Name=Files\n\
                    Name[de]=Dateien\n\
                    Name[fr]=Fichiers\n\
                    Exec=/usr/bin/files\n";
        assert_eq!(parse(text, &p()).unwrap().name, "Files");
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let text = "# a comment\n\n[Desktop Entry]\n# another\nType=Application\nName=A\nExec=a\n";
        assert!(parse(text, &p()).is_some());
    }

    #[test]
    fn scanning_a_missing_directory_is_empty_not_an_error() {
        // Half the XDG_DATA_DIRS entries do not exist on any given machine.
        let dirs = vec![PathBuf::from("/nonexistent-a"), PathBuf::from("/nonexistent-b")];
        assert!(scan_in(&dirs).is_empty());
    }

    #[test]
    fn the_search_path_puts_the_users_own_entries_first() {
        let dirs = search_directories();
        assert!(!dirs.is_empty());
        assert!(
            dirs[0].ends_with("applications"),
            "first entry should be a user applications dir, got {:?}",
            dirs[0]
        );
    }
}
