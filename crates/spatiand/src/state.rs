//! Compositor state and the Wayland protocol handlers.
//!
//! This is the plumbing half of Spatiand: it makes us a working Wayland compositor that
//! clients can connect to and post buffers on. What makes it *spatial* — the stereo cameras,
//! the sphere of windows, the glass — sits on top and is deliberately kept out of here, so
//! this file stays comparable to any other Smithay compositor and can be read against
//! upstream's examples when the API moves.

use smithay::desktop::{PopupKind, PopupManager, Space};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_decoration, delegate_xdg_shell,
};

use crate::window::WindowLayout;

/// Pixel size proposed to a new toplevel.
///
/// 1280x800 across a window that fills a third of a 40 degree field works out at roughly one
/// surface pixel per display pixel, so text is neither soft nor pointlessly oversampled.
const DEFAULT_WINDOW_SIZE: (i32, i32) = (1280, 800);

/// Per-client data. Smithay requires the compositor's per-client state to live here.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

pub struct Spatiand {
    pub display_handle: DisplayHandle,
    /// Windows that have appeared since the render loop last looked, each with the process id
    /// of the client that owns it.
    ///
    /// Collected here rather than acted on directly because the audio engine lives in the
    /// render loop, and a Wayland handler is not the place to reach into it. The process id is
    /// what ties a window back to the app that was launched for it — see [`crate::audio`].
    pub arrived_windows: Vec<(usize, Option<u32>)>,
    /// Windows that have gone since the render loop last looked.
    pub departed_windows: Vec<usize>,
    pub running: bool,
    pub socket_name: String,

    // --- wayland globals ---
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// Advertised only so it can be answered with "server side" — see [`XdgDecorationHandler`].
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<Self>,

    /// Smithay's 2D window bookkeeping. We do not use its 2D *rendering*, but its mapping,
    /// stacking and surface-under queries are exactly right and there is no reason to
    /// reimplement them; the spatial layer reads window geometry out of here and decides
    /// where each one goes in 3D.
    pub space: Space<smithay::desktop::Window>,
    /// Where each window sits on the sphere, keyed alongside `space`.
    pub layout: WindowLayout,
    /// Menus, dropdowns, tooltips — every surface a client hangs off one of its own windows.
    ///
    /// These are not toplevels and never appear in `space`, which is why they need their own
    /// bookkeeping. Smithay's manager is what knows where each one sits relative to the window
    /// it belongs to, including submenus hanging off other popups.
    pub popups: PopupManager,
    /// The X11 window manager, once the X server has finished starting.
    ///
    /// `None` before then, and for the whole session if no X server could be started — which
    /// is a session where X11 applications do not run, and everything else is unaffected.
    pub xwm: Option<smithay::xwayland::X11Wm>,
    /// The protocol XWayland uses to tell us which surface belongs to which X11 window.
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// Yaw the wearer is currently facing, radians, refreshed once a frame by the backend.
    ///
    /// Lives here because `new_toplevel` needs it and has no access to the tracker: a window
    /// has to be placed the moment it is mapped, which is deep inside a protocol callback.
    pub spawn_yaw: f64,
}

