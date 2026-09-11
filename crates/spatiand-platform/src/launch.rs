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
use std::process::{Child, Command, Stdio};

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

/// Tell the session's own services where this compositor is.
///
/// A portal is not a window an application opens for itself: the application asks a service
/// over D-Bus, and that service opens the window. The service is started on demand by
/// systemd, with the *session's* environment -- which, unless someone says otherwise, is the
/// environment of whatever ran before us. It then has no `WAYLAND_DISPLAY`, or one naming a
/// compositor that is no longer running, and cannot put a window anywhere. What that looks
/// like from the wearer's side is a file chooser that never appears: no error, no window, and
/// an application that seems to have ignored the button.
///
/// Every compositor does this at startup; it is not a Spatiand quirk. Failing is not fatal --
/// a machine with no session bus has no portals to tell, and everything else still works.
pub fn publish_session_environment(vars: &[(&str, &str)]) {
    let mut command = Command::new("dbus-update-activation-environment");
    // --systemd as well as the bus: the portal ships a systemd user unit, and the unit's
    // environment is the one that decides where its windows go.
    command.arg("--systemd");
    for (key, value) in vars {
        command.arg(format!("{key}={value}"));
    }
    match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {
            let names: Vec<&str> = vars.iter().map(|(k, _)| *k).collect();
            log::info!("told the session bus about {}", names.join(", "));
        }
        Ok(status) => log::warn!("could not publish the session environment ({status})"),
        Err(e) => log::info!("no dbus-update-activation-environment ({e}); portals may not find us"),
    }
}

/// Take back what [`publish_session_environment`] said, because it has stopped being true.
///
/// The other half of publishing, and it was missing. `WAYLAND_DISPLAY` and `DISPLAY` name
/// sockets belonging to *this* compositor; the moment it exits they name nothing. But the
/// systemd user manager outlives the session -- the same `systemd --user` serves the desktop,
/// the spatial session and game mode one after another -- so an unretracted value sits there
/// pointing at a dead socket for everything started afterwards.
///
/// That is not theoretical. Leaving spatial mode and asking for game mode gave:
///
/// ```text
/// gamescope-session: Failed to connect to wayland socket: wayland-1.
/// gamescope-session.service: Main process exited, code=exited, status=1/FAILURE
/// ```
///
/// `wayland-1` was ours. Gamescope found a `WAYLAND_DISPLAY` in its inherited environment,
/// concluded it should run nested inside that compositor, and failed -- so game mode could
/// not be entered again until a full power cycle, which is what finally restarts the user
/// manager and clears its environment.
///
/// Unset rather than blanked, deliberately. A variable set to the empty string is still set,
/// and a program testing whether it has a Wayland display to connect to will believe it has
/// one -- which is the same fault with a shorter socket name. There is no way to withdraw a
/// variable from the *D-Bus* activation environment, only from systemd's; systemd's is the
/// one a session unit inherits, so it is the one that matters here.
pub fn withdraw_session_environment(names: &[&str]) {
    let mut command = Command::new("systemctl");
    command.arg("--user").arg("unset-environment");
    for name in names {
        command.arg(name);
    }
    match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {
            log::info!("took back {} from the session", names.join(", "))
        }
        Ok(status) => log::warn!("could not withdraw the session environment ({status})"),
        Err(e) => log::warn!("no systemctl ({e}); the session environment still names us"),
    }
}

/// Where an option has to be inserted on a `flatpak run` command line, if this is one.
///
/// A sandbox does not inherit our environment. Everything `extra` carries -- which is how a
/// window's sound is told which window it belongs to -- is simply dropped on the way in, so a
/// Flatpak application plays to the machine's default output while the sink made for its
/// window sits silent beside it. On this machine VLC and Kodi are both Flatpaks, which is to
/// say the two applications the spatial audio was built for were the two it could not reach.
///
/// The position matters: `flatpak run [OPTIONS] APP [ARGS]`, so an option after the
/// application id is an argument to the application instead, which flatpak accepts and the
/// application ignores.
fn flatpak_option_slot(program: &str, args: &[String]) -> Option<usize> {
    if !program.rsplit('/').next().is_some_and(|p| p == "flatpak") {
        return None;
    }
    let run = args.iter().position(|a| a == "run")?;
    Some(run + 1)
}

