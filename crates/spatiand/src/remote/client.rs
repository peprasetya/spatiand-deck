//! The Wayland client half: surfaces, and buffers made from decoded pictures.
//!
//! Ordinary client code, against our own compositor. The only unusual part is where the
//! buffers come from — `zwp_linux_dmabuf_v1`, wrapping a picture the GPU decoded, so a frame
//! becomes a window without being copied.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;

use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use spatiand_proto::client::{spatiand_xr_surface_v1, spatiand_xr_v1};
use spatiand_stream::Input;
use spatiand_video::Converted;

/// One remote window, as a client sees it.
pub struct Window {
    pub surface: wl_surface::WlSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    xdg: xdg_surface::XdgSurface,
    /// Buffers still held by the compositor, with the picture each was made from.
    ///
    /// A buffer may not be reused until the compositor says it has finished with it, and the
    /// picture behind it may not be freed either — the buffer *is* that picture.
    in_flight: HashMap<u32, (wl_buffer::WlBuffer, Converted)>,
    /// The picture currently on screen, kept alive for as long as it is on screen.
    ///
    /// **A release is not permission to free this.** A compositor releases a buffer when it
    /// has taken what it needs to *draw* — but what it took is a handle to this memory, and it
    /// keeps drawing from it every frame until something replaces it. Freeing it on release
    /// hands the memory back to the decoder's pool, which writes the next picture into it
    /// while the old one is still being shown: the window turns flat grey and stays that way.
    showing: Option<(wl_buffer::WlBuffer, Converted)>,
    next_buffer: u32,
    /// What the compositor last asked this window to be, if anything.
    pub configured: Option<(i32, i32)>,
    /// Whether the compositor's first configure has come. That one is only its default for a
    /// window it has just been handed, and the window already has a size -- the host's, which
    /// is where the application lives. Passed on, it resized a viewer that was drawing both
    /// eyes at 3840x1080 down to 1280x800 whenever the session came back after a dropout.
    first_configured: bool,
    pub closed: bool,
    /// This window's `spatiand_xr_v1` surface, made the first time its application says it is
    /// anything other than an ordinary mono window.
    xr: Option<spatiand_xr_surface_v1::SpatiandXrSurfaceV1>,
}

/// Everything the client half owns.
///
/// The event queue is deliberately **not** in here: dispatching hands the queue a mutable
/// reference to this, so the two cannot live in the same structure. The session owns both and
/// keeps them side by side.
pub struct Client {
    pub handle: QueueHandle<Client>,
    compositor: wl_compositor::WlCompositor,
    shell: xdg_wm_base::XdgWmBase,
    dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    /// Only for the diagnostic path below.
    shm: wl_shm::WlShm,
    /// How a remote window claims a layer or an eye layout: the same protocol any local
    /// application uses, so a remote world is judged — granted, refused, made exclusive — by
    /// exactly the rules a local one is. Absent only against a compositor too old to offer it.
    xr: Option<spatiand_xr_v1::SpatiandXrV1>,
    pub windows: HashMap<u32, Window>,
    /// Windows whose buffers the compositor has released since the last look.
    released: Vec<(u32, u32)>,
    /// Sizes the compositor has asked for since the last look.
    pub resized: Vec<(u32, i32, i32)>,
    pub closing: Vec<u32>,
    /// What the wearer has done, waiting to be sent to the host.
    pub input: Vec<(u32, Input)>,
    /// Which window the pointer is over, so a button knows where it landed.
    pointer_on: Option<u32>,
    /// Which window has the keys.
    keyboard_on: Option<u32>,
    /// What is being held down, so it can be let go of when focus leaves.
    ///
    /// A belt to the ordered stream's braces. Wayland says a client may hear no more key
    /// events after a leave, and a key whose release never happens is a key the application
    /// at the other end holds for ever — which for an X11 one means it repeats for ever too.
    held_keys: Vec<(u32, u32)>,
    held_buttons: Vec<(u32, u32)>,
    /// Surfaces by window id, for turning an event's surface back into a window.
    ids: HashMap<wl_surface::WlSurface, u32>,
    /// Format and modifier pairs the compositor says it can import.
    importable: std::collections::HashSet<(u32, u64)>,
}

