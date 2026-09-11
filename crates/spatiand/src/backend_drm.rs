//! DRM backend — Spatiand owning the display, with no desktop environment underneath.
//!
//! This is the real session. It takes DRM master on the GPU, puts the glasses into
//! side-by-side stereo, and scans out to them directly. Nothing else is running: no KDE
//! managing modes, no other compositor holding the connector, no arbitration over who owns
//! the panel.
//!
//! The rendering strategy is worth stating, because it is not the obvious one. Smithay's
//! `DrmCompositor` wants *render elements*, and our scene is raw GL — a skybox, textured
//! quads, a glass shader. Rather than force 3D drawing into the element model, we render the
//! whole scene into one offscreen texture with plain GL and hand that to the compositor as a
//! single fullscreen element. Smithay keeps doing what it is good at (buffer management,
//! atomic commits, page-flip timing) and we keep full control of the pixels.
//!
//! Running this needs a seat. From SSH there is no logind session on seat0, so use seatd:
//!
//! ```text
//! sudo systemctl stop sddm
//! sudo systemctl start seatd
//! LIBSEAT_BACKEND=seatd spatiand   # with SPATIAND_BACKEND=drm
//! ```

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use glam::{DQuat, DVec3, Mat4, Vec3};
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::gles::{ffi, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Color32F, Offscreen, Renderer};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Session;
use smithay::backend::udev;
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, Transform};

use spatiand_hmd::{DisplayMode, HmdEvent};
use spatiand_render::ray::{ray_from_pad, PointerConfig};
use spatiand_render::{EyeSide, StereoConfig, TextRenderer};
use spatiand_shell::{DesktopPanels, HudAction, Mode, Shell, ShellEvent};
use spatiand_track::{AxisMap, HeadTracker, TrackerConfig};

