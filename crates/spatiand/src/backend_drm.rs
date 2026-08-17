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
use spatiand_shell::{HudAction, Mode, Shell, ShellEvent};
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

/// The largest single-frame pad movement treated as real, in pad units (−1..1).
///
/// The pads are absolute, so a scroll is the difference between two samples. That is fine
/// while a thumb is down and nonsense across the moment it lifts: the contact's last reported
/// position is not where the thumb was, and the difference can be the entire width of the pad.
/// A real thumb moves a few hundredths of that in one 14 ms frame, so anything past a quarter
/// of the pad is a report artefact and is dropped rather than scrolled.
const MAX_PAD_STEP: f64 = 0.25;
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

pub fn run(
    event_loop: &mut EventLoop<'static, Runtime>,
    display: &mut Display<Spatiand>,
    runtime: &mut Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    GLOBAL_DISPLAY_HANDLE.with(|h| *h.borrow_mut() = Some(runtime.display_handle.clone()));

    // --- session ---
    let (session, _notifier) = LibSeatSession::new()?;
    log::info!("seat: {}", session.seat());

    // --- gpu ---
    let gpu = udev::primary_gpu(&session.seat())?
        .and_then(|p| DrmNode::from_path(p).ok()?.node_with_type(NodeType::Primary)?.ok())
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
    let mut hmd: Option<Box<dyn spatiand_hmd::Hmd>> = None;
    // Has a headset ever been open in this session?
    //
    // The difference between "you started Spatiand without plugging the glasses in" and "your
    // glasses blinked". The first has nothing to lose and any button should back out of it;
    // the second has every open window to lose, and must not be ended by a stray press. See
    // where this is read, below.
    let mut had_headset = false;
    // When the button now being held went down, while waiting with windows to lose.
    let mut leave_held_since: Option<std::time::Instant> = None;

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
    // Where the left thumb was last frame, so an absolute pad reads as a scroll delta rather
    // than jumping the moment it lands.
    let mut last_left_pad: Option<(f32, f32)> = None;
    let mut keyboard = spatiand_shell::Keyboard::default();
    // A keyboard resize in progress: the scale when the frame was grabbed, and how far from
    // the middle the pointer was at that moment. Held rather than recomputed so the drag
    // measures against where it started instead of against the size it is producing — which
    // would feed the result back into its own input and run away.
    type KeyboardResize = (f32, f64);
    let mut keyboard_resize: Option<KeyboardResize> = None;
    let mut keyboard_border_hot = false;
    // Left thumb position while a window is being dragged, for the depth adjustment.
    let mut drag_left_y: Option<f32> = None;
    let mut monitors = crate::system::Monitors::new();
    // The panel is a touchscreen, and in a spatial session nothing else is reading it.
    //
    // Opened through the session rather than with `File::open`: the event node is
    // `root:input` with no ACL for the logged-in user, so only logind can hand it over. It is
    // attached to seat0, which is what makes that possible — a device on no seat cannot be
    // taken this way, however permissive its mode bits.
    let mut touchscreen = open_touchscreen(&mut session.clone());
    let backlight = crate::system::Backlight::find();
    // What the sidecar shows and changes. Read here once so the panel has something to draw
    // before the first two-second poll comes round; the glasses filled in when one is opened.
    let mut levels = crate::sidecar::Levels {
        screen: backlight.as_ref().and_then(|b| b.level()),
        glasses: None,
        volume: crate::system::volume(),
    };
    let mut audio = crate::sidecar::Audio::default();
    let mut slow_status = std::time::Instant::now();

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
    let has_kde = std::path::Path::new("/usr/bin/kcmshell6").exists()
        || std::path::Path::new("/usr/bin/systemsettings").exists();
    let mut shell = Shell::new(apps, has_kde);
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
    let mut tracker =
        HeadTracker::new(stored.unwrap_or(AxisMap::XREAL_AIR), TrackerConfig::default());

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
        let allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
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
        let _global = output.create_global::<Spatiand>(&runtime.display_handle);
        output.change_current_state(Some(output_mode), Some(Transform::Normal), None, Some((0, 0).into()));
        output.set_preferred(output_mode);
        runtime.state.space.map_output(&output, (0, 0));

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
                    log::info!("stereo mode {sw}x{sh}@{} appeared; adopting", stereo_mode.vrefresh());
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



        // --- scene setup ---
        let stereo = StereoConfig {
            h_fov_deg: hmd.as_ref().map(|x| x.info().h_fov_deg).unwrap_or(40.0),
            ipd_m: hmd.as_ref().map(|x| x.info().default_ipd_mm).unwrap_or(63.0) / 1000.0,
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
        let frame_target: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, (w as i32, h as i32).into())?;
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
        let mut last_rebuild = std::time::Instant::now();
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
            let mut two_handed: Option<spatiand_input::GestureDelta> = None;
            let mut leaving = false;
            let mut screenshot = false;
            if let Some(c) = controller.as_mut() {
                c.poll();
                if hmd.is_none() && !had_headset {
                    // Nothing has ever been open, so there is nothing to lose and no headset to
                    // read a more specific instruction from. Any button backs out.
                    if c.any_pressed() {
                        log::info!("button pressed while waiting — returning to the desktop");
                        leaving = true;
                    }
                } else if hmd.is_none() {
                    // The glasses were here and have gone. This is the dangerous case: every
                    // open window is still alive and leaving would take the lot.
                    //
                    // A USB-C link that drops rarely stays dropped -- the observed case came
                    // back nine seconds later on its own -- so the right behaviour while
                    // waiting is to keep waiting. A single press used to end the session here,
                    // which turned a blink of the cable into losing everything that was open,
                    // and read as a crash rather than as a button doing what it said.
                    if any_button_held(c.state()) {
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
                        // In the world the D-pad moves focus between windows. The shell has no
                        // window list -- deliberately, it has no Wayland at all -- so this is
                        // the one navigation case the compositor answers itself.
                        if !shell.menu_is_open() {
                            let step = match control {
                                spatiand_input::Control::Left => -1i32,
                                spatiand_input::Control::Right => 1,
                                _ => 0,
                            };
                            if step != 0 {
                                let all: Vec<smithay::desktop::Window> =
                                    runtime.state.space.elements().cloned().collect();
                                if !all.is_empty() {
                                    let current = all
                                        .iter()
                                        .position(|w| runtime.state.layout.is_focused(w))
                                        .unwrap_or(0) as i32;
                                    let next =
                                        (current + step).rem_euclid(all.len() as i32) as usize;
                                    let window = all[next].clone();
                                    runtime.state.focus_window(&window);
                                }
                                continue;
                            }
                        }
                        if let Some(intent) = intent_for(*control) {
                            if let Some(event) = shell.handle(intent) {
                                shell_events.push(event);
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
                        if let Err(e) =
                            spatiand_platform::launch(&app.exec, &runtime.state.socket_name)
                        {
                            log::warn!("could not launch {}: {e}", app.name);
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
                    ShellEvent::Hud(action) => match action {
                        HudAction::Recentre => {
                            tracker.recenter();
                            log::info!("recentred");
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
                            shell.set_environments(
                                environments.entries(),
                                environments.choice(),
                            );
                        }
                        HudAction::Screenshot => screenshot = true,
                        HudAction::ToggleKeyboard => {
                            keyboard.open = !keyboard.open;
                            log::info!("keyboard {}", if keyboard.open { "shown" } else { "hidden" });
                        }
                        HudAction::ReturnToDesktop => leaving = true,
                        HudAction::OpenSystemSettings(module) => {
                            let command = format!("kcmshell6 {module}");
                            if let Err(e) =
                                spatiand_platform::launch(&command, &runtime.state.socket_name)
                            {
                                log::warn!("could not open {module}: {e}");
                            }
                        }
                        HudAction::Dismiss => {}
                    },
                }
            }
            // A signal is a request to leave, handled exactly like the button that means the
            // same thing -- so the controller gets handed back and the glasses go back to 2D.
            if crate::shutdown::requested() {
                log::info!("asked to stop; returning the hardware and exiting");
                leaving = true;
            }
            if leaving {
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
                            Some(_) => log::info!("calibration agrees with the hardware: {}", map.summary()),
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

            let orientation = tracker.predicted_orientation(
                spatiand_track::DEFAULT_PREDICTION_SECONDS,
                spatiand_track::DEFAULT_PREDICTION_MAX_DEGREES,
            );
            let prompt_text = match calibration.as_ref() {
                _ if hmd.is_none() => {
                    // Follow HoloFrame here: with no glasses, say so plainly on whatever screen
                    // there is rather than presenting a spatial world nobody can see. Creating a
                    // stereo desktop for absent glasses is how you end up with windows scattered
                    // across a display you cannot look at.
                    let exit_hint = match (controller.is_some(), had_headset) {
                        // Two different offers, because the stakes are different. With windows
                        // open, saying "press any button to leave" next to a screen that has
                        // just gone dark is an invitation to lose them.
                        (true, true) => "\n\nHold any button for 2s\nto end the session.",
                        (true, false) => "\n\nPress any button to\nreturn to the desktop.",
                        (false, _) => "",
                    };
                    if had_headset {
                        format!(
                            "Glasses disconnected\n\nYour windows are still open.\n\nSpatiand is waiting for the\nglasses to come back — plug\nthem in again and this screen\nwill hand over to them.{exit_hint}"
                        )
                    } else {
                        format!(
                            "Plug in your XR glasses\n\nSpatiand is waiting.\n\nConnect XREAL Air glasses\nover USB-C and this screen\nwill hand over to them.{exit_hint}"
                        )
                    }
                }
                Some(c) => {
                    let p = c.prompt();
                    format!("{}\n\n{}\n\n{}", p.heading, p.body, p.status)
                }
                // Nothing in the middle of the view during normal use. That space belongs to
                // the windows; the readout that used to live there is in the corner bar now.
                None => String::new(),
            };
            let waiting = hmd.is_none();

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
                status_text = crate::status::line(runtime.state.space.elements().count());
                last_status_update = std::time::Instant::now();
            }
            scene.sync_status(&mut renderer, &mut text, &status_text, ppd)?;
            if keyboard.open {
                scene.sync_keyboard(&mut renderer, &mut text, &keyboard, ppd)?;
            }
            scene.sync_apps(&mut renderer, &mut text, &shell, ppd)?;
            scene.sync_menu(
                &mut renderer,
                &mut text,
                crate::menu::model(&shell).as_ref(),
                ppd,
                (stereo.h_fov_deg, stereo.v_fov_deg()),
            )?;

            // Import client buffers before the draw closure takes the context.
            let mut windows = crate::scene::collect_windows(&mut renderer, &runtime.state);
            for quad in windows.iter_mut() {
                let title = runtime
                    .state
                    .title_of(&quad.window)
                    .unwrap_or_else(|| "Untitled".to_string());
                quad.title = scene.title_texture(&mut renderer, &mut text, &title, ppd);
                if let Some(app_id) = runtime.state.app_id_of(&quad.window) {
                    quad.icon = scene.window_icon(&mut renderer, &app_id);
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

            // Light the close button whichever hand is over it. Either pad can press it, so
            // lighting only the one under the dominant hand would leave the other pressing a
            // control that never acknowledged it was aimed at.
            for aim in [right_aim.as_ref(), left_aim.as_ref()].into_iter().flatten() {
                if aim.zone == Some(Zone::Close) {
                    if let Some((index, _)) = aim.hit {
                        if let Some(quad) = windows.get_mut(index) {
                            quad.close_hot = true;
                        }
                    }
                }
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
                                                placement.radius =
                                                    (placement.radius + delta * 2.5).clamp(0.8, 8.0);
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
                    Some(Drag::Depth {
                        ref window,
                        start_radius,
                        start_y,
                    }) => {
                        if let Some(p) = pads.as_ref() {
                            {
                                if let Some(mut placement) = runtime.state.layout.get(window) {
                                    // Thumb up pushes it away. Clamped so a window can never
                                    // end up inside your head or so far off it is unreadable.
                                    let delta = (p.left_pad.y - start_y) as f64;
                                    placement.radius = (start_radius + delta * 2.0).clamp(0.8, 8.0);
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
                        let gesturing = two_handed
                            .map(|d| !d.is_negligible())
                            .unwrap_or(false);
                        if gesturing {
                            // A gesture is not a scroll. Forgetting where the left thumb was
                            // means the first frame after the gesture ends measures nothing,
                            // instead of measuring the whole distance the thumb travelled
                            // while it was busy moving a window -- which arrived as one
                            // enormous scroll the moment you let go.
                            last_left_pad = None;
                            if let Some(delta) = two_handed {
                                let focused = runtime
                                    .state
                                    .space
                                    .elements()
                                    .find(|w| runtime.state.layout.is_focused(w))
                                    .cloned();
                                if let Some(window) = focused {
                                    if let Some(mut placement) = runtime.state.layout.get(&window)
                                    {
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
                            let cursor_aim = match (right_aim.as_ref(), left_aim.as_ref()) {
                                (Some(right), _) => Some(right),
                                (None, left) => left,
                            };
                            if let Some(a) = cursor_aim {
                                pointers.motion(&mut runtime.state, a, &windows, time_ms);
                            }
                            // The left pad is the wheel, and it turns whatever the cursor is
                            // on -- which is the right pad's target when that thumb is down.
                            if let Some(p) = pads.as_ref() {
                                if p.left_pad.touched && !p.left_pad.clicked {
                                    if let Some((px, py)) = last_left_pad {
                                        let step = |now: f32, then: f32| {
                                            let d = (now - then) as f64;
                                            // A thumb cannot cross the pad inside one frame.
                                            // The pads report a stray sample as a contact ends,
                                            // and unguarded that arrives as the full width of
                                            // the pad in 14 ms -- a scroll of hundreds of
                                            // lines from a thumb that was merely lifting off.
                                            // This is the jumpiness that was impossible to
                                            // describe because it had nothing to do with what
                                            // the thumb was doing.
                                            if d.abs() > MAX_PAD_STEP {
                                                0.0
                                            } else {
                                                d * SCROLL_SCALE
                                            }
                                        };
                                        let dx = step(p.left_pad.x, px);
                                        let dy = step(p.left_pad.y, py);
                                        // Natural direction: dragging the thumb up sends the
                                        // content up, which is what every touchpad does.
                                        pointers.scroll(&mut runtime.state, -dx, dy, time_ms);
                                    }
                                    last_left_pad = Some((p.left_pad.x, p.left_pad.y));
                                } else {
                                    if last_left_pad.is_some() {
                                        pointers.scroll_stop(&mut runtime.state, time_ms);
                                    }
                                    last_left_pad = None;
                                }
                            }
                        }
                    }
                }

                // Buttons. The pads and the face buttons both click, because pressing a pad
                // moves the thumb slightly as it goes down -- fine for a button, bad for a
                // precise click on something small.
                if let Some(p) = pads.as_ref() {
                    let right_click = p.right_pad.clicked;
                    let left_click = p.left_pad.clicked;

                    // Confirm the press under the thumb that made it. Without this the pads
                    // feel dead: the click registers, the world responds, and the hand is
                    // told nothing -- which reads as the pad being broken rather than as
                    // missing feedback.
                    if let Some(c) = controller.as_ref() {
                        if right_click && !right_was_down {
                            c.pulse(spatiand_input::HapticPad::Right, spatiand_input::Feel::Click);
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

                    // Is the pointer over the keyboard's frame right now? Only for lighting it
                    // up, so the wearer can tell the border is a thing that can be grabbed.
                    keyboard_border_hot = keyboard_quad
                        .as_ref()
                        .zip(right_aim.as_ref())
                        .and_then(|(q, a)| spatiand_render::intersect_quad(&a.ray, q))
                        .map(|hit| {
                            keyboard.target_at(hit.u, hit.v)
                                == Some(spatiand_shell::keyboard::Target::Border)
                        })
                        .unwrap_or(false)
                        || keyboard_resize.is_some();

                    // A resize in progress owns the pointer until it is let go.
                    if let (Some(start), Some(q)) = (keyboard_resize, keyboard_quad.as_ref()) {
                        if right_click {
                            if let Some(a) = right_aim.as_ref() {
                                if let Some(hit) = spatiand_render::ray::intersect_plane(&a.ray, q) {
                                    // How far out from the middle the pointer is now, against
                                    // where it was when the frame was grabbed. Measured from
                                    // the centre so the gesture is "pull it bigger" in any
                                    // direction rather than a per-edge drag -- the keyboard
                                    // keeps its aspect, so there is nothing a per-edge drag
                                    // could mean that this does not.
                                    let now = span(hit.u, hit.v);
                                    if start.1 > 1e-4 {
                                        keyboard.scale =
                                            (start.0 * (now / start.1) as f32).clamp(
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
                                    if let Some(stroke) = keyboard.press(key) {
                                        send_stroke(&mut runtime.state, stroke, time_ms);
                                    }
                                    keyboard.after_press(key);
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
                        match right_aim.as_ref() {
                            // Before the title bar: the button sits inside the bar, so testing
                            // the bar first would start a drag and never reach this.
                            Some(a) if a.zone == Some(Zone::Close) => {
                                if let Some(quad) = a.hit.and_then(|(i, _)| windows.get(i)) {
                                    log::info!("closing {}", runtime
                                        .state
                                        .title_of(&quad.window)
                                        .unwrap_or_else(|| "a window".into()));
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

                    if left_click && !left_was_down && !left_typed {
                        match right_aim.as_ref() {
                            // The left pad changes distance while the right one is holding a
                            // window's bar, which is the two-handed way to place something.
                            Some(a) if a.on_title => {
                                if let Some(quad) = a.hit.and_then(|(i, _)| windows.get(i)) {
                                    let radius = runtime
                                        .state
                                        .layout
                                        .get(&quad.window)
                                        .map(|pl| pl.radius)
                                        .unwrap_or(2.2);
                                    pointers.drag = Some(Drag::Depth {
                                        window: quad.window.clone(),
                                        start_radius: radius,
                                        start_y: p.left_pad.y,
                                    });
                                }
                            }
                            _ => pointers.button(&mut runtime.state, BTN_RIGHT, true, time_ms),
                        }
                    } else if !left_click && left_was_down {
                        if matches!(pointers.drag, Some(Drag::Depth { .. })) {
                            pointers.drag = None;
                        } else {
                            pointers.button(&mut runtime.state, BTN_RIGHT, false, time_ms);
                        }
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
                let keyboard_scale = keyboard.scale;
                let keyboard_hot = keyboard_border_hot;
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
                                keyboard_scale,
                                keyboard_hot,
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
                        let right_owns = pads
                            .as_ref()
                            .map(|p| p.right_pad.clicked)
                            .unwrap_or(false);
                        for (aim, right_hand) in
                            [(right_aim.as_ref(), true), (left_aim.as_ref(), false)]
                        {
                            let Some(a) = aim else { continue };
                            if !right_hand && right_owns && !pointers.is_dragging() {
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
                                a.hit.map(|(_, h)| h),
                                right_hand,
                                cursor,
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
                    levels.volume = crate::system::volume();
                    levels.screen = backlight.as_ref().and_then(|b| b.level());
                    // Re-read on the same tick, which is how a headset or a Bluetooth speaker
                    // plugged in mid-session turns up in the list without anything having to
                    // watch for it.
                    audio.outputs = crate::system::audio_devices(crate::system::Direction::Output);
                    audio.inputs = crate::system::audio_devices(crate::system::Direction::Input);
                    // Not re-read from the glasses on this timer. Every MCU exchange waits up
                    // to 1.5 s for an ack, and doing that twice a second on the render thread
                    // would stall the frame loop far worse than a stale reading ever shows.
                    // The value is read once when the headset opens and tracked from there.
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
                                    crate::system::set_default_device(id);
                                    // Move the tick's mark straight away rather than waiting
                                    // up to two seconds for the next poll to confirm it. A
                                    // list that does not respond until later reads as a tap
                                    // that missed, and the wearer taps again.
                                    for device in match direction {
                                        crate::system::Direction::Output => &mut audio.outputs,
                                        crate::system::Direction::Input => &mut audio.inputs,
                                    } {
                                        device.is_default = device.id == id;
                                    }
                                    // The volume shown belongs to whichever sink is default,
                                    // so it has to follow the choice.
                                    if direction == crate::system::Direction::Output {
                                        levels.volume = crate::system::volume();
                                    }
                                    continue;
                                }
                                crate::sidecar::Action::PressKey(key) => {
                                    // The same keyboard the 3D one uses, so a shift latched on
                                    // the panel is latched in the world too -- two copies of
                                    // that state would let it be on in one place and off in
                                    // the other.
                                    if let Some(stroke) = keyboard.press(key) {
                                        send_stroke(&mut runtime.state, stroke, time_ms);
                                    }
                                    keyboard.after_press(key);
                                    continue;
                                }
                            };
                            let Some(value) = ui.knob_value(knob, levels) else {
                                continue;
                            };
                            match knob {
                                crate::sidecar::Knob::Volume => {
                                    levels.volume = Some(value);
                                    crate::system::set_volume(value);
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
                let prepared =
                    ui.prepare(&mut renderer, &mut text, &monitors, &status_text, levels, &audio, &keyboard);
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
                    ui.draw(gl, quads, rounded, &monitors, levels, &audio, keyboard_for_panel, &prepared);
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
                    let presented = side
                        .compositor
                        .render_frame(
                            &mut renderer,
                            &[element],
                            Color32F::from([0.0, 0.0, 0.0, 1.0]),
                            FrameFlags::DEFAULT,
                        )
                        .is_ok()
                        && side.compositor.queue_frame(()).is_ok();
                    if presented {
                        side.pending = true;
                        side.failures = 0;
                    } else {
                        side.failures += 1;
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
                log::error!(
                    "the sidecar failed to present {SIDECAR_FAILURE_LIMIT} times in a row; \
                     giving up on it. The glasses are unaffected."
                );
                sidecar_surface = None;
                sidecar_ui = None;
            }

            // --- present ---
            // Acknowledge a completed flip before drawing the next frame.
            if vblank.borrow_mut().remove(&crtc) {
                if let Err(e) = compositor.frame_submitted() {
                    log::error!("frame_submitted failed: {e}");
                }
                pending_flip = false;
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

            runtime.state.space.elements().for_each(|window| {
                window.send_frame(&output, Duration::ZERO, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                })
            });
            runtime.state.space.refresh();
            display.dispatch_clients(&mut runtime.state)?;
            display.flush_clients()?;
            event_loop.dispatch(Some(Duration::from_millis(4)), runtime)?;
        }

        if !runtime.state.running {
            break;
        }
        // Fell out of the frame loop without quitting: the display situation changed, so
        // go round and rebuild against whatever is there now.
        runtime.state.space.unmap_output(&output);
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
fn pick_output(
    drm: &DrmDevice,
) -> Option<(connector::Info, crtc::Handle, smithay::reexports::drm::control::Mode)> {
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
fn open_touchscreen(session: &mut LibSeatSession) -> Option<spatiand_input::Touchscreen> {
    let node = spatiand_input::touch::find_touchscreens().into_iter().next()?;
    log::info!("touchscreen: {} at {}", node.name, node.path.display());
    let fd = match session.open(&node.path, OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK) {
        Ok(fd) => fd,
        Err(e) => {
            log::warn!("could not open {}: {e}", node.path.display());
            return None;
        }
    };
    match spatiand_input::Touchscreen::from_fd(fd) {
        Ok(t) => Some(t),
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
    _global: smithay::reexports::wayland_server::backend::GlobalId,
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
            c.handle() != used
                && c.state() == connector::State::Connected
                && !c.modes().is_empty()
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
        output.change_current_state(Some(output_mode), Some(Transform::Normal), None, Some((0, 0).into()));
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
            _global: output.create_global::<Spatiand>(&GLOBAL_DISPLAY_HANDLE.with(|h| {
                h.borrow().clone().expect("display handle set before build_sidecar")
            })),
            _output: output,
        }));
    }
    Ok(None)
}

thread_local! {
    /// The display handle, so `build_sidecar` can create the output's global without threading
    /// `runtime` through a function that has no other use for it.
    static GLOBAL_DISPLAY_HANDLE: RefCell<Option<smithay::reexports::wayland_server::DisplayHandle>> =
        const { RefCell::new(None) };
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
pub fn head_locked_panel_sized(orientation: DQuat, width: f32, height: f32, portrait: bool) -> Mat4 {
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