impl Client {
    /// Connect over an already-made socket pair, returning the client and its queue.
    pub fn new(stream: UnixStream) -> Result<(Client, EventQueue<Client>), String> {
        let connection = Connection::from_socket(stream)
            .map_err(|e| format!("could not speak to the compositor: {e}"))?;
        let (globals, queue): (GlobalList, EventQueue<Client>) = registry_queue_init(&connection)
            .map_err(|e| format!("could not read the compositor's globals: {e}"))?;
        let handle = queue.handle();

        let compositor: wl_compositor::WlCompositor = globals
            .bind(&handle, 4..=6, ())
            .map_err(|e| format!("no wl_compositor: {e}"))?;
        let shell: xdg_wm_base::XdgWmBase = globals
            .bind(&handle, 1..=6, ())
            .map_err(|e| format!("no xdg_wm_base: {e}"))?;
        // Version 3 is enough: the parameters interface has not changed, and asking for 4 means
        // handling the feedback objects, which a client that is told what to send does not need.
        let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 = globals
            .bind(&handle, 3..=3, ())
            .map_err(|e| format!("no linux-dmabuf, so pictures cannot be shown without a copy: {e}"))?;
        // The seat is how the wearer reaches a remote application at all: what arrives here as
        // ordinary pointer and keyboard events is forwarded to the host, which delivers it to
        // that window. Without it a remote window is a picture that cannot be touched.
        let _seat: wl_seat::WlSeat = globals
            .bind(&handle, 5..=9, ())
            .map_err(|e| format!("no seat, so nothing can be typed or clicked: {e}"))?;

        let shm: wl_shm::WlShm = globals
            .bind(&handle, 1..=2, ())
            .map_err(|e| format!("no wl_shm: {e}"))?;
        let xr: Option<spatiand_xr_v1::SpatiandXrV1> = globals.bind(&handle, 1..=4, ()).ok();
        if xr.is_none() {
            log::warn!("remote: the compositor offers no spatiand_xr_v1; remote worlds stay windows");
        }

        let client = Client {
            handle,
            compositor,
            shell,
            dmabuf,
            shm,
            xr,
            windows: HashMap::new(),
            released: Vec::new(),
            resized: Vec::new(),
            closing: Vec::new(),
            input: Vec::new(),
            pointer_on: None,
            keyboard_on: None,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            ids: HashMap::new(),
            importable: std::collections::HashSet::new(),
        };
        Ok((client, queue))
    }

    /// Open a window for a remote one.
    pub fn open(&mut self, id: u32, app_id: &str, title: &str) {
        let surface = self.compositor.create_surface(&self.handle, ());
        let xdg = self.shell.get_xdg_surface(&surface, &self.handle, id);
        let toplevel = xdg.get_toplevel(&self.handle, id);
        // The application id is what the session keys a controller layout on, so a remote
        // application gets its own layout, per host. See `remote::app_id`.
        toplevel.set_app_id(app_id.to_string());
        toplevel.set_title(title.to_string());
        surface.commit();
        self.ids.insert(surface.clone(), id);
        self.windows.insert(
            id,
            Window {
                surface,
                toplevel,
                xdg,
                in_flight: HashMap::new(),
                showing: None,
                next_buffer: 0,
                configured: None,
                first_configured: false,
                closed: false,
                xr: None,
            },
        );
    }

    /// Say what a remote window is in the room and how its two eyes are packed, as its
    /// application has just told its host.
    ///
    /// Claimed through `spatiand_xr_v1`, and committed straight away rather than on the next
    /// picture: a viewer that has just become the world may not send another frame until it
    /// has one worth sending, and the room should change when it said so, not when it next
    /// happens to draw. A claim the compositor refuses comes back as `layer_refused` and is
    /// logged; the window simply stays a window.
    pub fn present(&mut self, id: u32, eyes: spatiand_stream::Eyes, layer: spatiand_stream::Layer) {
        use spatiand_xr_surface_v1::{EyeLayout, Layer};
        let Some(xr) = self.xr.as_ref() else { return };
        let Some(window) = self.windows.get_mut(&id) else { return };
        let surface = window
            .xr
            .get_or_insert_with(|| xr.get_xr_surface(&window.surface, &self.handle, id));
        surface.set_eye_layout(match eyes {
            spatiand_stream::Eyes::Mono => EyeLayout::Mono,
            spatiand_stream::Eyes::SideBySide => EyeLayout::SideBySide,
            spatiand_stream::Eyes::TopBottom => EyeLayout::TopBottom,
        });
        surface.set_layer(match layer {
            spatiand_stream::Layer::Window => Layer::Window,
            spatiand_stream::Layer::Projection => Layer::Projection,
        });
        window.surface.commit();
    }