use crate::calib::Calibration;
use crate::environment::Environments;
use crate::gl::upload_rgba;
use crate::input_map::intent_for;
use crate::pointer::{self, Aim, Drag, PointerState, Zone, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use crate::scene::{eye_centre, Cursor, Scene};
use crate::{Runtime, Spatiand};

const PANEL_DISTANCE: f32 = 1.4;
/// Scroll distance for a full sweep of the left pad, in wl_pointer units.
///
/// A pad spans -1..1, so a corner-to-corner drag is 2 units. 260 makes that about two screens
/// of a text document, which is the same ballpark as a laptop touchpad.
const SCROLL_SCALE: f64 = 260.0;

/// How long the headset may stay silent before it is reopened.
///
/// The glasses stream at about a kilohertz, so a second of nothing is already thousands of
/// missing samples. Three is generous enough to survive a stall without flapping.
const IMU_SILENCE_TIMEOUT: Duration = Duration::from_secs(3);

/// A gap between poll attempts longer than this means *we* stopped asking, not that the
/// headset stopped answering.
///
/// The silence watchdog reads "time since the last sample arrived", which is the right
/// question only while we are actually reading. Under a heavy client -- a browser running a
/// speed test was the case that found this -- the render loop can stall for seconds, and the
/// watchdog then blames the glasses for our own pause and tears down a healthy headset. The
/// world disappears mid-use, which is indistinguishable from a crash to anyone wearing it.
const POLL_STALL_FORGIVENESS: Duration = Duration::from_millis(500);
const PANEL_WIDTH: f32 = 0.9;

/// How long to wait for the glasses' stereo mode to appear on the connector.
///
/// Switching mode makes the DisplayPort link retrain and the kernel re-read the EDID, which
/// is not instant. The Python spike measured this at a couple of seconds; ten is generous
/// enough to cover a cold link without hanging startup if the glasses never comply.
const STEREO_MODE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many consecutive failed presents before the sidecar is abandoned.
///
/// Small on purpose. A transient failure recovers within a frame or two; anything that fails
/// this many times running is a configuration the kernel will not accept, and retrying it at
/// 72 Hz achieves nothing except filling the disk.
const SIDECAR_FAILURE_LIMIT: u32 = 30;

/// How soon after rebuilding for a failing sidecar it is worth doing so again.
///
/// Long enough that a sidecar which is broken for good settles into being off rather than
/// cycling the displays forever, and short enough that a transient failure is repaired while
/// somebody is still looking at it.
const SIDECAR_REBUILD_INTERVAL: Duration = Duration::from_secs(60);

pub fn run(
    event_loop: &mut EventLoop<'static, Runtime>,
    display: &mut Display<Spatiand>,
    runtime: &mut Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    // --- session ---
    let (session, _notifier) = LibSeatSession::new()?;
    log::info!("seat: {}", session.seat());

    // --- gpu ---
    let gpu = udev::primary_gpu(&session.seat())?
        .and_then(|p| {
            DrmNode::from_path(p)
                .ok()?
                .node_with_type(NodeType::Primary)?
                .ok()
        })
        .ok_or("no primary GPU found")?;
    log::info!("gpu: {gpu:?}");

    let mut session_for_open = session.clone();
    let fd = session_for_open.open(
        &gpu.dev_path().ok_or("gpu has no device path")?,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
    )?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (mut drm, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;
    let gbm = GbmDevice::new(device_fd)?;

    // --- renderer ---
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };
    log::info!("renderer ready");

    // --- headset ---
    //
    // Deliberately NOT opened here. The rebuild loop below opens it as its first act, and
    // opening it twice is actively harmful: dropping a handle stops the IMU stream, and
    // `hmd = open_any()` evaluates the new handle *before* dropping the old one. So the second
    // open starts the stream and the first one's Drop immediately stops it again.
    //
    // The result was a session that looked completely fine -- the world drew, stereo
    // negotiated over the MCU, the glasses lit up -- with no head tracking and no headset
    // buttons at all, because the device had been told to stop sending. Unplugging and
    // replugging appeared to "fix" it only because that resets the device and leaves whichever
    // handle opened last as the only one.
    let mut hmd: Option<Box<dyn spatiand_hmd::Hmd>>;
    // Has a headset ever been open in this session?
    //
    // The difference between "you started Spatiand without plugging the glasses in" and "your
    // glasses blinked". The first has nothing to lose and any button should back out of it;
    // the second has every open window to lose, and must not be ended by a stray press. See
    // where this is read, below.
    let mut had_headset = false;
    // When the button now being held went down, while waiting with windows to lose.
    let mut leave_held_since: Option<std::time::Instant> = None;
    // Whether every button has been seen released since the session started. Until it has, a
    // held button is the one that launched Spatiand rather than a request to leave.
    let mut leave_armed = false;

    // Persistent across output changes: these belong to the renderer or the wearer, not
    // to whichever screen is currently being driven.
    let test_pattern = std::env::var("SPATIAND_TEST_PATTERN").is_ok();
    if test_pattern {
        log::info!("SPATIAND_TEST_PATTERN set: drawing flat colour per eye, nothing else");
    }
    let mut text = TextRenderer::new();
    let mut panel: Option<(u32, f32)> = None;
    let mut last_prompt = String::new();
    // The status readout contains live pose numbers, so as a string it changes every frame.
    // Rebuilding on every change then re-rasterises and re-uploads ~2 MB of RGBA at 72 Hz to
    // show digits nobody can read that fast. Recompute it a few times a second instead; the
    // "only rebuild when the text changes" check is right, it was the text that was wrong.
    let mut last_status_update = std::time::Instant::now();
    // When we last got as far as polling the headset. See [`POLL_STALL_FORGIVENESS`].
    let mut last_poll_attempt = std::time::Instant::now();
    let mut status_text = String::new();
    let mut controller = spatiand_input::DeckController::open();
    if controller.is_none() {
        log::warn!("no controller; the return-to-desktop button will not work");
    }
    let mut gesture = spatiand_input::TwoPadGesture::new();
    let pointer_config = PointerConfig::default();
    let mut pointers = PointerState::default();
    let started = std::time::Instant::now();
    // Click edges. Held here rather than in PointerState because they are about the physical
    // pad, not about what the pointer is doing with it.
    let mut right_was_down = false;
    let mut left_was_down = false;
    let mut face_down: Vec<u32> = Vec::new();
    // The left pad read as a wheel. Absolute positions in, deltas out, and the samples taken
    // across the moment the thumb lifts thrown away rather than scrolled.
    let mut left_scroll = spatiand_input::PadScroll::default();
    // The triggers read as mouse buttons, the way they are on the desktop: right pulls the left
    // button, left pulls the right one.
    let mut right_trigger = spatiand_input::Trigger::default();
    let mut left_trigger = spatiand_input::Trigger::default();
    let mut prefs = crate::prefs::Prefs::load();
    let mut keyboard = spatiand_shell::Keyboard {
        click: prefs.keyboard_click,
        ..Default::default()
    };
    // The sound a key makes. Nothing is started here: the first press opens the stream, so a
    // session that never types -- or one where the wearer has turned the click off -- never
    // touches the sound device at all.
    let clicks = crate::click::Clicks::new();
    // When the displays were last rebuilt because the sidecar could not present. Outside the
    // rebuild loop on purpose: its whole job is to notice the second time.
    let mut sidecar_rebuilt: Option<std::time::Instant> = None;
    // An X server for applications that cannot speak Wayland. Started before anything can be
    // launched, so the first application already has a DISPLAY to find. `None` means there is
    // no X server, which is a session where such applications do not start -- and everything
    // else is untouched.
    let x_display = crate::xwayland::start(&runtime.display_handle, &event_loop.handle());
    // Now that both displays exist, tell the session's own services about them.
    //
    // Only the DRM backend does this, and the distinction matters: this is the session, so
    // saying "the compositor is here" is true. The nested backend is a window inside somebody
    // else's session, and it would be pointing that session's portals at a compositor that
    // closes when the window does.
    //
    // What is published is remembered, because it has to be taken back on the way out: these
    // name our sockets, the systemd user manager outlives the session, and a value left
    // behind points whatever runs next at a compositor that is gone.
    let published_names: Vec<&'static str> = {
        let mut published: Vec<(&str, &str)> = vec![
            ("WAYLAND_DISPLAY", runtime.state.socket_name.as_str()),
            ("XDG_SESSION_TYPE", "wayland"),
        ];
        let display = x_display.map(|n| format!(":{n}"));
        if let Some(display) = display.as_deref() {
            published.push(("DISPLAY", display));
        }
        spatiand_platform::publish_session_environment(&published);
        let mut names = vec!["WAYLAND_DISPLAY", "XDG_SESSION_TYPE"];
        if display.is_some() {
            names.push("DISPLAY");
        }
        names
    };
    // Every window's sound, placed where the window is. Started here rather than lazily
    // because the connection to the audio server is what takes the time, and doing it on the
    // first launch would stall the launcher rather than the startup.
    let mut spatial_audio = crate::audio::Audio::new(prefs.spatial_audio, prefs.directness());
    // A keyboard resize in progress: the scale when the frame was grabbed, and how far from
    // the middle the pointer was at that moment. Held rather than recomputed so the drag
    // measures against where it started instead of against the size it is producing — which
    // would feed the result back into its own input and run away.
    type KeyboardResize = (f32, f64);
    let mut keyboard_resize: Option<KeyboardResize> = None;
    let mut keyboard_border_hot = false;
    // Which keys the pointers are over, so they can be drawn raised. At most one per pad.
    use spatiand_shell::keyboard::Key;
    let mut keyboard_hover: Vec<&'static Key> = Vec::new();
    // Where each ray meets the keyboard, as `[right, left]`, so the reticle can be put *on* it.
    //
    // The keyboard is not a window and so is not in the list the aim is cast against. Without
    // this the pointer is drawn at whatever the ray found behind the keyboard, or at the parked
    // distance when it found nothing — and while each eye's picture is separately right, the
    // disparity between them places the cursor a good way behind the keys. Shut one eye and it
    // looks correct, which is exactly what a depth error looks like.
    let mut keyboard_reach: [Option<spatiand_render::ray::Hit>; 2] = [None, None];
    // Left thumb position while a window is being dragged, for the depth adjustment.
    let mut drag_left_y: Option<f32> = None;
    let mut monitors = crate::system::Monitors::new();
    // The panel is a touchscreen, and in a spatial session nothing else is reading it.
    //
    // Opened through the session rather than with `File::open`: the event node is
    // `root:input` with no ACL for the logged-in user, so only logind can hand it over. It is
    // attached to seat0, which is what makes that possible — a device on no seat cannot be
    // taken this way, however permissive its mode bits.
    let (mut touchscreen, touchscreen_node) = match open_touchscreen(&mut session.clone()) {
        Some((t, path)) => (Some(t), Some(path)),
        None => (None, None),
    };
    // Anything a person has plugged in or paired: a keyboard, a mouse, a keyboard with a
    // trackpad on it. Until this existed none of them did anything -- one would pair, report
    // itself connected, and type into nothing.
    // The node we just took is named explicitly rather than recognised, because recognising
    // it did not work: deriving a device's identity from its path through sysfs looked right
    // and quietly matched nothing, so libinput opened the touchscreen anyway and the panel
    // went dead. The path is a fact we already have.
    let mut desk = crate::desk::Desk::new(&session, touchscreen_node.into_iter().collect());
    let backlight = crate::system::Backlight::find();
    // What the sidecar shows and changes. The screen is read here once so the panel has
    // something to draw before the first two-second poll comes round; the glasses are filled in
    // when one is opened, and the volume and the audio devices by the mixer, which reads them
    // as soon as it starts.
    let mut levels = crate::sidecar::Levels {
        screen: backlight.as_ref().and_then(|b| b.level()),
        glasses: None,
        volume: None,
    };
    let mut audio = crate::sidecar::Audio::default();
    let mut slow_status = std::time::Instant::now();
    // A volume button being held down, so that holding it keeps going -- and the thread that
    // actually changes the volume, because asking the audio server takes 27 ms a call and the
    // frame loop has 14 to spend. The sidecar's slider, its device picker and its reading of
    // the volume and the devices all go through it too.
    let mut volume_repeat = crate::volume_keys::Repeat::default();
    let mixer = crate::volume_keys::Mixer::start();

    // The shell — what is on screen and what a button means. Deliberately built once, outside
    // the output loop: unplugging the glasses must not close your launcher.
    let apps: Vec<spatiand_shell::AppEntry> = spatiand_platform::scan()
        .into_iter()
        .map(|e| spatiand_shell::AppEntry {
            name: e.name,
            exec: e.exec,
            icon: e.icon,
            categories: e.categories,
        })
        .collect();
    log::info!("launcher: {} application(s)", apps.len());
    // Asked per panel rather than once. "A desktop is installed" does not imply "the
    // Bluetooth module is installed", and a row that opens nothing reads as a broken HUD.
    let panels = DesktopPanels {
        network: spatiand_platform::panel_available("wifi"),
        bluetooth: spatiand_platform::panel_available("bluetooth"),
    };
    log::info!(
        "desktop settings panels: wi-fi {}, bluetooth {}",
        panels.network,
        panels.bluetooth
    );
    // Whether head tracking has ever been calibrated, which decides where the calibration row
    // sits: near the top while it is the thing most likely to be wrong, at the bottom once it
    // is done and choosing it by accident would cost the calibration you already had.
    let calibrated = spatiand_track::config::load_axes().is_some();
    let mut shell = Shell::new(apps, panels, calibrated);
    let mut environments = Environments::discover();
    shell.set_environments(environments.entries(), environments.choice());
    let mut browser = crate::environment::Browser::new();
    let mut sky_image = environments.current();
    let mut sky_dirty = false;
    // Owns the three pipelines and every texture that outlives one frame.
    let mut scene = Scene::new(&mut renderer, &sky_image)?;
    // Deliberately not decided here. Which axis convention applies is a fact about the
    // headset, so it cannot be settled before one is open — see `settle_axes`, called from
    // the rebuild loop below.
    let stored = spatiand_track::config::load_axes();
    let mut calibration: Option<Calibration> = None;
    let mut tracker = HeadTracker::new(
        stored.unwrap_or(AxisMap::XREAL_AIR),
        TrackerConfig::default(),
    );

    // Page-flip completion drives the render loop.
    //
    // A queued frame stays "pending" until the flip completes and we acknowledge it with
    // frame_submitted(). Skipping that acknowledgement does not fail loudly - the first frame
    // scans out and then nothing ever presents again, which looks like the display going
    // blank a moment after start. That was real: the test pattern showed red for an instant
    // and then went dark.
    // Keyed by CRTC, not a single flag. With two screens the flip completions interleave, and
    // a shared flag lets one screen consume the other's -- after which the compositor believes
    // a flip is still outstanding and never presents again. That failure is silent and looks
    // like the second screen having simply stopped.
    let vblank: Rc<RefCell<HashSet<crtc::Handle>>> = Rc::new(RefCell::new(HashSet::new()));
    let vblank_signal = vblank.clone();
    event_loop
        .handle()
        .insert_source(drm_notifier, move |event, _, _| match event {
            DrmEvent::VBlank(crtc) => {
                vblank_signal.borrow_mut().insert(crtc);
            }
            DrmEvent::Error(e) => log::error!("drm error: {e}"),
        })
        .map_err(|e| format!("could not register the drm source: {e}"))?;

    // Registered once, outside the session loop: the DRM device outlives any individual
    // output, and re-registering on every hotplug would try to move a source that has
    // already been consumed.
    // Rebuild the output whenever the glasses come or go.
    //
    // The Wayland state - clients, their surfaces, the window layout - lives in `runtime`,
    // outside this loop, so it survives a rebuild untouched. That is what lets the desktop
    // and its apps be retained while the glasses are unplugged: only the output is torn
    // down and recreated, never the session.
    loop {
        // Re-open on every rebuild. The device may have just appeared, and a handle from a
        // previous session refers to hardware that has since been unplugged.
        hmd = match spatiand_hmd::open_any() {
            Ok(h) => {
                log::info!("headset: {}", h.info().name);
                settle_axes(h.info(), stored, &mut tracker, &mut calibration);
                // Once, here, rather than on the sidecar's poll: an MCU exchange blocks for up
                // to 1.5 s waiting for its ack, and the render loop cannot afford that twice a
                // second. A headset that will not answer simply has no slider.
                let mut h = h;
                levels.glasses = match h.brightness() {
                    Ok(level) => {
                        log::info!("glasses brightness {:.0}%", level * 100.0);
                        Some(level)
                    }
                    Err(e) => {
                        log::info!("no glasses brightness control ({e})");
                        None
                    }
                };
                had_headset = true;
                Some(h)
            }
            Err(e) => {
                log::info!("no headset ({e}); showing the waiting screen");
                None
            }
        };

        // --- pick a connector, at its native mode ---
        //
        // Every failure from here to the end of setup is *retried*, not propagated. Unplugging
        // the glasses tears the connector out from under this loop mid-rebuild, and treating
        // that as fatal ended the whole session - which is exactly the opposite of what
        // unplugging should do. Whatever went wrong, the right answer is to wait and look
        // again.
        let Some((connector_info, crtc, mode)) = pick_output(&drm) else {
            log::warn!("no usable connector right now; waiting for one");
            std::thread::sleep(Duration::from_millis(500));
            event_loop.dispatch(Some(Duration::from_millis(50)), runtime)?;
            continue;
        };
        let (w, h) = mode.size();
        log::info!(
            "output: {}-{} at {w}x{h}@{}",
            connector_info.interface().as_str(),
            connector_info.interface_id(),
            mode.vrefresh()
        );

        // Two facts shape everything drawn below.
        //
        // `on_glasses` decides stereo versus mono. It is about the *connector*, not the USB side:
        // an external display with a headset attached is the only thing that can show a
        // side-by-side pair usefully. Rendering stereo onto the Deck's own screen produces two
        // squashed copies, which is exactly what happened before this distinction existed.
        let internal = matches!(
            connector_info.interface(),
            connector::Interface::EmbeddedDisplayPort | connector::Interface::LVDS
        );
        // The two halves of a pair of glasses fail independently, and when only the USB half
        // arrives the result is genuinely baffling: the headset answers, so head tracking works
        // and the world turns when you turn -- but it turns on the Deck's own screen while the
        // glasses stay black. That reads as Spatiand having put the world in the wrong place,
        // which is why this says out loud that the picture never had anywhere else to go.
        //
        // `pick_output` already prefers any external connector, so reaching here with a headset
        // open means there was no external connector to prefer.
        if internal && hmd.is_some() {
            log::warn!(
                "the headset is connected over USB, but no external display is: the glasses' \
                 DisplayPort side has not come up, so the world is on the Deck's own panel"
            );
            log::warn!(
                "  this is a link, not a setting. Check the cable actually carries video -- \
                 plenty of USB-C cables are data-only -- that it is fully seated, and that the \
                 glasses are awake rather than asleep"
            );
        }

        // The Deck's panel is mounted in portrait: 800x1280 with the top of the image along the
        // long edge. Anything drawn for it has to be rolled a quarter turn or it reads sideways.
        let portrait = h > w;
        if portrait {
            log::info!("output is portrait ({w}x{h}); rotating content a quarter turn");
        }

        let surface = match drm.create_surface(crtc, mode, &[connector_info.handle()]) {
            Ok(s) => s,
            Err(e) => {
                // Almost always "the display just went away". Retry rather than quit.
                log::warn!("could not create a surface ({e}); retrying");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let formats = renderer.egl_context().dmabuf_render_formats().clone();

        let output = Output::new(
            format!(
                "{}-{}",
                connector_info.interface().as_str(),
                connector_info.interface_id()
            ),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Spatiand".into(),
                model: "DRM".into(),
            },
        );
        let output_mode = OutputMode {
            size: (w as i32, h as i32).into(),
            refresh: (mode.vrefresh() * 1000) as i32,
        };
        // Not advertised to clients, and that is the point. This output is the *scanout*: the
        // glasses' 3840x1080 framebuffer, half of it per eye. Telling an application that its
        // screen is 3840x1080 is how Kodi ended up laying itself out for a display twice as
        // wide as the world and then having it squeezed onto a quad. Clients see
        // `state.screen` instead, whose mode is the size of a window.
        //
        // This also retires an old bug rather than working around it: a `GlobalId` is a handle
        // and not a guard, so every display rebuild used to leave another stale output on
        // offer -- and a session that started before the glasses were plugged in went on
        // advertising the Deck's own 800x1280 portrait panel for the rest of its life.
        // There is nothing to leak now, because nothing is created.
        output.change_current_state(
            Some(output_mode),
            Some(Transform::Normal),
            None,
            Some((0, 0).into()),
        );
        output.set_preferred(output_mode);

        let mut compositor: DrmCompositor<
            GbmAllocator<DrmDeviceFd>,
            GbmFramebufferExporter<DrmDeviceFd>,
            (),
            DrmDeviceFd,
        > = DrmCompositor::new(
            // Auto, not Static.
            //
            // A static mode source is fixed at construction, and construction has to happen
            // before the stereo switch (the link must be up first). Adopting 3840x1080 later
            // with use_mode resizes the surface and swapchain but leaves a static source
            // still reporting 1920 - so the compositor composites into the left half of the
            // framebuffer and never writes the right. The symptom is a perfect left eye and
            // a black right eye, which looks like a stereo bug rather than a sizing one.
            //
            // Auto follows the Output, so updating the output's mode after use_mode keeps
            // everything in step.
            smithay::output::OutputModeSource::Auto(output.clone()),
            surface,
            None,
            allocator,
            GbmFramebufferExporter::new(gbm.clone(), None),
            [Fourcc::Argb8888, Fourcc::Xrgb8888],
            formats,
            drm.cursor_size(),
            Some(gbm.clone()),
        )?;

        // --- the sidecar, on whatever screen the glasses are not using ---
        //
        // Only when the glasses have an external connector: with no headset the world is
        // already on the panel, and a sidecar competing for it would leave nowhere to show the
        // "plug in your glasses" prompt.
        let mut sidecar_surface: Option<SidecarSurface> = None;
        if !internal {
            match build_sidecar(&mut drm, &gbm, &mut renderer, connector_info.handle(), crtc) {
                Ok(Some(side)) => {
                    log::info!(
                        "sidecar on {} at {}x{}",
                        side.name,
                        side.size.0,
                        side.size.1
                    );
                    sidecar_surface = Some(side);
                }
                Ok(None) => log::info!("no second connector for a sidecar"),
                Err(e) => log::warn!("could not bring up the sidecar ({e}); carrying on"),
            }
        }
        let mut sidecar_ui = sidecar_surface
            .as_ref()
            .map(|s| crate::sidecar::Sidecar::new(scene.white_texture(), s.size));

        // From here the panel says what is happening. Six seconds of two black screens reads
        // as a machine that has hung, and the first thing anyone does with a hung machine is
        // press something. See `crate::startup` for where the six seconds actually go.
        //
        // A macro rather than a closure: a closure would have to capture the renderer, the
        // event loop and the runtime for the whole of setup, and every one of them is needed
        // by the setup itself a few lines later.
        macro_rules! say {
            ($stage:expr) => {
                if let (Some(side), Some(ui)) = (sidecar_surface.as_mut(), sidecar_ui.as_ref()) {
                    show_startup_stage(
                        &mut renderer,
                        &mut text,
                        &mut scene,
                        side,
                        ui,
                        &vblank,
                        event_loop,
                        runtime,
                        $stage,
                    );
                }
            };
        }
        say!(crate::startup::Stage::Link);

        let mut pending_flip = false;

        // Present one blank frame before doing anything else.
        //
        // Creating a DrmSurface does NOT perform a modeset - that happens on the first atomic
        // commit. Without this the "switch to stereo after the link is up" ordering below is a
        // lie: the link is still down, the MCU acks into the void, and no wider mode ever
        // appears. This costs one black frame and is the difference between 1920x1080 and
        // 3840x1080.
        {
            let empty: &[TextureRenderElement<GlesTexture>] = &[];
            compositor.render_frame(
                &mut renderer,
                empty,
                Color32F::from([0.0, 0.0, 0.0, 1.0]),
                FrameFlags::DEFAULT,
            )?;
            compositor.queue_frame(())?;
            // Let the link finish training before asking the glasses to renegotiate.
            std::thread::sleep(Duration::from_millis(600));
            event_loop.dispatch(Some(Duration::from_millis(50)), runtime)?;
            // Acknowledge this frame properly rather than discarding its completion.
            //
            // Clearing the flag without calling frame_submitted() leaves DrmCompositor believing
            // a flip is still outstanding, so it never flips again: the loop draws one frame and
            // then waits forever for a vblank that cannot arrive. From outside that is
            // indistinguishable from "the renderer draws nothing" - the display just stays blank
            // - which is exactly how it presented.
            if vblank.borrow_mut().remove(&crtc) {
                let _ = compositor.frame_submitted();
            } else {
                // No completion seen yet; let the main loop handle it as a normal pending flip.
                pending_flip = true;
            }
            log::info!("link up (blank frame presented)");
        }
        // The long one: the glasses are told to become a side-by-side display and the wider
        // mode turns up on the connector when it turns up. Two seconds of nothing, measured,
        // and the only stage where saying which box is busy is also the diagnosis -- a wearer
        // stuck here has glasses answering over USB that will not switch, which is a cable.
        say!(crate::startup::Stage::Stereo);

        // --- stereo negotiation, after the link is up ---
        //
        // This ordering is the whole trick, and it took a while to see. Stopping the display
        // manager brings the DisplayPort link DOWN - the connector drops to reporting 800x600
        // with no EDID. Sending the glasses a side-by-side command in that state gets a perfectly
        // happy MCU ack and changes nothing, because there is no link to renegotiate. The mode
        // never appears and Spatiand settles for 1920x1080.
        //
        // So: modeset first at the native mono mode, which brings the link up, and only then ask
        // the glasses to switch. The 3840x1080 mode appears on the connector a moment later and
        // we adopt it with use_mode.
        let mut w = w;
        let mut h = h;
        let mut on_glasses = false;
        if !internal {
            if let Some(x) = hmd.as_mut() {
                // Force a real transition: mono first, then stereo.
                //
                // Writing the mode the glasses are already in is a no-op. They ack it happily,
                // R_DISP_MODE reads back the right value, and *nothing renegotiates* - so the
                // connector keeps advertising whatever timing it learned at link-up and
                // 3840x1080 never appears. Since our own shutdown path sets mono, and a crash
                // leaves them in stereo, the glasses can easily already be in the target mode
                // when we start.
                //
                // Captured on hardware: from 0x01 (2D) the switch publishes 3840x1080 within two
                // seconds; from 0x04 (already stereo) it publishes nothing, indefinitely.
                if let Err(e) = x.set_display_mode(DisplayMode::Mono) {
                    log::warn!("could not force mono before switching ({e})");
                }
                std::thread::sleep(Duration::from_millis(800));
                match x.set_display_mode(DisplayMode::Stereo) {
                    Ok(m) => log::info!("headset display mode -> {m:?}"),
                    Err(e) => log::warn!("could not switch to stereo ({e}); staying mono"),
                }
                // Ask the *headset* how wide one eye is, not the connector.
                //
                // This used to pass `w`, the mode we had just picked, which is the widest the
                // connector advertises. That is right exactly once — on a cold start, where
                // the glasses are in 2D and the widest mode is the mono one. After a hot
                // reconnect it is wrong and self-defeating: the connector still lists the
                // 3840x1080 it learned during the previous stereo session, so `w` came back as
                // 3840 and this then waited for a mode at least 7680 wide. Nothing is ever that
                // wide, so it timed out after ten seconds every single time, reported that the
                // glasses "never advertised a double-width mode", and left the session in mono
                // while the rebuild loop above started it all over again.
                //
                // `per_eye` is a fact about the hardware and does not drift with whatever the
                // connector happens to be advertising right now.
                let mono_width = x.info().per_eye.0 as u16;
                log::info!(
                    "looking for a stereo mode at least {}px wide (one eye is {}px; connector currently offers {}px)",
                    mono_width.saturating_mul(2),
                    mono_width,
                    w
                );
                if let Some(stereo_mode) =
                    wait_for_stereo_mode(&drm, connector_info.handle(), mono_width)
                {
                    let (sw, sh) = stereo_mode.size();
                    log::info!(
                        "stereo mode {sw}x{sh}@{} appeared; adopting",
                        stereo_mode.vrefresh()
                    );
                    match compositor.use_mode(stereo_mode) {
                        Ok(()) => {
                            w = sw;
                            h = sh;
                            on_glasses = true;
                            // The compositor's mode source follows this; without it the
                            // composited area stays the old size.
                            let adopted = OutputMode {
                                size: (sw as i32, sh as i32).into(),
                                refresh: (stereo_mode.vrefresh() * 1000) as i32,
                            };
                            output.change_current_state(Some(adopted), None, None, None);
                            output.set_preferred(adopted);
                        }
                        Err(e) => log::warn!("could not adopt the stereo mode ({e}); staying mono"),
                    }
                } else {
                    log::warn!("the glasses never advertised a double-width mode; staying mono");
                }
            }
        }
        log::info!("presenting at {w}x{h}, stereo: {on_glasses}");
        say!(crate::startup::Stage::World);
        // Now that there is a renderer, clients can be offered dmabuf -- the formats come from
        // it, and there is nothing truthful to advertise before it exists.
        crate::dmabuf::advertise(&mut runtime.state, &renderer);
        // Where head poses are published for clients that draw their own eye views. Created on
        // demand: a session where nothing asks for poses never makes it.
        let mut pose_channel: Option<crate::pose::Channel> = None;
        // How long one frame is, for the predicted display time. Read from the mode rather
        // than assumed, because it is 72 Hz on the glasses and something else on a panel.
        let frame_ns: i64 = output
            .current_mode()
            .map(|m| 1_000_000_000_000i64 / m.refresh.max(1) as i64)
            .unwrap_or(1_000_000_000 / 60);
        // Read the panel's brightness *after* the mode switch, not before it.
        //
        // It is read once when the headset opens, which is early enough to have something to
        // draw and too early to be right: going mono and back to stereo makes the glasses
        // re-light their panel, and what they come back at is not what they were at. The
        // slider then said "dim" while the glasses were plainly bright, and the first thing a
        // drag did was dim them to match the handle. This is the reading that agrees with what
        // is in front of the wearer's eyes.
        if let Some(x) = hmd.as_mut() {
            match x.brightness() {
                Ok(level) => {
                    if levels.glasses != Some(level) {
                        log::info!(
                            "glasses brightness settled at {:.0}% after the mode switch",
                            level * 100.0
                        );
                    }
                    levels.glasses = Some(level);
                }
                // Not cleared: a read that fails here after one that worked at open is a busy
                // MCU, not a headset without the control.
                Err(e) => log::info!("could not re-read the glasses brightness ({e})"),
            }
        }
        // Clients are told how often the world is redrawn, but never how large it is: their
        // screen is a window. Getting the rate right matters to anything that picks a frame
        // cadence -- a player told 60 while the glasses run at 72 judders.
        if let Some(m) = output.current_mode() {
            runtime.state.set_screen_refresh(m.refresh);
        }

        // --- scene setup ---
        let stereo = StereoConfig {
            h_fov_deg: hmd.as_ref().map(|x| x.info().h_fov_deg).unwrap_or(40.0),
            ipd_m: hmd
                .as_ref()
                .map(|x| x.info().default_ipd_mm)
                .unwrap_or(63.0)
                / 1000.0,
            per_eye: if on_glasses {
                (w as u32 / 2, h as u32)
            } else {
                (w as u32, h as u32)
            },
            ..Default::default()
        };

        // The scene is drawn here with raw GL, then handed to DrmCompositor as one element.
        //
        // We own the framebuffer object rather than using `Renderer::bind`. Smithay's
        // `bind_texture` creates an FBO, checks it is complete, and then *unbinds* it — the FBO
        // is only made current when smithay itself renders. Raw GL calls issued afterwards
        // therefore land on framebuffer 0, which in a DRM/GBM context has no surface behind it,
        // and every call fails with GL_INVALID_FRAMEBUFFER_OPERATION while the screen stays
        // black. Attaching our own FBO to the same texture is the fix.
        let frame_target: GlesTexture =
            renderer.create_buffer(Fourcc::Abgr8888, (w as i32, h as i32).into())?;
        let target_fbo = renderer.with_context(|gl| unsafe {
            let mut fbo = 0;
            gl.GenFramebuffers(1, &mut fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                frame_target.tex_id(),
                0,
            );
            let status = gl.CheckFramebufferStatus(ffi::FRAMEBUFFER);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            (fbo, status)
        })?;
        if target_fbo.1 != ffi::FRAMEBUFFER_COMPLETE {
            return Err(format!("scene framebuffer incomplete: {:#x}", target_fbo.1).into());
        }
        let target_fbo = target_fbo.0;
        log::info!("scene framebuffer {target_fbo} ready at {w}x{h}");

        log::info!(
            "running; clients can connect with WAYLAND_DISPLAY={}",
            runtime.state.socket_name
        );

        // Frame accounting. "Blank" has two very different causes - a loop that presents once
        // and stalls, versus one running at full rate drawing nothing visible - and they are
        // indistinguishable from the outside. Counting is the only way to tell them apart
        // without asking someone to stare at the glasses.
        let mut frames = 0u32;
        let mut skipped = 0u32;
        let mut last_report = std::time::Instant::now();
        // Presentation feedback for the frame currently on its way to the glass, answered
        // when its flip completes. See `Spatiand::take_presentation_feedback`.
        let mut in_flight: Vec<smithay::wayland::presentation::PresentationFeedbackCallback> =
            Vec::new();
        // Which vblank this is, which is what lets a client tell a late frame from a dropped
        // one: two frames reported one sequence apart were consecutive, and a gap was not.
        let mut presented_seq: u64 = 0;
        // What the hands were doing last frame, so "a hand moved" can be told from "the pads
        // were read again". See `crate::attention::Hands`.
        let mut last_hands = crate::attention::Hands::default();
        // For the idle-fade clock, which needs a frame delta rather than a frame count.
        let mut last_tick = std::time::Instant::now();

        // What the output was built for. If reality diverges, rebuild.
        //
        // Presence, **not** `on_glasses`. Those are different questions and conflating them
        // cost a session: `on_glasses` is true only once a double-width mode has actually been
        // negotiated, while the presence check below asks whether the headset is on the USB
        // bus at all. A headset that is plugged in but fails to advertise stereo — which is
        // what a flapping cable produces, since the mode exchange needs the device to stay
        // still for a couple of seconds — therefore read as "present != built_for_glasses"
        // forever, and rebuilt the output every time round. Each rebuild tears down and
        // recreates the surfaces for *both* connectors, so the Deck's own panel flickered once
        // per cycle, and the session never settled long enough to draw anything.
        let built_with_glasses = spatiand_hmd::is_present();
        let mut last_presence_check = std::time::Instant::now();
        // When the output was last rebuilt, so a device that is flapping cannot drive a rebuild
        // loop. A rebuild costs the better part of twenty seconds — mode forcing, link-up, and
        // up to ten seconds waiting for a stereo mode to appear — so without a floor here a
        // headset dropping every thirty seconds keeps the compositor permanently mid-rebuild.
        let last_rebuild = std::time::Instant::now();
        // Reset per rebuild: a freshly opened headset has not sent anything yet, and counting
        // from before it existed would trip the watchdog immediately.
        let mut last_imu = std::time::Instant::now();

        while runtime.state.running {
            // --- input ---
            //
            // Polled once a frame and read from a snapshot, so the menus, the pointer and the
            // two-thumb gesture all see the same instant. Reading the device separately for
            // each would let them disagree about whether a thumb is down.
            let mut shell_events: Vec<ShellEvent> = Vec::new();
            let mut pads: Option<spatiand_input::ControllerState> = None;
            // Hold the sound device open exactly while a keyboard is up and clicking is on.
            // The stream has to be fed without gaps to be reliable -- see `click` -- and that
            // is a wakeup every few milliseconds, which is not a thing to leave running through
            // a session nobody types in.
            clicks.wanted(
                keyboard.click
                    && (keyboard.open
                        || sidecar_ui
                            .as_ref()
                            .is_some_and(|ui| ui.page() == crate::sidecar::Page::Keyboard)),
            );

            // Whether there is anywhere the wearer can actually look at the world, and if not,
            // which half is missing. Everything below reads this rather than asking about the
            // headset directly: the case that was wrong for months is the one where the headset
            // is *present* and there is still nothing to show.
            let missing = if hmd.is_none() {
                Some(crate::waiting::Missing::Headset)
            } else if internal {
                Some(crate::waiting::Missing::Picture)
            } else {
                None
            };

            // Always `None`: the two-thumb move-and-scale gesture is computed below and then
            // discarded, so the code that reads this never runs.
            //
            // **Kept on purpose, and not to be deleted.** It was tried on hardware for
            // resizing windows and was not effective enough to leave switched on, but the
            // geometry is correct and tested and the intent is to come back to it. What is
            // missing is not the maths -- it is the decision about when a two-thumb gesture
            // should outrank the two cursors, which is a question for a headset and a pair of
            // hands, not for a compiler.
            let two_handed: Option<spatiand_input::GestureDelta> = None;
            let mut leaving = false;
            // Asked for from the HUD, or over a signal -- see `shutdown::picture_requested`.
            let mut screenshot = crate::shutdown::picture_requested();
            if let Some(c) = controller.as_mut() {
                c.poll();
                // A button that was already down when this screen appeared does not count.
                // Spatiand is started *with* a button, and that press is still being held a
                // moment later when the waiting screen comes up -- so without this the session
                // can end itself before the wearer has looked up, which is indistinguishable
                // from never having started. Arming on the first frame with nothing held is
                // what separates "still holding what launched me" from "reaching for the exit".
                if !any_button_held(c.state()) {
                    leave_armed = true;
                }

                if missing.is_some() {
                    // Leaving is a hold, never a press, whether or not anything is open.
                    //
                    // These used to be two cases: with nothing yet open there was nothing to
                    // lose, so any button backed out. What that missed is that "nothing to
                    // lose" is about the *cost* of leaving, while what makes an accidental exit
                    // read as a crash is that it was not meant -- and that is the same either
                    // way. It was duly reported as one: Spatiand started, showed the waiting
                    // screen, and quit a second later when a button was pressed.
                    //
                    // A USB-C link that drops rarely stays dropped -- the observed case came
                    // back nine seconds later on its own -- so the right behaviour while
                    // waiting is to keep waiting.
                    if leave_armed && any_button_held(c.state()) {
                        let since = leave_held_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= HOLD_TO_LEAVE {
                            log::info!(
                                "button held for {}s while waiting — returning to the desktop",
                                HOLD_TO_LEAVE.as_secs()
                            );
                            leaving = true;
                        }
                    } else {
                        leave_held_since = None;
                    }
                } else {
                    leave_held_since = None;
                    let pointing = c.state().right_pad.touched && !shell.menu_is_open();
                    for control in c.pressed() {
                        // While a thumb is on the pad, A/B/X are mouse buttons rather than
                        // menu keys. Letting them be both means every click also opens or
                        // closes something.
                        if pointing
                            && matches!(
                                control,
                                spatiand_input::Control::A
                                    | spatiand_input::Control::B
                                    | spatiand_input::Control::X
                            )
                        {
                            continue;
                        }
                        // Calibration owns B while it runs. It takes over the whole view and
                        // drives itself on timers, so the shell's menus are unreachable
                        // anyway - and without this there is no way to abandon a run.
                        if *control == spatiand_input::Control::B && calibration.is_some() {
                            match calibration.as_mut() {
                                Some(c) if c.is_finished() => calibration = None,
                                Some(c) => c.cancel(),
                                None => {}
                            }
                            continue;
                        }
                        // In the world, the D-pad belongs to whatever window has focus.
                        //
                        // It used to move focus between windows instead, which meant an
                        // application that wants arrow keys -- a media centre, a file list,
                        // anything driven from a sofa -- could not be driven at all.
                        //
                        // Cycling windows went to the shoulder bumpers for a while, which is
                        // where every tabbed thing puts "previous" and "next". They have since
                        // been given back: the shoulders are two of the buttons a game wants
                        // most, and stepping blind through a ring of windows meant looking at
                        // each one to find out where you were. Both problems are the window
                        // switcher's now -- a paddle, and a list you can read.
                        //
                        // This is the small version of something bigger: eventually every
                        // control should be remappable per application and forwarded without
                        // the application knowing, the way Game Mode does it. What is here is
                        // the fixed mapping that makes the common case work today, kept in
                        // one table so that replacing it is replacing one table.
                        if !shell.menu_is_open() {
                            // Pressed here and released below, so a held direction repeats in
                            // the application exactly as a held arrow key does -- scrolling a
                            // long list is one press, not forty.
                            if let Some(code) = crate::input_map::key_for(*control) {
                                let now = started.elapsed().as_millis() as u32;
                                send_key_state(&mut runtime.state, code, true, now);
                                continue;
                            }
                        }
                        if let Some(intent) = intent_for(*control) {
                            // The switcher's list is stale the moment anything is launched or
                            // closed, so it is rebuilt on the way in rather than kept up to
                            // date. This is also what lets the shell decline to open with
                            // nothing open.
                            if intent == spatiand_shell::Intent::ToggleSwitcher {
                                shell.set_windows(runtime.state.open_windows());
                            }
                            if let Some(event) = shell.handle(intent) {
                                shell_events.push(event);
                            }
                        }
                    }
                    // The other half of the D-pad mapping. Without it the key is never let
                    // go, which a client reads as a direction held down forever.
                    if !shell.menu_is_open() {
                        for control in c.released() {
                            if let Some(code) = crate::input_map::key_for(*control) {
                                let now = started.elapsed().as_millis() as u32;
                                send_key_state(&mut runtime.state, code, false, now);
                            }
                        }
                    }
                    let input = *c.state();
                    // Two thumbs down is a window gesture and takes the pads away from the
                    // pointer. Running both at once sends the laser racing across the world
                    // while you are resizing something.
                    // The two-thumb gesture still takes precedence for *scaling*, but the
                    // cursors stay visible through it: hiding them mid-gesture makes it
                    // impossible to see what is being resized.
                    // Updated so the gesture keeps its own state consistent, and dropped: see
                    // `two_handed` above for why nothing consumes it yet.
                    let _two_handed = gesture.update(&input.left_pad, &input.right_pad);
                    if !shell.menu_is_open() {
                        pads = Some(input);
                    }
                }
            }

            for event in shell_events {
                match event {
                    ShellEvent::ModeChanged(mode) => {
                        if mode == Mode::World {
                            scene.forget_anchor();
                        } else {
                            // Pin the menu to where the wearer is facing as it opens.
                            let yaw = tracker.euler_degrees().yaw.to_radians() as f32;
                            scene.anchor_menu(mode, yaw);
                        }
                    }
                    ShellEvent::Launch(app) => {
                        // Claim a sink before the app starts, so its very first sound already
                        // knows which window it belongs to. Nothing here fails if spatial
                        // audio is off -- the app simply launches as it always did.
                        let claim = spatial_audio.prepare_launch();
                        let mut env: Vec<(String, String)> =
                            claim.as_ref().map(|(_, e)| e.clone()).unwrap_or_default();
                        // Where the X server is, for anything that cannot speak Wayland.
                        env.extend(crate::xwayland::client_environment(x_display));
                        match spatiand_platform::launch(&app.exec, &runtime.state.socket_name, &env)
                        {
                            Ok(pid) => {
                                if let Some((slot, _)) = claim {
                                    spatial_audio.launched(pid, slot);
                                }
                            }
                            Err(e) => log::warn!("could not launch {}: {e}", app.name),
                        }
                    }
                    ShellEvent::ChooseEnvironment(choice) => {
                        environments.select(choice);
                        sky_image = environments.current();
                        sky_dirty = true;
                    }
                    ShellEvent::ListDirectory(name) => {
                        if let Some(name) = name {
                            browser.enter(&name);
                        }
                        shell.show_directory(browser.label(), browser.entries());
                    }
                    ShellEvent::AddEnvironment(name) => {
                        let path = browser.resolve(&name);
                        let choice = environments.add(&path);
                        environments.select(choice);
                        sky_image = environments.current();
                        sky_dirty = true;
                    }
                    // Switching to a window brings it to you rather than turning you to it.
                    // A 3DoF room has no way to move the wearer, and the alternative -- "your
                    // window is over there somewhere" -- is the problem the switcher exists to
                    // solve.
                    ShellEvent::FocusWindow(id) => {
                        let window = runtime
                            .state
                            .space
                            .elements()
                            .find(|w| runtime.state.layout.id_of(w) == Some(id))
                            .cloned();
                        if let Some(window) = window {
                            let yaw = tracker.euler_degrees().yaw.to_radians();
                            if let Some(mut placement) = runtime.state.layout.get(&window) {
                                // Size and distance are the wearer's choices and are left
                                // alone. Only where it sits changes.
                                placement.yaw = yaw;
                                placement.pitch = 0.0;
                                runtime.state.layout.set(&window, placement);
                            }
                            runtime.state.focus_window(&window);
                        }
                    }
                    ShellEvent::Hud(action) => match action {
                        HudAction::Recentre => {
                            // Recentring brings the room to the wearer. It used to move only
                            // the tracker's idea of zero, which is a different thing entirely:
                            // afterwards the wearer's forward read 0 while every window kept
                            // the absolute yaw it was placed at, so everything jumped by
                            // however far they had turned since the session began. Turn round
                            // once and press it and the whole room -- including a VR180 film
                            // that had been directly in front -- was behind you. Which is
                            // exactly how it was reported.
                            //
                            // So: pick what should end up in front, re-peg the tracker, and
                            // turn the room by the same amount. Relative bearings are
                            // untouched, so what was to the left of what stays there.
                            let anchor = runtime
                                .state
                                .layout
                                .focused_placement()
                                .map(|p| (p.yaw, p.pitch))
                                // With nothing focused, hold whatever is being looked at now,
                                // which makes recentring with an empty room cost nothing
                                // visible rather than swinging an environment away.
                                .unwrap_or_else(|| {
                                    let e = tracker.euler_degrees();
                                    (e.yaw.to_radians(), e.pitch.to_radians())
                                });
                            tracker.recenter();
                            runtime.state.layout.rotate_all(-anchor.0, -anchor.1);
                            crate::xr::rotate_sky_anchor(&mut runtime.state, -anchor.0);
                            log::info!(
                                "recentred; brought the room round by {:.0} deg",
                                (-anchor.0).to_degrees()
                            );
                        }
                        HudAction::Calibrate => {
                            log::info!("restarting axis calibration from the HUD");
                            calibration = Some(Calibration::new());
                        }
                        // Opening the picker re-reads the folders, so an image dropped in
                        // while Spatiand was running appears without a restart. The shell has
                        // already switched mode; all that is owed is a fresh list.
                        HudAction::OpenEnvironments => {
                            environments.refresh();
                            shell.set_environments(environments.entries(), environments.choice());
                        }
                        HudAction::Screenshot => screenshot = true,
                        // The shell has already switched mode; all that is owed is the list,
                        // exactly as for the environment picker.
                        HudAction::OpenSwitcher => {
                            shell.set_windows(runtime.state.open_windows())
                        }
                        HudAction::ToggleKeyboard => {
                            keyboard.open = !keyboard.open;
                            log::info!(
                                "keyboard {}",
                                if keyboard.open { "shown" } else { "hidden" }
                            );
                        }
                        HudAction::ReturnToDesktop => leaving = true,
                        HudAction::OpenSystemSettings(panel) => {
                            // What "wifi" means on this machine is the platform crate's
                            // business, not the shell's and not this loop's.
                            match spatiand_platform::settings_command(panel) {
                                Some(command) => {
                                    log::info!("{panel}: {command}");
                                    if let Err(e) = spatiand_platform::launch(
                                        &command,
                                        &runtime.state.socket_name,
                                        &[],
                                    ) {
                                        log::warn!("could not open {panel}: {e}");
                                    }
                                }
                                None => log::warn!("no way to open {panel} on this system"),
                            }
                        }
                        HudAction::Dismiss => {}
                    },
                }
            }
            // The sidecar's exit button, which fires on the clock rather than on an event: a
            // finger resting perfectly still sends no motion, so nothing would ever notice the
            // hold completing. Decided here with every other way out, so all of them go
            // through the one block that hands the hardware back.
            if let Some(ui) = sidecar_ui.as_mut() {
                if ui.settle() == Some(crate::sidecar::Action::LeaveSession) {
                    log::info!("exit held on the sidecar — returning to the desktop");
                    leaving = true;
                }
            }
            // A signal is a request to leave, handled exactly like the button that means the
            // same thing -- so the controller gets handed back and the glasses go back to 2D.
            if crate::shutdown::requested() {
                log::info!("asked to stop; returning the hardware and exiting");
                leaving = true;
            }
            if leaving {
                // Before anything else, stop claiming to be the session.
                //
                // First because of the ordering: the switch below can have SDDM starting the
                // next session while this process is still winding down, and a session that
                // starts before the retraction lands inherits the stale value anyway. This is
                // what game mode was tripping over -- see `withdraw_session_environment`.
                spatiand_platform::withdraw_session_environment(&published_names);
                // Exiting is not enough. SDDM restarts whatever the default session is, and
                // getting here means that is Spatiand - so quitting just relaunches us, which
                // looks like the button doing nothing. Hand the default back to Plasma first.
                return_to_desktop();
                runtime.state.running = false;
                break;
            }

            // A headset that is open but silent is worse than one that is absent: the world
            // renders, everything looks healthy, and nothing responds. Reopening costs a
            // fraction of a second and is always the right answer -- the device is either
            // wedged or something has told it to stop streaming.
            // Forgive time we spent not asking, so the watchdog only ever measures the
            // device's silence and never our own.
            let stall = last_poll_attempt.elapsed();
            if stall > POLL_STALL_FORGIVENESS {
                log::debug!("render loop stalled for {stall:?}; not counting it against the IMU");
                last_imu = (last_imu + stall).min(std::time::Instant::now());
            }
            last_poll_attempt = std::time::Instant::now();

            if hmd.is_some() && last_imu.elapsed() >= IMU_SILENCE_TIMEOUT {
                log::warn!(
                    "no IMU samples for {:?}; reopening the headset",
                    IMU_SILENCE_TIMEOUT
                );
                hmd = None;
                break;
            }

            // Poll for the glasses appearing or disappearing.
            //
            // Once a second, not per frame: this walks sysfs, and at 72 Hz that would be
            // 72 directory scans a second to answer a question that changes at human speed.
            if last_presence_check.elapsed() >= Duration::from_secs(1) {
                last_presence_check = std::time::Instant::now();
                let present = spatiand_hmd::is_present();
                if present != built_with_glasses {
                    // Wait out a device that is coming and going. Rebuilding on every edge of a
                    // flapping cable means never finishing one, and the wearer sees a machine
                    // that flickers rather than one that is waiting for hardware to settle.
                    if last_rebuild.elapsed() < REBUILD_COOLDOWN {
                        log::debug!(
                            "headset presence changed to {present} but the last rebuild was {:.1}s ago; waiting",
                            last_rebuild.elapsed().as_secs_f32()
                        );
                    } else {
                        if present {
                            log::info!("glasses connected — moving the world onto them");
                        } else {
                            // Everything the wearer had open stays open. Only the output is
                            // rebuilt, onto the Deck's own panel, showing the waiting screen.
                            log::info!("glasses disconnected — holding the session on the panel");
                        }
                        hmd = None;
                        break;
                    }
                }

                // The USB side is not the only thing that can change. When the glasses arrive
                // without their picture, what the wearer goes and fixes is the cable — and
                // reseating one usually drops USB too, which the check above would catch. It
                // does not have to, though: a DisplayPort lane can come up on its own, and then
                // presence has not changed and nothing here would rebuild. The screen telling
                // them to check the cable would still be there after they had.
                if internal && hmd.is_some() && external_connector_present(&drm) {
                    log::info!("a display arrived for the glasses — moving the world onto it");
                    break;
                }
            }

            if let Some(x) = hmd.as_mut() {
                while let Ok(Some(event)) = x.poll(Duration::ZERO) {
                    match event {
                        HmdEvent::Imu(sample) => {
                            last_imu = std::time::Instant::now();
                            if let Some(c) = calibration.as_mut() {
                                c.feed(&sample);
                            }
                            tracker.integrate(&sample);
                        }
                        HmdEvent::Disconnected => {
                            log::warn!("headset disconnected");
                            hmd = None;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            if let Some(c) = calibration.as_mut() {
                c.tick();
                if c.is_finished() {
                    if let Some(map) = c.result() {
                        // A headset that states its mounting has already answered this, and a
                        // measurement that disagrees with the hardware is a measurement that
                        // went wrong -- almost always the nod and the tilt performed the
                        // wrong way round, which produces a valid rotation that nothing
                        // downstream can flag. Saying so is more use than adopting it.
                        let known = hmd
                            .as_ref()
                            .and_then(|h| h.info().sensor_axes)
                            .and_then(AxisMap::from_mounting);
                        match known {
                            Some(known) if known != map => log::warn!(
                                "calibration measured {} but these glasses are built {}; \
                                 keeping the hardware answer. The nod and the tilt were most \
                                 likely performed the wrong way round.",
                                map.summary(),
                                known.summary()
                            ),
                            Some(_) => log::info!(
                                "calibration agrees with the hardware: {}",
                                map.summary()
                            ),
                            None => {
                                log::info!("adopting measured axes: {}", map.summary());
                                tracker.set_axes(map);
                            }
                        }
                    }
                    if matches!(
                        c.stage(),
                        crate::calib::Stage::Done | crate::calib::Stage::Cancelled
                    ) {
                        calibration = None;
                    }
                }
            }

            // Where new windows will open. Refreshed every frame so an app launched after a
            // head turn appears in front of the wearer rather than at world zero.
            runtime.state.spawn_yaw = tracker.euler_degrees().yaw.to_radians();

            // Surfaces that asked to be head-locked follow the view, every frame.
            //
            // Kept here rather than in the scene because it is a change to where a window
            // *is*, not to how it is drawn: it has to be true for the pointer, for a drag, and
            // for anything else that reads the layout, not only for the pixels.
            //
            // This is also what makes a client that does its own head tracking possible. If
            // the compositor moved such a window as well, tracking would be applied twice and
            // the result is unusable -- see the `head_locked` layer in `spatiand_xr_v1`.
            {
                let view_yaw = runtime.state.spawn_yaw;
                let locked: Vec<smithay::desktop::Window> = runtime
                    .state
                    .space
                    .elements()
                    .filter(|w| {
                        use smithay::wayland::seat::WaylandFocus;
                        // `head_locked` specifically, not "anything that is not a window".
                        // The sky is also not a window, and steering its placement to face
                        // the wearer means moving something that is not drawn as a panel at
                        // all -- harmless today only because nothing reads that placement.
                        w.wl_surface()
                            .map(|s| {
                                crate::xr::state_of(&s).layer == crate::xr::Layer::HeadLocked
                            })
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                for window in locked {
                    if let Some(mut placement) = runtime.state.layout.get(&window) {
                        // Distance and size stay the wearer's business; only the direction
                        // is taken over.
                        placement.yaw = view_yaw;
                        placement.pitch = 0.0;
                        runtime.state.layout.set(&window, placement);
                    }
                }
            }

            let orientation = tracker.predicted_orientation(
                spatiand_track::DEFAULT_PREDICTION_SECONDS,
                spatiand_track::DEFAULT_PREDICTION_MAX_DEGREES,
            );

            // --- is anyone paying attention ---
            //
            // Hands only. The head deliberately does not reach this: watching an immersive
            // video *is* moving your head, so a transport bar woken by head movement is a bar
            // that never goes away. See `crate::attention`.
            {
                let dt = last_tick.elapsed();
                last_tick = std::time::Instant::now();
                runtime.state.attention.tick(hmd.is_some(), dt);
            }

            // Surfaces that named an edge keep it still when their shape changes. Here rather
            // than in the scene for the same reason the head-locked pass is: it changes where
            // a window *is*, which the pointer and a drag read as well as the pixels.
            crate::window::apply_resize_anchors(&mut runtime.state);

            // --- poses, for clients that draw their own eye views ---
            //
            // Answered here rather than in the protocol callback for the same reason dmabuf
            // imports are: only this loop knows whether there is a head being tracked, and
            // telling a client "yes" and then never writing a pose would be worse than
            // telling it "no".
            if !runtime.state.pose_clients.is_empty() {
                if hmd.is_none() {
                    for client in runtime.state.pose_clients.drain(..) {
                        client.unavailable("no headset is being tracked in this session".into());
                    }
                } else {
                    if pose_channel.is_none() {
                        match crate::pose::Channel::new() {
                            Ok(channel) => pose_channel = Some(channel),
                            Err(e) => log::warn!("could not make a pose channel: {e}"),
                        }
                    }
                    match pose_channel.as_ref() {
                        Some(channel) => {
                            for client in runtime.state.pose_clients.drain(..) {
                                client.channel(channel.fd(), channel.size());
                            }
                            log::info!("a client is reading head poses");
                        }
                        None => {
                            for client in runtime.state.pose_clients.drain(..) {
                                client.unavailable("the pose channel could not be created".into());
                            }
                        }
                    }
                }
                runtime.state.pose_channels_to_open = false;
            }
            if let Some(channel) = pose_channel.as_mut() {
                // The pose that is about to be drawn with, so a client reading now and the
                // compositor drawing now agree about where the head is.
                let sample = crate::pose::now_ns();
                channel.write(orientation, DVec3::ZERO, &stereo, sample, sample + frame_ns);
            }
            let prompt_text = match calibration.as_ref() {
                _ if missing.is_some() => {
                    // Follow HoloFrame here: with no glasses, say so plainly on whatever screen
                    // there is rather than presenting a spatial world nobody can see. Creating a
                    // stereo desktop for absent glasses is how you end up with windows scattered
                    // across a display you cannot look at.
                    crate::waiting::message(
                        missing.unwrap_or(crate::waiting::Missing::Headset),
                        had_headset,
                        crate::waiting::exit_hint(controller.is_some(), had_headset),
                    )
                }
                Some(c) => {
                    let p = c.prompt();
                    format!("{}\n\n{}\n\n{}", p.heading, p.body, p.status)
                }
                // Nothing in the middle of the view during normal use. That space belongs to
                // the windows; the readout that used to live there is in the corner bar now.
                None => String::new(),
            };
            // No sky, no windows: the waiting screen is not a place. This is what stops the
            // half-connected case rendering a head-tracked world onto the Deck's own panel,
            // which looked like the compositor having lost track of where the wearer was.
            let waiting = missing.is_some();

            let ppd = TextRenderer::px_per_degree(stereo.per_eye.0, stereo.h_fov_deg);
            if prompt_text.is_empty() {
                // Not the same as "unchanged": the panel has to actually go away when a menu
                // opens, or the status text hangs in front of it.
                if let Some((id, _)) = panel.take() {
                    renderer.with_context(|gl| unsafe { gl.DeleteTextures(1, &id) })?;
                }
                last_prompt.clear();
            } else if prompt_text != last_prompt {
                last_prompt = prompt_text.clone();
                let image = text.render(
                    &prompt_text,
                    ppd * 1.6,
                    stereo.per_eye.0.saturating_sub(120).max(64),
                    [235, 240, 255, 255],
                );
                // "Blank" can also mean the text rasterised to nothing. ink_fraction is the
                // cheap way to tell a drawing problem from an empty texture.
                log::info!(
                    "panel rebuilt: {}x{} px, ink {:.1}%",
                    image.width,
                    image.height,
                    image.ink_fraction() * 100.0
                );
                let old = panel.take();
                panel = Some(renderer.with_context(|gl| unsafe {
                    if let Some((id, _)) = old {
                        gl.DeleteTextures(1, &id);
                    }
                    (
                        upload_rgba(gl, &image),
                        image.width as f32 / image.height.max(1) as f32,
                    )
                })?);
            }

            if sky_dirty {
                sky_dirty = false;
                let image = &sky_image;
                renderer.with_context(|gl| unsafe { scene.set_sky(gl, image) })?;
                log::info!("environment now {}", environments.describe());
            }
            // Once a second is plenty: the clock changes once a minute and the battery
            // slower still, while rebuilding rasterises and uploads a texture.
            if last_status_update.elapsed() >= Duration::from_secs(1) || status_text.is_empty() {
                status_text = crate::status::line(runtime.state.window_count());
                last_status_update = std::time::Instant::now();
            }
            scene.sync_status(&mut renderer, &mut text, &status_text, ppd)?;
            if keyboard.open {
                scene.sync_keyboard(&mut renderer, &mut text, &keyboard, ppd)?;
                // A picture of each raised key. Built here rather than in the draw closure,
                // which holds the GL context and cannot also take the renderer.
                for key in &keyboard_hover {
                    scene.sync_key_cap(&mut renderer, &mut text, &keyboard, key)?;
                }
                debug_assert!(keyboard_hover.len() <= 2, "at most one key per pad");
            }
            scene.sync_apps(&mut renderer, &mut text, &shell, ppd)?;
            scene.sync_menu(
                &mut renderer,
                &mut text,
                crate::menu::model(&shell).as_ref(),
                ppd,
                (stereo.h_fov_deg, stereo.v_fov_deg()),
            )?;

            // Answer any dmabuf a client offered since the last frame, before importing:
            // a buffer nobody has said yes to yet is one the client has not committed.
            crate::dmabuf::settle(&mut runtime.state, &mut renderer);
            // And find out whether an application is the environment this frame. Asked every
            // frame rather than remembered: a client that crashes or stops posting simply
            // stops being found, and the wearer's own environment comes back by itself.
            let sky_from_client = crate::scene::sky_surface(&mut renderer, &runtime.state);
            scene.set_sky_override(sky_from_client);
            // Import client buffers before the draw closure takes the context.
            let mut windows = crate::scene::collect_windows(&mut renderer, &runtime.state);
            for quad in windows.iter_mut() {
                let title = runtime.state.display_title(&quad.window);
                quad.title = scene.title_texture(&mut renderer, &mut text, &title, ppd);
                if let Some(app_id) = runtime.state.app_id_of(&quad.window) {
                    quad.icon = scene.window_icon(&mut renderer, &app_id);
                }
            }

            // A keyboard and a mouse, if there are any.
            //
            // Keys go straight to whatever has focus, exactly as the on-screen keyboard's do --
            // except the volume keys, which are the machine's and not the application's. The
            // Deck's own rocker is one of those: to libinput it is a keyboard with two keys.
            // Mouse buttons and the wheel are held for the pointer section below, which is
            // where every other cursor is dealt with and where the ray has been built.
            let mut mouse_events = Vec::new();
            if let Some(d) = desk.as_mut() {
                for event in d.poll() {
                    match event {
                        crate::desk::DeskEvent::Key { code, pressed } => {
                            if let Some(key) = crate::volume_keys::Volume::of(code) {
                                if pressed {
                                    volume_repeat.press(key, std::time::Instant::now());
                                    mixer.send(crate::volume_keys::Change::Key(key));
                                } else {
                                    volume_repeat.release(key);
                                }
                                continue;
                            }
                            let now = started.elapsed().as_millis() as u32;
                            send_key_state(&mut runtime.state, code, pressed, now);
                        }
                        other => mouse_events.push(other),
                    }
                }
            }
            if let Some(key) = volume_repeat.due(std::time::Instant::now()) {
                mixer.send(crate::volume_keys::Change::Key(key));
            }
            // Whatever the worker last set or read, so the sidecar's slider follows the buttons
            // and anything else that changes the volume.
            if let Some(level) = mixer.take_level() {
                levels.volume = level;
            }

            // Windows that have come and gone since the last frame. Collected by the Wayland
            // handlers, which have no business reaching into an audio engine, and drained
            // here where the engine lives.
            for (id, pid) in std::mem::take(&mut runtime.state.arrived_windows) {
                spatial_audio.adopt(id, pid);
            }
            for id in std::mem::take(&mut runtime.state.departed_windows) {
                spatial_audio.forget(id);
            }
            // Where every window's sound is, now, and what it is doing. The head has moved
            // since the last frame even if nothing else has, so the aim is unconditional --
            // and cheap when the answer has not changed, because the renderer only fetches
            // new filters once a direction has moved further than anyone can hear.
            if spatial_audio.is_on() {
                let head = tracker.orientation();
                // Every window that could be what an app's sound is coming from -- including
                // the one that has become the room, which has no quad to be found among and
                // so was silently left out of this for as long as environments have existed.
                // Its sound stopped being pointed the moment it took the room, which meant it
                // stopped counter-rotating with the head as well.
                let mut sources: Vec<crate::audio::Source> = Vec::new();
                for window in runtime.state.space.elements() {
                    use smithay::wayland::seat::WaylandFocus;
                    let (Some(id), Some(surface)) =
                        (runtime.state.layout.id_of(window), window.wl_surface())
                    else {
                        continue;
                    };
                    let xr = crate::xr::state_of(&surface);
                    let kind = if xr.is_environment() {
                        crate::audio::Kind::Environment {
                            yaw: xr.sky_yaw_urad() as f64 * 1e-6,
                        }
                    } else if let Some(placement) = runtime.state.layout.get(window) {
                        crate::audio::Kind::Window(placement)
                    } else {
                        continue;
                    };
                    sources.push(crate::audio::Source { window: id, kind });
                }
                spatial_audio.aim_all(&sources, head);
                for quad in windows.iter_mut() {
                    let Some(id) = runtime.state.layout.id_of(&quad.window) else {
                        continue;
                    };
                    // A window only grows a speaker once it has actually made a sound, and
                    // keeps it from then on: one that vanished between tracks would be a
                    // control that moved out from under a thumb reaching for it.
                    if let Some(status) = spatial_audio.status(id) {
                        let ever = quad.sound.is_some() || status.sounding.is_some();
                        quad.sound = ever.then_some(crate::scene::WindowSound {
                            muted: status.muted,
                            sounding: status.sounding,
                            peak: status.peak,
                        });
                    }
                }
            }

            // --- pointing and clicking ---
            //
            // Built from the head pose latched this frame, so the cursors track with the world
            // rather than lagging a frame behind it.
            let time_ms = started.elapsed().as_millis() as u32;
            let origin = eye_centre(orientation, &stereo);
            let aim_of = |pad: &spatiand_input::Pad| -> Option<Aim> {
                pad.touched.then(|| {
                    pointer::aim(
                        ray_from_pad(pad.x, pad.y, orientation, origin, &pointer_config),
                        &windows,
                    )
                })
            };
            let right_aim = pads.as_ref().and_then(|p| aim_of(&p.right_pad));
            let left_aim = pads.as_ref().and_then(|p| aim_of(&p.left_pad));
            // Whether a hand moved, in the pad's own coordinates rather than the ray's. The
            // ray is cast through the head, so it sweeps the room when the wearer turns with
            // no thumb involved -- watching it would be head tracking wearing a hat.
            {
                let touched = |pad: &spatiand_input::Pad| pad.touched.then_some((pad.x, pad.y));
                let hands = crate::attention::Hands {
                    right: pads.as_ref().and_then(|p| touched(&p.right_pad)),
                    left: pads.as_ref().and_then(|p| touched(&p.left_pad)),
                    mouse: desk.as_ref().and_then(|d| d.cursor()).map(|(x, y, _)| (x, y)),
                };
                if hands.moved_from(&last_hands) {
                    runtime.state.attention.stir();
                }
                last_hands = hands;
            }
            // The mouse aims the same way a thumb does: a position from -1 to 1, through the
            // head's orientation. It keeps its own position because a mouse only ever says how
            // far it has moved.
            let mouse_cursor = desk.as_ref().and_then(|d| d.cursor());
            let mouse_aim = mouse_cursor.map(|(x, y, _)| {
                pointer::aim(
                    ray_from_pad(x, y, orientation, origin, &pointer_config),
                    &windows,
                )
            });

            // Light the close button whichever hand is over it. Either pad can press it, so
            // lighting only the one under the dominant hand would leave the other pressing a
            // control that never acknowledged it was aimed at.
            for aim in [right_aim.as_ref(), left_aim.as_ref()]
                .into_iter()
                .flatten()
            {
                let Some((index, _)) = aim.hit else { continue };
                let Some(quad) = windows.get_mut(index) else {
                    continue;
                };
                match aim.zone {
                    Some(Zone::Close) => quad.close_hot = true,
                    Some(Zone::Mute) => quad.mute_hot = true,
                    _ => {}
                }
            }

            // The left thumb is only a wheel while nothing else is claiming it: a menu takes the
            // pads entirely, and a drag uses that same thumb to set the window's distance. The
            // ground it covers meanwhile has to be forgotten rather than saved up, or it all
            // arrives as one scroll the moment the thumb is handed back.
            if shell.menu_is_open() || pointers.drag.is_some() {
                left_scroll.forget();
            }

            if shell.menu_is_open() {
                // A menu takes the pointer away. Anything held has to be let go, or the client
                // underneath is left believing a drag is still running.
                pointers.release_all(&mut runtime.state, time_ms);
            } else {
                // A drag in progress owns the window and ignores everything else.
                match pointers.drag {
                    Some(Drag::Move {
                        ref window,
                        yaw_offset,
                        pitch_offset,
                    }) => {
                        if let Some(a) = right_aim.as_ref() {
                            let d = a.ray.direction;
                            {
                                if let Some(mut placement) = runtime.state.layout.get(window) {
                                    // Relative to where it was grabbed, so the window hangs
                                    // from the point you took hold of rather than snapping its
                                    // centre to the ray.
                                    placement.yaw = d.y.atan2(d.x) + yaw_offset;
                                    placement.pitch = (d.z.clamp(-1.0, 1.0).asin() + pitch_offset)
                                        .clamp(-1.2, 1.2);
                                    // The left thumb sets the distance while the right holds
                                    // the window. No click needed: reaching for a second
                                    // button while already holding something is awkward, and
                                    // the thumb is on the pad anyway.
                                    if let Some(p) = pads.as_ref() {
                                        if p.left_pad.touched {
                                            if let Some(previous) = drag_left_y {
                                                let delta = (p.left_pad.y - previous) as f64;
                                                placement.radius = (placement.radius + delta * 2.5)
                                                    .clamp(0.8, 8.0);
                                            }
                                            drag_left_y = Some(p.left_pad.y);
                                        } else {
                                            drag_left_y = None;
                                        }
                                    }
                                    runtime.state.layout.set(window, placement);
                                }
                            }
                        }
                    }
                    Some(Drag::Resize { ref window, .. }) => {
                        if let Some(a) = right_aim.as_ref() {
                            if let Some(out) =
                                pointers.drag.as_ref().and_then(|d| d.resized(&a.ray))
                            {
                                runtime.state.layout.set(window, out.placement);
                                // Ask the client for a buffer the new size. It answers in its
                                // own time -- a configure is a request, not an assignment --
                                // so the world-space size changes now and the pixels catch up
                                // over the next frame or two. Waiting for the commit instead
                                // would make the frame lag the pointer visibly.
                                if let Some(toplevel) = window.toplevel() {
                                    let size = smithay::utils::Size::from((
                                        out.pixels.0 as i32,
                                        out.pixels.1 as i32,
                                    ));
                                    if toplevel.current_state().size != Some(size) {
                                        toplevel.with_pending_state(|s| s.size = Some(size));
                                        toplevel.send_pending_configure();
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        // Both thumbs moving together manipulates the focused window and takes
                        // the pads away from pointing. Merely *resting* a thumb does not --
                        // that is common while pointing with the other hand, and the gesture's
                        // deadband is what separates the two.
                        let gesturing = two_handed.map(|d| !d.is_negligible()).unwrap_or(false);
                        if gesturing {
                            // A gesture is not a scroll.
                            left_scroll.forget();
                            if let Some(delta) = two_handed {
                                let focused = runtime
                                    .state
                                    .space
                                    .elements()
                                    .find(|w| runtime.state.layout.is_focused(w))
                                    .cloned();
                                if let Some(window) = focused {
                                    if let Some(mut placement) = runtime.state.layout.get(&window) {
                                        // Pad units are roughly a radian of arc across, so the
                                        // window follows the thumbs at about the rate they move.
                                        placement.yaw -= delta.pan.0 as f64 * 0.6;
                                        placement.pitch = (placement.pitch
                                            + delta.pan.1 as f64 * 0.6)
                                            .clamp(-1.2, 1.2);
                                        placement.width =
                                            (placement.width * delta.scale as f64).clamp(0.3, 3.0);
                                        runtime.state.layout.set(&window, placement);
                                    }
                                }
                            }
                        } else {
                            // One cursor at a time, and the right thumb wins.
                            //
                            // Two pointers both claiming to be "the" cursor is why interacting
                            // with a real application felt arbitrary: whatever the left thumb
                            // was resting on decided where a scroll landed, while the visible
                            // thing being aimed was the right one. The right pad owns the
                            // cursor whenever it is touched; the left pad only inherits it
                            // when the right thumb is off the pad entirely.
                            // A mouse that is being used wins, because somebody with a hand on
                            // one is not also aiming a thumb; when it has faded it has no aim
                            // at all and the pads have it back.
                            // A thumb on a pad outranks the mouse, and that is the way round it
                            // has to be. A mouse cursor lingers after the last movement so it
                            // can be found again -- and while it lingered it held the cursor,
                            // so clicking the right pad clicked wherever the mouse had been
                            // left. Whoever is actively aiming owns the cursor; a resting mouse
                            // is only a mark on the view until it moves again.
                            let cursor_aim =
                                match (right_aim.as_ref(), left_aim.as_ref(), mouse_aim.as_ref()) {
                                    (Some(right), _, _) => Some(right),
                                    (None, Some(left), _) => Some(left),
                                    (None, None, mouse) => mouse,
                                };
                            if let Some(a) = cursor_aim {
                                pointers.motion(&mut runtime.state, a, &windows, time_ms);
                            }
                            // The left pad is the wheel, and it turns whatever the cursor is
                            // on -- which is the right pad's target when that thumb is down.
                            if let Some(p) = pads.as_ref() {
                                match left_scroll.update(&p.left_pad) {
                                    // Natural direction: dragging the thumb up sends the
                                    // content up, which is what every touchpad does.
                                    spatiand_input::Scroll::By { dx, dy } => pointers.scroll(
                                        &mut runtime.state,
                                        -(dx as f64) * SCROLL_SCALE,
                                        dy as f64 * SCROLL_SCALE,
                                        time_ms,
                                    ),
                                    // Only ever sent when the thumb left from the rim: the
                                    // client turns this into a kinetic fling, so it is the
                                    // pad saying "carry on", not "settle down".
                                    spatiand_input::Scroll::Fling => {
                                        pointers.scroll_fling(&mut runtime.state, time_ms)
                                    }
                                    spatiand_input::Scroll::Idle => {}
                                }
                            }
                        }
                    }
                }

                // What the mouse's own buttons and wheel did. Aimed wherever its cursor is, which
                // is the only pointer it can be talking about.
                if let Some(aim) = mouse_aim.as_ref() {
                    for event in &mouse_events {
                        match *event {
                            crate::desk::DeskEvent::Button { code, pressed } => {
                                if pressed {
                                    pointers.motion(&mut runtime.state, aim, &windows, time_ms);
                                    if let Some(quad) = aim.hit.and_then(|(i, _)| windows.get(i)) {
                                        runtime.state.focus_window(&quad.window);
                                    }
                                }
                                pointers.button(&mut runtime.state, code, pressed, time_ms);
                            }
                            crate::desk::DeskEvent::Scroll { dx, dy } => {
                                // A wheel notch is about fifteen units to a client, and libinput
                                // reports it in the same terms, so this is passed on as it comes.
                                pointers.scroll(&mut runtime.state, dx, dy, time_ms);
                            }
                            crate::desk::DeskEvent::Key { .. } => {}
                        }
                    }
                }

                // Buttons. The pads and the face buttons both click, because pressing a pad
                // moves the thumb slightly as it goes down -- fine for a button, bad for a
                // precise click on something small.
                if let Some(p) = pads.as_ref() {
                    // The triggers are the same two buttons as the pad clicks, not a third and
                    // fourth thing to learn: R2 is the left button and L2 the right one, as
                    // they are everywhere else on this machine. Folding them in here rather
                    // than handling them separately is what makes that true without exception
                    // -- a trigger types on the keyboard, grabs a title bar and drags an edge,
                    // because as far as everything below is concerned the pad was clicked.
                    //
                    // Updated unconditionally: hysteresis is state, and `||` would skip the
                    // call on any frame the pad was already down and strand it there.
                    let r2 = right_trigger.update(
                        p.right_trigger,
                        p.buttons.is_down(spatiand_input::Control::R2),
                    );
                    let l2 = left_trigger.update(
                        p.left_trigger,
                        p.buttons.is_down(spatiand_input::Control::L2),
                    );
                    // A click is the level *or* the edge, and the edge is what makes this
                    // reliable. The controller is read at 250 Hz and drawn at 72: every frame
                    // drains several reports and keeps the last one's state, so a click that
                    // began and ended inside one frame's batch leaves no level behind at all.
                    // It was reported as roughly a third of clicks doing nothing, worked
                    // around by pressing twice -- which is exactly what you would do if the
                    // first press had fallen between two samples.
                    //
                    // The edge is recorded per report as they are drained, so it survives.
                    // Counting it here turns a press-and-release inside one frame into a
                    // press this frame and a release the next, which is a click.
                    let clicked_within_the_frame =
                        |control| controller.as_ref().is_some_and(|c| c.just_pressed(control));
                    let right_click = p.right_pad.clicked
                        || r2
                        || clicked_within_the_frame(spatiand_input::Control::RPadClick);
                    let left_click = p.left_pad.clicked
                        || l2
                        || clicked_within_the_frame(spatiand_input::Control::LPadClick);

                    // Confirm the press under the thumb that made it. Without this the pads
                    // feel dead: the click registers, the world responds, and the hand is
                    // told nothing -- which reads as the pad being broken rather than as
                    // missing feedback.
                    if let Some(c) = controller.as_ref() {
                        if right_click && !right_was_down {
                            c.pulse(
                                spatiand_input::HapticPad::Right,
                                spatiand_input::Feel::Click,
                            );
                        }
                        if left_click && !left_was_down {
                            c.pulse(spatiand_input::HapticPad::Left, spatiand_input::Feel::Click);
                        }
                    }

                    // Where the keyboard is this frame, if it is up at all. Worked out once:
                    // the press, the resize drag and the drawing all have to agree, and three
                    // copies of this arithmetic is three chances for the keys to be somewhere
                    // other than where they are drawn.
                    let keyboard_quad = keyboard.open.then(|| {
                        let focus = windows.iter().find(|w| w.focused);
                        let (centre, facing, width, height) = scene.keyboard_placement(
                            focus.map(|w| &w.placement),
                            focus.map(|w| w.pixels).unwrap_or((16, 9)),
                            orientation,
                            (stereo.h_fov_deg, stereo.v_fov_deg()),
                            keyboard.scale,
                        );
                        spatiand_render::Quad {
                            centre: centre.as_dvec3(),
                            orientation: facing.as_dquat(),
                            width: width as f64,
                            height: height as f64,
                        }
                    });

                    // What each pointer is over. Both pads, because both can type, and a key
                    // that lights up under one thumb but not the other would make the left one
                    // feel broken right up until you pressed it.
                    keyboard_hover.clear();
                    keyboard_reach = [None, None];
                    keyboard_border_hot = keyboard_resize.is_some();
                    if let Some(q) = keyboard_quad.as_ref() {
                        for (hand, aim) in [right_aim.as_ref(), left_aim.as_ref()]
                            .into_iter()
                            .enumerate()
                        {
                            let Some(aim) = aim else { continue };
                            let Some(hit) = spatiand_render::intersect_quad(&aim.ray, q) else {
                                continue;
                            };
                            // The whole plate, border included: the reticle belongs on the
                            // surface wherever it lands on it, not only over a key.
                            keyboard_reach[hand] = Some(hit);
                            match keyboard.target_at(hit.u, hit.v) {
                                Some(spatiand_shell::keyboard::Target::Key(k)) => {
                                    if !keyboard_hover.iter().any(|e: &&Key| e.code == k.code) {
                                        keyboard_hover.push(k);
                                    }
                                }
                                Some(spatiand_shell::keyboard::Target::Border) => {
                                    keyboard_border_hot = true
                                }
                                // Nothing to light up. The toggle does not rise the way a key
                                // does -- a raised cap is a picture of a key, and the reticle
                                // is already sitting on it saying where the ray landed.
                                Some(spatiand_shell::keyboard::Target::SoundToggle) => {}
                                None => {}
                            }
                        }
                    }

                    // A resize in progress owns the pointer until it is let go.
                    if let (Some(start), Some(q)) = (keyboard_resize, keyboard_quad.as_ref()) {
                        if right_click {
                            if let Some(a) = right_aim.as_ref() {
                                if let Some(hit) = spatiand_render::ray::intersect_plane(&a.ray, q)
                                {
                                    // How far out from the middle the pointer is now, against
                                    // where it was when the frame was grabbed. Measured from
                                    // the centre so the gesture is "pull it bigger" in any
                                    // direction rather than a per-edge drag -- the keyboard
                                    // keeps its aspect, so there is nothing a per-edge drag
                                    // could mean that this does not.
                                    let now = span(hit.u, hit.v);
                                    if start.1 > 1e-4 {
                                        keyboard.scale = (start.0 * (now / start.1) as f32).clamp(
                                            spatiand_shell::keyboard::MIN_SCALE,
                                            spatiand_shell::keyboard::MAX_SCALE,
                                        );
                                    }
                                }
                            }
                        } else {
                            log::info!("keyboard resized to {:.2}x", keyboard.scale);
                            keyboard_resize = None;
                        }
                    }

                    // The keyboard is tested before any window. It deliberately hangs in
                    // front, and a keystroke must never also click through to what is behind.
                    //
                    // **Either thumb types.** Both pads aim, so both should be able to press a
                    // key -- one finger hunting across a whole keyboard is the slowest way to
                    // type anything, and the two-thumb reach is the one thing this layout has
                    // over a phone's. Only the right pad grabs the resize frame, because the
                    // left one is also the scroll and depth control and a frame it could seize
                    // would make those unpredictable near the keyboard's edge.
                    let mut typed = false;
                    let mut left_typed = false;
                    if keyboard_resize.is_none() {
                        for (aim, went_down, right_hand) in [
                            (right_aim.as_ref(), right_click && !right_was_down, true),
                            (left_aim.as_ref(), left_click && !left_was_down, false),
                        ] {
                            if !went_down {
                                continue;
                            }
                            let (Some(a), Some(q)) = (aim, keyboard_quad.as_ref()) else {
                                continue;
                            };
                            let Some(hit) = spatiand_render::intersect_quad(&a.ray, q) else {
                                continue;
                            };
                            match keyboard.target_at(hit.u, hit.v) {
                                Some(spatiand_shell::keyboard::Target::Key(key)) => {
                                    if right_hand {
                                        typed = true;
                                    } else {
                                        left_typed = true;
                                    }
                                    // No pulse here: the block above already buzzed whichever
                                    // pad was clicked, and a second one on the same press is
                                    // felt as a rattle rather than as confirmation.
                                    if keyboard.click {
                                        clicks.play();
                                    }
                                    if let Some(stroke) = keyboard.press(key) {
                                        send_stroke(&mut runtime.state, stroke, time_ms);
                                    }
                                    keyboard.after_press(key);
                                }
                                Some(spatiand_shell::keyboard::Target::SoundToggle) => {
                                    // Counts as having typed, so this press does not also fall
                                    // through to the code that shuts a client's menus.
                                    if right_hand {
                                        typed = true;
                                    } else {
                                        left_typed = true;
                                    }
                                    toggle_click(&mut keyboard, &mut prefs, &clicks);
                                }
                                Some(spatiand_shell::keyboard::Target::Border) if right_hand => {
                                    typed = true;
                                    keyboard_resize = Some((keyboard.scale, span(hit.u, hit.v)));
                                }
                                _ => {}
                            }
                        }
                    }

                    if !typed && right_click && pointers.drag.is_none() && !right_was_down {
                        // Anything but a click on the menu itself closes the menus -- the title
                        // bar, another window, empty sky. Spatiand hands out no popup grabs, so
                        // this is the only thing that ever tells a client its menu is over.
                        if !right_aim.as_ref().map(|a| a.on_popup()).unwrap_or(false) {
                            pointers.dismiss_popups(&mut runtime.state);
                        }
                        match right_aim.as_ref() {
                            // Before the title bar: the button sits inside the bar, so testing
                            // the bar first would start a drag and never reach this.
                            Some(a) if a.zone == Some(Zone::Mute) => {
                                if let Some(quad) = a.hit.and_then(|(i, _)| windows.get(i)) {
                                    if let Some(id) = runtime.state.layout.id_of(&quad.window) {
                                        let now = quad.sound.map(|s| s.muted).unwrap_or(false);
                                        spatial_audio.set_muted(id, !now);
                                        log::info!(
                                            "{} {}",
                                            if now { "unmuted" } else { "muted" },
                                            runtime
                                                .state
                                                .title_of(&quad.window)
                                                .unwrap_or_else(|| "a window".into())
                                        );
                                    }
                                }
                            }
                            Some(a) if a.zone == Some(Zone::Close) => {
                                if let Some(quad) = a.hit.and_then(|(i, _)| windows.get(i)) {
                                    log::info!(
                                        "closing {}",
                                        runtime
                                            .state
                                            .title_of(&quad.window)
                                            .unwrap_or_else(|| "a window".into())
                                    );
                                    runtime.state.close_window(&quad.window);
                                }
                            }
                            Some(a) if a.on_title => {
                                if let Some((index, _)) = a.hit {
                                    if let Some(quad) = windows.get(index) {
                                        runtime.state.focus_window(&quad.window);
                                        let d = a.ray.direction;
                                        let (ray_yaw, ray_pitch) =
                                            (d.y.atan2(d.x), d.z.clamp(-1.0, 1.0).asin());
                                        let (yaw_offset, pitch_offset) = runtime
                                            .state
                                            .layout
                                            .get(&quad.window)
                                            .map(|p| (p.yaw - ray_yaw, p.pitch - ray_pitch))
                                            .unwrap_or((0.0, 0.0));
                                        drag_left_y = None;
                                        pointers.drag = Some(Drag::Move {
                                            window: quad.window.clone(),
                                            yaw_offset,
                                            pitch_offset,
                                        });
                                    }
                                }
                            }
                            Some(a) if matches!(a.zone, Some(Zone::Resize(_))) => {
                                if let (Some((index, hit)), Some(Zone::Resize(edge))) =
                                    (a.hit, a.zone)
                                {
                                    if let Some(quad) = windows.get(index) {
                                        runtime.state.focus_window(&quad.window);
                                        if let Some(placement) =
                                            runtime.state.layout.get(&quad.window)
                                        {
                                            pointers.drag = Some(Drag::Resize {
                                                window: quad.window.clone(),
                                                edge,
                                                start_quad: crate::pointer::quad_of(
                                                    quad.pixels,
                                                    &placement,
                                                ),
                                                start_u: hit.u,
                                                start_v: hit.v,
                                                start_placement: placement,
                                                start_pixels: quad.pixels,
                                            });
                                        }
                                    }
                                }
                            }
                            Some(a) => {
                                // Same reason as the right button below: the cursor is not
                                // moved on a frame the two-thumb gesture owns the pads, so a
                                // click on one of those frames would land wherever it was left.
                                pointers.motion(&mut runtime.state, a, &windows, time_ms);
                                if let Some((index, _)) = a.hit {
                                    if let Some(quad) = windows.get(index) {
                                        runtime.state.focus_window(&quad.window);
                                    }
                                }
                                pointers.button(&mut runtime.state, BTN_LEFT, true, time_ms);
                            }
                            None => {}
                        }
                    } else if !right_click && right_was_down {
                        if matches!(pointers.drag, Some(Drag::Move { .. } | Drag::Resize { .. })) {
                            pointers.drag = None;
                        } else {
                            pointers.button(&mut runtime.state, BTN_LEFT, false, time_ms);
                        }
                    }

                    // The left pad and the left trigger are the right mouse button, and nothing
                    // else. There used to be a second job here -- clicking left while the right
                    // pad was over a title bar pushed the window away or pulled it closer --
                    // and it had to go: it fired on the right pad merely *hovering* a bar
                    // rather than holding one, so pointing anywhere near the top of a window
                    // silently swallowed the context menu. Distance is already set by sliding
                    // the left thumb during a move, with no click at all (see Drag::Move
                    // above), so nothing was lost by taking the click back.
                    if left_click && !left_was_down && !left_typed {
                        // Aimed where the visible laser is, and only if there is one. A click
                        // with no thumb on either pad has no target but the stale one the
                        // cursor was left on, which is the same reason the face buttons below
                        // wait for an aim.
                        if let Some(a) = right_aim.as_ref().or(left_aim.as_ref()) {
                            if !a.on_popup() {
                                pointers.dismiss_popups(&mut runtime.state);
                            }
                            // Put the cursor under the ray *first*. Motion is skipped on any
                            // frame the two-thumb gesture claimed the pads, and pressing the
                            // left pad while the right thumb rests on its own is exactly that
                            // frame -- so without this the menu opens wherever the cursor was
                            // stranded rather than where the laser is pointing.
                            pointers.motion(&mut runtime.state, a, &windows, time_ms);
                            if let Some(quad) = a.hit.and_then(|(i, _)| windows.get(i)) {
                                runtime.state.focus_window(&quad.window);
                            }
                            pointers.button(&mut runtime.state, BTN_RIGHT, true, time_ms);
                        }
                    } else if !left_click && left_was_down {
                        pointers.button(&mut runtime.state, BTN_RIGHT, false, time_ms);
                    }
                    right_was_down = right_click;
                    left_was_down = left_click;

                    // Face buttons, only while a thumb is on a pad -- otherwise A would click
                    // whatever the stale cursor was over, and A is also the menus' select.
                    if right_aim.is_some() {
                        for (control, button) in [
                            (spatiand_input::Control::A, BTN_LEFT),
                            (spatiand_input::Control::B, BTN_RIGHT),
                            (spatiand_input::Control::X, BTN_MIDDLE),
                        ] {
                            let down = p.buttons.is_down(control);
                            let was = face_down.contains(&button);
                            if down && !was {
                                face_down.push(button);
                                pointers.button(&mut runtime.state, button, true, time_ms);
                            } else if !down && was {
                                face_down.retain(|b| *b != button);
                                pointers.button(&mut runtime.state, button, false, time_ms);
                            }
                        }
                    }
                }
            }

            // --- draw the scene into the offscreen texture ---
            {
                let snapshot = panel;
                let scene = &scene;
                let shell = &shell;
                let windows = &windows;
                let right_aim = &right_aim;
                let left_aim = &left_aim;
                let keyboard_open = keyboard.open;
                let keyboard_state = &keyboard;
                let keyboard_hot = keyboard_border_hot;
                let keyboard_raised = &keyboard_hover;
                renderer.with_context(|gl| unsafe {
                    gl.BindFramebuffer(ffi::FRAMEBUFFER, target_fbo);
                    gl.Disable(ffi::SCISSOR_TEST);
                    gl.ClearColor(0.02, 0.02, 0.05, 1.0);
                    gl.Viewport(0, 0, w as i32, h as i32);
                    gl.Clear(ffi::COLOR_BUFFER_BIT);

                    // SPATIAND_TEST_PATTERN bisects "is anything reaching the display at all"
                    // from "is my scene drawing correctly". It uses nothing but scissored clears
                    // - no shader, no texture, no transform - so if this does not show up, the
                    // problem is in presentation rather than in the scene. Distinct colours per
                    // eye also prove the side-by-side split is landing where it should: each eye
                    // should see ONE flat colour, not both.
                    if test_pattern {
                        gl.Enable(ffi::SCISSOR_TEST);
                        let half = if on_glasses { w as i32 / 2 } else { w as i32 };
                        gl.Scissor(0, 0, half, h as i32);
                        gl.ClearColor(0.8, 0.1, 0.1, 1.0); // left eye: red
                        gl.Clear(ffi::COLOR_BUFFER_BIT);
                        if on_glasses {
                            gl.Scissor(half, 0, half, h as i32);
                            gl.ClearColor(0.1, 0.7, 0.2, 1.0); // right eye: green
                            gl.Clear(ffi::COLOR_BUFFER_BIT);
                        }
                        gl.Disable(ffi::SCISSOR_TEST);
                        gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
                        return;
                    }

                    let views: &[(EyeSide, i32, i32)] = if on_glasses {
                        &[
                            (EyeSide::Left, 0, w as i32 / 2),
                            (EyeSide::Right, w as i32 / 2, w as i32 / 2),
                        ]
                    } else {
                        &[(EyeSide::Left, 0, w as i32)]
                    };
                    // Flip vertically when drawing into the offscreen texture.
                    //
                    // GL renders with the origin at the BOTTOM-left, so rendering into a
                    // texture stores the image with row 0 holding its bottom. The compositor
                    // then samples that texture with row 0 as the top, and everything comes
                    // out mirrored top-to-bottom. It does not read as a clean 180 degree
                    // rotation - glyphs are individually flipped while the line order
                    // reverses - which is why it is harder to read than upside-down text.
                    //
                    // Folded into the eye's projection rather than applied per draw call, so
                    // that everything downstream - the skybox's inverse view-projection
                    // included - stays consistent with it automatically.
                    //
                    // Only the offscreen path needs this. The nested winit backend draws
                    // straight into a GL surface that is presented with GL's own convention,
                    // so no correction applies there.
                    let flip_y = Mat4::from_scale(Vec3::new(1.0, -1.0, 1.0));

                    for (side, x, vw) in views {
                        gl.Viewport(*x, 0, *vw, h as i32);
                        let mut eye =
                            spatiand_render::eye_for(*side, orientation, DVec3::ZERO, &stereo);
                        eye.projection = flip_y * eye.projection;

                        // Behind everything, and only once there is a world to be behind: the
                        // waiting screen is not a place, so it keeps its plain dark backdrop.
                        if !waiting {
                            scene.draw_sky(gl, &eye);
                            scene.draw_windows(gl, &eye, &windows);
                        }
                        // Under the focused window, and drawn before the menus so a menu opened
                        // over it still reads as being in front.
                        if keyboard_open {
                            let focus = windows.iter().find(|w| w.focused);
                            scene.draw_keyboard(
                                gl,
                                &eye,
                                focus.map(|w| &w.placement),
                                focus.map(|w| w.pixels).unwrap_or((16, 9)),
                                orientation,
                                (stereo.h_fov_deg, stereo.v_fov_deg()),
                                keyboard_state,
                                keyboard_hot,
                                keyboard_raised,
                            );
                        }
                        scene.draw_menu(gl, &eye, &shell, (stereo.h_fov_deg, stereo.v_fov_deg()));
                        // While a resize is running the cursor keeps the edge's shape even
                        // once the ray has left the window -- which it does immediately, since
                        // dragging an edge outward means aiming past where the window was.
                        let dragging_edge = match pointers.drag {
                            Some(Drag::Resize { edge, .. }) => Some(edge),
                            _ => None,
                        };
                        // Only the thumb that owns the cursor gets a pointer and a beam.
                        // Two lasers with only one of them meaning anything is worse than one:
                        // it is not discoverable which is which, and the idle one is drawn
                        // across whatever you are trying to read.
                        // Hidden only while the right pad is actually *pressed*. Merely resting
                        // a thumb on it is the normal state while the other hand does
                        // something -- during a two-thumb zoom, for instance -- and blanking
                        // the left beam then removes the pointer you are working with.
                        let right_owns =
                            pads.as_ref().map(|p| p.right_pad.clicked).unwrap_or(false);
                        for (aim, aimed_by, on_keys, fade) in [
                            (
                                right_aim.as_ref(),
                                crate::scene::Pointing::RightThumb,
                                keyboard_reach[0],
                                1.0,
                            ),
                            (
                                left_aim.as_ref(),
                                crate::scene::Pointing::LeftThumb,
                                keyboard_reach[1],
                                1.0,
                            ),
                            // Last, and fading: a mouse nobody has touched for a while should
                            // stop covering what is behind it.
                            (
                                mouse_aim.as_ref(),
                                crate::scene::Pointing::Mouse,
                                None,
                                mouse_cursor.map(|c| c.2).unwrap_or(0.0),
                            ),
                        ] {
                            let Some(a) = aim else { continue };
                            let right_hand = aimed_by == crate::scene::Pointing::RightThumb;
                            if aimed_by == crate::scene::Pointing::LeftThumb
                                && right_owns
                                && !pointers.is_dragging()
                            {
                                continue;
                            }
                            let cursor = match (right_hand, dragging_edge, a.zone) {
                                (true, Some(edge), _) => Cursor::for_edge(edge),
                                (_, _, Some(Zone::Resize(edge))) => Cursor::for_edge(edge),
                                _ => Cursor::Point,
                            };
                            scene.draw_pointer_ray(
                                gl,
                                &eye,
                                orientation,
                                &a.ray,
                                nearer(a.hit.map(|(_, h)| h), on_keys),
                                aimed_by,
                                cursor,
                                fade,
                            );
                        }

                        // The head-locked panel: the waiting prompt, the calibration flow, or
                        // the status readout. Drawn last so it is never behind the world.
                        if let Some((tex, aspect)) = snapshot {
                            let (pw, ph) =
                                fit_panel(aspect, stereo.h_fov_deg, stereo.v_fov_deg(), portrait);
                            let model = head_locked_panel_sized(orientation, pw, ph, portrait);
                            scene.quads().draw(
                                gl,
                                tex,
                                &(eye.view_projection() * model),
                                [1.0, 1.0, 1.0, 1.0],
                                (0.0, 1.0),
                            );
                        }
                    }
                    gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
                })?;
            }

            if screenshot {
                // Read back the frame we just drew rather than re-rendering it, so what lands
                // in the file is exactly what was on the glass -- including both eyes.
                match capture(&mut renderer, target_fbo, w as u32, h as u32) {
                    Ok(path) => log::info!("screenshot saved to {}", path.display()),
                    Err(e) => log::warn!("could not save a screenshot: {e}"),
                }
            }

            // --- the sidecar ---
            if let (Some(side), Some(ui)) = (sidecar_surface.as_mut(), sidecar_ui.as_mut()) {
                monitors.tick();
                if slow_status.elapsed() >= Duration::from_secs(2) {
                    slow_status = std::time::Instant::now();
                    levels.screen = backlight.as_ref().and_then(|b| b.level());
                    // Not the volume or the audio devices, which the mixer's thread re-reads:
                    // the volume is picked up with the buttons' level, the lists just below.
                    // Asking the sound server for them here took 87 ms every two seconds --
                    // six frames at 72 Hz, a regular hitch in the world. See
                    // `crate::volume_keys` for the numbers.
                    //
                    // Not re-read from the glasses on this timer. Every MCU exchange waits up
                    // to 1.5 s for an ack, and doing that twice a second on the render thread
                    // would stall the frame loop far worse than a stale reading ever shows.
                    // The value is read once when the headset opens and tracked from there.
                }
                // Re-read every couple of seconds, which is how a headset or a Bluetooth
                // speaker plugged in mid-session turns up in the list without anything having
                // to watch for it. Taking them is also what keeps them being read.
                if let Some(lists) = mixer.take_devices() {
                    audio = lists;
                }

                // --- touch ---
                //
                // The cached values are updated from the finger rather than waited for on the
                // next two-second poll. Reading them back instead would leave the bar sitting
                // where it was for up to two seconds while the thumb is already elsewhere,
                // which feels like the control has stuck.
                if let Some(touch) = touchscreen.as_mut() {
                    let events = touch.poll();
                    if !events.is_empty() {
                        for action in ui.touch(&events, levels, &audio) {
                            let knob = match action {
                                crate::sidecar::Action::Moved(knob) => knob,
                                crate::sidecar::Action::ChooseDevice(direction, id) => {
                                    // On the mixer's thread, which reads the volume again
                                    // straight after: the level shown belongs to whichever
                                    // sink is default, so it has to follow the choice. Both
                                    // calls used to be made here, 27 ms each.
                                    mixer.choose_device(id);
                                    // Move the tick's mark straight away rather than waiting
                                    // for the worker to confirm it. A list that does not
                                    // respond until later reads as a tap that missed, and the
                                    // wearer taps again.
                                    for device in match direction {
                                        crate::system::Direction::Output => &mut audio.outputs,
                                        crate::system::Direction::Input => &mut audio.inputs,
                                    } {
                                        device.is_default = device.id == id;
                                    }
                                    continue;
                                }
                                // Never produced by a touch: the hold is decided on the clock,
                                // by `settle`, above. Listed so that adding a way for a touch
                                // to end the session has to be a deliberate edit here.
                                crate::sidecar::Action::LeaveSession => continue,
                                crate::sidecar::Action::PressKey(key) => {
                                    // The same keyboard the 3D one uses, so a shift latched on
                                    // the panel is latched in the world too -- two copies of
                                    // that state would let it be on in one place and off in
                                    // the other.
                                    if keyboard.click {
                                        clicks.play();
                                    }
                                    if let Some(stroke) = keyboard.press(key) {
                                        send_stroke(&mut runtime.state, stroke, time_ms);
                                    }
                                    keyboard.after_press(key);
                                    continue;
                                }
                                crate::sidecar::Action::ToggleKeyClick => {
                                    toggle_click(&mut keyboard, &mut prefs, &clicks);
                                    continue;
                                }
                            };
                            let Some(value) = ui.knob_value(knob, levels) else {
                                continue;
                            };
                            match knob {
                                crate::sidecar::Knob::Volume => {
                                    levels.volume = Some(value);
                                    // Through the worker, like the buttons: a slider drag
                                    // sends a value per touch sample, and each one used to
                                    // stop the renderer for a call to the audio server.
                                    mixer.send(crate::volume_keys::Change::Set(value));
                                }
                                crate::sidecar::Knob::Screen => {
                                    levels.screen = Some(value);
                                    if let Some(b) = backlight.as_ref() {
                                        if let Err(e) = b.set(value) {
                                            log::warn!("could not set screen brightness: {e}");
                                        }
                                    }
                                }
                                crate::sidecar::Knob::Glasses => {
                                    // Show the finger's position straight away and correct it
                                    // to whatever step the hardware settled on. The glasses
                                    // have eight of them, so a drag that does not snap back
                                    // would let the handle sit between two settings that do
                                    // not exist.
                                    levels.glasses = Some(value);
                                    if let Some(h) = hmd.as_mut() {
                                        match h.set_brightness(value) {
                                            Ok(reached) => levels.glasses = Some(reached),
                                            Err(e) => {
                                                log::warn!("could not set glasses brightness: {e}");
                                                levels.glasses = None;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                let prepared = ui.prepare(
                    &mut renderer,
                    &mut text,
                    &monitors,
                    &status_text,
                    levels,
                    &audio,
                    &keyboard,
                );
                let (sw, sh) = (side.size.0 as i32, side.size.1 as i32);
                let fbo = side.fbo;
                let quads = scene.quads();
                let rounded = scene.rounded();
                // Borrowed for the draw closure, which cannot also take `keyboard` mutably.
                let keyboard_for_panel = &keyboard;
                renderer.with_context(|gl| unsafe {
                    gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
                    gl.Disable(ffi::SCISSOR_TEST);
                    gl.Viewport(0, 0, sw, sh);
                    gl.ClearColor(0.02, 0.03, 0.05, 1.0);
                    gl.Clear(ffi::COLOR_BUFFER_BIT);
                    ui.draw(
                        gl,
                        quads,
                        rounded,
                        &monitors,
                        levels,
                        &audio,
                        keyboard_for_panel,
                        &prepared,
                    );
                    gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
                })?;

                if !side.pending {
                    let element = TextureRenderElement::from_static_texture(
                        Id::new(),
                        renderer.context_id(),
                        (0.0, 0.0),
                        side.scene.clone(),
                        1,
                        Transform::Normal,
                        Some(1.0),
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    );
                    // The reason is kept rather than reduced to a boolean. A black second
                    // screen with `is_ok()` in front of it says only that something went
                    // wrong, and the two causes -- a mode that cannot be set and a CRTC
                    // somebody else is holding -- want opposite fixes.
                    let presented = match side.compositor.render_frame(
                        &mut renderer,
                        &[element],
                        Color32F::from([0.0, 0.0, 0.0, 1.0]),
                        FrameFlags::DEFAULT,
                    ) {
                        Ok(_) => side.compositor.queue_frame(()).map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    };
                    match presented {
                        Ok(()) => {
                            side.pending = true;
                            side.failures = 0;
                        }
                        Err(reason) => {
                            // Said once per run of failures, not once per frame: the whole
                            // reason this counter exists is that the noisy version produced a
                            // 5.8 GB log for a screen that was simply black.
                            if side.failures == 0 {
                                log::warn!("the sidecar could not present: {reason}");
                            }
                            side.failures += 1;
                        }
                    }
                }
            }

            // A sidecar that cannot present is not worth retrying at frame rate.
            //
            // When it shared a CRTC with the main output, every commit failed and every
            // failure logged the entire atomic state -- 344,113 of them, and a 5.8 GB session
            // log, for a screen that was simply black. The commit failing is a bug to fix
            // wherever it comes from, but the compositor's job in the meantime is to give up
            // on the second screen and carry on driving the first.
            if sidecar_surface
                .as_ref()
                .is_some_and(|s| s.failures >= SIDECAR_FAILURE_LIMIT)
            {
                // Rebuilt rather than abandoned, if it has been a while since the last time.
                //
                // Giving up for the rest of the session was the wrong response, and the
                // evidence is direct: a panel that had given up came straight back when the
                // glasses were unplugged and replugged, which does nothing except force this
                // same rebuild. Whatever the sidecar loses when the panel changes hands
                // between being the main output and being the sidecar, building it again
                // recovers it.
                //
                // Bounded by time, not by nothing. Retrying at frame rate is what once
                // produced a 5.8 GB log for a black screen, and a rebuild that immediately
                // fails again would do the same thing more slowly.
                let long_enough = sidecar_rebuilt
                    .map(|at: std::time::Instant| at.elapsed() >= SIDECAR_REBUILD_INTERVAL)
                    .unwrap_or(true);
                sidecar_surface = None;
                sidecar_ui = None;
                if long_enough {
                    log::warn!(
                        "the sidecar failed to present {SIDECAR_FAILURE_LIMIT} times in a row; \
                         rebuilding the displays. The glasses are unaffected."
                    );
                    sidecar_rebuilt = Some(std::time::Instant::now());
                    break;
                }
                log::error!(
                    "the sidecar failed to present {SIDECAR_FAILURE_LIMIT} times in a row \
                     again, less than {}s after rebuilding for the same reason; leaving it \
                     off. The glasses are unaffected.",
                    SIDECAR_REBUILD_INTERVAL.as_secs()
                );
            }

            // --- present ---
            // Acknowledge a completed flip before drawing the next frame.
            if vblank.borrow_mut().remove(&crtc) {
                if let Err(e) = compositor.frame_submitted() {
                    log::error!("frame_submitted failed: {e}");
                }
                pending_flip = false;
                // The frame that was in flight is now on the glass, so answer everyone who
                // asked when it got there.
                //
                // The time is taken here rather than read out of the DRM event, because
                // smithay's `DrmEvent::VBlank` carries only the CRTC. That makes this the
                // moment the event was *serviced* rather than the moment the scanout began,
                // which is an event-loop hop late -- under a millisecond, and honest about
                // being an approximation. The sequence number, which is the part that
                // distinguishes late from dropped, is exact.
                if !in_flight.is_empty() {
                    let now = crate::pose::now_ns().max(0) as u64;
                    let refresh = std::time::Duration::from_nanos(frame_ns.max(1) as u64);
                    let screen = runtime.state.screen.clone();
                    for feedback in in_flight.drain(..) {
                        feedback.presented(
                            &screen,
                            std::time::Duration::from_nanos(now),
                            smithay::wayland::presentation::Refresh::fixed(refresh),
                            presented_seq,
                            smithay::reexports::wayland_protocols::wp::presentation_time::server
                                ::wp_presentation_feedback::Kind::Vsync,
                        );
                    }
                }
                presented_seq += 1;
            }
            // The sidecar flips on its own schedule -- a different CRTC at a different refresh
            // -- so its completion is acknowledged independently of the glasses'.
            if let Some(side) = sidecar_surface.as_mut() {
                if vblank.borrow_mut().remove(&side.crtc) {
                    if let Err(e) = side.compositor.frame_submitted() {
                        log::error!("sidecar frame_submitted failed: {e}");
                    }
                    side.pending = false;
                }
            }
            if pending_flip {
                skipped += 1;
                // The display has not finished with the last frame. Keep servicing clients and
                // the event loop rather than piling up frames it cannot show.
                display.dispatch_clients(&mut runtime.state)?;
                display.flush_clients()?;
                event_loop.dispatch(Some(Duration::from_millis(4)), runtime)?;
                continue;
            }

            let element = TextureRenderElement::from_static_texture(
                Id::new(),
                renderer.context_id(),
                (0.0, 0.0),
                frame_target.clone(),
                1,
                Transform::Normal,
                Some(1.0),
                None,
                None,
                None,
                Kind::Unspecified,
            );
            compositor.render_frame(
                &mut renderer,
                &[element],
                Color32F::from([0.0, 0.0, 0.0, 1.0]),
                FrameFlags::DEFAULT,
            )?;
            compositor.queue_frame(())?;
            pending_flip = true;
            // Taken after the frame is queued and before the next one is built, so these are
            // exactly the callbacks belonging to the frame that was just handed to the
            // display. Anything a client commits from here on belongs to the next one.
            //
            // Anything still in the list has been superseded rather than shown -- which
            // happens when a flip is skipped -- and is discarded rather than reported, because
            // "presented" is a claim about pixels the wearer saw.
            for stale in in_flight.drain(..) {
                stale.discarded();
            }
            in_flight = runtime.state.take_presentation_feedback();
            frames += 1;
            if last_report.elapsed() >= Duration::from_secs(2) {
                let secs = last_report.elapsed().as_secs_f32();
                log::info!(
                    "presented {frames} frames in {secs:.1}s ({:.0} fps), {skipped} waits for flip",
                    frames as f32 / secs
                );
                frames = 0;
                skipped = 0;
                last_report = std::time::Instant::now();
            }

            // Frame callbacks go out against the output the client believes it is on, which
            // is the only one it has ever been told about. Menus get them too -- see
            // `Spatiand::send_frames`.
            let screen = runtime.state.screen.clone();
            runtime.state.send_frames(&screen, Duration::ZERO);
            runtime.state.space.refresh();
            display.dispatch_clients(&mut runtime.state)?;
            display.flush_clients()?;
            event_loop.dispatch(Some(Duration::from_millis(4)), runtime)?;
        }

        if !runtime.state.running {
            break;
        }
        // Fell out of the frame loop without quitting: the display situation changed, so
        // go round and rebuild against whatever is there now. Nothing to withdraw from
        // clients -- what they were told about is `state.screen`, which does not change when
        // the hardware does.
    }

    if let Some(x) = hmd.as_mut() {
        let _ = x.set_display_mode(DisplayMode::Mono);
    }
    Ok(())
}

/// Choose which connector to drive, and at its native mode.
///
/// **Resolution is never a user setting.** Every display runs at whatever it natively is, the
/// way a phone or tablet does — a spatial desktop has no use for the desktop convention of
/// picking a resolution, because apparent size is controlled by moving and scaling objects in
/// the world instead. That removes mode-selection UI, mode-change races with clients, and the
/// question of what "native" means for a headset entirely.
///
/// For the glasses, native *is* the double-width mode once side-by-side has been enabled: the
/// panel genuinely takes a 3840x1080 signal and gives each eye half of it. So the only choice
/// made here is which connector, not which mode.
///
/// Prefers the glasses and falls back to whatever is connected, so running with no glasses
/// still puts something on a screen rather than failing.
/// Whether any connector that is not the machine's own built-in panel is live.
///
/// Asked once a second while the picture is missing, because the thing the wearer goes off to
/// fix is the cable, and a DisplayPort lane can come up without the USB side ever dropping.
/// Watching only USB would leave "check your cable" on screen after they had.
fn external_connector_present(drm: &DrmDevice) -> bool {
    let Ok(resources) = drm.resource_handles() else {
        return false;
    };
    resources
        .connectors()
        .iter()
        .filter_map(|c| drm.get_connector(*c, false).ok())
        .any(|c| {
            c.state() == connector::State::Connected
                && !c.modes().is_empty()
                && !matches!(
                    c.interface(),
                    connector::Interface::EmbeddedDisplayPort | connector::Interface::LVDS
                )
        })
}

fn pick_output(
    drm: &DrmDevice,
) -> Option<(
    connector::Info,
    crtc::Handle,
    smithay::reexports::drm::control::Mode,
)> {
    let resources = drm.resource_handles().ok()?;
    let mut connected: Vec<connector::Info> = resources
        .connectors()
        .iter()
        .filter_map(|c| drm.get_connector(*c, true).ok())
        .filter(|c| c.state() == connector::State::Connected && !c.modes().is_empty())
        .collect();

    for c in &connected {
        log::info!(
            "connector {}-{}: {} modes, largest {:?}",
            c.interface().as_str(),
            c.interface_id(),
            c.modes().len(),
            c.modes().iter().map(|m| m.size()).max()
        );
    }

    // Always prefer an external connector, whether or not the headset's HID side opened.
    // Tying this to `want_stereo` was wrong: failing to open the glasses over USB says
    // nothing about which panel the wearer is looking at, and the result was Spatiand
    // rendering a stereo pair onto the Deck's portrait screen while the glasses stayed dark.
    connected.sort_by_key(|c| {
        u8::from(matches!(
            c.interface(),
            connector::Interface::EmbeddedDisplayPort | connector::Interface::LVDS
        ))
    });

    for info in &connected {
        // Native mode, full stop: the connector's PREFERRED flag is the display telling us
        // its own resolution, and that is the only answer we accept. Resolution is never a
        // user setting - see the note above.
        let native = info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| info.modes().first())
            .copied();
        if let (Some(mode), Some(crtc)) = (native, first_free_crtc(drm, info, &[])) {
            return Some((info.clone(), crtc, mode));
        }
    }
    None
}

/// Wait for a double-width mode to appear after asking the glasses to go side-by-side.
///
/// The DisplayPort link has to retrain and the kernel re-read the EDID, which is not instant.
/// A forced re-probe each time is what makes the new mode visible.
fn wait_for_stereo_mode(
    drm: &DrmDevice,
    handle: connector::Handle,
    mono_width: u16,
) -> Option<smithay::reexports::drm::control::Mode> {
    let deadline = std::time::Instant::now() + STEREO_MODE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok(fresh) = drm.get_connector(handle, true) {
            // A double-width mode is how the glasses advertise that side-by-side is live -
            // the same signal XREAL's own software uses (docs/xreal-air.md §9).
            //
            // Matched by *shape* rather than against a hardcoded 3840x1080. That number is one
            // model's answer: the Air is 1920x1080 per eye, but the One Pro is 1920x1200, and
            // whatever comes next will be something else again. What is always true is that
            // the side-by-side mode is twice as wide as the mono one at the same height.
            let best = fresh
                .modes()
                .iter()
                .filter(|m| {
                    let (w, _) = m.size();
                    w >= mono_width.saturating_mul(2)
                })
                .max_by_key(|m| (m.size().0 as u32) * (m.size().1 as u32));
            if let Some(m) = best {
                return Some(*m);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// A CRTC this connector can be driven by, that nothing else is already using.
///
/// The `taken` set is the whole point, and it was missing. This used to return the first CRTC
/// the *encoder* could theoretically use, which on the Deck is `crtc-0` for both the panel and
/// the DisplayPort output — so the main output took crtc-0 and the sidecar then took crtc-0 as
/// well. Two `DrmCompositor`s on one scanout engine is not a thing, and the second one's every
/// atomic commit came back `EINVAL`: "New screen configuration invalid", once per frame, for
/// ever, with the Deck's screen simply staying black. Nothing failed loudly enough to say why,
/// because from the main output's point of view everything was fine — it kept presenting at
/// 72 fps throughout.
fn first_free_crtc(
    drm: &DrmDevice,
    connector: &connector::Info,
    taken: &[crtc::Handle],
) -> Option<crtc::Handle> {
    let resources = drm.resource_handles().ok()?;
    connector
        .encoders()
        .iter()
        .filter_map(|e| drm.get_encoder(*e).ok())
        .find_map(|encoder| {
            resources
                .filter_crtcs(encoder.possible_crtcs())
                .into_iter()
                .find(|c| !taken.contains(c))
        })
}

/// Open the first direct-touch panel the kernel is advertising, if there is one.
///
/// Failure is not fatal and barely worth a warning at error level: a Deck has a touchscreen,
/// a desktop with glasses attached does not, and the sidecar is perfectly readable either way.
fn open_touchscreen(
    session: &mut LibSeatSession,
) -> Option<(spatiand_input::Touchscreen, std::path::PathBuf)> {
    let node = spatiand_input::touch::find_touchscreens()
        .into_iter()
        .next()?;
    log::info!("touchscreen: {} at {}", node.name, node.path.display());
    let fd = match session.open(
        &node.path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
    ) {
        Ok(fd) => fd,
        Err(e) => {
            log::warn!("could not open {}: {e}", node.path.display());
            return None;
        }
    };
    match spatiand_input::Touchscreen::from_fd(fd) {
        Ok(t) => Some((t, node.path.clone())),
        Err(e) => {
            log::warn!("touchscreen {} is unusable: {e}", node.path.display());
            None
        }
    }
}

/// A second screen, showing the sidecar.
struct SidecarSurface {
    compositor: DrmCompositor<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    crtc: crtc::Handle,
    scene: GlesTexture,
    fbo: u32,
    size: (u32, u32),
    name: String,
    pending: bool,
    /// Consecutive failed presents. See [`SIDECAR_FAILURE_LIMIT`].
    failures: u32,
    /// Held so the wayland global lives as long as the surface.
    _output: Output,
}

/// Say on the panel what the session is doing, while the glasses are still dark.
///
/// Best effort throughout: a startup that cannot draw its own progress screen must still
/// start. Every failure here is swallowed rather than reported, because the alternative is a
/// session that refuses to begin over a message about beginning.
///
/// Presents synchronously and waits briefly for the flip, which a frame loop would never do —
/// but there is no frame loop yet, and a progress screen that is queued and never scanned out
/// is exactly as useful as no progress screen.
#[allow(clippy::too_many_arguments)]
fn show_startup_stage(
    renderer: &mut GlesRenderer,
    text: &mut TextRenderer,
    scene: &mut Scene,
    side: &mut SidecarSurface,
    ui: &crate::sidecar::Sidecar,
    vblank: &Rc<RefCell<HashSet<crtc::Handle>>>,
    event_loop: &mut EventLoop<'static, Runtime>,
    runtime: &mut Runtime,
    stage: crate::startup::Stage,
) {
    // Acknowledge whatever is still in the air, or this present is simply dropped.
    if side.pending {
        let deadline = std::time::Instant::now() + Duration::from_millis(80);
        while std::time::Instant::now() < deadline {
            if vblank.borrow_mut().remove(&side.crtc) {
                let _ = side.compositor.frame_submitted();
                side.pending = false;
                break;
            }
            let _ = event_loop.dispatch(Some(Duration::from_millis(4)), runtime);
        }
        if side.pending {
            return;
        }
    }

    // Rasterised through the same cache the window titles use, so three short strings cost
    // three textures for the life of the session.
    let label = scene
        .title_texture(renderer, text, stage.label(), 80.0)
        .map(|t| (t.id, t.aspect));

    let (sw, sh) = (side.size.0 as i32, side.size.1 as i32);
    let fbo = side.fbo;
    let step = stage.step();
    let total = crate::startup::STAGES.len();
    let quads = scene.quads();
    let rounded = scene.rounded();
    let drawn = renderer.with_context(|gl| unsafe {
        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
        gl.Disable(ffi::SCISSOR_TEST);
        gl.Viewport(0, 0, sw, sh);
        gl.ClearColor(0.02, 0.03, 0.05, 1.0);
        gl.Clear(ffi::COLOR_BUFFER_BIT);
        ui.draw_startup(gl, quads, rounded, label, step, total);
        gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
    });
    if drawn.is_err() {
        return;
    }

    let element = TextureRenderElement::from_static_texture(
        Id::new(),
        renderer.context_id(),
        (0.0, 0.0),
        side.scene.clone(),
        1,
        Transform::Normal,
        Some(1.0),
        None,
        None,
        None,
        Kind::Unspecified,
    );
    if side
        .compositor
        .render_frame(
            renderer,
            &[element],
            Color32F::from([0.0, 0.0, 0.0, 1.0]),
            FrameFlags::DEFAULT,
        )
        .is_ok()
        && side.compositor.queue_frame(()).is_ok()
    {
        side.pending = true;
        log::info!("startup {}/{}: {}", step, total, stage.label());
    }
}

/// Bring up the sidecar on a connector the main output is not using.
///
/// Returns `Ok(None)` rather than an error when there is simply no second screen: a Deck with
/// the glasses on its only external port and the panel already in use is a normal state, not a
/// failure.
fn build_sidecar(
    drm: &mut DrmDevice,
    gbm: &GbmDevice<DrmDeviceFd>,
    renderer: &mut GlesRenderer,
    used: connector::Handle,
    used_crtc: crtc::Handle,
) -> Result<Option<SidecarSurface>, Box<dyn std::error::Error>> {
    let resources = drm.resource_handles()?;
    let candidates: Vec<connector::Info> = resources
        .connectors()
        .iter()
        .filter_map(|c| drm.get_connector(*c, true).ok())
        .filter(|c| {
            c.handle() != used && c.state() == connector::State::Connected && !c.modes().is_empty()
        })
        .collect();

    for info in candidates {
        let Some(mode) = info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| info.modes().first())
            .copied()
        else {
            continue;
        };
        let Some(crtc) = first_free_crtc(drm, &info, &[used_crtc]) else {
            log::warn!(
                "no free CRTC for a sidecar on {}-{}; the main output has the only one it can \
                 use",
                info.interface().as_str(),
                info.interface_id()
            );
            continue;
        };
        let (w, h) = mode.size();

        let surface = drm.create_surface(crtc, mode, &[info.handle()])?;
        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let formats = renderer.egl_context().dmabuf_render_formats().clone();

        let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Spatiand".into(),
                model: "Sidecar".into(),
            },
        );
        let output_mode = OutputMode {
            size: (w as i32, h as i32).into(),
            refresh: (mode.vrefresh() * 1000) as i32,
        };
        output.change_current_state(
            Some(output_mode),
            Some(Transform::Normal),
            None,
            Some((0, 0).into()),
        );
        output.set_preferred(output_mode);

        let compositor = DrmCompositor::new(
            smithay::output::OutputModeSource::Auto(output.clone()),
            surface,
            None,
            allocator,
            GbmFramebufferExporter::new(gbm.clone(), None),
            [Fourcc::Argb8888, Fourcc::Xrgb8888],
            formats,
            drm.cursor_size(),
            Some(gbm.clone()),
        )?;

        let scene: GlesTexture =
            renderer.create_buffer(Fourcc::Abgr8888, (w as i32, h as i32).into())?;
        let built = renderer.with_context(|gl| unsafe {
            let mut fbo = 0;
            gl.GenFramebuffers(1, &mut fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                scene.tex_id(),
                0,
            );
            let status = gl.CheckFramebufferStatus(ffi::FRAMEBUFFER);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            (fbo, status)
        })?;
        if built.1 != ffi::FRAMEBUFFER_COMPLETE {
            return Err(format!("sidecar framebuffer incomplete: {:#x}", built.1).into());
        }

        return Ok(Some(SidecarSurface {
            compositor,
            crtc,
            scene,
            fbo: built.0,
            size: (w as u32, h as u32),
            name,
            pending: false,
            failures: 0,
            _output: output,
        }));
    }
    Ok(None)
}

/// The least time between two output rebuilds.
///
/// A rebuild is expensive — forcing the display mode, waiting for the DP link, and up to ten
/// seconds for a stereo mode to be advertised — so a headset that drops every thirty seconds
/// can otherwise keep the compositor permanently mid-rebuild, flickering both screens and
/// never settling long enough to draw. Waiting is strictly better than thrashing: the hardware
/// either comes back, in which case nothing needed doing, or it does not, in which case the
/// rebuild happens a few seconds later than it would have.
const REBUILD_COOLDOWN: Duration = Duration::from_secs(8);

/// How long a button must be held to end a session whose glasses have dropped out.
///
/// Long enough that it cannot be done by accident while fumbling for a machine that has just
/// gone dark, short enough that someone who means it does not think it is broken.
const HOLD_TO_LEAVE: Duration = Duration::from_secs(2);

/// Is any real button down?
///
/// Pad *touch* is excluded deliberately. A thumb resting on a pad is how the machine is held,
/// so counting it would mean the hold never expires -- and, worse, that simply picking the Deck
/// up starts a countdown to ending the session.
fn any_button_held(state: &spatiand_input::ControllerState) -> bool {
    spatiand_input::Control::ALL
        .into_iter()
        .filter(|c| {
            !matches!(
                c,
                spatiand_input::Control::LPadTouch | spatiand_input::Control::RPadTouch
            )
        })
        .any(|c| state.buttons.is_down(c))
}

/// How far a point on a quad is from the quad's middle, in the quad's own units.
///
/// The measurement a keyboard resize is driven by: the ratio of this now to this when the frame
/// was grabbed is how much bigger the keyboard should be.
fn span(u: f64, v: f64) -> f64 {
    let (du, dv) = (u - 0.5, v - 0.5);
    (du * du + dv * dv).sqrt()
}

/// Whichever of two surfaces a ray reached first.
///
/// The pointer is drawn at its hit's distance, and that distance is the *only* thing setting how
/// far away the two eyes agree it is. Every surface the ray can land on therefore has to be in
/// this comparison, or the reticle is drawn at the depth of something else entirely — visibly
/// on the right spot in each eye, and floating behind the thing it is pointing at in stereo.
fn nearer(
    a: Option<spatiand_render::ray::Hit>,
    b: Option<spatiand_render::ray::Hit>,
) -> Option<spatiand_render::ray::Hit> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.distance < a.distance { b } else { a }),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;
    use spatiand_render::ray::Hit;

    fn hit(distance: f64) -> Hit {
        Hit {
            distance,
            u: 0.5,
            v: 0.5,
            point: DVec3::ZERO,
        }
    }

    #[test]
    fn the_pointer_lands_on_the_nearer_of_two_surfaces() {
        // The keyboard hangs in front of whatever is behind it, so when a ray reaches both, the
        // keyboard is what the wearer is pointing at.
        assert_eq!(nearer(Some(hit(2.5)), Some(hit(1.3))), Some(hit(1.3)));
        assert_eq!(nearer(Some(hit(1.0)), Some(hit(1.3))), Some(hit(1.0)));
    }

    #[test]
    fn a_surface_reached_by_only_one_ray_still_counts() {
        // The case that was actually broken: a ray passing under every window but across the
        // keyboard used to find nothing at all and park the reticle at a fixed distance, well
        // behind the keys it was sitting on.
        assert_eq!(nearer(None, Some(hit(1.3))), Some(hit(1.3)));
        assert_eq!(nearer(Some(hit(1.3)), None), Some(hit(1.3)));
        assert_eq!(nearer(None, None), None);
    }
}

/// Send one keystroke, wrapped in whatever modifiers were latched, to whatever has focus.
///
/// The modifiers are pressed around the key rather than merely changing the label. Latching
/// shift used to do nothing but redraw the face, so the keyboard showed `A` and typed `a` — a
/// failure that looks like a broken keymap from the client's side and like a working keyboard
/// from the wearer's.
///
/// Press and release together: the on-screen keyboard has no notion of holding a key, and a
/// press with no matching release leaves the client repeating that character for ever -- which
/// is a spectacular way to discover the bug. The modifiers are released in the reverse order
/// they were pressed, so the client never sees a stray one left down.
/// Flip the keyboard's click, store the choice, and let the wearer hear what they chose.
///
/// One function because both keyboards reach it and the three steps have to stay together: the
/// state that the drawing reads, the file that outlives the session, and the sound device,
/// which is handed back when the answer is "off" rather than left open and silent.
fn toggle_click(
    keyboard: &mut spatiand_shell::Keyboard,
    prefs: &mut crate::prefs::Prefs,
    clicks: &crate::click::Clicks,
) {
    let on = keyboard.toggle_click();
    prefs.keyboard_click = on;
    prefs.save();
    log::info!("keyboard click {}", if on { "on" } else { "off" });
    if on {
        // The control's own confirmation. Turning the sound on and hearing nothing until the
        // next letter leaves you unsure the button did anything.
        clicks.play();
    }
    // Nothing to do when it goes off: the stream is held open by `Clicks::wanted`, which the
    // frame below sets from this same flag, so switching the sound off gives the device back
    // on the next pass. A second way to say it here is a second thing to keep in step.
}

fn send_stroke(state: &mut Spatiand, stroke: spatiand_shell::keyboard::Stroke, time_ms: u32) {
    use spatiand_shell::keyboard as kb;
    let mut held = Vec::new();
    if stroke.ctrl {
        held.push(kb::KEY_LEFTCTRL);
    }
    if stroke.alt {
        held.push(kb::KEY_LEFTALT);
    }
    if stroke.shift {
        held.push(kb::KEY_LEFTSHIFT);
    }

    for code in &held {
        send_key_state(state, *code, true, time_ms);
    }
    send_key_state(state, stroke.code, true, time_ms);
    send_key_state(state, stroke.code, false, time_ms);
    for code in held.iter().rev() {
        send_key_state(state, *code, false, time_ms);
    }
}

/// One key transition.
///
/// Wayland keycodes are evdev codes offset by 8, a historical debt from X11. The table in
/// `spatiand_shell::keyboard` stores the evdev numbers so it can be read against the kernel
/// header, and the offset is applied here, once.
fn send_key_state(state: &mut Spatiand, evdev_code: u32, pressed: bool, time_ms: u32) {
    // Typing is attention, even with the head perfectly still and the pointer nowhere near
    // what is being typed into. See `crate::attention`.
    state.attention.stir();
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let code = smithay::input::keyboard::Keycode::new(evdev_code + 8);
    let key_state = if pressed {
        smithay::backend::input::KeyState::Pressed
    } else {
        smithay::backend::input::KeyState::Released
    };
    keyboard.input::<(), _>(
        state,
        code,
        key_state,
        smithay::utils::SERIAL_COUNTER.next_serial(),
        time_ms,
        |_, _, _| smithay::input::keyboard::FilterResult::Forward,
    );
}

/// Decide which sensor axis convention to track with, now that a headset is open.
///
/// The order of preference is the whole point, and it is the reverse of what it used to be.
///
/// A headset that publishes its IMU mounting wins outright, because the mounting is a fact
/// about where a chip is soldered — the same on every unit of that product, and not something
/// a wearer can have a different answer to. A stored map used to beat it, and that is the
/// defect this function exists to close: a calibration is three head movements, two of which
/// (nodding and tilting) are adjacent enough that performing one when asked for the other
/// records them swapped. The result is still a proper rotation, so nothing downstream can
/// tell it is wrong; the only symptom is that looking down rolls the world. It then survived
/// every restart, because the stored file beat the correct built-in answer — which is exactly
/// the loop the wearer was stuck in, reaching for "Try pitch and roll" after every launch.
///
/// Calibration remains the right answer for hardware whose mounting nobody has measured, and
/// that is the only case that now reaches it.
pub fn settle_axes(
    info: &spatiand_hmd::HmdInfo,
    stored: Option<spatiand_track::AxisMap>,
    tracker: &mut HeadTracker,
    calibration: &mut Option<Calibration>,
) {
    if let Some(mounting) = info.sensor_axes {
        match AxisMap::from_mounting(mounting) {
            Some(map) => {
                if let Some(old) = stored.filter(|s| *s != map) {
                    log::info!(
                        "ignoring the stored axis map ({}): {} states its own IMU mounting, \
                         which gives {}",
                        old.summary(),
                        info.name,
                        map.summary()
                    );
                }
                log::info!("axes from {} hardware: {}", info.name, map.summary());
                tracker.set_axes(map);
                // Nothing left to measure, so an in-progress flow is asking the wearer for
                // an answer we already have.
                *calibration = None;
                return;
            }
            // An authoring error in `devices.toml`, caught by its own unit test, so this is
            // a belt-and-braces path rather than an expected one.
            None => log::error!(
                "{} has an impossible IMU mounting in the device table ({mounting:?}); \
                 falling back to measuring it",
                info.name
            ),
        }
    }

    match stored {
        Some(map) => {
            log::info!("axes from the stored calibration: {}", map.summary());
            tracker.set_axes(map);
        }
        None if calibration.is_none() => {
            log::info!(
                "{} does not state its IMU mounting and nothing is stored — calibrating",
                info.name
            );
            *calibration = Some(Calibration::new());
        }
        None => {}
    }
}

/// Save the frame just drawn.
///
/// Goes to the user's screenshot folder rather than anywhere Spatiand-specific, because the
/// point of it is to be found: with the world only visible inside the glasses, a photograph is
/// impossible and describing a layout bug is slow and lossy. A file with a timestamp is the
/// difference between "the text is off screen" and being able to see which text and by how far.
fn capture(
    renderer: &mut GlesRenderer,
    fbo: u32,
    width: u32,
    height: u32,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    renderer.with_context(|gl| unsafe {
        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
        gl.ReadPixels(
            0,
            0,
            width as i32,
            height as i32,
            ffi::RGBA,
            ffi::UNSIGNED_BYTE,
            pixels.as_mut_ptr() as *mut _,
        );
        gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
    })?;

    // The scene texture already holds the image flipped (see the note on flip_y), so reading
    // it back bottom-row-first flips it a second time and lands the right way up.
    let dir = screenshot_directory();
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("spatiand-{stamp}.png"));
    image::save_buffer(&path, &pixels, width, height, image::ColorType::Rgba8)?;
    Ok(path)
}

/// Where screenshots go: `XDG_PICTURES_DIR/Screenshots` if the user has one, else the
/// conventional `~/Pictures/Screenshots`.
fn screenshot_directory() -> std::path::PathBuf {
    if let Ok(explicit) = std::env::var("SPATIAND_SCREENSHOTS") {
        return std::path::PathBuf::from(explicit);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    // user-dirs.dirs is the freedesktop record of where Pictures actually is, which is not
    // always ~/Pictures once a locale is involved.
    let config = std::path::Path::new(&home).join(".config/user-dirs.dirs");
    if let Ok(text) = std::fs::read_to_string(&config) {
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("XDG_PICTURES_DIR=") {
                let cleaned = value.trim().trim_matches('"').replace("$HOME", &home);
                return std::path::PathBuf::from(cleaned).join("Screenshots");
            }
        }
    }
    std::path::Path::new(&home).join("Pictures/Screenshots")
}

/// Point SDDM back at the desktop session, so exiting actually leaves spatial mode.
///
/// steamosctl owns the autologin drop-in; writing that file directly does not work, because
/// SteamOS regenerates it and ignores hand edits.
fn return_to_desktop() {
    for args in [
        ["set-default-desktop-session", "plasma.desktop"],
        ["switch-to-desktop-mode", "plasma.desktop"],
    ] {
        match std::process::Command::new("steamosctl").args(args).status() {
            Ok(s) if s.success() => {}
            Ok(s) => log::warn!("steamosctl {args:?} exited with {s}"),
            Err(e) => log::warn!("could not run steamosctl {args:?}: {e}"),
        }
    }
}

/// Largest panel size that fits the field of view, in metres at [`PANEL_DISTANCE`].
///
/// A fixed width cannot work across both outputs. 0.9 m suits the glasses, and on the Deck's
/// portrait panel the same panel is rolled a quarter turn, so the image's *width* now runs
/// along the screen's short axis and the heading runs off the edge. Fitting to the actual
/// FOV handles both, and any future headset, without a magic number per device.
pub fn fit_panel(aspect: f32, h_fov_deg: f64, v_fov_deg: f64, portrait: bool) -> (f32, f32) {
    // After a quarter turn the image's width spans the screen's vertical extent and its
    // height spans the horizontal one.
    let (fov_for_width, fov_for_height) = if portrait {
        (v_fov_deg, h_fov_deg)
    } else {
        (h_fov_deg, v_fov_deg)
    };
    // Leave a margin. Text touching the edge of the field is uncomfortable to read even when
    // it technically fits, because it sits where the optics are worst -- and the glasses'
    // usable area is smaller than their nominal field, so 0.8 was still too generous in
    // practice: every prompt reached the edge.
    let usable = 0.68;
    let extent = |fov: f64| 2.0 * PANEL_DISTANCE * ((fov * usable / 2.0).to_radians().tan() as f32);
    let max_w = extent(fov_for_width);
    let max_h = extent(fov_for_height);
    let aspect = aspect.max(0.01);
    // Fit inside both bounds while keeping the image's proportions.
    let width = max_w.min(max_h * aspect);
    (width, width / aspect)
}

/// Head-locked panel transform. See `backend_winit` for the frame conventions.
///
/// `portrait` rolls the content a quarter turn for the Deck's built-in screen, which is
/// mounted sideways. The roll is applied about the view axis (+X, forward) *after* the head
/// orientation, so it rotates the image on the glass rather than tilting the world.
///
/// The panel hangs off the **eye centre**, not the origin. The neck model puts the eyes about
/// 7.5 cm above and 10 cm in front of the pivot, so a panel centred on the origin sits a few
/// degrees below where you are looking - enough that a correctly-sized panel still loses its
/// bottom line off the edge of the field. That was the bug behind "the text is off screen":
/// the panel was the right size and in the wrong place.
pub fn head_locked_panel_sized(
    orientation: DQuat,
    width: f32,
    height: f32,
    portrait: bool,
) -> Mat4 {
    let _ = PANEL_WIDTH;
    let cfg = StereoConfig::default();
    let centre = Vec3::new(
        (cfg.neck_forward_m as f32) + PANEL_DISTANCE,
        0.0,
        cfg.neck_up_m as f32,
    );
    let basis = Mat4::from_cols(
        (-Vec3::Y * width).extend(0.0),
        (Vec3::Z * height).extend(0.0),
        Vec3::X.extend(0.0),
        centre.extend(1.0),
    );
    let roll = if portrait {
        Mat4::from_rotation_x(std::f32::consts::FRAC_PI_2)
    } else {
        Mat4::IDENTITY
    };
    Mat4::from_quat(orientation.as_quat()) * roll * basis
}
