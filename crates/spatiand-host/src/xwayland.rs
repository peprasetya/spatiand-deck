//! Windows from applications that only speak X11.
//!
//! Firestorm is the reason this exists on the host: its Linux build draws through GLX, and
//! offered only Wayland it opened an SDL window there and crashed setting up OpenGL — "We're
//! not running under X11? Wild." Most games through Proton and Wine are X11 too. So the host
//! runs an XWayland of its own, with a window manager, and an X11 window becomes a
//! [`smithay::desktop::Window`] tracked and streamed like any other.
//!
//! This is Spatiand's own `xwayland.rs`, cut down to what a headless host needs: there is no
//! room to place windows in, so X11's requests for a size are granted, requests for a position
//! are answered at the origin, and everything else — the lessons in `docs/x11.md` — applies
//! unchanged. Override-redirect windows (X11's menus and tooltips) are not drawn yet; an
//! application that draws its own interface, as a game or a viewer does, never uses them.

use smithay::utils::{Logical, Rectangle};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler};

use crate::state::Host;

/// The size an X11 window is told it has when it asks for nothing better.
const DEFAULT_SIZE: (i32, i32) = (1280, 800);

/// Start an X server for the host's applications. The display number, for `DISPLAY`.
///
/// Failure is said and survived: without it, X11-only applications do not start, which is where
/// the host was before, and everything else carries on.
pub fn start(
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, Host>,
) -> Option<u32> {
    let (xwayland, client) = match XWayland::spawn(
        display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| (),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            log::warn!("no X server ({e}); applications that only speak X11 will not start");
            return None;
        }
    };
    // Known before the server is ready, which matters: an application launched in the first
    // moments needs `DISPLAY` too.
    let number = xwayland.display_number();
    let handle = loop_handle.clone();
    let inserted = loop_handle.insert_source(xwayland, move |event, _, host| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => match X11Wm::start_wm(handle.clone(), x11_socket, client.clone()) {
            Ok(wm) => {
                log::info!("X server ready on :{display_number}");
                host.xwm = Some(wm);
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

impl XWaylandShellHandler for Host {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(
        &mut self,
        _xwm: XwmId,
        _wl_surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        surface: X11Surface,
    ) {
        log::info!("X11 window {:?} has a surface to draw into", surface.title());
    }
}

impl XwmHandler for Host {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("the window manager asked for itself")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    /// Answered, always: an X11 client that asks to be mapped and hears nothing waits for ever.
    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let size = window.geometry().size;
        let size = if size.w > 0 && size.h > 0 {
            size
        } else {
            DEFAULT_SIZE.into()
        };
        if let Err(e) = window.configure(Rectangle::new((0, 0).into(), size)) {
            log::warn!("could not configure an X11 window: {e}");
        }
        if let Err(e) = window.set_mapped(true) {
            log::warn!("could not map an X11 window: {e}");
            return;
        }
        self.adopt_x11(window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        log::debug!("an X11 menu or tooltip {:?}, not drawn yet", window.title());
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11(&window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11(&window);
    }

    /// The size is granted and the position is not: there is no screen for it to be on.
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

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        grant(&window, true, X11Surface::set_fullscreen);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        grant(&window, false, X11Surface::set_fullscreen);
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        grant(&window, true, X11Surface::set_maximized);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        grant(&window, false, X11Surface::set_maximized);
    }

    /// Declined: the session moves and resizes windows, not the applications in them.
    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _edge: ResizeEdge) {}

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}
}

/// Say yes to a state change, and confirm the size: several clients wait for the configure
/// before they redraw.
fn grant<E: std::fmt::Display>(
    window: &X11Surface,
    on: bool,
    set: impl Fn(&X11Surface, bool) -> Result<(), E>,
) {
    if let Err(e) = set(window, on) {
        log::warn!("could not change an X11 window's state: {e}");
        return;
    }
    let size = window.geometry().size;
    if size.w > 0 && size.h > 0 {
        if let Err(e) = window.configure(Rectangle::new((0, 0).into(), size)) {
            log::warn!("could not confirm an X11 window's size: {e}");
        }
    }
}

/// What to put in an application's environment so it can reach the X server, while still
/// preferring Wayland where its toolkit can. See Spatiand's `xwayland::client_environment`.
pub fn client_environment(display_number: Option<u32>) -> Vec<(String, String)> {
    let Some(number) = display_number else {
        return Vec::new();
    };
    vec![
        ("DISPLAY".to_string(), format!(":{number}")),
        ("QT_QPA_PLATFORM".to_string(), "wayland;xcb".to_string()),
        ("GDK_BACKEND".to_string(), "wayland,x11".to_string()),
    ]
}
