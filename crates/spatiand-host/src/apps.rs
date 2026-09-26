//! Starting what is in the catalogue. The file itself is `spatiand-host-catalog`'s, shared with
//! the settings app that edits it.
//!
//! Launching is deliberately plain: a command, its arguments, its environment, and the socket
//! this compositor is listening on. What it is *not* is a shell — an application is started
//! the way a desktop file's `Exec` says, with no `sh -c`, so a stray quote in a catalogue
//! entry is a bad argument rather than a command someone else gets to choose.

use std::collections::HashMap;
use std::process::Command;

use spatiand_stream::App;

/// What starting an application produced.
pub struct Launched {
    pub pid: u32,
    /// Our end of the application's control socket, for one that takes the view. See
    /// `appcontrol`: the caller hands it to `appcontrol::watch`.
    pub control: Option<std::os::fd::OwnedFd>,
}

/// Start an application, and say which process it became.
///
/// The environment it is given is everything that makes it behave as a remote application:
/// which compositor to talk to, and — later — which sink its sound belongs to and which pad it
/// may see. Anything the catalogue sets is applied last, so an entry can override any of it.
pub fn launch(
    app: &App,
    wayland_display: &str,
    x11_display: Option<u32>,
    sound: &[(String, String)],
    pose: Option<std::os::fd::BorrowedFd<'_>>,
) -> std::io::Result<Launched> {
    let mut command = Command::new(&app.exec);
    command.args(&app.args);
    if let Some(dir) = &app.workdir {
        command.current_dir(dir);
    }
    for (key, value) in environment(app, wayland_display, x11_display) {
        command.env(key, value);
    }
    // Where its sound goes: its own sink, see `audio`. After the catalogue's environment, so
    // an entry cannot send it elsewhere by accident.
    for (key, value) in sound {
        command.env(key, value);
    }
    // **The head, for an application that takes the view.** A read-only descriptor onto the
    // pose ring, left open across exec and named in the environment. Only for `kind = "vr"`:
    // a window among others has no use for the wearer's head, and there is no reason to tell
    // every program on the machine where somebody is looking.
    //
    // Its own number rather than a fixed one: moving it onto, say, 3 would mean `dup2` in the
    // child, and 3 may be exactly where the standard library put the pipe it reports a failed
    // exec through — which would turn every launch into a spawn error that makes no sense.
    if let Some(pose) = pose.filter(|_| app.kind == spatiand_stream::AppKind::Vr) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let fd = pose.as_raw_fd();
        command.env("SPATIAND_POSE_FD", fd.to_string());
        command.env("SPATIAND_POSE_SIZE", spatiand_proto::pose::channel_size().to_string());
        // SAFETY: between fork and exec only async-signal-safe calls are made, and they touch
        // nothing but the child's own copy of the descriptor table.
        unsafe {
            command.pre_exec(move || keep_across_exec(fd));
        }
    }
    // **And a way to say what it has become.** The other half of taking the view: the pose is
    // what the application is told, this is what it tells. See `appcontrol`. Held here only
    // until the spawn, so the child is the only one left holding its end and the socket closes
    // when the application does.
    let mut theirs: Option<std::os::fd::OwnedFd> = None;
    let mut ours: Option<std::os::fd::OwnedFd> = None;
    if app.kind == spatiand_stream::AppKind::Vr {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let (host_end, app_end) = crate::appcontrol::pair()?;
        let fd = app_end.as_raw_fd();
        command.env("SPATIAND_CONTROL_FD", fd.to_string());
        // SAFETY: as for the pose descriptor above.
        unsafe {
            command.pre_exec(move || keep_across_exec(fd));
        }
        theirs = Some(app_end);
        ours = Some(host_end);
    }
    // **Its own process group**, so that force-quitting it can reach everything it started —
    // a browser's renderers, a game's launcher and the game — with one signal, and so that
    // doing so cannot reach the host, which it would otherwise share a group with.
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    drop(theirs);
    log::info!("launched {} as pid {}", app.id, child.id());
    // The child is deliberately not waited on here: an application outliving a viewer is the
    // whole point, and the host reaps it when it exits.
    Ok(Launched {
        pid: child.id(),
        control: ours,
    })
}

