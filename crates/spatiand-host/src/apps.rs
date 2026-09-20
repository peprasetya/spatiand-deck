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
) -> std::io::Result<u32> {
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
    // **Its own process group**, so that force-quitting it can reach everything it started —
    // a browser's renderers, a game's launcher and the game — with one signal, and so that
    // doing so cannot reach the host, which it would otherwise share a group with.
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    log::info!("launched {} as pid {}", app.id, child.id());
    // The child is deliberately not waited on here: an application outliving a viewer is the
    // whole point, and the host reaps it when it exits.
    Ok(child.id())
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
    // Says which host and which application, for anything that wants to know it is remote.
    env.insert("SPATIAND_REMOTE_APP".into(), app.id.clone());
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
    fn an_entry_can_override_anything_it_is_given() {
        let app = App {
            env: vec![("QT_QPA_PLATFORM".into(), "xcb".into())],
            ..App::new("thing", "Thing", "/usr/bin/thing")
        };
        let env = environment(&app, "wayland-1", None);
        assert!(env.contains(&("QT_QPA_PLATFORM".into(), "xcb".into())));
    }
}
