//! A Wayland compositor with no screen.
//!
//! It is a compositor for the same reason a display server is one: it is the thing
//! applications draw into, and being it is what makes everything else honest. Frames arrive
//! when the application commits them, already on the GPU; input is delivered to one window
//! rather than typed at whatever happens to be focused; and a window's size is negotiated
//! rather than cropped out of a screenshot. None of that is available to something capturing a
//! desktop from outside.
//!
//! There is no output to speak of, no rendering to a display, and no window management.
//! Applications are laid out by the headset at the other end; here they each just have a size.
//!
//! What it deliberately does not have: a shell, a cursor theme, a background, a screen
//! locker, or any notion of a desktop. A host that grew those would be a desktop being
//! streamed, which is the thing this exists not to be.

use std::collections::HashMap;
use std::sync::Arc;

use smithay::backend::allocator::Format;
use smithay::backend::egl::EGLDevice;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportDma;
use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{Serial, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
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
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat,
    delegate_shm, delegate_xdg_decoration, delegate_xdg_shell,
};

use spatiand_stream::WindowId;

/// The size a window is offered before anybody has said how big it looks in the room.
///
/// A number has to be chosen, because an application asked for no size at all picks its own
/// and some pick something enormous. The session replaces it with the size the window really
/// occupies as soon as it has placed it.
const DEFAULT_WINDOW_SIZE: (i32, i32) = (1280, 800);

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

/// One window, as the host tracks it.
pub struct Tracked {
    pub id: WindowId,
    /// Which catalogue entry launched the client this window belongs to.
    pub app: String,
    pub window: Window,
    /// How many times the window has committed a new buffer, and when it last did.
    pub commits: u64,
    pub last_commit: Option<std::time::Instant>,
}

pub struct Host {
    pub display_handle: DisplayHandle,
    pub running: bool,
    pub socket_name: String,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// Kept only so the global stays alive; see the `XdgDecorationHandler` below.
    pub _decorations: smithay::wayland::shell::xdg::decoration::XdgDecorationState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
    /// Held because the globals live as long as it does, not because anything reads it.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    /// Where network input will be delivered from, once there is any.
    #[allow(dead_code)]
    pub seat: Seat<Self>,
    pub output: Output,

    pub space: Space<Window>,
    pub popups: PopupManager,
    /// Windows in the order they were mapped, by the id the wire uses.
    pub windows: Vec<Tracked>,
    next_window_id: u32,
    /// Which catalogue entry a client belongs to, by process id.
    ///
    /// The host knows what it launched, and a window has to be attributed to an application
    /// the moment it is mapped — the session needs to know which launcher bubble it came from,
    /// and which controller layout to use, before the first frame.
    pub app_of_pid: HashMap<i32, String>,
    /// Each launched application's own sink, when there is a network to send sound to.
    pub sounds: Option<crate::audio::Sounds>,
    /// The X11 window manager, once XWayland is ready. See `xwayland`.
    pub xwm: Option<smithay::xwayland::X11Wm>,
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// The X server's display number, for `DISPLAY`.
    pub x11_display: Option<u32>,
    /// Pointer buttons a session has pressed and not released, so they can be let go of if
    /// it leaves while one is down. See `input::release_everything`.
    pub held_buttons: Vec<u32>,
    /// Copy and paste, both ways across the link. See `clipboard`.
    pub clipboard: crate::clipboard::Clipboard,
    /// Where to post to the session, once there is a network. The clipboard needs it from
    /// inside a protocol handler, where nothing else is reachable.
    pub out: Option<tokio::sync::mpsc::UnboundedSender<crate::net::ToSession>>,
    /// The event loop, which serving an X11 selection needs in order to wait on a pipe.
    pub loop_handle: smithay::reexports::calloop::LoopHandle<'static, Host>,
    /// Windows that have appeared or gone since the last time anyone looked.
    pub arrived: Vec<WindowId>,
    pub departed: Vec<WindowId>,
}

