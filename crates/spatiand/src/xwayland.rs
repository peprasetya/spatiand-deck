//! Windows from applications that only speak X11.
//!
//! A great many programs have no Wayland support and never will — this is the compatibility
//! layer that lets them run anyway. XWayland is an X server that draws into Wayland surfaces
//! instead of onto hardware; it needs a window manager on the X11 side to tell it where things
//! go and which window has focus, and that is what this module is.
//!
//! Once a window is mapped it is an ordinary [`smithay::desktop::Window`] in the same `Space`
//! as everything else, so the rest of the compositor — placement, the pointer ray, the title
//! bar, the audio — never learns that it came from X11. That is the whole design: the seam is
//! here and nowhere else.
//!
//! ## What made this necessary
//!
//! A Flatpak of VLC would not start at all. Its manifest asks only for `sockets=x11`, its Qt
//! interface has no Wayland support compiled in, and with no X server to fall back to it gave
//! up silently and shut down. That is not a rare shape: an application built before Wayland, or
//! packaged by someone who did not think about it, has no other way in.
//!
//! ## Two things X11 does that Wayland does not
//!
//! **Windows place themselves.** An X11 client asks for a position on a screen, in pixels, and
//! expects to get it. There is no screen here and pixels mean nothing on a sphere, so the
//! request is answered rather than obeyed: the client is told the size it asked for at the
//! origin, and where the window actually *is* is decided by the same placement every other
//! window gets. Refusing to answer at all is not an option — a client that never receives a
//! configure will sit waiting and never draw.
//!
//! **Override-redirect windows.** Menus, tooltips and drag icons bypass the window manager
//! entirely; the client says "do not manage this" and positions it itself. On a desktop they
//! are drawn wherever the client asked. Here they are mapped as ordinary windows, which is
//! wrong in the sense that a dropdown will not hang off its parent — and right in the sense
//! that it is visible and can be clicked, which is what a menu is for. Doing better means
//! treating them as popups against their parent's quad, which is the same work the Wayland
//! popup path already does and is worth doing once, later, for both.

use smithay::utils::{Logical, Rectangle};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler};

use crate::state::Spatiand;
use crate::Runtime;

/// The size an X11 window is told it has, when it asks and we have nothing better to say.
///
/// The same number a Wayland toplevel is given, for the same reason: it is a count of surface
/// pixels stretched across a quad, not a size on any screen.
const DEFAULT_SIZE: (i32, i32) = (1280, 800);

/// Start an X server for this session.
///
/// Returns the display number, which is what `DISPLAY` has to be set to for a client to find
/// it. Failure is reported and swallowed: a session with no X server is a session where X11
/// applications do not start, which is exactly where we were before, and is much better than
/// no session at all.
/// Set to `off` to run the session without an X server.
///
/// An escape hatch, and it exists because of how this fails when it fails. A compositor that
/// dies during startup is restarted by the session manager, into the same death: a flickering
/// screen, no way in, and nothing to read. Being able to turn off the newest moving part
/// without a rebuild is what turns that into a diagnosis.
pub const DISABLE_ENV: &str = "SPATIAND_XWAYLAND";

pub fn start(
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, Runtime>,
) -> Option<u32> {
    if std::env::var(DISABLE_ENV).as_deref() == Ok("off") {
        log::info!("{DISABLE_ENV}=off: no X server, so X11-only applications will not start");
        return None;
    }
    let (xwayland, client) = match XWayland::spawn(
        display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        // X server chatter is voluminous and almost never the thing that is wrong. When it is,
        // it is reachable by running Xwayland by hand.
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| (),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            log::warn!("no X server: {e}; applications that only speak X11 will not start");
            return None;
        }
    };

    // The display number is known before the server finishes starting, which matters: the
    // launcher needs `DISPLAY` from the moment the session is up, and waiting for readiness
    // would mean the first application launched had no X server to find.
    let number = xwayland.display_number();

    let handle = loop_handle.clone();
    let inserted = loop_handle.insert_source(xwayland, move |event, _, runtime| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => match X11Wm::start_wm(handle.clone(), x11_socket, client.clone()) {
            Ok(wm) => {
                log::info!("X server ready on :{display_number}");
                runtime.state.xwm = Some(wm);
            }
            Err(e) => log::warn!("could not manage the X server's windows: {e}"),
        },
        XWaylandEvent::Error => {
            log::warn!("the X server failed to start; X11 applications will not run");
        }
    });
    if let Err(e) = inserted {
        log::warn!("could not watch the X server: {e}");
        return None;
    }
    Some(number)
}

/// The Wayland side: XWayland tells us which surface belongs to which X11 window through a
/// protocol of its own, and that is dispatched against the compositor state like any other.
impl XWaylandShellHandler for Spatiand {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

/// And again for the event loop's own data, which is what the X11 window manager is handed.
///
/// Two implementations of one trait for what is really one piece of state, because the Wayland
/// display dispatches against [`Spatiand`] while the event loop carries a [`Runtime`] that
/// holds it. Forwarding is the whole body.
impl XWaylandShellHandler for Runtime {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.state.xwayland_shell_state
    }
}