/// The `--env=` options that carry `extra` across the sandbox boundary.
///
/// Deliberately **not** `WAYLAND_DISPLAY`: flatpak binds our socket into the sandbox under
/// whatever name it likes and sets the variable itself, so forcing ours would name a socket
/// that does not exist in there -- an application that launches and connects to nothing.
fn flatpak_environment(extra: &[(String, String)]) -> Vec<String> {
    extra
        .iter()
        .filter(|(key, _)| key != "WAYLAND_DISPLAY")
        .map(|(key, value)| format!("--env={key}={value}"))
        .collect()
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
    // Before anything else is decided about the command line, because this inserts rather than
    // appends and every position after it would shift.
    if let Some(at) = flatpak_option_slot(program, &args) {
        let options = flatpak_environment(extra);
        if !options.is_empty() {
            log::info!("passing {} variable(s) into the flatpak sandbox", options.len());
            args.splice(at..at, options);
        }
    }

    let log = open_app_log(program);
    let (out, err) = match log
        .as_ref()
        .and_then(|f| Some((f.try_clone().ok()?, f.try_clone().ok()?)))
    {
        Some((a, b)) => (Stdio::from(a), Stdio::from(b)),
        None => (Stdio::null(), Stdio::null()),
    };
    let mut command = Command::new(program);
    // Whatever the session inherited is not what an application launched into *this* session
    // should see. A DISPLAY from an outer session points at another machine's X server, and
    // is exactly the kind of thing that works on a developer's desktop and not on a Deck.
    // `extra` puts back a DISPLAY that means something, if there is one -- see
    // `spatiand::xwayland::client_environment`.
    command.env_remove("DISPLAY");
    for (key, value) in extra {
        command.env(key, value);
    }
    let child = command
        .args(&args)
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("XDG_SESSION_TYPE", "wayland")
        .stdin(Stdio::null())
        // Into the application log, not ours and not /dev/null. See `app_log_path`.
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))?;

    let pid = child.id();
    prefer_as_oom_victim(pid);
    log::info!("launched {program} (pid {pid})");
    // After the OOM adjustment, not before: once reaped the pid is free to be reused, and the
    // write would land on whatever took it.
    reap_when_it_exits(child, program);
    Ok(pid)
}

