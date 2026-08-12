//! Starting an application from the launcher.
//!
//! Three things have to be true or the spatial session breaks in ways that are hard to trace
//! back here:
//!
//! * The child must connect to **our** Wayland display, not to whatever the environment
//!   inherited from the session that started us.
//! * The child must be fully detached. A launcher that waits on its children stalls the render
//!   loop; one that leaves them as zombies accumulates them for the life of the session.
//! * A command that fails to start must say so, rather than leaving the wearer pressing A at
//!   an icon that does nothing.

use std::process::{Command, Stdio};

/// Split an `Exec` line into a program and arguments.
///
/// Handles the quoting the desktop-entry spec actually uses — double quotes around arguments
/// containing spaces — and nothing more exotic. A full shell parse would be wrong here: these
/// are not shell commands, and treating them as such is how a file named `; rm` becomes
/// interesting.
pub fn split_command(exec: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    let mut started = false;

    for c in exec.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quotes => escaped = true,
            '"' => {
                in_quotes = !in_quotes;
                // A pair of quotes with nothing between them is still an argument.
                started = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if started || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started || !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Programs that default to X11 and need telling otherwise.
///
/// Chromium and everything built on it — Chrome, Edge, Brave, Vivaldi, Opera, and every
/// Electron application — pick X11 unless `--ozone-platform=wayland` says otherwise. With
/// `DISPLAY` removed, as it is here, that means they try to reach an X server that is not
/// running and exit immediately. From the launcher it looks exactly like the icon doing
/// nothing, which is the least debuggable failure available.
const OZONE_FAMILY: &[&str] = &[
    "chrome", "chromium", "brave", "vivaldi", "opera", "msedge", "electron", "code", "slack",
    "discord", "spotify", "signal",
];

/// Extra arguments a program needs to run under Wayland.
///
/// Returned rather than applied so it can be tested without launching anything.
pub fn wayland_arguments(program: &str, existing: &[String]) -> Vec<String> {
    // The whole command line, not just the program. Flatpak apps are launched as
    // `/usr/bin/flatpak run … com.google.Chrome`, so the program name says "flatpak" and the
    // only place the browser is mentioned is in the arguments. Checking the program alone
    // meant every Flatpak-packaged browser -- which on this machine is all of them -- silently
    // missed the flag it needs.
    let haystack = std::iter::once(program.to_string())
        .chain(existing.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if !OZONE_FAMILY.iter().any(|f| haystack.contains(f)) {
        return Vec::new();
    }
    // Never argue with a desktop file that already made a choice: some ship
    // `--ozone-platform-hint=auto`, and adding a second, contradictory flag is worse than
    // adding none.
    if existing.iter().any(|a| a.contains("ozone-platform")) {
        return Vec::new();
    }
    vec![
        "--ozone-platform=wayland".to_string(),
        // Older builds need the feature switched on as well as selected.
        "--enable-features=UseOzonePlatform,WaylandWindowDecorations".to_string(),
    ]
}

/// Launch an application into the spatial session.
///
/// `wayland_display` is the socket name Spatiand is listening on; it is set in the child's
/// environment so the new window arrives here rather than on a desktop underneath.
pub fn launch(exec: &str, wayland_display: &str) -> Result<u32, String> {
    let parts = split_command(exec);
    let (program, args) = parts
        .split_first()
        .ok_or_else(|| format!("empty command: {exec:?}"))?;
    let mut args = args.to_vec();
    args.extend(wayland_arguments(program, &args));

    let child = Command::new(program)
        .args(&args)
        .env("WAYLAND_DISPLAY", wayland_display)
        // Some toolkits prefer X11 when DISPLAY is set, and would then try to reach an X
        // server that is not running. Removing it makes the Wayland path the only option.
        .env_remove("DISPLAY")
        .env("XDG_SESSION_TYPE", "wayland")
        .stdin(Stdio::null())
        // Inheriting our stdout would interleave the child's chatter with the compositor's
        // frame logs, which are the only diagnostics available in a session with no terminal.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))?;

    let pid = child.id();
    // Deliberately dropped rather than waited on. `Child`'s own drop does not reap, so the
    // process is left to init — which is what we want for something that should outlive the
    // launcher, and avoids a wait() that would block the render loop.
    std::mem::forget(child);
    log::info!("launched {program} (pid {pid})");
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_plain_command() {
        assert_eq!(
            split_command("/usr/bin/firefox --new-window"),
            vec!["/usr/bin/firefox", "--new-window"]
        );
    }

    #[test]
    fn keeps_quoted_arguments_together() {
        // Paths with spaces are the common case, and splitting them turns one argument into
        // two files that do not exist.
        assert_eq!(
            split_command(r#"/usr/bin/app "/home/deck/My Documents/a.txt" --flag"#),
            vec!["/usr/bin/app", "/home/deck/My Documents/a.txt", "--flag"]
        );
    }

    #[test]
    fn handles_escaped_quotes_inside_quotes() {
        assert_eq!(
            split_command(r#"app "a \"b\" c""#),
            vec!["app", r#"a "b" c"#]
        );
    }

    #[test]
    fn collapses_runs_of_whitespace() {
        assert_eq!(split_command("app   -x    -y"), vec!["app", "-x", "-y"]);
        assert_eq!(split_command("  app  "), vec!["app"]);
    }

    #[test]
    fn an_empty_quoted_argument_is_still_an_argument() {
        assert_eq!(split_command(r#"app "" -x"#), vec!["app", "", "-x"]);
    }

    #[test]
    fn an_empty_command_yields_nothing_rather_than_a_blank_program() {
        assert!(split_command("").is_empty());
        assert!(split_command("   ").is_empty());
        assert!(launch("", "wayland-1").is_err());
    }

    #[test]
    fn chromium_family_programs_are_told_to_use_wayland() {
        // Without this they try X11, find no server because DISPLAY is removed, and exit --
        // which from the launcher is indistinguishable from the icon doing nothing.
        for program in [
            "/usr/bin/google-chrome-stable",
            "/opt/brave/brave",
            "chromium",
            "/usr/share/code/code",
        ] {
            let extra = wayland_arguments(program, &[]);
            assert!(
                extra.iter().any(|a| a == "--ozone-platform=wayland"),
                "{program} was not given the wayland flag"
            );
        }
    }

    #[test]
    fn a_flatpak_packaged_browser_is_recognised_from_its_app_id() {
        // The program is /usr/bin/flatpak; the only mention of Chrome is an argument.
        let args: Vec<String> = "run --branch=stable --command=/app/bin/chrome com.google.Chrome"
            .split_whitespace()
            .map(String::from)
            .collect();
        let extra = wayland_arguments("/usr/bin/flatpak", &args);
        assert!(extra.iter().any(|a| a == "--ozone-platform=wayland"), "got {extra:?}");
    }

    #[test]
    fn ordinary_programs_are_left_alone() {
        // A flag Chromium understands is a fatal unknown-argument error to most other things.
        for program in ["/usr/bin/kate", "dolphin", "/usr/bin/firefox"] {
            assert!(wayland_arguments(program, &[]).is_empty(), "{program} was modified");
        }
        // And a Flatpak of something that is not Chromium-based must stay untouched too.
        let args = vec!["run".to_string(), "org.videolan.VLC".to_string()];
        assert!(wayland_arguments("/usr/bin/flatpak", &args).is_empty());
    }

    #[test]
    fn a_desktop_file_that_already_chose_is_not_overridden() {
        // Some ship --ozone-platform-hint=auto. Adding a second, contradictory flag is worse
        // than adding none.
        let existing = vec!["--ozone-platform-hint=auto".to_string()];
        assert!(wayland_arguments("google-chrome", &existing).is_empty());
    }

    #[test]
    fn a_missing_program_is_reported_not_swallowed() {
        // The wearer's symptom is "pressing A does nothing", and without an error there is
        // nothing in the log to connect that to a bad Exec line.
        let result = launch("/nonexistent/program/xyzzy", "wayland-1");
        assert!(result.is_err(), "should have failed to start");
        assert!(result.unwrap_err().contains("xyzzy"));
    }

    #[test]
    fn shell_metacharacters_are_not_interpreted() {
        // These are not shell commands. Passing them through a shell would make a filename
        // into an instruction.
        let parts = split_command("app; rm -rf /");
        assert_eq!(parts[0], "app;", "must stay one literal argument");
    }
}