    /// Say whether a remote window's application draws the pointer over it itself.
    ///
    /// Only a compositor at version 4 understands it; an older one keeps drawing its own
    /// reticle, which is the right thing to fall back to -- a pointer at the wrong depth is
    /// better than none.
    pub fn set_cursor_drawn(&mut self, id: u32, drawn: bool) {
        let Some(xr) = self.xr.as_ref() else { return };
        if xr.version() < 4 {
            return;
        }
        let Some(window) = self.windows.get_mut(&id) else { return };
        let surface = window
            .xr
            .get_or_insert_with(|| xr.get_xr_surface(&window.surface, &self.handle, id));
        surface.set_cursor_drawn(drawn as u32);
        window.surface.commit();
    }

    /// A window's application renamed it — a browser moving to another page.
    pub fn retitle(&mut self, id: u32, title: &str) {
        if let Some(window) = self.windows.get(&id) {
            window.toplevel.set_title(title.to_string());
        }
    }

    /// Close every window, as the link to their host goes. The host still has them: they come
    /// back, announced afresh, when the link does.
    pub fn close_all(&mut self) {
        let ids: Vec<u32> = self.windows.keys().copied().collect();
        for id in ids {
            self.close(id);
        }
    }

    /// Let go of every key that is down. Called when focus leaves, because after that the
    /// release will never arrive and the application would hold the key for ever.
    fn let_go_of_keys(&mut self) {
        for (id, code) in std::mem::take(&mut self.held_keys) {
            self.input.push((
                id,
                Input::Key {
                    code,
                    pressed: false,
                },
            ));
        }
    }

    /// The same for the mouse: an unreleased button is a drag that never ends.
    fn let_go_of_buttons(&mut self) {
        for (id, button) in std::mem::take(&mut self.held_buttons) {
            self.input.push((
                id,
                Input::Button {
                    button,
                    pressed: false,
                },
            ));
        }
    }

    pub fn close(&mut self, id: u32) {
        // Nothing needs releasing on a window that has gone, but its entries must not be
        // left behind to be released against whatever wears that id next.
        self.held_keys.retain(|(window, _)| *window != id);
        self.held_buttons.retain(|(window, _)| *window != id);
        if let Some(window) = self.windows.remove(&id) {
            self.ids.remove(&window.surface);
            window.toplevel.destroy();
            window.xdg.destroy();
            window.surface.destroy();
        }
    }

    /// Take back the buffers the compositor has finished with.
    ///
    /// **Called every time round the loop, not only when a frame is shown.** Reaping inside
    /// `show` looks equivalent and is not: `show` is skipped while too many buffers are
    /// outstanding, so the count could never come down again. The window froze on its second
    /// frame and every frame after it was dropped — 1759 of them in two seconds, with the
    /// thread spinning — while the picture on the glasses sat still.
    pub fn reap(&mut self) {
        for (window, buffer) in std::mem::take(&mut self.released) {
            if let Some(window) = self.windows.get_mut(&window) {
                if let Some((buffer, _picture)) = window.in_flight.remove(&buffer) {
                    buffer.destroy();
                }
            }
        }
    }

