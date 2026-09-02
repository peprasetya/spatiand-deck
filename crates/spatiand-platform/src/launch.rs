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

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The most this file is allowed to grow to before it is started again, in bytes.
///
/// One misbehaving application logging every frame would otherwise fill a partition, and the
/// partition it would fill is the one holding the session.
const APP_LOG_LIMIT: u64 = 4 * 1024 * 1024;

/// Where a launched application's own complaints go.
///
/// Not `/dev/null`, which is where they used to go. An application that fails in here fails
/// silently and invisibly: there is no terminal to have shown the error in, and the
/// compositor's own log is a frame-rate trace that a Qt backtrace would bury. A KDE settings
/// panel reported "could not prompt the user for which application to start" and there was
/// nothing at all behind it -- not because nothing was written, but because we were throwing
/// it away.
///
/// Its own file rather than ours for that second reason: interleaving a chatty toolkit with
/// the render loop's output costs both of them.
fn app_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".local/share");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("spatiand-apps.log"))
}

/// Open the application log, ready to be handed to a child as its output.
///
/// Returns `None` rather than failing the launch. Losing the diagnostics is a worse session;
/// refusing to start the application because we could not open a log file would be a broken
/// one, and the wearer did not ask for a log.
fn open_app_log(program: &str) -> Option<std::fs::File> {
    let path = app_log_path()?;
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > APP_LOG_LIMIT {
        let _ = std::fs::remove_file(&path);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    // A header per launch, so a burst of warnings can be attributed to the thing that made
    // them. Without it the file is one undifferentiated stream from every app of the session.
    let _ = writeln!(file, "\n=== {program} ===");
    let _ = file.flush();
    Some(file)
}

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
///
/// `extra` is set alongside it, and is how a caller says something to the child that only the
/// child's own libraries will read — which is how an app's audio is told which window it
/// belongs to, without the app knowing anything about it. Inherited by grandchildren, which
/// is the whole reason it is the environment rather than an argument: a browser plays its
/// sound from a process it forks itself.
pub fn launch(
    exec: &str,
    wayland_display: &str,
    extra: &[(String, String)],
) -> Result<u32, String> {
    let parts = split_command(exec);
    let (program, args) = parts
        .split_first()
        .ok_or_else(|| format!("empty command: {exec:?}"))?;
    let mut args = args.to_vec();
    args.extend(wayland_arguments(program, &args));

    let log = open_app_log(program);
    let (out, err) = match log
        .as_ref()
        .and_then(|f| Some((f.try_clone().ok()?, f.try_clone().ok()?)))
    {
        Some((a, b)) => (Stdio::from(a), Stdio::from(b)),
        None => (Stdio::null(), Stdio::null()),
    };
    let mut command = Command::new(program);
    for (key, value) in extra {
        command.env(key, value);
    }
    let child = command
        .args(&args)
        .env("WAYLAND_DISPLAY", wayland_display)
        // Some toolkits prefer X11 when DISPLAY is set, and would then try to reach an X
        // server that is not running. Removing it makes the Wayland path the only option.
        .env_remove("DISPLAY")
        .env("XDG_SESSION_TYPE", "wayland")
        .stdin(Stdio::null())
        // Into the application log, not ours and not /dev/null. See `app_log_path`.
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))?;

    let pid = child.id();
    prefer_as_oom_victim(pid);
    // Deliberately dropped rather than waited on. `Child`'s own drop does not reap, so the
    // process is left to init — which is what we want for something that should outlive the
    // launcher, and avoids a wait() that would block the render loop.
    std::mem::forget(child);
    log::info!("launched {program} (pid {pid})");
    Ok(pid)
}

/// How much more willing the kernel should be to kill a launched application than the
/// compositor that launched it.
///
/// The scale runs -1000..1000 and is added to a badness score that is already roughly
/// proportional to how much memory a process is using. A browser is the heaviest thing most
/// people will run in here — several processes, video decode, a GPU cache — so it is already
/// the natural candidate; this makes it a decisive one.
const OOM_PREFERENCE: i32 = 300;