impl Spatiand {
    pub fn new(
        display: &mut Display<Self>,
        loop_handle: &smithay::reexports::calloop::LoopHandle<'static, crate::Runtime>,
    ) -> Self {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, Vec::new());
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);

        // One seat. The Deck's controller, its touchscreen and any USB keyboard all feed
        // this; the spatial input router decides what reaches a client.
        let mut seat = seat_state.new_wl_seat(&dh, "spatiand");
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("failed to create keyboard");
        seat.add_pointer();

        let socket_name = Self::init_socket(loop_handle);
        // Built before the struct, because `dh` is moved into it.
        let xwayland_shell_state =
            smithay::wayland::xwayland_shell::XWaylandShellState::new::<Self>(&dh);

        Self {
            display_handle: dh,
            arrived_windows: Vec::new(),
            departed_windows: Vec::new(),
            running: true,
            socket_name,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
            space: Space::default(),
            layout: WindowLayout::default(),
            popups: PopupManager::default(),
            xwm: None,
            xwayland_shell_state,
            spawn_yaw: 0.0,
        }
    }

    /// Open a Wayland socket and register it with the event loop.
    fn init_socket(
        loop_handle: &smithay::reexports::calloop::LoopHandle<'static, crate::Runtime>,
    ) -> String {
        let source = ListeningSocketSource::new_auto().expect("could not open a wayland socket");
        let name = source.socket_name().to_string_lossy().into_owned();
        loop_handle
            .insert_source(source, |client_stream, _, runtime| {
                if let Err(e) = runtime
                    .display_handle
                    .insert_client(client_stream, std::sync::Arc::new(ClientState::default()))
                {
                    log::warn!("could not accept a client: {e}");
                }
            })
            .expect("could not register the wayland socket");
        log::info!("listening on WAYLAND_DISPLAY={name}");
        name
    }

    /// The title a client has set, for the title bar.
    pub fn title_of(&self, window: &smithay::desktop::Window) -> Option<String> {
        let surface = window.toplevel()?.wl_surface().clone();
        smithay::wayland::compositor::with_states(&surface, |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok())
                .and_then(|d| d.title.clone())
        })
    }

    /// The window's application id, which is what an icon can be looked up by.
    ///
    /// Not the title: a title is whatever the application decided to write there this second,
    /// and changes with the open document. The app id is stable for the window's whole life,
    /// which is what a texture cache needs as a key.
    pub fn app_id_of(&self, window: &smithay::desktop::Window) -> Option<String> {
        let surface = window.toplevel()?.wl_surface().clone();
        smithay::wayland::compositor::with_states(&surface, |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok())
                .and_then(|d| d.app_id.clone())
        })
    }

    /// Ask a window to close itself.
    ///
    /// A request, not an order — that is the whole protocol. An application with unsaved work
    /// puts up its own dialog and stays, which is right, and is why this cannot report whether
    /// anything happened.
    pub fn close_window(&self, window: &smithay::desktop::Window) {
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_close();
        }
    }

    /// Give a window focus, and bring it to the front.
    ///
    /// Takes the window rather than a position, because raising reorders `Space::elements()`
    /// and any index taken before the raise refers to something else afterwards.
    pub fn focus_window(&mut self, window: &smithay::desktop::Window) {
        self.layout.focus(window);
        self.space.raise_element(window, true);
        if let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) {
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(
                    self,
                    Some(surface),
                    smithay::utils::SERIAL_COUNTER.next_serial(),
                );
            }
        }
    }

    /// The surface under a point, for pointer focus.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(
                        pos - location.to_f64(),
                        smithay::desktop::WindowSurfaceType::ALL,
                    )
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
    }
}

// --- compositor ---

impl CompositorHandler for Spatiand {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    /// Per-client compositor state, for whichever kind of client this is.
    ///
    /// **Nothing here may panic**, and that is not a style preference. This runs inside a
    /// callback the Wayland C library invokes through libffi, and a panic cannot unwind across
    /// that boundary — Rust aborts the process instead. The result is a session that dies with
    /// no message at all and is restarted by the session manager, which restarts it into the
    /// same abort: a flickering screen and no way to stop it. That is exactly what an
    /// `unwrap()` here did.
    ///
    /// It was reached the moment XWayland connected, because XWayland is a client we did not
    /// create and its data is smithay's own [`XWaylandClientData`] rather than our
    /// [`ClientState`] — so asking for ours found nothing.
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(ours) = client.get_data::<ClientState>() {
            return &ours.compositor_state;
        }
        // XWayland's client is made by smithay, not by us, and carries its own.
        if let Some(theirs) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &theirs.compositor_state;
        }
        // Any other client is one nobody has accounted for. A shared, empty state is wrong in
        // that two such clients would share buffer bookkeeping; it is right in that the
        // session survives to say so, which the alternative does not.
        static UNACCOUNTED: std::sync::OnceLock<CompositorClientState> = std::sync::OnceLock::new();
        log::warn!("a client arrived with no compositor state of its own");
        UNACCOUNTED.get_or_init(CompositorClientState::default)
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // A sync subsurface's commit is not applied until its parent commits, so there is
        // nothing to do for it yet.
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|w| {
                    w.toplevel()
                        .map(|t| t.wl_surface() == &root)
                        .unwrap_or(false)
                })
                .cloned()
            {
                window.on_commit();
            }
        }

        // Popup bookkeeping is keyed on the popup's own surface, not on the root found above:
        // a menu's root is the toplevel it belongs to, and passing that would leave the popup
        // itself never advancing its state.
        self.popups.commit(surface);
        self.ensure_initial_configure(surface);
    }
}

use smithay::backend::renderer::utils::on_commit_buffer_handler;