    /// Show a picture on a window.
    pub fn show(&mut self, id: u32, picture: Converted) -> Result<(), String> {
        self.reap();
        let handle = self.handle.clone();
        let dmabuf = self.dmabuf.clone();
        let self_importable = self.importable.clone();
        let Some(window) = self.windows.get_mut(&id) else {
            return Ok(());
        };
        let key = window.next_buffer;
        window.next_buffer = window.next_buffer.wrapping_add(1);

        if window.next_buffer == 1 {
            let known = self_importable.contains(&(picture.fourcc, picture.modifier));
            log::info!(
                "remote window {id}: the compositor {} this format and tiling",
                if known { "understands" } else { "DOES NOT list" }
            );
            // Said once per window: the format and modifier are exactly what a picture that
            // arrives as flat grey turns out to have got wrong.
            let fourcc = picture.fourcc.to_le_bytes();
            log::info!(
                "remote window {id}: {}x{} {} modifier {:#x}, {} plane(s)",
                picture.width,
                picture.height,
                String::from_utf8_lossy(&fourcc),
                picture.modifier,
                picture.planes.len()
            );
            for (i, (_, offset, stride)) in picture.planes.iter().enumerate() {
                log::info!(
                    "remote window {id}: plane {i} offset {offset} stride {stride}                      (a linear {}-wide picture would be {})",
                    picture.width,
                    picture.width * 4
                );
            }
        }
        let params = dmabuf.create_params(&handle, ());
        for (plane, (fd, offset, stride)) in picture.planes.iter().enumerate() {
            // SAFETY: the descriptor belongs to the picture, which is kept alive below for as
            // long as the compositor holds the buffer made from it.
            let fd = unsafe { BorrowedFd::borrow_raw(*fd) };
            params.add(
                fd.as_fd(),
                plane as u32,
                *offset,
                *stride,
                (picture.modifier >> 32) as u32,
                (picture.modifier & 0xffff_ffff) as u32,
            );
        }
        let buffer = params.create_immed(
            picture.width as i32,
            picture.height as i32,
            picture.fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &handle,
            (id, key),
        );
        params.destroy();

        window.surface.attach(Some(&buffer), 0, 0);
        window.surface.damage_buffer(0, 0, picture.width as i32, picture.height as i32);
        window.surface.commit();
        // What was on screen until a moment ago may go once the compositor lets it go; what is
        // on screen now may not.
        if let Some((old_buffer, old_picture)) = window.showing.take() {
            window
                .in_flight
                .insert(key.wrapping_sub(1), (old_buffer, old_picture));
        }
        window.showing = Some((buffer, picture));
        Ok(())
    }

    /// Show a picture by copying it through shared memory.
    ///
    /// Slow on purpose — a whole picture memcpy'd every frame — and only ever used to answer
    /// one question: is the picture wrong, or is the way it is being handed over wrong? If
    /// this looks right and the dmabuf path does not, the fault is in the handover.
    pub fn show_by_copy(&mut self, id: u32, picture: &Converted) -> Result<(), String> {
        use std::io::Write;
        let pixels = picture
            .to_bgra()
            .map_err(|e| format!("could not read the picture back: {e}"))?;
        let mut file = tempfile()?;
        file.write_all(&pixels)
            .map_err(|e| format!("could not fill a shared buffer: {e}"))?;
        let handle = self.handle.clone();
        let pool = self.shm.create_pool(
            std::os::fd::AsFd::as_fd(&file),
            pixels.len() as i32,
            &handle,
            (),
        );
        let buffer = pool.create_buffer(
            0,
            picture.width as i32,
            picture.height as i32,
            picture.width as i32 * 4,
            wl_shm::Format::Xrgb8888,
            &handle,
            (id, u32::MAX),
        );
        let Some(window) = self.windows.get_mut(&id) else {
            return Ok(());
        };
        window.surface.attach(Some(&buffer), 0, 0);
        window
            .surface
            .damage_buffer(0, 0, picture.width as i32, picture.height as i32);
        window.surface.commit();
        pool.destroy();
        // Leaked deliberately: this path exists to look at one frame, not to run.
        std::mem::forget(buffer);
        std::mem::forget(file);
        Ok(())
    }