impl Host {
    pub fn new(
        display: &mut Display<Self>,
        loop_handle: &smithay::reexports::calloop::LoopHandle<'static, Host>,
    ) -> Host {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state =
            smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, Vec::new());
        // The global waits for a renderer: until there is one, there are no formats that can
        // honestly be advertised. See [`advertise_dmabuf`].
        let dmabuf_state = DmabufState::new();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        // How XWayland says which surface is which X11 window.
        let xwayland_shell_state =
            smithay::wayland::xwayland_shell::XWaylandShellState::new::<Self>(&dh);

        // One seat, fed entirely from the network. Nothing local ever types here.
        let mut seat = seat_state.new_wl_seat(&dh, "spatiand-host");
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("could not create a keyboard");
        seat.add_pointer();

        // Clients need an output to be told about; frame callbacks reference one. Its refresh
        // is a placeholder until a session says what its display does, because pacing
        // applications to the wearer's own vblank is the point of having it at all.
        let output = Output::new(
            "spatiand-host".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Spatiand".into(),
                model: "Remote".into(),
            },
        );
        let mode = OutputMode {
            size: DEFAULT_WINDOW_SIZE.into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
        output.set_preferred(mode);
        output.create_global::<Self>(&dh);

        let socket_name = Self::init_socket(loop_handle);

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        Host {
            display_handle: dh,
            running: true,
            socket_name,
            compositor_state,
            xdg_shell_state,
            shm_state,
            dmabuf_state,
            dmabuf_global: None,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
            output,
            space,
            popups: PopupManager::default(),
            _decorations: xdg_decoration_state,
            windows: Vec::new(),
            next_window_id: 1,
            app_of_pid: HashMap::new(),
            sounds: None,
            xwm: None,
            xwayland_shell_state,
            x11_display: None,
            held_buttons: Vec::new(),
            clipboard: crate::clipboard::Clipboard::new(),
            out: None,
            loop_handle: loop_handle.clone(),
            arrived: Vec::new(),
            departed: Vec::new(),
        }
    }

    /// Open a Wayland socket of our own and register it with the event loop.
    ///
    /// Of our own, and never the session's: an application launched here talks to this
    /// compositor, on this machine. `WAYLAND_DISPLAY` is handed to it at launch.
    fn init_socket(loop_handle: &smithay::reexports::calloop::LoopHandle<'static, Host>) -> String {
        let source = ListeningSocketSource::new_auto().expect("could not open a wayland socket");
        let name = source.socket_name().to_string_lossy().into_owned();
        loop_handle
            .insert_source(source, |stream, _, host| {
                if let Err(e) = host
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState::default()))
                {
                    log::warn!("could not accept a client: {e}");
                }
            })
            .expect("could not register the wayland socket");
        log::info!("host listening on WAYLAND_DISPLAY={name}");
        name
    }

    #[allow(dead_code)]
    pub fn tracked(&self, surface: &WlSurface) -> Option<&Tracked> {
        self.windows
            .iter()
            .find(|t| surface_of(&t.window).as_ref() == Some(surface))
    }

    fn tracked_mut(&mut self, surface: &WlSurface) -> Option<&mut Tracked> {
        self.windows
            .iter_mut()
            .find(|t| surface_of(&t.window).as_ref() == Some(surface))
    }

    /// Keep the screen X11 sees at least as big as the biggest X11 window.
    ///
    /// X11's pointer lives on a screen and cannot leave it. XWayland takes its screen from the
    /// output it is shown, which starts at one window's worth — so Firestorm's 1421x954 login
    /// screen had its Log In button below the 1280x800 edge, and clicks there stopped at the
    /// boundary: the picture was whole, the pointer could not reach the bottom of it. Grown in
    /// steps rather than fitted exactly, because the size is also the answer to "how big is the
    /// display", which Wine hands to a game. Spatiand does the same; see `docs/x11.md`.
    pub fn fit_screen_to_x11(&mut self) {
        const STEP: i32 = 256;
        let up = |n: i32| (n + STEP - 1) / STEP * STEP;
        let mut size = DEFAULT_WINDOW_SIZE;
        for x11 in self.space.elements().filter_map(|w| w.x11_surface()) {
            let geometry = x11.geometry().size;
            size.0 = size.0.max(up(geometry.w));
            size.1 = size.1.max(up(geometry.h));
        }
        let size: smithay::utils::Size<i32, smithay::utils::Physical> = size.into();
        let current = self.output.current_mode();
        if current.map(|m| m.size) == Some(size) {
            return;
        }
        let mode = OutputMode {
            size,
            refresh: current.map(|m| m.refresh).unwrap_or(60_000),
        };
        log::info!("the screen X11 sees is now {}x{}", size.w, size.h);
        self.output.change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
    }

    /// Track a newly mapped X11 window exactly as a new toplevel is tracked.
    pub fn adopt_x11(&mut self, surface: smithay::xwayland::X11Surface) {
        if self
            .windows
            .iter()
            .any(|t| t.window.x11_surface() == Some(&surface))
        {
            return;
        }
        // `_NET_WM_PID`, which most clients set; one that does not is still served.
        let app = surface
            .pid()
            .map(|pid| self.app_of_process(pid as i32))
            .unwrap_or_else(|| "unknown".into());
        let window = Window::new_x11_window(surface);
        self.space.map_element(window.clone(), (0, 0), false);
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        log::info!("window {} opened for {app} (X11)", id.0);
        self.windows.push(Tracked {
            id,
            app,
            window,
            commits: 0,
            last_commit: None,
        });
        self.arrived.push(id);
    }

    pub fn forget_x11(&mut self, surface: &smithay::xwayland::X11Surface) {
        let gone: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|t| t.window.x11_surface() == Some(surface))
            .map(|t| t.id)
            .collect();
        for window in self
            .windows
            .iter()
            .filter(|t| gone.contains(&t.id))
            .map(|t| t.window.clone())
            .collect::<Vec<_>>()
        {
            self.space.unmap_elem(&window);
        }
        self.windows.retain(|t| !gone.contains(&t.id));
        for id in gone {
            log::info!("window {} closed", id.0);
            self.departed.push(id);
        }
    }

    /// Which application a client belongs to, worked out from the process that connected.
    ///
    /// A client that was not launched from the catalogue — something a launched application
    /// started for itself, or anything else that found the socket — is still served, and is
    /// attributed to whatever its parent was launched as when that can be seen.
    fn app_for(&self, client: Option<&Client>) -> String {
        let Some(client) = client else {
            return "unknown".into();
        };
        let Some(credentials) = client.get_credentials(&self.display_handle).ok() else {
            return "unknown".into();
        };
        self.app_of_process(credentials.pid)
    }

    /// Which catalogue entry a process belongs to, looking up through its parents.
    fn app_of_process(&self, pid: i32) -> String {
        let mut pid = pid;
        // Up to a few generations: a game is often a launcher that started a script that
        // started the binary, and the window belongs to the application either way.
        for _ in 0..8 {
            if let Some(app) = self.app_of_pid.get(&pid) {
                return app.clone();
            }
            match parent_of(pid) {
                Some(parent) if parent > 1 => pid = parent,
                _ => break,
            }
        }
        "unknown".into()
    }

    /// Offer dmabuf in whatever formats this renderer can import.
    ///
    /// Called once, after the renderer exists. Clients without this fall back to shared memory,
    /// which for a 3D application means reading every frame back off the GPU — the one thing
    /// this design cannot afford.
    pub fn advertise_dmabuf(&mut self, renderer: &GlesRenderer) {
        if self.dmabuf_global.is_some() {
            return;
        }
        // The device is asked of the renderer rather than of the GBM handle, because what a
        // client needs is the *render* node its buffers will be imported on, and the renderer
        // is the only thing that can say which that is.
        let node = match EGLDevice::device_for_display(renderer.egl_context().display())
            .and_then(|device| device.try_get_render_node())
        {
            Ok(Some(node)) => Some(node),
            Ok(None) => {
                log::warn!("EGL names no render node for this display");
                None
            }
            Err(e) => {
                log::warn!("could not identify this renderer's EGL device ({e})");
                None
            }
        };
        let formats: Vec<Format> = renderer.dmabuf_formats().into_iter().collect();
        if formats.is_empty() {
            log::warn!("this renderer imports no dmabuf formats; clients will fall back to shm");
            return;
        }
        let feedback = node.map(|node| {
            smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(node.dev_id(), formats.clone())
                .build()
        });
        let global = match feedback {
            Some(Ok(feedback)) => {
                log::info!(
                    "offering dmabuf v4, {} format/modifier pair(s)",
                    formats.len()
                );
                self.dmabuf_state
                    .create_global_with_default_feedback::<Host>(&self.display_handle, &feedback)
            }
            other => {
                // Worth saying loudly: with no device named, a GL client has no way to find
                // the GPU and quietly renders in software, which looks like "the host is slow"
                // rather than like a missing line of setup.
                if let Some(Err(e)) = other {
                    log::warn!("could not build dmabuf feedback ({e})");
                }
                log::warn!(
                    "offering dmabuf v3 with no device; clients may render in software"
                );
                self.dmabuf_state
                    .create_global::<Host>(&self.display_handle, formats.clone())
            }
        };
        self.dmabuf_global = Some(global);
    }
}

