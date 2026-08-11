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
use smithay::utils::{DeviceFd, Scale, Transform};

use spatiand_hmd::{DisplayMode, Hmd, HmdEvent};
use spatiand_render::{EyeSide, StereoConfig, TextRenderer};
use spatiand_track::{AxisMap, HeadTracker, TrackerConfig};

use crate::calib::Calibration;
use crate::gl::{upload_rgba, QuadPipeline};
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

    // --- pick a connector, at its native mode ---
    let (connector_info, crtc, mode) =
        pick_output(&drm).ok_or("no usable connector found")?;
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

    let surface = drm.create_surface(crtc, mode, &[connector_info.handle()])?;
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
            Some(gbm),
        )?;

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
            if let Some(stereo_mode) = wait_for_stereo_mode(&drm, connector_info.handle()) {
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
    let stored = spatiand_track::config::load_axes();
    let mut calibration = if stored.is_none() && hmd.is_some() {
        log::info!("no stored axis calibration — starting the in-world flow");
        Some(Calibration::new())
    } else {
        None
    };
    let mut tracker = HeadTracker::new(stored.unwrap_or(AxisMap::IDENTITY), TrackerConfig::default());

    let test_pattern = std::env::var("SPATIAND_TEST_PATTERN").is_ok();
    if test_pattern {
        log::info!("SPATIAND_TEST_PATTERN set: drawing flat colour per eye, nothing else");
    }
    let pipeline = QuadPipeline::new(&mut renderer)?;
    let mut text = TextRenderer::new();
    let mut panel: Option<(u32, f32)> = None;
    let mut last_prompt = String::new();
    // The status readout contains live pose numbers, so as a string it changes every frame.
    // Rebuilding on every change then re-rasterises and re-uploads ~2 MB of RGBA at 72 Hz to
    // show digits nobody can read that fast. Recompute it a few times a second instead; the
    // "only rebuild when the text changes" check is right, it was the text that was wrong.
    let mut last_status_update = std::time::Instant::now();
    let mut status_text = String::new();

    // The scene is drawn here with raw GL, then handed to DrmCompositor as one element.
    //
    // We own the framebuffer object rather than using `Renderer::bind`. Smithay's
    // `bind_texture` creates an FBO, checks it is complete, and then *unbinds* it — the FBO
    // is only made current when smithay itself renders. Raw GL calls issued afterwards
    // therefore land on framebuffer 0, which in a DRM/GBM context has no surface behind it,
    // and every call fails with GL_INVALID_FRAMEBUFFER_OPERATION while the screen stays
    // black. Attaching our own FBO to the same texture is the fix.
    let scene: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, (w as i32, h as i32).into())?;
    let scene_fbo = renderer.with_context(|gl| unsafe {
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
    if scene_fbo.1 != ffi::FRAMEBUFFER_COMPLETE {
        return Err(format!("scene framebuffer incomplete: {:#x}", scene_fbo.1).into());
    }
    let scene_fbo = scene_fbo.0;
    log::info!("scene framebuffer {scene_fbo} ready at {w}x{h}");

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

    while runtime.state.running {
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
                if c.stage() == crate::calib::Stage::Done {
                    calibration = None;
                }
            }
        }

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
                "Plug in your XR glasses\n\n                 Spatiand is waiting.\n                 Connect XREAL Air glasses over USB-C\n                 and this screen will hand over to them."
                    .to_string()
            }
            Some(c) => {
                let p = c.prompt();
                format!("{}\n\n{}\n\n{}", p.heading, p.body, p.status)
            }
            None => {
                if status_text.is_empty()
                    || last_status_update.elapsed() >= Duration::from_millis(250)
                {
                    let e = tracker.euler_degrees();
                    status_text = format!(
                        "Spatiand\n\nyaw {:.0}   pitch {:.0}   roll {:.0}\n\n{} window(s)",
                        e.yaw,
                        e.pitch,
                        e.roll,
                        runtime.state.space.elements().count()
                    );
                    last_status_update = std::time::Instant::now();
                }
                status_text.clone()
            }
        };

        if prompt_text != last_prompt {
            last_prompt = prompt_text.clone();
            let ppd = TextRenderer::px_per_degree(stereo.per_eye.0, stereo.h_fov_deg);
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

        // --- draw the scene into the offscreen texture ---
        {
            let snapshot = panel;
            renderer.with_context(|gl| unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, scene_fbo);
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

                let Some((tex, aspect)) = snapshot else {
                    gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
                    return;
                };
                let views: &[(EyeSide, i32, i32)] = if on_glasses {
                    &[
                        (EyeSide::Left, 0, w as i32 / 2),
                        (EyeSide::Right, w as i32 / 2, w as i32 / 2),
                    ]
                } else {
                    &[(EyeSide::Left, 0, w as i32)]
                };
                for (side, x, vw) in views {
                    gl.Viewport(*x, 0, *vw, h as i32);
                    let eye = spatiand_render::eye_for(*side, orientation, DVec3::ZERO, &stereo);
                    let model = head_locked_panel(orientation, aspect, portrait);
                    // Flip vertically when drawing into the offscreen texture.
                    //
                    // GL renders with the origin at the BOTTOM-left, so rendering into a
                    // texture stores the image with row 0 holding its bottom. The compositor
                    // then samples that texture with row 0 as the top, and everything comes
                    // out mirrored top-to-bottom. It does not read as a clean 180 degree
                    // rotation - glyphs are individually flipped while the line order
                    // reverses - which is why it is harder to read than upside-down text.
                    //
                    // Only the offscreen path needs this. The nested winit backend draws
                    // straight into a GL surface that is presented with GL's own convention,
                    // so no correction applies there.
                    let flip_y = Mat4::from_scale(Vec3::new(1.0, -1.0, 1.0));
                    let mvp = flip_y * eye.projection * eye.view * model;
                    pipeline.draw(gl, tex, &mvp, [1.0, 1.0, 1.0, 1.0], (0.0, 1.0));
                }
                gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            })?;
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
            scene.clone(),
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
) -> Option<smithay::reexports::drm::control::Mode> {
    let deadline = std::time::Instant::now() + STEREO_MODE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok(fresh) = drm.get_connector(handle, true) {
            // A double-width mode is how the glasses advertise that side-by-side is live -
            // the same signal XREAL's own software uses (docs/xreal-air.md §9).
            if let Some(m) = fresh
                .modes()
                .iter()
                .find(|m| m.size().0 >= 3840 && m.size().1 == 1080)
            {
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

/// Head-locked panel transform. See `backend_winit` for the frame conventions.
///
/// `portrait` rolls the content a quarter turn for the Deck's built-in screen, which is
/// mounted sideways. The roll is applied about the view axis (+X, forward) *after* the head
/// orientation, so it rotates the image on the glass rather than tilting the world.
fn head_locked_panel(orientation: DQuat, aspect: f32, portrait: bool) -> Mat4 {
    let height = PANEL_WIDTH / aspect.max(0.01);
    let basis = Mat4::from_cols(
        (-Vec3::Y * PANEL_WIDTH).extend(0.0),
        (Vec3::Z * height).extend(0.0),
        Vec3::X.extend(0.0),
        (Vec3::X * PANEL_DISTANCE).extend(1.0),
    );
    let roll = if portrait {
        Mat4::from_rotation_x(std::f32::consts::FRAC_PI_2)
    } else {
        Mat4::IDENTITY
    };
    Mat4::from_quat(orientation.as_quat()) * roll * basis
}
