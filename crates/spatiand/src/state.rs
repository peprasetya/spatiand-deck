//! Compositor state and the Wayland protocol handlers.
//!
//! This is the plumbing half of Spatiand: it makes us a working Wayland compositor that
//! clients can connect to and post buffers on. What makes it *spatial* — the stereo cameras,
//! the sphere of windows, the glass — sits on top and is deliberately kept out of here, so
//! this file stays comparable to any other Smithay compositor and can be read against
//! upstream's examples when the API moves.

use smithay::desktop::Space;
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
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
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
    pub running: bool,
    pub socket_name: String,

    // --- wayland globals ---
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
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
}

impl Spatiand {
    pub fn new(
        display: &mut Display<Self>,
        loop_handle: &smithay::reexports::calloop::LoopHandle<'static, crate::Runtime>,
    ) -> Self {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
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

        Self {
            display_handle: dh,
            running: true,
            socket_name,
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
            space: Space::default(),
            layout: WindowLayout::default(),
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

    /// The surface under a point, for pointer focus.
    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), smithay::desktop::WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        })
    }
}

// --- compositor ---

impl CompositorHandler for Spatiand {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
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
                .find(|w| w.toplevel().map(|t| t.wl_surface() == &root).unwrap_or(false))
                .cloned()
            {
                window.on_commit();
            }
        }

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

// --- xdg shell ---

impl XdgShellHandler for Spatiand {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
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
        self.layout.place(&window, index as usize);
        log::info!("new toplevel ({} windows)", self.space.elements().count());
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        // Popups render as part of their parent's quad, so nothing spatial to decide.
    }

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
            self.layout.remove(&window);
            self.space.unmap_elem(&window);
        }
    }
}

impl Spatiand {
    /// xdg_surface requires a configure before the client may attach a buffer.
    fn ensure_initial_configure(&mut self, surface: &WlSurface) {
        if let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
            .cloned()
        {
            if let Some(toplevel) = window.toplevel() {
                let initial_configure_sent = smithay::wayland::compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .initial_configure_sent
                });
                if !initial_configure_sent {
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
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: smithay::input::pointer::CursorImageStatus) {}
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
delegate_seat!(Spatiand);
delegate_output!(Spatiand);
delegate_data_device!(Spatiand);