    /// Show a buffer described by hand, without a `Converted` behind it.
    ///
    /// Only for finding out what the compositor can and cannot import — the picture is not
    /// kept alive afterwards, so this is a diagnostic and not a path to use.
    pub fn show_raw(
        &mut self,
        id: u32,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        planes: &[(std::os::fd::RawFd, u32, u32)],
    ) -> Result<(), String> {
        let handle = self.handle.clone();
        let dmabuf = self.dmabuf.clone();
        let Some(window) = self.windows.get_mut(&id) else {
            return Ok(());
        };
        let params = dmabuf.create_params(&handle, ());
        for (plane, (fd, offset, stride)) in planes.iter().enumerate() {
            let fd = unsafe { BorrowedFd::borrow_raw(*fd) };
            params.add(
                fd.as_fd(),
                plane as u32,
                *offset,
                *stride,
                (modifier >> 32) as u32,
                (modifier & 0xffff_ffff) as u32,
            );
        }
        let buffer = params.create_immed(
            width as i32,
            height as i32,
            fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &handle,
            (id, u32::MAX),
        );
        params.destroy();
        window.surface.attach(Some(&buffer), 0, 0);
        window.surface.damage_buffer(0, 0, width as i32, height as i32);
        window.surface.commit();
        log::info!(
            "remote window {id}: handed over {width}x{height} {} modifier {modifier:#x}, {} plane(s), unconverted",
            String::from_utf8_lossy(&fourcc.to_le_bytes()),
            planes.len()
        );
        Ok(())
    }

    /// How many buffers this window is waiting to get back, including the one on screen.
    ///
    /// The answer is the whole flow control: while the compositor is holding several, sending
    /// it more only builds a queue of pictures that will be stale by the time they are shown.
    pub fn in_flight(&self, id: u32) -> usize {
        self.windows
            .get(&id)
            .map_or(0, |w| w.in_flight.len() + usize::from(w.showing.is_some()))
    }

}

/// Talk to the compositor: send what is pending, take what has arrived.
///
/// Never blocks. A remote thread has a network to attend to as well, and the two are polled in
/// turn rather than either waiting on the other.
pub fn pump(client: &mut Client, queue: &mut EventQueue<Client>) -> Result<(), String> {
    client.reap();
    queue
        .flush()
        .map_err(|e| format!("could not send to the compositor: {e}"))?;
    if let Some(guard) = queue.prepare_read() {
        match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("lost the compositor: {e}")),
        }
    }
    queue
        .dispatch_pending(client)
        .map_err(|e| format!("could not read the compositor: {e}"))?;
    // Again, because dispatching is what delivered the releases in the first place.
    client.reap();
    Ok(())
}

// --- events ---

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<spatiand_xr_v1::SpatiandXrV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &spatiand_xr_v1::SpatiandXrV1,
        _: spatiand_xr_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The global has no events.
    }
}