/// The parent process of a pid, or `None` if it cannot be read.
pub fn parent_of(pid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
}

// --- compositor ---

impl CompositorHandler for Host {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(ours) = client.get_data::<ClientState>() {
            return &ours.compositor_state;
        }
        // XWayland is a client smithay inserts with data of its own. Unwrapping ours here
        // panicked inside a Wayland callback, which cannot unwind, the moment the X server
        // connected — and systemd restarted the host into the same death every three seconds.
        if let Some(xwayland) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &xwayland.compositor_state;
        }
        static UNACCOUNTED: std::sync::OnceLock<CompositorClientState> = std::sync::OnceLock::new();
        log::warn!("a client arrived with no compositor state of its own");
        UNACCOUNTED.get_or_init(CompositorClientState::default)
    }

    fn commit(&mut self, surface: &WlSurface) {
        // A subsurface that is in sync with its parent has not produced anything to send on
        // its own; the parent's commit is the one that matters.
        if is_sync_subsurface(surface) {
            return;
        }
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        // A popup is not a subsurface: it belongs to its window by the popup tree instead.
        // Without this a menu opening changed nothing that counted, so it was never sent.
        if let Some(popup) = self.popups.find_popup(&root) {
            // And a popup is only shown once it has been told where it is. xdg-shell says
            // that answer follows the popup's first commit, so this is where it goes.
            if let smithay::desktop::PopupKind::Xdg(ref xdg) = popup {
                if !xdg.is_initial_configure_sent() {
                    if let Err(e) = xdg.send_configure() {
                        log::warn!("could not configure a popup: {e}");
                    }
                }
            }
            if let Ok(owner) = smithay::desktop::find_popup_root_surface(&popup) {
                root = owner;
            }
        }
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);
        if let Some(window) = self
            .space
            .elements()
            .find(|w| surface_of(w).as_ref() == Some(&root))
            .cloned()
        {
            window.on_commit();
        }
        self.popups.commit(surface);
        if let Some(tracked) = self.tracked_mut(&root) {
            tracked.commits += 1;
            tracked.last_commit = Some(std::time::Instant::now());
        }
    }
}