impl BufferHandler for Spatiand {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for Spatiand {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

// --- decorations ---

/// Every window is decorated by Spatiand, and no client is allowed to decorate itself.
///
/// A client drawing its own header is right on a desktop and wrong here. The frame in this
/// world is a pane of glass with the title on it, drawn *around* the surface at a size and
/// distance the wearer set, and a second bar drawn *inside* the surface duplicates the title
/// and puts a close button somewhere the 3D chrome does not know about. That is what KDE's
/// applications do by default, because they see no decoration protocol and reasonably conclude
/// they are on their own.
///
/// So the mode is not negotiated. `request_mode` ignores what was asked for and answers
/// `ServerSide` regardless, which the protocol explicitly allows — the compositor's mode is
/// final. There is nothing to be gained by honouring a request for client-side decorations in
/// a compositor that has no flat screen to draw them on.
///
/// Minimise and maximise are not answered anywhere, and that is deliberate rather than
/// unfinished: neither means anything in a room. A window is moved, pushed away or closed.
impl XdgDecorationHandler for Spatiand {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        Self::decorate(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        Self::decorate(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        Self::decorate(&toplevel);
    }
}

impl Spatiand {
    fn decorate(toplevel: &ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        // Only once the client has had its initial configure. Sending one before that is a
        // protocol error, and it is reachable here: a client may create its decoration object
        // in the same batch as the toplevel itself.
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

// --- xdg shell ---

impl XdgShellHandler for Spatiand {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Say up front that this compositor decorates, whether or not the client asks. A
        // toolkit that never binds the decoration protocol still reads this out of the
        // toplevel's state, and it is the difference between one title bar and two.
        surface.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        // Propose a size before anything else. A toplevel configured with 0x0 is telling the
        // client "pick your own", and while most do, several toolkits wait for a real size and
        // never commit a buffer -- which presents as an app that launches, appears in the
        // window count, and draws nothing.
        //
        // The number is the surface's pixel resolution, not its size in the world: a window is
        // a quad of whatever width the layout gives it, and this is how many pixels get
        // stretched across that quad.
        surface.with_pending_state(|state| {
            state.size = Some(DEFAULT_WINDOW_SIZE.into());
        });
        let window = smithay::desktop::Window::new_wayland_window(surface);
        // Smithay's Space is 2D, so every window is given a slot on a notional plane. The
        // spatial layer never reads these coordinates as pixels — it reads the *ordering* and
        // maps each window onto the sphere. Position here only has to be unique and stable.
        let index = self.space.elements().count() as i32;
        self.space
            .map_element(window.clone(), (index * 32, index * 32), false);
        self.layout.place(&window, self.spawn_yaw);
        // Newly opened windows take focus, so the thing you just launched is the thing the
        // pointer and keyboard talk to.
        self.layout.focus(&window);
        // Which process this window belongs to, so its sound can be found. Read from the
        // Wayland connection's own credentials, which the kernel supplies and a client cannot
        // lie about.
        let pid = window
            .toplevel()
            .and_then(|t| {
                use smithay::reexports::wayland_server::Resource;
                t.wl_surface().client()
            })
            .and_then(|c| c.get_credentials(&self.display_handle).ok())
            .map(|c| c.pid as u32);
        if let Some(id) = self.layout.id_of(&window) {
            self.arrived_windows.push((id, pid));
        }
        log::info!(
            "new toplevel at yaw {:.0} deg ({} windows), offered {}x{}",
            self.spawn_yaw.to_degrees(),
            self.space.elements().count(),
            DEFAULT_WINDOW_SIZE.0,
            DEFAULT_WINDOW_SIZE.1
        );
    }

    /// A menu, a dropdown, a tooltip — anything a client hangs off one of its own surfaces.
    ///
    /// There is nothing spatial to decide: a popup is positioned relative to the window it
    /// belongs to, and that window already has a place in the world. What there *is* to do is
    /// take the positioner's word for where it goes and start tracking it, because a popup
    /// nobody tracks is a popup nobody can configure, and an xdg_surface that has never been
    /// configured may not attach a buffer. That is why menus were not merely invisible — they
    /// were never mapped at all.
    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
        });
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            log::warn!("could not track a popup: {e}");
        }
    }

    /// A client asking to own the pointer and keyboard for the duration of a menu.
    ///
    /// Not honoured, and the choice is deliberate rather than unfinished. A grab is a promise
    /// that every event goes to the menu until it is dismissed, and the only input here is a
    /// ray cast through a room — one that can quite reasonably be pointing at another window,
    /// at the keyboard, or at nothing. Enforcing an exclusive grab would mean a menu that ate
    /// every click in the world until it closed, and the wearer's obvious escape (look
    /// somewhere else and click) is exactly the thing a grab forbids.
    ///
    /// Instead a click that lands outside the popup dismisses it, which is what a grab is for
    /// from the wearer's side. See the pointer's `dismiss_popups`.
    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Resolve to an owned Window before touching `space` again: `elements()` holds an
        // immutable borrow for as long as the iterator chain is alive.
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            if let Some(id) = self.layout.id_of(&window) {
                self.departed_windows.push(id);
            }
            self.layout.remove(&window);
            self.space.unmap_elem(&window);
        }
    }
}