/// Clear close-on-exec on one descriptor, in the child, between fork and exec.
///
/// Only async-signal-safe calls, touching nothing but the child's own descriptor table.
fn keep_across_exec(fd: i32) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor number, with no memory involved.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Everything a launched application is told.
pub fn environment(
    app: &App,
    wayland_display: &str,
    x11_display: Option<u32>,
) -> Vec<(String, String)> {
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("WAYLAND_DISPLAY".into(), wayland_display.into());
    // A desktop session's own display, if the host was started from one, must not leak
    // through: a toolkit that finds X first will open its window there, on a screen nobody is
    // looking at, and the host will never see it. The host's own X server replaces it below.
    env.insert("DISPLAY".into(), String::new());
    env.insert("GDK_BACKEND".into(), "wayland".into());
    env.insert("QT_QPA_PLATFORM".into(), "wayland".into());
    env.insert("CLUTTER_BACKEND".into(), "wayland".into());
    // SDL is left to choose. It used to be told Wayland, and Firestorm — SDL for its window,
    // GLX for its OpenGL — made a Wayland window and crashed setting up GL. With the host's X
    // server in `DISPLAY`, SDL 2 picks X11 and SDL 3 picks Wayland, which is what each of them
    // was built and tested against. An entry can still say `SDL_VIDEODRIVER` itself.
    for (key, value) in crate::xwayland::client_environment(x11_display) {
        env.insert(key, value);
    }
    // **File dialogs in the window, not on the desktop.** An application here shares the
    // desktop's session bus, and a toolkit that finds a desktop portal there hands its Open
    // and Save dialogs to it — which opens them on this machine's own screen, where nobody in
    // the headset can see them, and leaves the application waiting on a dialog nobody will
    // answer. These make GTK 3 and GTK 4 draw their own.
    env.insert("GTK_USE_PORTAL".into(), "0".into());
    env.insert("GDK_DEBUG".into(), "no-portals".into());
    // **One controller, and it is the host's.** SDL refuses Steam's virtual gamepad identity
    // unless told otherwise, and that identity is what the pad here wears — see `spatiand_pad`
    // for the measurement that chose it. The same pair hides any controller plugged into this
    // machine from the applications the host starts, so one of them can never find two pads
    // and play with the wrong one; it hides nothing from anybody else's programs.
    for (key, value) in spatiand_pad::hide_other_controllers() {
        env.insert(key, value);
    }
    // **A VR application keeps the pad when it loses the keyboard.** SDL 2 throws joystick
    // input away while its window has no keyboard focus, and Firestorm draws its window with
    // SDL -- the same SDL its joystick library reads the pad through. So the wearer clicking a
    // window in the session took this machine's X focus off the viewer, and the stick went dead
    // until something gave it back. An application that is the room has no window to click
    // back to. The session already decides who gets the pad, and sends nothing to an
    // application that does not have it; the viewer does not need to decide again.
    if app.kind == spatiand_stream::AppKind::Vr {
        env.insert("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS".into(), "1".into());
    }
    // Says which host and which application, for anything that wants to know it is remote.
    env.insert("SPATIAND_REMOTE_APP".into(), app.id.clone());
    // **Being remote is not the same as being in a headset.** Today every session is a pair of
    // eyes, so an application could get away with reading `SPATIAND_REMOTE_APP` and assuming
    // stereo — and would then be wrong the first time this protocol carries a window to an
    // ordinary flat desktop. An application that draws its own two eyes reads this instead.
    env.insert("SPATIAND_EYES".into(), app.eyes.as_str().into());
    for (key, value) in &app.env {
        env.insert(key.clone(), value.clone());
    }
    let mut env: Vec<(String, String)> = env.into_iter().collect();
    env.sort();
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_an_x_server_it_is_offered_and_wayland_still_preferred() {
        let app = App::new("firestorm", "Firestorm", "/opt/firestorm");
        let env = environment(&app, "wayland-1", Some(3));
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("DISPLAY"), ":3");
        assert!(get("QT_QPA_PLATFORM").starts_with("wayland"));
        assert_eq!(get("SDL_VIDEODRIVER"), "", "SDL chooses for itself");
    }

    #[test]
    fn a_vr_application_hears_the_pad_without_the_keyboard() {
        let get = |app: &App, k: &str| {
            environment(app, "wayland-1", Some(1))
                .into_iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v)
                .unwrap_or_default()
        };
        let mut viewer = App::new("spatiworld", "SpatiWorld", "/opt/spatiworld");
        viewer.kind = spatiand_stream::AppKind::Vr;
        assert_eq!(get(&viewer, "SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS"), "1");
        // A window among others shares the one pad with them, and focus decides.
        let game = App::new("game", "Game", "/usr/bin/game");
        assert_eq!(get(&game, "SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS"), "");
    }

    #[test]
    fn a_launch_points_the_application_at_this_compositor() {
        let app = App::new("thing", "Thing", "/usr/bin/thing");
        let env = environment(&app, "wayland-7", None);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("WAYLAND_DISPLAY"), "wayland-7");
        assert_eq!(get("SPATIAND_REMOTE_APP"), "thing");
        assert_eq!(
            get("DISPLAY"),
            "",
            "an inherited X display would take the window somewhere we cannot see it"
        );
    }

    #[test]
    fn an_application_is_told_whether_it_has_two_eyes_to_draw() {
        let get = |app: &App| {
            environment(app, "wayland-1", None)
                .iter()
                .find(|(key, _)| key == "SPATIAND_EYES")
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        // The default is one eye, so a remote window on a flat desktop stays flat even though
        // everything reaching it today is a headset.
        assert_eq!(get(&App::new("thing", "Thing", "/usr/bin/thing")), "mono");
        let stereo = App {
            eyes: spatiand_stream::Eyes::SideBySide,
            ..App::new("viewer", "Viewer", "/usr/bin/viewer")
        };
        assert_eq!(get(&stereo), "side_by_side");
    }

    /// Start `/bin/sh -c script` the way the host starts anything, and say whether it succeeded.
    fn exits_cleanly(kind: spatiand_stream::AppKind, script: String) -> bool {
        use std::os::fd::AsFd;
        let poses = crate::pose::Poses::new().expect("ring");
        let ro = poses.read_only().expect("read-only descriptor");
        let app = App {
            kind,
            args: vec!["-c".into(), script],
            ..App::new("probe", "Probe", "/bin/sh")
        };
        let launched = launch(&app, "wayland-probe", None, &[], Some(ro.as_fd())).expect("launched");
        let mut status = 0;
        // SAFETY: waiting on a child this test just started.
        unsafe { libc::waitpid(launched.pid as i32, &mut status, 0) };
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
    }

    #[test]
    fn an_application_that_takes_the_view_is_handed_the_head() {
        // Named, and actually open on the other side of exec -- the flag that would have
        // closed it is the whole thing being tested.
        assert!(exits_cleanly(
            spatiand_stream::AppKind::Vr,
            r#"[ -n "$SPATIAND_POSE_FD" ] && [ -e "/proc/self/fd/$SPATIAND_POSE_FD" ] && [ "$SPATIAND_POSE_SIZE" -gt 0 ]"#.into(),
        ));
    }

    #[test]
    fn an_application_that_takes_the_view_can_say_what_it_has_become() {
        // Its end of the control socket is open on the other side of exec, and writable.
        assert!(exits_cleanly(
            spatiand_stream::AppKind::Vr,
            r#"[ -n "$SPATIAND_CONTROL_FD" ] && [ -w "/proc/self/fd/$SPATIAND_CONTROL_FD" ]"#.into(),
        ));
    }

    #[test]
    fn a_window_is_not_told_where_the_wearer_is_looking() {
        assert!(exits_cleanly(
            spatiand_stream::AppKind::Window,
            r#"[ -z "$SPATIAND_POSE_FD" ]"#.into(),
        ));
    }

    #[test]
    fn an_entry_can_override_anything_it_is_given() {
        let app = App {
            env: vec![("QT_QPA_PLATFORM".into(), "xcb".into())],
            ..App::new("thing", "Thing", "/usr/bin/thing")
        };
        let env = environment(&app, "wayland-1", None);
        assert!(env.contains(&("QT_QPA_PLATFORM".into(), "xcb".into())));
    }
}