impl Host {
    /// Where a popup goes, kept within its window's rectangle.
    fn unconstrained(
        &self,
        popup: &PopupSurface,
        positioner: &PositionerState,
    ) -> smithay::utils::Rectangle<i32, smithay::utils::Logical> {
        let kind = smithay::desktop::PopupKind::Xdg(popup.clone());
        let Ok(root) = smithay::desktop::find_popup_root_surface(&kind) else {
            return positioner.get_geometry();
        };
        let Some(window) = self
            .windows
            .iter()
            .find(|t| surface_of(&t.window).as_ref() == Some(&root))
        else {
            return positioner.get_geometry();
        };
        // The window's rectangle, in the coordinates of this popup's parent.
        let mut target = smithay::utils::Rectangle::from_size(window.window.geometry().size);
        target.loc -= smithay::desktop::get_popup_toplevel_coords(&kind);
        positioner.get_unconstrained_geometry(target)
    }
}

impl BufferHandler for Host {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for Host {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for Host {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Accepted here and proved by the encoder on the next frame. A client told its buffer
        // is fine and then never asked for another one is the worst of both answers, so the
        // import is not refused blindly — but nor is it claimed to have worked before anything
        // has touched it.
        let _ = notifier.successful::<Host>();
    }
}

// --- shell ---

impl XdgShellHandler for Host {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());
        // Somewhere to be. The space's coordinates mean nothing here — the session decides
        // where a window is in the room — but smithay's window bookkeeping wants a position.
        self.space.map_element(window.clone(), (0, 0), false);
        surface.with_pending_state(|state| {
            state.size = Some(DEFAULT_WINDOW_SIZE.into());
        });
        surface.send_configure();