/// Ask the kernel to kill this process before it kills us.
///
/// The compositor and its clients share one machine, and under real memory pressure the OOM
/// killer picks by badness score alone. It has no idea that killing the browser costs a tab
/// while killing the compositor costs the whole session — every window, on every desktop, at
/// once. Left alone that is a coin toss, and the wrong side of it is indistinguishable from a
/// crash.
///
/// **Raising** another process's score is unprivileged; lowering it needs `CAP_SYS_RESOURCE`,
/// which a session started by SDDM as an ordinary user does not have. So the compositor cannot
/// protect itself directly, and this — pushing everything it launches up instead — is the one
/// version of this that works without root.
///
/// Best-effort by design. A failure here means the ordinary kernel behaviour, which is what
/// would have happened anyway, so it is a debug line rather than something a caller handles.
fn prefer_as_oom_victim(pid: u32) {
    let path = format!("/proc/{pid}/oom_score_adj");
    match std::fs::write(&path, OOM_PREFERENCE.to_string()) {
        Ok(()) => log::debug!("pid {pid} set to oom_score_adj {OOM_PREFERENCE}"),
        // Racing a program that exited immediately is the common case and not interesting.
        Err(e) => log::debug!("could not set oom_score_adj for pid {pid}: {e}"),
    }
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
        assert!(launch("", "wayland-1", &[]).is_err());
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
        assert!(
            extra.iter().any(|a| a == "--ozone-platform=wayland"),
            "got {extra:?}"
        );
    }

    #[test]
    fn ordinary_programs_are_left_alone() {
        // A flag Chromium understands is a fatal unknown-argument error to most other things.
        for program in ["/usr/bin/kate", "dolphin", "/usr/bin/firefox"] {
            assert!(
                wayland_arguments(program, &[]).is_empty(),
                "{program} was modified"
            );
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
        let result = launch("/nonexistent/program/xyzzy", "wayland-1", &[]);
        assert!(result.is_err(), "should have failed to start");
        assert!(result.unwrap_err().contains("xyzzy"));
    }

    #[test]
    fn what_a_launched_application_says_is_kept_rather_than_discarded() {
        // The point of the file. This used to go to /dev/null, which is why a settings panel
        // could report an internal error with nothing anywhere to say what it was.
        let Some(path) = app_log_path() else {
            return; // no HOME, which is not a case worth failing a test over
        };
        let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let marker = "spatiand-launch-test-marker";
        launch(
            &format!("/bin/sh -c \"echo {marker} >&2\""),
            "wayland-test",
            &[],
        )
        .expect("sh should start");
        // The child writes and exits immediately, but "immediately" is not "before this line".
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > before {
                break;
            }
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            text.contains(marker),
            "what the child wrote to stderr should reach {}",
            path.display()
        );
    }

    #[test]
    fn a_launched_application_is_offered_to_the_oom_killer_first() {
        // The whole point: under memory pressure the kernel picks by badness score, and it has
        // no idea that killing a browser costs a tab while killing the compositor costs every
        // window on every desktop at once.
        let pid = launch("/bin/sleep 2", "wayland-test", &[]).expect("sleep should start");
        let adjusted = std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))
            .expect("the child should still be alive to read");
        assert_eq!(
            adjusted.trim().parse::<i32>().unwrap(),
            OOM_PREFERENCE,
            "a launched app must be a likelier victim than its compositor"
        );
        // Left to exit on its own -- it is a two-second sleep, and reaping it here would mean
        // the wait() this module deliberately never does.
    }

    #[test]
    fn shell_metacharacters_are_not_interpreted() {
        // These are not shell commands. Passing them through a shell would make a filename
        // into an instruction.
        let parts = split_command("app; rm -rf /");
        assert_eq!(parts[0], "app;", "must stay one literal argument");
    }
}