impl Dispatch<spatiand_xr_surface_v1::SpatiandXrSurfaceV1, u32> for Client {
    fn event(
        _: &mut Self,
        _: &spatiand_xr_surface_v1::SpatiandXrSurfaceV1,
        event: spatiand_xr_surface_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Said out loud, because the wearer's only other evidence is a world that stayed a
        // window, which looks exactly like the application never having asked.
        match event {
            spatiand_xr_surface_v1::Event::LayerRefused { layer, reason } => {
                log::warn!("remote: window {id} was refused {layer:?}: {reason}");
            }
            spatiand_xr_surface_v1::Event::LayerLost { layer } => {
                log::info!("remote: window {id} lost {layer:?} and is a window again");
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(
        _state: &mut Self,
        shell: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        // Answering this is what proves a client is alive. A compositor that gets no reply is
        // entitled to assume the client has hung and close it.
        if let xdg_wm_base::Event::Ping { serial } = event {
            shell.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, u32> for Client {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        id: &u32,
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Some(window) = state.windows.get_mut(id) {
                let first = !std::mem::replace(&mut window.first_configured, true);
                if let Some((width, height)) = window.configured.take() {
                    // Only now is it settled, so this is where the host is told -- unless it
                    // is the compositor's opening suggestion, which the wearer never asked for.
                    if !first {
                        state.resized.push((*id, width, height));
                    }
                }
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, u32> for Client {
    fn event(
        state: &mut Self,
        _toplevel: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        id: &u32,
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 {
                    if let Some(window) = state.windows.get_mut(id) {
                        window.configured = Some((width, height));
                    }
                }
            }
            xdg_toplevel::Event::Close => {
                if let Some(window) = state.windows.get_mut(id) {
                    window.closed = true;
                }
                state.closing.push(*id);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (u32, u32)> for Client {
    fn event(
        state: &mut Self,
        _buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        &(window, key): &(u32, u32),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.released.push((window, key));
        }
    }
}

macro_rules! ignore {
    ($($interface:path => $data:ty),* $(,)?) => {
        $(impl Dispatch<$interface, $data> for Client {
            fn event(
                _state: &mut Self,
                _proxy: &$interface,
                _event: <$interface as Proxy>::Event,
                _data: &$data,
                _connection: &Connection,
                _handle: &QueueHandle<Self>,
            ) {
            }
        })*
    };
}

ignore! {
    wl_shm::WlShm => (),
    wl_shm_pool::WlShmPool => (),
    wl_compositor::WlCompositor => (),
    wl_surface::WlSurface => (),
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1 => (),
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for Client {
    fn event(
        state: &mut Self,
        _dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        // What the compositor can actually import. Worth collecting rather than assuming: a
        // buffer in a format or tiling it does not support is not refused, it is drawn wrong.
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            let modifier = ((modifier_hi as u64) << 32) | modifier_lo as u64;
            state.importable.insert((format, modifier));
        }
    }
}

/// A file in memory to share with the compositor.
fn tempfile() -> Result<std::fs::File, String> {
    let path = std::env::temp_dir().join(format!("spatiand-remote-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("could not make a shared buffer: {e}"))?;
    let _ = std::fs::remove_file(&path);
    Ok(file)
}

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for Client {
    fn event(
        _state: &mut Self,
        _feedback: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        _event: zwp_linux_dmabuf_feedback_v1::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Client {
    fn event(
        _state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _connection: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(handle, ());
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(handle, ());
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        state: &mut Self,
        _pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_on = state.ids.get(&surface).copied();
                if let Some(id) = state.pointer_on {
                    state.input.push((
                        id,
                        Input::Motion {
                            x: surface_x,
                            y: surface_y,
                        },
                    ));
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.let_go_of_buttons();
                if let Some(id) = state.pointer_on.take() {
                    state.input.push((id, Input::Leave));
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(id) = state.pointer_on {
                    state.input.push((
                        id,
                        Input::Motion {
                            x: surface_x,
                            y: surface_y,
                        },
                    ));
                }
            }
            wl_pointer::Event::Button { button, state: pressed, .. } => {
                if let Some(id) = state.pointer_on {
                    let pressed = matches!(
                        pressed,
                        wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed)
                    );
                    state.held_buttons.retain(|held| *held != (id, button));
                    if pressed {
                        state.held_buttons.push((id, button));
                    }
                    state.input.push((id, Input::Button { button, pressed }));
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if let Some(id) = state.pointer_on {
                    let (horizontal, vertical) = match axis {
                        wayland_client::WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                            (value, 0.0)
                        }
                        _ => (0.0, value),
                    };
                    state.input.push((
                        id,
                        Input::Scroll {
                            horizontal,
                            vertical,
                        },
                    ));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Client {
    fn event(
        state: &mut Self,
        _keyboard: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                state.keyboard_on = state.ids.get(&surface).copied();
            }
            wl_keyboard::Event::Leave { .. } => {
                state.let_go_of_keys();
                state.keyboard_on = None;
            }
            wl_keyboard::Event::Key { key, state: pressed, .. } => {
                if let Some(id) = state.keyboard_on {
                    let pressed = matches!(
                        pressed,
                        wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed)
                    );
                    state.held_keys.retain(|held| *held != (id, key));
                    if pressed {
                        state.held_keys.push((id, key));
                    }
                    state.input.push((
                        id,
                        Input::Key {
                            // `wl_keyboard.key` is already an evdev code, which is what the
                            // wire carries.
                            code: key,
                            pressed,
                        },
                    ));
                }
            }
            _ => {}
        }
    }
}
