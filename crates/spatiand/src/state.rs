//! Compositor state and the Wayland protocol handlers.
//!
//! This is the plumbing half of Spatiand: it makes us a working Wayland compositor that
//! clients can connect to and post buffers on. What makes it *spatial* — the stereo cameras,
//! the sphere of windows, the glass — sits on top and is deliberately kept out of here, so
//! this file stays comparable to any other Smithay compositor and can be read against
//! upstream's examples when the API moves.

use smithay::desktop::{PopupKind, PopupManager, Space};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{Logical, Point, Serial, Transform};
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
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
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
    /// Which process each X11 window belongs to, remembered from when it mapped.
    ///
    /// Kept because a window cannot be identified until its surface arrives, and by then the
    /// map request that carried the process id is long gone.
    pub x11_pids: std::collections::HashMap<u32, Option<u32>>,
    /// Override-redirect X11 windows that are currently on screen: menus, dropdowns,
    /// tooltips, drag icons.
    ///
    /// Kept apart from `space` on purpose. X11 has no notion of a popup that belongs to a
    /// window -- a menu is an ordinary top-level window that the window manager is told not to
    /// touch -- so mapping them as windows put every VLC menu in the room as a separate pane
    /// floating wherever the layout happened to put it. They are drawn instead as popups on
    /// their parent's own surface, which is what they look like everywhere else and what the
    /// Wayland path already does.
    pub x11_popups: Vec<smithay::xwayland::X11Surface>,
    /// The X11 window manager, once the X server has finished starting.
    ///
    /// `None` before then, and for the whole session if no X server could be started — which
    /// is a session where X11 applications do not run, and everything else is unaffected.
    pub xwm: Option<smithay::xwayland::X11Wm>,
    /// The protocol XWayland uses to tell us which surface belongs to which X11 window.
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// The one display clients are told about, and it is not a display.
    ///
    /// Its mode is the size of a *window*, not the size of the glasses' framebuffer, because
    /// Spatiand has no fullscreen: a window is a quad in a room, and the largest thing a
    /// client can ever fill is its own surface. An application that sizes itself from the
    /// screen — which is most media players, Kodi first among them — then picks a resolution
    /// that fits what it was actually given, instead of laying itself out for 3840x1080 and
    /// having the result squeezed onto a quad a fraction of that.
    ///
    /// The scanout outputs are deliberately *not* advertised. They exist for the DRM
    /// compositor's mode source and nothing else.
    pub screen: Output,
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

        // The screen clients see. Refresh is a placeholder until a backend reports the real
        // one; the size is the size every toplevel is offered.
        let screen = Output::new(
            "Spatiand".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Spatiand".into(),
                model: "Window".into(),
            },
        );
        let screen_mode = OutputMode {
            size: DEFAULT_WINDOW_SIZE.into(),
            refresh: 60_000,
        };
        screen.change_current_state(
            Some(screen_mode),
            Some(Transform::Normal),
            None,
            Some((0, 0).into()),
        );
        screen.set_preferred(screen_mode);
        screen.create_global::<Self>(&dh);

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
            space: {
                let mut space = Space::default();
                space.map_output(&screen, (0, 0));
                space
            },
            layout: WindowLayout::default(),
            popups: PopupManager::default(),
            x11_pids: std::collections::HashMap::new(),
            x11_popups: Vec::new(),
            xwm: None,
            xwayland_shell_state,
            screen,
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

    /// What to write on a window's title bar.
    ///
    /// Never empty, and X11 windows say so. Running under XWayland is not a detail the wearer
    /// can be expected to infer from a window misbehaving: X11 support here is a compatibility
    /// path, not a supported one, and the honest thing is for the window itself to say which
    /// it is. See `docs/x11.md`.
    pub fn display_title(&self, window: &smithay::desktop::Window) -> String {
        let own = self
            .title_of(window)
            .filter(|t| !t.trim().is_empty())
            .or_else(|| self.app_id_of(window))
            .unwrap_or_else(|| "Untitled".into());
        if window.x11_surface().is_some() {
            format!("{own}   ·   X11 (unsupported)")
        } else {
            own
        }
    }

    /// The title a client has set.
    ///
    /// Both protocols: an X11 window has no xdg toplevel to read one from, and asking only for
    /// that is how every X11 window came to be labelled "Untitled".
    pub fn title_of(&self, window: &smithay::desktop::Window) -> Option<String> {
        if let Some(x11) = window.x11_surface() {
            let title = x11.title();
            return if title.trim().is_empty() {
                Some(x11.class()).filter(|c| !c.trim().is_empty())
            } else {
                Some(title)
            };
        }
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
        if let Some(x11) = window.x11_surface() {
            // X11's nearest equivalent, and the one desktop files are matched against.
            return Some(x11.class()).filter(|c| !c.trim().is_empty());
        }
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

    /// Every open window, for the switcher.
    ///
    /// Titles rather than handles: the shell has no window type and is not being given one.
    /// A window that has not named itself is listed by what it is rather than left blank --
    /// an unlabelled row in a switcher is indistinguishable from a bug.
    pub fn open_windows(&self) -> Vec<spatiand_shell::WindowEntry> {
        self.space
            .elements()
            .filter_map(|window| {
                let id = self.layout.id_of(window)?;
                let title = self.display_title(window);
                Some(spatiand_shell::WindowEntry {
                    id,
                    title,
                    current: self.layout.is_focused(window),
                })
            })
            .collect()
    }

    /// Tell clients how often the world is redrawn.
    ///
    /// The size stays a window's size; only the rate is real. A media player picking a
    /// deinterlacer or a frame-doubling mode reads this, and 60 when the glasses are running
    /// at 72 is the kind of small lie that shows up as judder.
    pub fn set_screen_refresh(&self, refresh_mhz: i32) {
        let current = self.screen.current_mode();
        if current.map(|m| m.refresh) == Some(refresh_mhz) {
            return;
        }
        let mode = OutputMode {
            size: current.map(|m| m.size).unwrap_or(DEFAULT_WINDOW_SIZE.into()),
            refresh: refresh_mhz,
        };
        self.screen.change_current_state(Some(mode), None, None, None);
        self.screen.set_preferred(mode);
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
            // Matched on the surface itself rather than on an xdg toplevel, because an X11
            // window does not have one. Asking for a toplevel here meant an X11 window never
            // had `on_commit` called, so its buffer state never advanced -- and a window whose
            // buffer never advances has nothing to draw, however correctly everything else
            // about it worked. VLC reached the room, negotiated a surface, and stayed blank.
            use smithay::wayland::seat::WaylandFocus;
            if let Some(window) = self
                .space
                .elements()
                .find(|w| w.wl_surface().is_some_and(|s| *s == root))
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
    /// The pixel size a toplevel is already working at, or the size it was offered.
    ///
    /// Used to answer every "resize me" request, because in a room there is nothing bigger to
    /// resize *to*: the wearer chose how large this window is, and an application asking to
    /// be maximised is asking about a screen that does not exist.
    fn surface_size(toplevel: &ToplevelSurface) -> smithay::utils::Size<i32, Logical> {
        toplevel
            .current_state()
            .size
            .filter(|s| s.w > 0 && s.h > 0)
            .unwrap_or_else(|| DEFAULT_WINDOW_SIZE.into())
    }

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

    /// "Make me as big as the screen."
    ///
    /// Which here means: as big as your window, because that is the whole screen as far as
    /// this compositor is concerned. The state is granted — refusing it makes players hide
    /// their own controls and wait forever for a change that never comes — and the size sent
    /// back is the one the surface already has, so nothing moves and nothing is rescaled.
    /// What the client does with the permission is its own business: Kodi, for one, drops its
    /// window chrome and fills the surface, which is exactly what was wanted.
    ///
    /// The output the client asked for is ignored because there is only one, and it is a
    /// fiction.
    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let size = Self::surface_size(&surface);
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.size = Some(size);
        });
        surface.send_pending_configure();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let size = Self::surface_size(&surface);
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Fullscreen);
            state.size = Some(size);
        });
        surface.send_pending_configure();
    }

    /// Same answer as fullscreen, for the same reason.
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        let size = Self::surface_size(&surface);
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
            state.size = Some(size);
        });
        surface.send_pending_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        let size = Self::surface_size(&surface);
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
            state.size = Some(size);
        });
        surface.send_pending_configure();
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
        let geometry = positioner.get_geometry();
        surface.with_pending_state(|state| {
            state.geometry = geometry;
        });
        // Menus are rare enough to log every one, and a menu that does not appear is the sort
        // of thing where knowing whether the client even asked for it is half the answer.
        log::info!(
            "a client asked for a menu, {}x{} at ({}, {})",
            geometry.size.w,
            geometry.size.h,
            geometry.loc.x,
            geometry.loc.y
        );
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
    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        // Not taken, but worth saying: a toolkit that asks for one is a toolkit that believes
        // it has a valid input serial, which is half of what makes a menu open at all.
        log::info!("a client asked to grab the input for its menu; not granted");
    }

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
        let title = surface.title();
        self.x11_pids.insert(surface.window_id(), pid);
        // Whether XWayland has already told us which surface this window draws into. It can
        // arrive either side of the map request, and a window with none yet is invisible until
        // it does -- so this is the number to look at when a window runs and never appears.
        let has_surface = surface.wl_surface().is_some();
        let window = smithay::desktop::Window::new_x11_window(surface);
        let index = self.space.elements().count() as i32;
        self.space
            .map_element(window.clone(), (index * 32, index * 32), false);
        // Placed only if it can be identified yet, which needs its surface. When that has not
        // arrived, `place_x11_window` does it as soon as it does.
        self.place_x11_window(&window);
        log::info!(
            "new X11 window {:?} at yaw {:.0} deg ({} windows), surface {}",
            title,
            self.spawn_yaw.to_degrees(),
            self.space.elements().count(),
            if has_surface {
                "already attached"
            } else {
                "not attached yet"
            }
        );
    }

    /// Give an X11 window a place in the room, once it can be identified.
    ///
    /// Called both when the window maps and when its surface arrives, because either can come
    /// first. Doing nothing the second time is the normal case.
    pub fn place_x11_window(&mut self, window: &smithay::desktop::Window) {
        if self.layout.get(window).is_some() {
            return;
        }
        if self.layout.id_of(window).is_none() && Self::has_no_surface(window) {
            return;
        }
        self.layout.place(window, self.spawn_yaw);
        self.layout.focus(window);
        if let Some(id) = self.layout.id_of(window) {
            let pid = window
                .x11_surface()
                .and_then(|x| self.x11_pids.get(&x.window_id()).copied())
                .flatten();
            self.arrived_windows.push((id, pid));
            log::info!(
                "X11 window placed at yaw {:.0} deg ({} windows)",
                self.spawn_yaw.to_degrees(),
                self.space.elements().count()
            );
        }
    }

    fn has_no_surface(window: &smithay::desktop::Window) -> bool {
        use smithay::wayland::seat::WaylandFocus;
        window.wl_surface().is_none()
    }

    /// A menu, a dropdown or a tooltip from an X11 application.
    ///
    /// Not given a place in the room: it is drawn on the surface of the window it belongs to,
    /// at the position X put it. See `x11_popups`.
    pub fn adopt_x11_popup(&mut self, surface: smithay::xwayland::X11Surface) {
        if self.x11_popups.iter().any(|s| *s == surface) {
            return;
        }
        let geometry = surface.geometry();
        log::info!(
            "an X11 application opened a menu, {}x{} at ({}, {})",
            geometry.size.w,
            geometry.size.h,
            geometry.loc.x,
            geometry.loc.y
        );
        self.x11_popups.push(surface);
    }

    /// Which window an X11 menu should be drawn on.
    ///
    /// The focused X11 window, and failing that the first one there is. X11 gives a menu no
    /// reliable parent -- an override-redirect window is by definition one nobody is managing
    /// -- so this is a guess, and it is the same guess the wearer is making: menus belong to
    /// the application you are using.
    pub fn x11_menu_host(&self) -> Option<smithay::desktop::Window> {
        let x11: Vec<_> = self
            .space
            .elements()
            .filter(|w| w.x11_surface().is_some())
            .cloned()
            .collect();
        x11.iter()
            .find(|w| self.layout.is_focused(w))
            .or(x11.first())
            .cloned()
    }

    /// An X11 window has gone. Take it out of everything that was tracking it.
    pub fn forget_x11_window(&mut self, surface: &smithay::xwayland::X11Surface) {
        // A menu closing comes through here too, and closes by being unmapped rather than
        // destroyed -- so this runs for surfaces that are still perfectly alive.
        self.x11_popups.retain(|s| s != surface);
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