/// Wait for a launched application on a thread of its own, so that it is reaped when it exits.
///
/// This used to drop the child and call it "left to init", which is only true once *we* exit.
/// Until then we are its parent, and a process whose parent never waits on it stays behind as
/// a zombie -- so every application opened and closed in a session left one, holding its pid,
/// until the session ended.
///
/// A thread per child, blocked in `wait()` on that one pid, rather than anything
/// process-wide. `waitpid(-1)` from a `SIGCHLD` handler, or ignoring `SIGCHLD` so the kernel
/// reaps for us, would also collect children that other code is waiting on by pid: Smithay
/// reaps Xwayland from a thread exactly like this one, and every `Command::status()` and
/// `output()` in the compositor waits on its own child. Whichever of them lost the race would
/// get `ECHILD` instead of an exit status -- a volume change reported as failed, say, when it
/// worked. Naming the pid is what keeps this out of everyone else's way. The thread is also
/// what keeps the wait off the render loop, which cannot block on anything.
///
/// Its one other job is saying how the application ended. An application that starts and
/// dies straight away looks, from the launcher, exactly like one that never started; the exit
/// status is the line in the log that tells them apart.
fn reap_when_it_exits(mut child: Child, program: &str) {
    let pid = child.id();
    let name = program.to_string();
    let watcher = std::thread::Builder::new()
        .name(format!("reap {pid}"))
        .spawn(move || match child.wait() {
            Ok(status) if status.success() => log::info!("{name} (pid {pid}) exited"),
            Ok(status) => log::warn!("{name} (pid {pid}) exited: {status}"),
            Err(e) => log::warn!("could not wait for {name} (pid {pid}): {e}"),
        });
    // Failing leaves the application running and exactly as it was before this existed: a
    // zombie when it exits. Not a reason to fail a launch that has already happened.
    if let Err(e) = watcher {
        log::warn!("nothing to reap pid {pid} ({e}); it will linger as a zombie when it exits");
    }
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

    fn flatpak_line(exec: &str, extra: &[(&str, &str)]) -> Vec<String> {
        let parts = split_command(exec);
        let (program, args) = parts.split_first().unwrap();
        let mut args = args.to_vec();
        let extra: Vec<(String, String)> = extra
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some(at) = flatpak_option_slot(program, &args) {
            args.splice(at..at, flatpak_environment(&extra));
        }
        args
    }

    #[test]
    fn a_flatpak_is_told_where_to_send_its_sound() {
        // The variable is the whole spatial-audio mechanism, and a sandbox drops it. VLC and
        // Kodi are both Flatpaks here, so without this the two applications the feature exists
        // for are the two that cannot use it.
        let args = flatpak_line(
            "/usr/bin/flatpak run --branch=stable org.videolan.VLC",
            &[("PULSE_SINK", "spatiand.window.3")],
        );
        assert!(args.contains(&"--env=PULSE_SINK=spatiand.window.3".to_string()));
    }

    #[test]
    fn the_option_lands_before_the_application_id() {
        // `flatpak run [OPTIONS] APP [ARGS]`. After the id it is an argument to the
        // application, which flatpak accepts and the application ignores -- a fix that looks
        // applied and does nothing.
        let args = flatpak_line(
            "/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=kodi tv.kodi.Kodi",
            &[("PULSE_SINK", "spatiand.window.1")],
        );
        let env = args
            .iter()
            .position(|a| a.starts_with("--env="))
            .expect("no --env option");
        let app = args
            .iter()
            .position(|a| a == "tv.kodi.Kodi")
            .expect("no application id");
        assert!(env < app, "{args:?}");
    }

    #[test]
    fn the_sandbox_keeps_its_own_wayland_socket() {
        // Flatpak binds our socket in under a name of its choosing and sets the variable
        // itself. Forcing ours names a socket that does not exist inside the sandbox, and the
        // application launches and connects to nothing.
        let args = flatpak_line(
            "/usr/bin/flatpak run org.videolan.VLC",
            &[("WAYLAND_DISPLAY", "wayland-1"), ("DISPLAY", ":1")],
        );
        assert!(!args.iter().any(|a| a.contains("WAYLAND_DISPLAY")), "{args:?}");
        assert!(args.contains(&"--env=DISPLAY=:1".to_string()));
    }

    #[test]
    fn an_ordinary_command_is_left_alone() {
        assert_eq!(
            flatpak_option_slot("/usr/bin/dolphin", &["--new-window".to_string()]),
            None
        );
        // And something that merely mentions flatpak is not one.
        assert_eq!(
            flatpak_option_slot("/usr/bin/flatpak-spawn", &["run".to_string()]),
            None
        );
    }

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
        // Not waited on here: the launcher's own reaper already is, and a second wait on the
        // same pid would race it for the exit status.
    }

    /// The state letter from `/proc/<pid>/stat`, or `None` once the process is gone entirely.
    ///
    /// Read after the *last* `)` for the same reason as `spatiand::audio::parent_of`: the name
    /// field can contain anything, brackets included.
    fn process_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat[stat.rfind(')')? + 1..].trim_start().chars().next()
    }

    #[test]
    fn an_application_that_exits_is_reaped_rather_than_left_a_zombie() {
        // Every app opened and closed in a session used to leave one of these behind until the
        // session ended. A zombie keeps its `/proc` entry, in state Z, until its parent waits
        // on it -- so the entry disappearing is exactly the thing being tested.
        let pid = launch("/bin/true", "wayland-test", &[]).expect("true should start");
        let mut last = None;
        for _ in 0..100 {
            last = process_state(pid);
            if last.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("pid {pid} was never reaped; still in state {last:?}");
    }

    #[test]
    fn shell_metacharacters_are_not_interpreted() {
        // These are not shell commands. Passing them through a shell would make a filename
        // into an instruction.
        let parts = split_command("app; rm -rf /");
        assert_eq!(parts[0], "app;", "must stay one literal argument");
    }
}
