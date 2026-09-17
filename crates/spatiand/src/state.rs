//! Compositor state and the Wayland protocol handlers.
//!
//! This is the plumbing half of Spatiand: it makes us a working Wayland compositor that
//! clients can connect to and post buffers on. What makes it *spatial* — the stereo cameras,
//! the sphere of windows, the glass — sits on top and is deliberately kept out of here, so
//! this file stays comparable to any other Smithay compositor and can be read against
//! upstream's examples when the API moves.

use smithay::desktop::{PopupKind, PopupManager, Space};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::input::keyboard::KeyboardTarget;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{Logical, Point, Serial, Size, Transform};
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
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output,
    delegate_presentation, delegate_seat, delegate_shm, delegate_viewporter,
    delegate_xdg_decoration, delegate_xdg_shell,
};

use smithay::input::pointer::PointerHandle;
use smithay::wayland::pointer_constraints::{
    with_pointer_constraint, PointerConstraintsHandler, PointerConstraintsState,
};
use smithay::wayland::relative_pointer::RelativePointerManagerState;

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
    /// When a frame actually reached the glass — see [`crate::state::Spatiand::presented`].
    ///
    /// Held only so the global outlives the compositor; nothing is read out of it.
    pub _presentation_state: smithay::wayland::presentation::PresentationState,
    /// `wp_viewporter`: a surface saying its buffer is not its size. Held for the global's
    /// lifetime only; the state it produces is read through each surface's renderer state.
    pub _viewporter_state: smithay::wayland::viewporter::ViewporterState,
    /// `zwp_relative_pointer_v1`: mouse motion as movement rather than a position, which is what
    /// a game turning its camera reads. Held for the global's lifetime only.
    pub _relative_pointer_state: RelativePointerManagerState,
    /// `zwp_pointer_constraints_v1`: a game locking the pointer in place while it looks around.
    pub _pointer_constraints_state: PointerConstraintsState,
    /// Handing us a picture rather than a copy of one — see [`crate::dmabuf`].
    pub dmabuf_state: DmabufState,
    /// `None` until a backend has a renderer whose import formats can be advertised.
    pub dmabuf_global: Option<DmabufGlobal>,
    /// Buffers a client has offered and is waiting to hear about.
    ///
    /// Answered from the frame loop, which is the only place with a renderer to test them
    /// against. Same shape, and the same reason, as `arrived_windows`.
    pub pending_dmabufs: Vec<(smithay::backend::allocator::dmabuf::Dmabuf, ImportNotifier)>,
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
    /// What the keyboard was last pointed at, so the reconcile below only acts on a change.
    ///
    /// See [`Spatiand::settle_keyboard_focus`].
    pub focus_settled: Option<KeyboardFocus>,
    /// How evenly the window in front is drawing. See [`crate::cadence`].
    pub cadence: crate::cadence::Cadence,
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
    /// Surfaces a client has extended with `spatiand_xr_v1`, and the object that did it.
    ///
    /// Kept as a list rather than a map because it is walked once per commit and is never
    /// more than a handful long — a client extends the surfaces it draws stereoscopically,
    /// not every surface it has.
    pub xr_surfaces: Vec<(
        WlSurface,
        spatiand_proto::server::spatiand_xr_surface_v1::SpatiandXrSurfaceV1,
    )>,
    /// The surface that has taken the environment, if any.
    ///
    /// There is one room and it can only be one thing, so this is exclusive and first come
    /// first served. Claimed when the request arrives rather than when it commits, because
    /// two clients asking in the same frame have to get different answers.
    pub sky_owner: Option<WlSurface>,
    /// Clients waiting to be handed the shared-memory pose channel.
    pub pose_clients:
        Vec<spatiand_proto::server::spatiand_xr_pose_channel_v1::SpatiandXrPoseChannelV1>,
    /// A client has asked for poses and has not been answered yet.
    ///
    /// Answered from the frame loop, because only the backend knows whether there is a head
    /// being tracked — the same reason dmabuf imports are answered there.
    pub pose_channels_to_open: bool,
    /// Whether the wearer is busy, and the alpha that follows from it.
    ///
    /// Advanced once a frame by the backend, which is the only place with a head pose and a
    /// frame time. Read by the scene when it builds a surface that asked for `set_idle_fade`.
    pub attention: crate::attention::Attention,
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
        // The global itself waits for a renderer; see `crate::dmabuf::advertise`.
        let dmabuf_state = DmabufState::new();
        // `wp_presentation`: when a frame was actually shown, and on which vblank.
        //
        // A player asked for this and the reason is worth recording, because "we already tell
        // you the refresh rate" sounds like an answer and is not. The refresh rate says how
        // often frames *can* appear. It cannot distinguish a frame that arrived late from one
        // that was dropped entirely, and those want opposite corrections: a late frame means
        // present sooner, a dropped frame means the pipeline is over budget and something has
        // to give. The sequence number here is what tells them apart.
        //
        // CLOCK_MONOTONIC, which is the clock a frame callback's time already comes from.
        let presentation_state = smithay::wayland::presentation::PresentationState::new::<Self>(
            &dh,
            libc::CLOCK_MONOTONIC as u32,
        );
        // `wp_viewporter`: a buffer that is not the surface's size.
        //
        // Asked for by a player whose window is side by side for its whole life. Each eye
        // samples half the buffer, so a 1280-wide window gave each eye 640 pixels across a
        // panel the glasses show at about 1350 -- and there was no way out from the client's
        // side: a buffer twice as wide was drawn twice as wide, and one twice as tall was
        // minified two to one and lost its thin strokes. With a destination the client commits
        // 2560x800 and says it is 1280x800; shape and pointer coordinates come from the second,
        // sampling from the first, and each eye gets 1280 pixels one to one.
        //
        // A client that never binds it is untouched: its surface is its buffer, as before.
        let viewporter_state = smithay::wayland::viewporter::ViewporterState::new::<Self>(&dh);
        // Mouse-look. A game driven by a layout's mouse output -- a trackpad or the gyro as a
        // mouse -- turns its camera by relative motion and locks the pointer so it never reaches
        // the edge of the window. Without these, XWayland has nothing to turn a pointer grab
        // into and the camera stops at the window's border.
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);

        // One seat. The Deck's controller, its touchscreen and any USB keyboard all feed
        // this; the spatial input router decides what reaches a client.
        let mut seat = seat_state.new_wl_seat(&dh, "spatiand");
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("failed to create keyboard");
        seat.add_pointer();

        // Stereo, head-locked and immersive surfaces. Binding it says nothing and changes
        // nothing: an application that ignores it is an ordinary window, which is the whole
        // design. See `crate::xr` and the protocol XML.
        dh.create_global::<Self, spatiand_proto::server::spatiand_xr_v1::SpatiandXrV1, _>(3, ());

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
            _presentation_state: presentation_state,
            _viewporter_state: viewporter_state,
            _relative_pointer_state: relative_pointer_state,
            _pointer_constraints_state: pointer_constraints_state,
            dmabuf_state,
            dmabuf_global: None,
            pending_dmabufs: Vec::new(),
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
            focus_settled: None,
            cadence: Default::default(),
            xwm: None,
            xwayland_shell_state,
            xr_surfaces: Vec::new(),
            sky_owner: None,
            pose_clients: Vec::new(),
            pose_channels_to_open: false,
            attention: crate::attention::Attention::default(),
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
    /// Never empty: a window with no title of its own is labelled by its application, and
    /// failing that as untitled.
    ///
    /// X11 windows used to be marked "unsupported" here, as a warning that they were running
    /// through a compatibility layer that did not do everything. They are no longer marked,
    /// because it is no longer true: closing, focusing, resizing, menus and process ids all
    /// work through XWayland now, and a label saying otherwise only makes a working window
    /// look broken.
    pub fn display_title(&self, window: &smithay::desktop::Window) -> String {
        self.title_of(window)
            .filter(|t| !t.trim().is_empty())
            .or_else(|| self.app_id_of(window))
            .unwrap_or_else(|| "Untitled".into())
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
    /// The process a window belongs to: the Wayland connection's credentials, or for an X11
    /// window the process id remembered when it mapped.
    pub fn pid_of(&self, window: &smithay::desktop::Window) -> Option<u32> {
        if let Some(x11) = window.x11_surface() {
            return self.x11_pids.get(&x11.window_id()).copied().flatten();
        }
        use smithay::reexports::wayland_server::Resource;
        window
            .toplevel()?
            .wl_surface()
            .client()?
            .get_credentials(&self.display_handle)
            .ok()
            .map(|c| c.pid as u32)
    }

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
    ///
    /// Both protocols, and the X11 half was missing. An X11 window has no xdg toplevel, so
    /// asking only for one found nothing and sent nothing: the close button on VLC -- which
    /// only reaches us through XWayland -- did nothing at all, and the only way out was the
    /// application's own menu. X11's request is `WM_DELETE_WINDOW`, which is the same polite
    /// question; smithay destroys the window outright for the rare client that never said it
    /// understands one, which is what every X window manager does.
    pub fn close_window(&self, window: &smithay::desktop::Window) {
        if let Some(x11) = window.x11_surface() {
            if let Err(e) = x11.close() {
                log::warn!("could not ask an X11 window to close: {e}");
            }
        } else if let Some(toplevel) = window.toplevel() {
            toplevel.send_close();
        }
    }

    /// Ask a window for a buffer of this many pixels.
    ///
    /// Only when the size is new: a resize drag asks every frame, and a configure the client
    /// has already been sent is one more thing for it to answer for no reason.
    ///
    /// Both protocols, for the same reason as [`Self::close_window`]. Asking only an xdg
    /// toplevel meant dragging an X11 window's corner grew its frame and left the application
    /// drawing at its old size inside it -- stretched, because the quad is sized by the
    /// wearer and the pixels by the client. X11 is told its size directly, at the origin like
    /// every other X11 configure here, because there is no screen for a position to be on.
    pub fn request_size(
        &self,
        window: &smithay::desktop::Window,
        size: smithay::utils::Size<i32, Logical>,
    ) {
        if let Some(x11) = window.x11_surface() {
            if x11.geometry().size != size {
                if let Err(e) = x11.configure(smithay::utils::Rectangle::new((0, 0).into(), size))
                {
                    log::warn!("could not resize an X11 window: {e}");
                }
            }
        } else if let Some(toplevel) = window.toplevel() {
            if toplevel.current_state().size != Some(size) {
                toplevel.with_pending_state(|s| s.size = Some(size));
                toplevel.send_pending_configure();
            }
        }
    }

    /// Give a window focus, and bring it to the front.
    ///
    /// Takes the window rather than a position, because raising reorders `Space::elements()`
    /// and any index taken before the raise refers to something else afterwards.
    ///
    /// An X11 window's keyboard focus is the surface XWayland draws it into. Looking only for
    /// an xdg toplevel meant an X11 window could be clicked, raised and highlighted and still
    /// never be given the keyboard, which stayed with whatever Wayland window had it last --
    /// and XWayland is sent no keys at all while none of its surfaces has focus. It is also
    /// raised in X's own stacking order, which is separate from ours and which nothing else
    /// here touches: every X11 window is configured at the same X origin, so they all overlap
    /// as far as X is concerned, and its idea of which is on top should be ours.
    /// Make the seat's keyboard focus agree with the window the layout says is focused.
    ///
    /// Run once a frame, and it closes a gap that was there from the beginning: a window
    /// arriving took focus in the *layout* — which is what draws the highlight, aims the
    /// pointer and chooses the controller mapping — and nothing told the seat. Only a click
    /// did that. So an application launched from the launcher and never clicked inside had no
    /// keyboard focus at all: a Bluetooth keyboard typed into the window that was in front
    /// before it, or into nothing, and a game with the X11 focus never set believed it was in
    /// the background and ignored its controller too.
    ///
    /// Reconciled against the last focus this set rather than against the seat's current one,
    /// because a client that has taken a keyboard grab for a menu *should* hold the keyboard
    /// and its grab declines to give it back. Comparing against the seat would disagree with
    /// the grab every frame and ask for the focus again on each one.
    pub fn settle_keyboard_focus(&mut self) {
        let focused = self
            .space
            .elements()
            .find(|w| self.layout.is_focused(w))
            .cloned();
        let wanted = focused.as_ref().and_then(Self::keyboard_target);
        if wanted == self.focus_settled {
            return;
        }
        match focused.filter(|_| wanted.is_some()) {
            Some(window) => self.focus_window(&window),
            // Nothing left to type into. Saying so matters for an X11 window: the focus it was
            // given lives in the X server and outlives the window unless it is taken back.
            None => {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.set_focus(self, None, smithay::utils::SERIAL_COUNTER.next_serial());
                }
                self.focus_settled = None;
            }
        }
    }

    /// What the seat should be given for a window: the X11 window itself where there is one.
    fn keyboard_target(window: &smithay::desktop::Window) -> Option<KeyboardFocus> {
        if let Some(x11) = window.x11_surface() {
            // An X11 window with no surface yet cannot be typed into: XWayland has nothing to
            // deliver to. It gets the keyboard on the frame after its surface arrives.
            return x11
                .wl_surface()
                .map(|_| KeyboardFocus::X11(x11.clone()));
        }
        window
            .toplevel()
            .map(|t| KeyboardFocus::Wayland(t.wl_surface().clone()))
    }

    pub fn focus_window(&mut self, window: &smithay::desktop::Window) {
        self.layout.focus(window);
        self.space.raise_element(window, true);
        if let Some(x11) = window.x11_surface() {
            if let Some(wm) = self.xwm.as_mut() {
                if let Err(e) = wm.raise_window(x11) {
                    log::warn!("could not raise an X11 window: {e}");
                }
            }
        }
        let target = Self::keyboard_target(window);
        if let Some(target) = target {
            let Some(keyboard) = self.seat.get_keyboard() else {
                return;
            };
            // The window losing the keyboard is told so, which for an X11 window means taking
            // `_NET_WM_STATE_FOCUSED` off it. Smithay's `leave` gives back X's input focus but
            // not that property, and a Wine window left wearing it believes it is still in
            // front: two windows both convinced they are focused is how a game ends up
            // ignoring a keyboard that is being typed on.
            if let Some(KeyboardFocus::X11(old)) = keyboard.current_focus() {
                if KeyboardFocus::X11(old.clone()) != target {
                    if let Err(e) = old.set_activated(false) {
                        log::warn!("could not unfocus an X11 window: {e}");
                    }
                }
            }
            if let KeyboardFocus::X11(x11) = &target {
                if let Err(e) = x11.set_activated(true) {
                    log::warn!("could not activate an X11 window: {e}");
                }
            }
            self.focus_settled = Some(target.clone());
            keyboard.set_focus(
                self,
                Some(target),
                smithay::utils::SERIAL_COUNTER.next_serial(),
            );
        }
    }

    /// Tell every surface it may draw the next frame -- menus included.
    ///
    /// The menus are the point. A frame callback is how a toolkit is told "your last frame was
    /// shown, paint the next one", and Qt will not paint a surface until it has had one.
    /// Sending them only to windows meant a menu was created, configured, and then waited for
    /// ever for permission to draw: the log said the client had asked for a menu and committed
    /// nothing, and from inside the headset it looked exactly like a button that did nothing.
    ///
    /// A popup is a separate surface with its own callbacks, so a window's own `send_frame`
    /// does not reach it -- it walks the window's subsurfaces and stops there. This is the
    /// whole difference, and it took a client written by hand to find it: that one painted
    /// immediately without waiting to be asked, which is why it worked and every real
    /// application did not.
    pub fn send_frames(&self, output: &Output, time: std::time::Duration) {
        use smithay::desktop::utils::send_frames_surface_tree;
        use smithay::wayland::seat::WaylandFocus;
        for window in self.space.elements() {
            window.send_frame(output, time, Some(std::time::Duration::ZERO), |_, _| {
                Some(output.clone())
            });
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            for (popup, _) in smithay::desktop::PopupManager::popups_for_surface(&surface) {
                send_frames_surface_tree(
                    popup.wl_surface(),
                    output,
                    time,
                    Some(std::time::Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
        }
    }

    /// Collect every `wp_presentation` feedback a client has committed and is waiting on.
    ///
    /// Taken when the frame is queued and answered when the flip completes, because those are
    /// two different moments and the whole value of the protocol is in the gap between them.
    /// Answering at queue time would report the compositor's intention rather than the
    /// display's behaviour, which is the thing the client already knows.
    pub fn take_presentation_feedback(
        &self,
    ) -> Vec<smithay::wayland::presentation::PresentationFeedbackCallback> {
        use smithay::desktop::utils::with_surfaces_surface_tree;
        use smithay::wayland::seat::WaylandFocus;
        let mut out = Vec::new();
        let mut take = |surface: &WlSurface| {
            with_surfaces_surface_tree(surface, |_surface, states| {
                let mut cached = states
                    .cached_state
                    .get::<smithay::wayland::presentation::PresentationFeedbackCachedState>();
                out.append(&mut cached.current().callbacks);
            });
        };
        for window in self.space.elements() {
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            take(&surface);
            for (popup, _) in smithay::desktop::PopupManager::popups_for_surface(&surface) {
                take(popup.wl_surface());
            }
        }
        out
    }

    /// Every open window, for the switcher.
    ///
    /// Titles rather than handles: the shell has no window type and is not being given one.
    /// A window that has not named itself is listed by what it is rather than left blank --
    /// an unlabelled row in a switcher is indistinguishable from a bug.
    pub fn open_windows(&self) -> Vec<spatiand_shell::WindowEntry> {
        self.space
            .elements()
            .filter(|w| !Self::is_environment(w))
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

    /// Whether this window has become the room rather than a thing in it.
    ///
    /// See [`crate::xr::XrState::is_environment`]. A window holding an immersive layer has no
    /// panel, cannot be pointed at and cannot be switched to, so counting it produced a status
    /// bar reading "2 windows" for a player showing one film — which is the normal shape for
    /// immersive video, not an odd one.
    pub fn is_environment(window: &smithay::desktop::Window) -> bool {
        use smithay::wayland::seat::WaylandFocus;
        window
            .wl_surface()
            .map(|s| crate::xr::state_of(&s).is_environment())
            .unwrap_or(false)
    }

    /// How many windows the wearer would say are open.
    ///
    /// Not `space.elements().count()`, which counts the sky.
    pub fn window_count(&self) -> usize {
        self.space
            .elements()
            .filter(|w| !Self::is_environment(w))
            .count()
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

    /// Keep the screen at least as big as the biggest X11 window.
    ///
    /// X11 has a screen and a pointer that lives on it, and a pointer cannot leave it. XWayland
    /// takes its screen from the outputs it is shown, which here is one output the size of a
    /// window — so an X11 window dragged bigger than that had a corner the pointer could not
    /// reach. Measured on the Deck: a window 1804x1174 on a screen `xdpyinfo` reported as
    /// 1280x800, with the compositor sending a click at 1786,1162 and the X server putting the
    /// cursor wherever its edge was instead. The application draws its own cursor from what the
    /// X server tells it, which is why the window resized and the pointer did not follow.
    ///
    /// Grown to fit rather than simply made huge, because the screen's size is also the answer
    /// to "how big is the display": Wine hands it to a Windows game as the desktop size, and a
    /// game that opens at the desktop size would then open at whatever arbitrary maximum was
    /// chosen here. A screen that is exactly as big as the largest window is both true and the
    /// smallest thing that works.
    ///
    /// It shrinks back, so closing a large window does not leave every game after it opening at
    /// that size for the rest of the session.
    pub fn fit_screen_to_windows(&mut self) {
        // A mode is in physical pixels and a window's geometry is logical, and here they are
        // the same number: a surface's pixels are stretched across a quad, never scaled.
        let size: Size<i32, smithay::utils::Physical> = screen_size_for(
            self.space
                .elements()
                .filter_map(|w| w.x11_surface())
                .map(|x11| {
                    let size = x11.geometry().size;
                    (size.w, size.h)
                }),
        )
        .into();
        let current = self.screen.current_mode();
        if current.map(|m| m.size) == Some(size) {
            return;
        }
        let mode = OutputMode {
            size,
            refresh: current.map(|m| m.refresh).unwrap_or(60_000),
        };
        log::info!("the screen X11 sees is now {}x{}", size.w, size.h);
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
                if self.layout.is_focused(&window) {
                    self.cadence.drew(&root, std::time::Instant::now());
                }
            }
        }

        // Whatever the client asked `spatiand_xr_v1` for since its last commit takes effect
        // now, which is what makes every request in that protocol double-buffered like the
        // rest of a surface's state.
        if let Some((_, object)) = self.xr_surfaces.iter().find(|(s, _)| s == surface) {
            crate::xr::commit(surface, &object.clone());
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
            self.window_count(),
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

    /// "Put my menu somewhere else."
    ///
    /// Answering this is not optional, and not answering it is why no menu in any real
    /// application ever appeared.
    ///
    /// Qt opens a menu in two steps: it creates the popup, and then immediately repositions it
    /// once the widget knows its own size. The protocol says the compositor must reply with
    /// `repositioned` carrying the client's token, followed by a configure -- and Qt will not
    /// attach a buffer until it arrives. Doing nothing here left every menu created, correctly
    /// configured, and then waiting for ever for permission to draw. From inside the headset
    /// it looked exactly like a button that did nothing.
    ///
    /// It survived so long because a client written by hand to test popups does not do this:
    /// it paints at the first configure and works perfectly, which is what made the popup path
    /// look sound. The bug is only reachable through a toolkit -- which is to say through
    /// every application anyone actually runs.
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        // Carries the token *and* the configure: smithay sends `repositioned` and then the
        // same configure `send_configure` would have.
        surface.send_repositioned(token);
        log::info!("a menu asked to move; repositioned (token {token})");
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
            self.window_count(),
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
            match sent {
                Some(false) => {
                    // Cannot fail on a live surface with an unsent initial configure, but a
                    // client that raced its own destroy can still get here.
                    match popup.send_configure() {
                        Ok(serial) => log::info!("configured a menu (serial {serial:?})"),
                        Err(e) => log::warn!("could not configure a menu: {e}"),
                    }
                }
                Some(true) => log::debug!("a menu committed again"),
                // No popup data on a surface the popup manager says is a popup. Worth a word:
                // it would mean the configure never gets sent and the menu waits for ever.
                None => log::warn!("a menu has no popup state to configure from"),
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

/// How big the screen has to be to hold these X11 windows.
///
/// Rounded up in steps rather than fitted exactly, because a resize drag changes a window's
/// size on almost every frame and every change is broadcast to every client as a mode change.
/// In steps, dragging a window across half the room crosses two or three of them.
fn screen_size_for(windows: impl Iterator<Item = (i32, i32)>) -> (i32, i32) {
    /// Wide enough that a drag crosses one now and then, small enough that the screen is never
    /// far bigger than the window that asked for it.
    const STEP: i32 = 256;

    // Rounded up by hand: `div_ceil` is not stable on the compiler this builds with.
    let up = |n: i32| (n + STEP - 1) / STEP * STEP;
    let mut size = DEFAULT_WINDOW_SIZE;
    for (w, h) in windows {
        size.0 = size.0.max(up(w));
        size.1 = size.1.max(up(h));
    }
    size
}

// --- seat ---

/// What the keyboard is pointed at: a Wayland surface, or an X11 window.
///
/// It was the surface in both cases, and for an X11 window that is only half of it. XWayland
/// draws every X window into a Wayland surface, so handing the seat that surface delivers the
/// keys *to the X server* — where they stop, because X decides which of its own windows gets a
/// key from its own input focus, and nothing here had ever set it. Measured with `xdpyinfo`
/// against a window that had been clicked, raised and given the keyboard: `focus: PointerRoot`,
/// which means "whatever the cursor happens to be over" — so an application that hides or
/// grabs the cursor, which is every full-screen game, received nothing at all.
///
/// Smithay's [`X11Surface`](smithay::xwayland::X11Surface) is itself a `KeyboardTarget`, and
/// its `enter` does the three things the X side needs: `SetInputFocus` to the window,
/// `WM_TAKE_FOCUS` to a client that asked to be told, and the focused state on the window. The
/// last two are what Wine reads to decide whether its window is in the foreground, and a game
/// that believes it is in the background ignores the *gamepad* as well as the keyboard. That
/// is how this came to light: a mapped controller that worked everywhere except in the game it
/// was mapped for.
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
            // Named explicitly: `X11Surface` has an inherent `wl_surface` of its own that
            // hands back an owned surface, and it is the one that would be picked here.
            KeyboardFocus::X11(x11) => WaylandFocus::wl_surface(x11),
        }
    }
}

impl smithay::input::keyboard::KeyboardTarget<Spatiand> for KeyboardFocus {
    fn enter(
        &self,
        seat: &Seat<Spatiand>,
        data: &mut Spatiand,
        keys: Vec<smithay::input::keyboard::KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            KeyboardFocus::Wayland(surface) => {
                KeyboardTarget::enter(surface, seat, data, keys, serial)
            }
            KeyboardFocus::X11(x11) => KeyboardTarget::enter(x11, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<Spatiand>, data: &mut Spatiand, serial: Serial) {
        match self {
            KeyboardFocus::Wayland(surface) => KeyboardTarget::leave(surface, seat, data, serial),
            KeyboardFocus::X11(x11) => KeyboardTarget::leave(x11, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<Spatiand>,
        data: &mut Spatiand,
        key: smithay::input::keyboard::KeysymHandle<'_>,
        state: smithay::backend::input::KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            KeyboardFocus::Wayland(surface) => {
                KeyboardTarget::key(surface, seat, data, key, state, serial, time)
            }
            KeyboardFocus::X11(x11) => {
                KeyboardTarget::key(x11, seat, data, key, state, serial, time)
            }
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<Spatiand>,
        data: &mut Spatiand,
        modifiers: smithay::input::keyboard::ModifiersState,
        serial: Serial,
    ) {
        match self {
            KeyboardFocus::Wayland(surface) => {
                KeyboardTarget::modifiers(surface, seat, data, modifiers, serial)
            }
            KeyboardFocus::X11(x11) => {
                KeyboardTarget::modifiers(x11, seat, data, modifiers, serial)
            }
        }
    }
}

impl SeatHandler for Spatiand {
    type KeyboardFocus = KeyboardFocus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&KeyboardFocus>) {}
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        // Not drawn -- the room has its own reticle -- but counted: a game that shows its
        // cursor when the mouse moves has switched into a mode of its own. See `crate::cadence`.
        self.cadence
            .cursor(!matches!(image, smithay::input::pointer::CursorImageStatus::Hidden));
    }
}

// --- dmabuf ---

impl DmabufHandler for Spatiand {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    /// A client has offered us a buffer and wants to know whether we can use it.
    ///
    /// Parked rather than answered: the renderer that decides this is in the frame loop, and a
    /// protocol callback cannot reach it. See [`crate::dmabuf::settle`], which answers it a
    /// frame later — and the module note for why answering "yes" unconditionally, which is one
    /// line and very tempting, is the wrong trade.
    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        self.pending_dmabufs.push((dmabuf, notifier));
    }
}

delegate_dmabuf!(Spatiand);

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
delegate_presentation!(Spatiand);
delegate_viewporter!(Spatiand);
delegate_data_device!(Spatiand);
smithay::delegate_xwayland_shell!(Spatiand);

// --- pointer lock and relative motion ---

impl PointerConstraintsHandler for Spatiand {
    /// A lock is granted at once to the surface the pointer is over. There is no desktop
    /// cursor here for a lock to take away from anyone: the pointer only reaches a game through
    /// its layout, and a game asks for the lock precisely so that it can read that motion.
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
    }
}

smithay::delegate_pointer_constraints!(Spatiand);
smithay::delegate_relative_pointer!(Spatiand);

#[cfg(test)]
mod screen_tests {
    use super::{screen_size_for, DEFAULT_WINDOW_SIZE};

    #[test]
    fn with_nothing_open_the_screen_is_a_windows_size() {
        assert_eq!(screen_size_for(std::iter::empty()), DEFAULT_WINDOW_SIZE);
    }

    #[test]
    fn a_small_window_does_not_shrink_the_screen() {
        // The default is a floor, not a starting point: a game handed a 320x240 desktop
        // because somebody opened a small X11 window is worse than one handed a normal one.
        assert_eq!(
            screen_size_for([(320, 240)].into_iter()),
            DEFAULT_WINDOW_SIZE
        );
    }

    #[test]
    fn a_window_bigger_than_the_screen_grows_it_past_itself() {
        // Past, not to: the pointer has to reach the far corner, and a screen exactly as wide
        // as the window leaves the last pixel column on the boundary.
        let (w, h) = screen_size_for([(1804, 1174)].into_iter());
        assert!(w >= 1804 && h >= 1174, "{w}x{h} does not hold the window");
    }

    #[test]
    fn a_drag_of_a_few_pixels_does_not_change_the_screen() {
        // What the rounding is for: a resize drag changes the window on nearly every frame,
        // and every screen change is sent to every client on the machine.
        let first = screen_size_for([(1500, 900)].into_iter());
        for width in 1501..1530 {
            assert_eq!(screen_size_for([(width, 900)].into_iter()), first);
        }
    }

    #[test]
    fn the_widest_and_the_tallest_window_are_both_held() {
        let (w, h) = screen_size_for([(2000, 600), (900, 1500)].into_iter());
        assert!(w >= 2000 && h >= 1500, "{w}x{h} loses one of them");
    }
}