impl XwmHandler for Spatiand {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("the window manager asked for itself")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    /// The client wants its window on screen.
    ///
    /// Answering the configure is not optional. An X11 client that asks to be mapped and never
    /// hears back waits, and an application that starts, appears in no window list and draws
    /// nothing is indistinguishable from one that crashed.
    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let size = window.geometry().size;
        let size = if size.w > 0 && size.h > 0 {
            size
        } else {
            DEFAULT_SIZE.into()
        };
        // At the origin, because there is no screen for a position to be on. Where it ends up
        // is `WindowLayout`'s business, exactly as for a Wayland window.
        if let Err(e) = window.configure(Rectangle::new((0, 0).into(), size)) {
            log::warn!("could not configure an X11 window: {e}");
        }
        if let Err(e) = window.set_mapped(true) {
            log::warn!("could not map an X11 window: {e}");
            return;
        }
        self.adopt_x11_window(window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // A menu or a tooltip. Mapped as an ordinary window so it is at least visible and
        // clickable; see the module note on what doing this properly would mean.
        self.adopt_x11_window(window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11_window(&window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11_window(&window);
    }

    /// The client would like to be somewhere, or some size.
    ///
    /// The size is granted and the position is not. A window's place in the room is not a
    /// number an application can have an opinion about, and every X11 application has one.
    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut geometry = window.geometry();
        if let Some(w) = w {
            geometry.size.w = w as i32;
        }
        if let Some(h) = h {
            geometry.size.h = h as i32;
        }
        geometry.loc = (0, 0).into();
        if let Err(e) = window.configure(geometry) {
            log::warn!("could not answer an X11 configure request: {e}");
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
    }

    /// The client wants to be dragged or resized by its own decorations.
    ///
    /// Declined, both of them. Windows here are moved and resized by their own title bar and
    /// frame, which the compositor draws and which works the same on every window whatever
    /// protocol it speaks. An X11 client driving its own move would fight that.
    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edge: ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}
}

/// The event loop carries a [`Runtime`]; the Wayland display dispatches against the
/// [`Spatiand`] inside it. Both therefore have to be window managers, and only one of them can
/// sensibly hold the state — so this half is forwarding and nothing else.
impl XwmHandler for Runtime {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        self.state.xwm_state(xwm)
    }
    fn new_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_window(xwm, window)
    }
    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_override_redirect_window(xwm, window)
    }
    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.map_window_request(xwm, window)
    }
    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.mapped_override_redirect_window(xwm, window)
    }
    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.unmapped_window(xwm, window)
    }
    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.destroyed_window(xwm, window)
    }
    fn configure_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        self.state
            .configure_request(xwm, window, x, y, w, h, reorder)
    }
    fn configure_notify(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<u32>,
    ) {
        self.state.configure_notify(xwm, window, geometry, above)
    }
    fn resize_request(&mut self, xwm: XwmId, window: X11Surface, button: u32, edge: ResizeEdge) {
        self.state.resize_request(xwm, window, button, edge)
    }
    fn move_request(&mut self, xwm: XwmId, window: X11Surface, button: u32) {
        self.state.move_request(xwm, window, button)
    }
}

/// What to put in a launched application's environment so it can reach the X server.
///
/// `DISPLAY` alone is not enough, and is actively dangerous on its own: several toolkits
/// choose X11 the moment they see it, which would quietly move every Qt and GTK application
/// off Wayland and onto a compatibility layer they do not need. Removing `DISPLAY` is what
/// this session used to do, and the cost of that was that applications with no Wayland support
/// could not start at all.
///
/// So both are set, together with the two variables that say which to prefer. A toolkit that
/// understands them uses Wayland and keeps X11 as a fallback; one that understands neither has
/// an X server to fall back to, which is the entire point.
pub fn client_environment(display_number: Option<u32>) -> Vec<(String, String)> {
    let Some(number) = display_number else {
        return Vec::new();
    };
    vec![
        ("DISPLAY".to_string(), format!(":{number}")),
        // Qt takes a semicolon-separated list and tries them in order.
        ("QT_QPA_PLATFORM".to_string(), "wayland;xcb".to_string()),
        // GTK takes a comma-separated one.
        ("GDK_BACKEND".to_string(), "wayland,x11".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_escape_hatch_is_spelled_the_way_it_is_documented() {
        // Written down in one place, because the situation it is for is one where nothing can
        // be read off the screen and the name has to be right first time.
        assert_eq!(DISABLE_ENV, "SPATIAND_XWAYLAND");
    }

    #[test]
    fn without_an_x_server_nothing_is_promised() {
        // A session that could not start one must not tell applications otherwise: a DISPLAY
        // pointing at nothing is worse than no DISPLAY, because a toolkit will try it.
        assert!(client_environment(None).is_empty());
    }

    #[test]
    fn an_x_server_is_offered_but_not_preferred() {
        let env = client_environment(Some(7));
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert_eq!(get("DISPLAY"), ":7");
        // The order in both lists is what stops a toolkit that can do either from quietly
        // choosing the compatibility layer.
        assert!(get("QT_QPA_PLATFORM").starts_with("wayland"));
        assert!(get("GDK_BACKEND").starts_with("wayland"));
        assert!(get("QT_QPA_PLATFORM").contains("xcb"));
        assert!(get("GDK_BACKEND").contains("x11"));
    }
}
