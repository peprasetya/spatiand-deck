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

use std::cell::Cell;
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
use crate::pointer::{self, Aim, Drag, PointerState, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use crate::scene::{eye_centre, Scene};
use crate::{Runtime, Spatiand};

const PANEL_DISTANCE: f32 = 1.4;
const PANEL_WIDTH: f32 = 0.9;

/// How long to wait for the glasses' stereo mode to appear on the connector.
///
/// Switching mode makes the DisplayPort link retrain and the kernel re-read the EDID, which
/// is not instant. The Python spike measured this at a couple of seconds; ten is generous
/// enough to cover a cold link without hanging startup if the glasses never comply.
const STEREO_MODE_TIMEOUT: Duration = Duration::from_secs(10);

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
    // Opened here, but deliberately NOT switched to stereo yet. See the mode negotiation
    // below: the switch has to happen after the DisplayPort link is up, not before.
    let mut hmd = match spatiand_hmd::open_any() {
        Ok(h) => Some(h),
        Err(e) => {
            // Worth being loud. The usual cause is permissions, and the visible symptom is
            // Spatiand rendering to the wrong screen rather than anything mentioning access:
            // the hidraw ACL only exists for the active seat0 session, so running outside one
            // silently loses the glasses. tools/install-udev-rules.sh fixes it for good.
            log::error!("could not open a headset: {e}");
            log::error!("  if this is a permissions problem, run: sudo tools/install-udev-rules.sh");
            None
        }
    };
    if let Some(h) = hmd.as_ref() {
        log::info!("headset: {}", h.info().name);
    }

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

    // The shell — what is on screen and what a button means. Deliberately built once, outside
    // the output loop: unplugging the glasses must not close your launcher.
    let apps: Vec<spatiand_shell::AppEntry> = spatiand_platform::scan()
        .into_iter()
        .map(|e| spatiand_shell::AppEntry {
            name: e.name,
            exec: e.exec,
            icon: e.icon,
        })
        .collect();
    log::info!("launcher: {} application(s)", apps.len());
    let has_kde = std::path::Path::new("/usr/bin/kcmshell6").exists()
        || std::path::Path::new("/usr/bin/systemsettings").exists();
    let mut shell = Shell::new(apps, has_kde);
    let mut environments = Environments::discover();
    let mut sky_image = environments.current();
    let mut sky_dirty = false;
    // Owns the three pipelines and every texture that outlives one frame.
    let mut scene = Scene::new(&mut renderer, &sky_image)?;
    let stored = spatiand_track::config::load_axes();
    let mut calibration = if stored.is_none() && spatiand_hmd::is_present() {
        log::info!("no stored axis calibration — starting the in-world flow");
        Some(Calibration::new())
    } else {
        None
    };
    let mut tracker =
        HeadTracker::new(stored.unwrap_or(AxisMap::IDENTITY), TrackerConfig::default());

    // Page-flip completion drives the render loop.
    //
    // A queued frame stays "pending" until the flip completes and we acknowledge it with
    // frame_submitted(). Skipping that acknowledgement does not fail loudly - the first frame
    // scans out and then nothing ever presents again, which looks like the display going
    // blank a moment after start. That was real: the test pattern showed red for an instant
    // and then went dark.
    let vblank = Rc::new(Cell::new(false));
    let vblank_signal = vblank.clone();
    event_loop
        .handle()
        .insert_source(drm_notifier, move |event, _, _| match event {
            DrmEvent::VBlank(_) => vblank_signal.set(true),
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
            if vblank.take() {
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
                if let Some(stereo_mode) = wait_for_stereo_mode(&drm, connector_info.handle(), w) {
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
        let built_for_glasses = on_glasses;
        let mut last_presence_check = std::time::Instant::now();

        while runtime.state.running {
            // --- input ---
            //
            // Polled once a frame and read from a snapshot, so the menus, the pointer and the
            // two-thumb gesture all see the same instant. Reading the device separately for
            // each would let them disagree about whether a thumb is down.
            let mut shell_events: Vec<ShellEvent> = Vec::new();
            let mut pads: Option<spatiand_input::ControllerState> = None;
            let mut leaving = false;
            let mut screenshot = false;
            if let Some(c) = controller.as_mut() {
                c.poll();
                if hmd.is_none() {
                    // Any button returns to the desktop. Deliberately "any": the waiting
                    // screen has nothing to collide with, and there is no headset on to read
                    // a more specific instruction from.
                    if c.any_pressed() {
                        log::info!("button pressed while waiting — returning to the desktop");
                        leaving = true;
                    }
                } else {
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
                    ShellEvent::Hud(action) => match action {
                        HudAction::Recentre => {
                            tracker.recenter();
                            log::info!("recentred");
                        }
                        HudAction::Calibrate => {
                            log::info!("restarting axis calibration from the HUD");
                            calibration = Some(Calibration::new());
                        }
                        HudAction::NextEnvironment => {
                            environments.advance();
                            sky_image = environments.current();
                            sky_dirty = true;
                        }
                        HudAction::Screenshot => screenshot = true,
                        HudAction::CyclePitchRoll => {
                            // Applied live and stored, so the wearer can see which way round
                            // is right rather than having to reason about it. Calibration
                            // cannot tell a nod from a tilt performed in its place -- both
                            // produce a valid map with determinant +1 -- so nothing in the
                            // measurement can catch it and this is the only way to settle it.
                            //
                            // Toggling restores the *remembered* previous map rather than
                            // swapping a second time: the swap is not its own inverse, so
                            // pressing twice would otherwise land on a third map.
                            let swapped = tracker.axes().next_pitch_roll_variant();
                            tracker.set_axes(swapped);
                            match spatiand_track::config::save_axes(&swapped) {
                                Ok(path) => log::info!(
                                    "axes option {} of 4: {} (saved to {})",
                                    swapped.variant_index() + 1,
                                    swapped.summary(),
                                    path.display()
                                ),
                                Err(e) => log::warn!("swapped the axes but could not save: {e}"),
                            }
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
            if leaving {
                // Exiting is not enough. SDDM restarts whatever the default session is, and
                // getting here means that is Spatiand - so quitting just relaunches us, which
                // looks like the button doing nothing. Hand the default back to Plasma first.
                return_to_desktop();
                runtime.state.running = false;
                break;
            }

            // Poll for the glasses appearing or disappearing.
            //
            // Once a second, not per frame: this walks sysfs, and at 72 Hz that would be
            // 72 directory scans a second to answer a question that changes at human speed.
            if last_presence_check.elapsed() >= Duration::from_secs(1) {
                last_presence_check = std::time::Instant::now();
                let present = spatiand_hmd::is_present();
                if present != built_for_glasses {
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

            if let Some(x) = hmd.as_mut() {
                while let Ok(Some(event)) = x.poll(Duration::ZERO) {
                    match event {
                        HmdEvent::Imu(sample) => {
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
                        log::info!("adopting measured axes: {}", map.summary());
                        tracker.set_axes(map);
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
                    let exit_hint = if controller.is_some() {
                        "\n\nPress any button to\nreturn to the desktop."
                    } else {
                        ""
                    };
                    format!(
                        "Plug in your XR glasses\n\nSpatiand is waiting.\n\nConnect XREAL Air glasses\nover USB-C and this screen\nwill hand over to them.{exit_hint}"
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
            scene.sync_apps(&mut renderer, &mut text, &shell, ppd)?;
            scene.sync_menu(
                &mut renderer,
                &mut text,
                &menu_text_with(&shell, Some(tracker.axes())),
                ppd,
                stereo.per_eye.0.saturating_sub(160).max(64),
            )?;

            // Import client buffers before the draw closure takes the context.
            let mut windows = crate::scene::collect_windows(&mut renderer, &runtime.state);
            for (index, window) in windows.iter_mut().enumerate() {
                let title = runtime
                    .state
                    .title_for(index)
                    .unwrap_or_else(|| "Untitled".to_string());
                window.title = scene.title_texture(&mut renderer, &mut text, &title, ppd);
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

            if shell.menu_is_open() {
                // A menu takes the pointer away. Anything held has to be let go, or the client
                // underneath is left believing a drag is still running.
                pointers.release_all(&mut runtime.state, time_ms);
            } else {
                // A drag in progress owns the window and ignores everything else.
                match pointers.drag {
                    Some(Drag::Move { index }) => {
                        if let Some(a) = right_aim.as_ref() {
                            let d = a.ray.direction;
                            let window = runtime.state.space.elements().nth(index).cloned();
                            if let Some(window) = window {
                                if let Some(mut placement) = runtime.state.layout.get(&window) {
                                    // Straight onto the ray: the window goes where you point,
                                    // keeping its distance. Anything cleverer -- offsets from
                                    // the grab point, inertia -- reads as lag at this range.
                                    placement.yaw = d.y.atan2(d.x);
                                    placement.pitch = d.z.clamp(-1.0, 1.0).asin();
                                    runtime.state.layout.set(&window, placement);
                                }
                            }
                        }
                    }
                    Some(Drag::Depth { index, start_radius, start_y }) => {
                        if let Some(p) = pads.as_ref() {
                            let window = runtime.state.space.elements().nth(index).cloned();
                            if let Some(window) = window {
                                if let Some(mut placement) = runtime.state.layout.get(&window) {
                                    // Thumb up pushes it away. Clamped so a window can never
                                    // end up inside your head or so far off it is unreadable.
                                    let delta = (p.left_pad.y - start_y) as f64;
                                    placement.radius = (start_radius + delta * 2.0).clamp(0.8, 8.0);
                                    runtime.state.layout.set(&window, placement);
                                }
                            }
                        }
                    }
                    None => {
                        if let Some(a) = right_aim.as_ref() {
                            pointers.motion(&mut runtime.state, a, &windows, time_ms);
                        }
                    }
                }

                // Buttons. The pads and the face buttons both click, because pressing a pad
                // moves the thumb slightly as it goes down -- fine for a button, bad for a
                // precise click on something small.
                if let Some(p) = pads.as_ref() {
                    let right_click = p.right_pad.clicked;
                    let left_click = p.left_pad.clicked;

                    if right_click && pointers.drag.is_none() && !right_was_down {
                        match right_aim.as_ref() {
                            Some(a) if a.on_title => {
                                if let Some((index, _)) = a.hit {
                                    runtime.state.focus_window(index);
                                    pointers.drag = Some(Drag::Move { index });
                                }
                            }
                            Some(a) => {
                                if let Some((index, _)) = a.hit {
                                    runtime.state.focus_window(index);
                                }
                                pointers.button(&mut runtime.state, BTN_LEFT, true, time_ms);
                            }
                            None => {}
                        }
                    } else if !right_click && right_was_down {
                        if matches!(pointers.drag, Some(Drag::Move { .. })) {
                            pointers.drag = None;
                        } else {
                            pointers.button(&mut runtime.state, BTN_LEFT, false, time_ms);
                        }
                    }

                    if left_click && !left_was_down {
                        match right_aim.as_ref() {
                            // The left pad changes distance while the right one is holding a
                            // window's bar, which is the two-handed way to place something.
                            Some(a) if a.on_title => {
                                if let Some((index, _)) = a.hit {
                                    let radius = runtime
                                        .state
                                        .space
                                        .elements()
                                        .nth(index)
                                        .cloned()
                                        .and_then(|w| runtime.state.layout.get(&w))
                                        .map(|pl| pl.radius)
                                        .unwrap_or(2.2);
                                    pointers.drag = Some(Drag::Depth {
                                        index,
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
                        scene.draw_menu(gl, &eye, &shell, (stereo.h_fov_deg, stereo.v_fov_deg()));
                        if let Some(a) = right_aim.as_ref() {
                            scene.draw_pointer(gl, &eye, &a.ray, a.hit.map(|(_, h)| h), true);
                        }
                        if let Some(a) = left_aim.as_ref() {
                            scene.draw_pointer(gl, &eye, &a.ray, a.hit.map(|(_, h)| h), false);
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

            // --- present ---
            // Acknowledge a completed flip before drawing the next frame.
            if vblank.take() {
                if let Err(e) = compositor.frame_submitted() {
                    log::error!("frame_submitted failed: {e}");
                }
                pending_flip = false;
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
        if let (Some(mode), Some(crtc)) = (native, first_free_crtc(drm, info)) {
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

fn first_free_crtc(drm: &DrmDevice, connector: &connector::Info) -> Option<crtc::Handle> {
    let resources = drm.resource_handles().ok()?;
    connector
        .encoders()
        .iter()
        .filter_map(|e| drm.get_encoder(*e).ok())
        .find_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()).first().copied())
}

/// The text of whichever menu is open, or empty in the world.
///
/// The HUD is rendered as one text panel rather than as a row of separate textures. At the
/// resolution one eye actually resolves, a settings list *is* text — giving each row its own
/// quad would buy nothing and cost a dozen uploads every time the cursor moved.
pub fn menu_text(shell: &Shell) -> String {
    menu_text_with(shell, None)
}

/// As [`menu_text`], but able to show live state the shell itself does not hold.
fn menu_text_with(shell: &Shell, axes: Option<spatiand_track::AxisMap>) -> String {
    match shell.mode() {
        Mode::World => String::new(),
        Mode::Hud => {
            let hud = shell.hud();
            let mut out = String::from("Settings\n\n");
            for (i, item) in hud.items().iter().enumerate() {
                // A leading marker rather than a highlight rectangle: one texture, and it
                // survives being read at an angle far better than a background tint.
                out.push_str(if i == hud.cursor() { "\u{25b8} " } else { "   " });
                out.push_str(item.label);
                out.push('\n');
            }
            out.push_str(&format!("\n{}", hud.focused().detail));
            if let Some(map) = axes {
                if matches!(hud.focused().action, HudAction::CyclePitchRoll) {
                    out.push_str(&format!(
                        "\n\nnow using option {} of 4",
                        map.variant_index() + 1
                    ));
                }
            }
            out.push_str("\n\nA select    B back");
            out
        }
        Mode::Launcher if shell.launcher().is_empty() => {
            "No applications\n\nNothing was found in the\nsystem's application folders.\n\nB back".into()
        }
        Mode::Launcher => String::new(),
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