        let client = surface.wl_surface().client();
        let app = self.app_for(client.as_ref());
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        log::info!("window {} opened for {app}", id.0);
        self.windows.push(Tracked {
            id,
            app,
            window,
            commits: 0,
            last_commit: None,
        });
        self.arrived.push(id);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // **Kept inside its window.** A context menu is drawn into the window's own picture —
        // there is nowhere else for it to go — so one that hangs past the edge would be cut
        // off. The positioner's own rules (flip, slide, resize) move it back inside, which is
        // what a desktop does at the edge of the screen.
        let geometry = self.unconstrained(&surface, &positioner);
        log::debug!("a menu opened at {:?}", geometry);
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
        if let Err(e) = self.popups.track_popup(smithay::desktop::PopupKind::Xdg(surface)) {
            log::warn!("could not track a popup: {e}");
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        let geometry = self.unconstrained(&surface, &positioner);
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let gone: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|t| t.window.toplevel().is_some_and(|s| s == &surface))
            .map(|t| t.id)
            .collect();
        self.windows.retain(|t| !gone.contains(&t.id));
        for id in gone {
            log::info!("window {} closed", id.0);
            self.departed.push(id);
        }
    }
}

// --- seat, selection, output ---

impl SeatHandler for Host {
    type KeyboardFocus = KeyboardFocus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    /// **Give the clipboard to whoever has the keyboard.**
    ///
    /// This was an empty stub, and the whole of why copy and paste worked nowhere: without it
    /// no application is ever the data device's focus, so none is told what is on the
    /// clipboard and none may put anything there. Every application could still paste its own
    /// copy — that never leaves its own process — which makes the failure look like each
    /// application having a private clipboard rather than like a compositor serving none.
    ///
    /// A client with the keyboard may read the selection. That is the rule the protocol sets,
    /// and it is why this hangs off focus rather than being granted outright.
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&KeyboardFocus>) {
        use smithay::wayland::seat::WaylandFocus;
        let client = focused
            .and_then(|focus| focus.wl_surface().map(|s| s.into_owned()))
            .and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        smithay::wayland::selection::data_device::set_data_device_focus(
            &self.display_handle,
            seat,
            client,
        );
    }
}

impl SelectionHandler for Host {
    type SelectionUserData = ();

    /// An application here copied something, or stopped offering what it had.
    fn new_selection(
        &mut self,
        ty: smithay::wayland::selection::SelectionTarget,
        source: Option<smithay::wayland::selection::SelectionSource>,
        _seat: Seat<Self>,
    ) {
        // The primary selection — middle-click paste — is deliberately left alone. It changes
        // with every drag of a mouse over text, and sending that across a link would be a
        // stream of announcements nobody asked for.
        if ty != smithay::wayland::selection::SelectionTarget::Clipboard {
            return;
        }
        let Some(out) = self.out.as_ref() else { return };
        let _ = out;
        if let Some(source) = source {
            self.clipboard.copied_here(source, self.xwm.as_mut());
        }
    }

    /// Something here is pasting what the session holds.
    fn send_selection(
        &mut self,
        ty: smithay::wayland::selection::SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if ty != smithay::wayland::selection::SelectionTarget::Clipboard {
            return;
        }
        let Some(out) = self.out.clone() else { return };
        self.clipboard.paste_here(
            mime_type,
            fd,
            &self.seat,
            self.xwm.as_mut(),
            &self.loop_handle,
            &out,
        );
    }
}