impl Spatiand {
    /// Put a newly mapped X11 window into the room.
    ///
    /// Deliberately the same three steps `new_toplevel` takes, and in the same order: a slot
    /// in the `Space`, a place on the sphere, and focus. Everything downstream reads those and
    /// nothing downstream asks which protocol the window came from.
    pub fn adopt_x11_window(&mut self, surface: smithay::xwayland::X11Surface) {
        let pid = surface.pid();
        let window = smithay::desktop::Window::new_x11_window(surface);
        let index = self.space.elements().count() as i32;
        self.space
            .map_element(window.clone(), (index * 32, index * 32), false);
        self.layout.place(&window, self.spawn_yaw);
        self.layout.focus(&window);
        if let Some(id) = self.layout.id_of(&window) {
            self.arrived_windows.push((id, pid));
        }
        log::info!(
            "new X11 window at yaw {:.0} deg ({} windows)",
            self.spawn_yaw.to_degrees(),
            self.space.elements().count()
        );
    }

    /// An X11 window has gone. Take it out of everything that was tracking it.
    pub fn forget_x11_window(&mut self, surface: &smithay::xwayland::X11Surface) {
        let window = self
            .space
            .elements()
            .find(|w| w.x11_surface() == Some(surface))
            .cloned();
        if let Some(window) = window {
            if let Some(id) = self.layout.id_of(&window) {
                self.departed_windows.push(id);
            }
            self.layout.remove(&window);
            self.space.unmap_elem(&window);
        }
    }

    /// xdg_surface requires a configure before the client may attach a buffer.
    fn ensure_initial_configure(&mut self, surface: &WlSurface) {
        // Popups first, and they need their own branch rather than falling through the
        // toplevel search below: a popup is not in `space`, so looking for it there finds
        // nothing and it is silently never configured. A client that has asked for a menu then
        // waits for a configure that never comes, and the menu does not appear -- with no
        // error anywhere, because nothing has gone wrong. It is simply still waiting.
        if let Some(popup) = self.popups.find_popup(surface) {
            let PopupKind::Xdg(ref popup) = popup else {
                return;
            };
            let sent = smithay::wayland::compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgPopupSurfaceData>()
                    .and_then(|data| data.lock().ok().map(|d| d.initial_configure_sent))
            });
            if sent == Some(false) {
                // Cannot fail on a live surface with an unsent initial configure, but a client
                // that raced its own destroy can still get here.
                if let Err(e) = popup.send_configure() {
                    log::debug!("could not configure a popup: {e}");
                }
            }
            return;
        }

        if let Some(window) = self
            .space
            .elements()
            .find(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == surface)
                    .unwrap_or(false)
            })
            .cloned()
        {
            if let Some(toplevel) = window.toplevel() {
                // Read defensively rather than unwrapping, even though the `find` above means
                // the role data should always be there. This runs inside a Wayland request
                // handler, and a panic in one of those does not fail a request — it unwinds
                // through the dispatch loop and takes the compositor with it, which costs
                // every window belonging to every client. A misjudged invariant here would be
                // indistinguishable from a crash, and the price of being wrong is far higher
                // than the price of an `if let`.
                let initial_configure_sent =
                    smithay::wayland::compositor::with_states(surface, |states| {
                        states
                            .data_map
                            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                            .and_then(|data| data.lock().ok().map(|d| d.initial_configure_sent))
                    });
                // `None` means the surface has no toplevel role data, or another thread
                // panicked holding that lock. Neither is a reason to configure it again.
                if initial_configure_sent == Some(false) {
                    toplevel.send_configure();
                }
            }
        }
    }
}

// --- seat ---

impl SeatHandler for Spatiand {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

// --- output ---

impl smithay::wayland::output::OutputHandler for Spatiand {}

// --- data device (clipboard, dnd) ---

/// Clipboard payloads pass straight through; Spatiand attaches nothing of its own to a
/// selection, so the associated data is unit.
impl SelectionHandler for Spatiand {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Spatiand {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Spatiand {}
impl ServerDndGrabHandler for Spatiand {}

delegate_compositor!(Spatiand);
delegate_shm!(Spatiand);
delegate_xdg_shell!(Spatiand);
delegate_xdg_decoration!(Spatiand);
delegate_seat!(Spatiand);
delegate_output!(Spatiand);
delegate_data_device!(Spatiand);
smithay::delegate_xwayland_shell!(Spatiand);