impl DataDeviceHandler for Host {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Host {}
impl ServerDndGrabHandler for Host {}

impl smithay::wayland::output::OutputHandler for Host {}

delegate_compositor!(Host);
delegate_shm!(Host);
delegate_dmabuf!(Host);
delegate_xdg_shell!(Host);
delegate_xdg_decoration!(Host);

// --- decorations ---
//
// **The headset draws every window's frame, so no application should.** Without this protocol
// a toolkit assumes it must decorate itself: winit (the settings app) draws a title bar and a
// border, and Chrome draws a shadow around its window — margins that end up inside the picture
// the headset receives, under the headset's own title bar. Saying "the server decorates" to
// every window, whatever it asks for, removes them.
impl smithay::wayland::shell::xdg::decoration::XdgDecorationHandler for Host {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        server_side(&toplevel);
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        _mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    ) {
        server_side(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        server_side(&toplevel);
    }
}

fn server_side(toplevel: &ToplevelSurface) {
    use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
    toplevel.with_pending_state(|state| state.decoration_mode = Some(Mode::ServerSide));
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}
delegate_seat!(Host);
delegate_output!(Host);
delegate_data_device!(Host);

/// The surface a window draws into, whichever protocol it speaks. An X11 window has no xdg
/// toplevel, and every lookup that asked for one quietly found nothing — see `docs/x11.md`.
pub fn surface_of(window: &Window) -> Option<WlSurface> {
    if let Some(x11) = window.x11_surface() {
        return x11.wl_surface();
    }
    window.toplevel().map(|t| t.wl_surface().clone())
}

smithay::delegate_xwayland_shell!(Host);

/// What the keyboard is given: a Wayland surface, or an X11 window itself.
///
/// Not the X11 window's surface. Keys sent to that reach the X server and stop there, because X
/// hands a key to whichever of its own windows holds its input focus, and only focusing the X11
/// window sets that — along with `WM_TAKE_FOCUS` and the focused state, which Wine reads to
/// decide whether it is in the foreground. See "Keys went nowhere" in `docs/x11.md`.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocus {
    Wayland(WlSurface),
    X11(smithay::xwayland::X11Surface),
}

impl smithay::utils::IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            KeyboardFocus::Wayland(surface) => surface.alive(),
            KeyboardFocus::X11(x11) => x11.alive(),
        }
    }
}

impl smithay::wayland::seat::WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<std::borrow::Cow<'_, WlSurface>> {
        use smithay::wayland::seat::WaylandFocus;
        match self {
            KeyboardFocus::Wayland(surface) => Some(std::borrow::Cow::Borrowed(surface)),
            KeyboardFocus::X11(x11) => WaylandFocus::wl_surface(x11),
        }
    }
}

impl smithay::input::keyboard::KeyboardTarget<Host> for KeyboardFocus {
    fn enter(
        &self,
        seat: &Seat<Host>,
        data: &mut Host,
        keys: Vec<smithay::input::keyboard::KeysymHandle<'_>>,
        serial: Serial,
    ) {
        use smithay::input::keyboard::KeyboardTarget;
        match self {
            KeyboardFocus::Wayland(surface) => {
                KeyboardTarget::enter(surface, seat, data, keys, serial)
            }
            KeyboardFocus::X11(x11) => KeyboardTarget::enter(x11, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<Host>, data: &mut Host, serial: Serial) {
        use smithay::input::keyboard::KeyboardTarget;
        match self {
            KeyboardFocus::Wayland(surface) => KeyboardTarget::leave(surface, seat, data, serial),
            KeyboardFocus::X11(x11) => KeyboardTarget::leave(x11, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<Host>,
        data: &mut Host,
        key: smithay::input::keyboard::KeysymHandle<'_>,
        state: smithay::backend::input::KeyState,
        serial: Serial,
        time: u32,
    ) {
        use smithay::input::keyboard::KeyboardTarget;
        match self {
            KeyboardFocus::Wayland(surface) => {
                KeyboardTarget::key(surface, seat, data, key, state, serial, time)
            }
            KeyboardFocus::X11(x11) => KeyboardTarget::key(x11, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<Host>,
        data: &mut Host,
        modifiers: smithay::input::keyboard::ModifiersState,
        serial: Serial,
    ) {
        use smithay::input::keyboard::KeyboardTarget;
        match self {
            KeyboardFocus::Wayland(surface) => {
                KeyboardTarget::modifiers(surface, seat, data, modifiers, serial)
            }
            KeyboardFocus::X11(x11) => KeyboardTarget::modifiers(x11, seat, data, modifiers, serial),
        }
    }
}
