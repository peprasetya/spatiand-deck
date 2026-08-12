//! Render one frame to a PNG, with no display, no session and no headset.
//!
//! This exists because of a specific and repeated failure: layout bugs — text running off the
//! edge, labels overlapping the row below, a panel sized for the wrong field of view — are
//! invisible in a log and invisible in a test. The only way anyone found them was to put the
//! glasses on and describe what was wrong, which is a slow and lossy way to iterate on
//! millimetres.
//!
//! A render node is enough for this. Only *scanout* needs DRM master; EGL and GBM will happily
//! give a context on `/dev/dri/renderD128`, which means this runs over SSH while the desktop
//! carries on untouched.
//!
//! ```text
//! SPATIAND_BACKEND=snapshot SPATIAND_SNAPSHOT=/tmp/hud.png SPATIAND_VIEW=hud spatiand
//! ```
//!
//! `SPATIAND_VIEW` is `world`, `hud`, `launcher` or `calibrate`; `SPATIAND_SNAPSHOT_SIZE` is
//! `WIDTHxHEIGHT` and defaults to one eye of the glasses (1920x1080). `SPATIAND_SNAPSHOT_YAW`
//! turns the head, in degrees, which is how the arc's edges get checked.
//!
//! `SPATIAND_CLIENT` goes further and launches a real Wayland application into the snapshot:
//! a full compositor runs, the client connects, commits a buffer, and the frame is rendered
//! with that window in it. That is the only way to answer "what does an app actually look like
//! in there" without wearing the glasses.
//!
//! ```text
//! SPATIAND_BACKEND=snapshot SPATIAND_CLIENT=foot SPATIAND_SNAPSHOT=/tmp/win.png spatiand
//! ```

use std::path::PathBuf;

use glam::{DQuat, DVec3};
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::{ffi, GlesRenderer, GlesTexture};
use smithay::backend::renderer::Offscreen;
use smithay::utils::DeviceFd;

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;

use spatiand_render::{EyeSide, StereoConfig, TextRenderer};
use spatiand_shell::{Intent, Shell};

use crate::calib::Calibration;
use crate::environment::Environments;
use crate::scene::Scene;
use crate::{Runtime, Spatiand};

/// Which state to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    World,
    Hud,
    Launcher,
    Calibrate,
}

impl View {
    fn from_env() -> Self {
        match std::env::var("SPATIAND_VIEW").as_deref() {
            Ok("hud") => Self::Hud,
            Ok("launcher") => Self::Launcher,
            Ok("calibrate") => Self::Calibrate,
            _ => Self::World,
        }
    }
}

fn size_from_env() -> (u32, u32) {
    let default = (1920, 1080);
    let Ok(spec) = std::env::var("SPATIAND_SNAPSHOT_SIZE") else {
        return default;
    };
    match spec.split_once(['x', 'X']) {
        Some((w, h)) => match (w.trim().parse(), h.trim().parse()) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
            _ => default,
        },
        None => default,
    }
}

pub fn run(
    event_loop: &mut EventLoop<'static, Runtime>,
    display: &mut Display<Spatiand>,
    runtime: &mut Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    let out: PathBuf = std::env::var("SPATIAND_SNAPSHOT")
        .unwrap_or_else(|_| "/tmp/spatiand-frame.png".into())
        .into();
    let (width, height) = size_from_env();
    let view = View::from_env();
    let yaw_deg: f64 = std::env::var("SPATIAND_SNAPSHOT_YAW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    // The Deck's own panel is portrait and its content is rolled a quarter turn, which is
    // exactly the case where panel sizing has gone wrong before.
    let portrait = std::env::var("SPATIAND_SNAPSHOT_PORTRAIT").is_ok();
    log::info!("snapshot: {view:?} at {width}x{height}, yaw {yaw_deg}, portrait {portrait} -> {}", out.display());

    // --- a GL context with no display attached ---
    let node = std::env::var("SPATIAND_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&node)?;
    let gbm = GbmDevice::new(DeviceFd::from(std::os::fd::OwnedFd::from(file)))?;
    let egl_display = unsafe { EGLDisplay::new(gbm)? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };
    log::info!("offscreen renderer ready on {node}");

    // --- the same objects the real backends build ---
    let apps: Vec<spatiand_shell::AppEntry> = spatiand_platform::scan()
        .into_iter()
        .map(|e| spatiand_shell::AppEntry {
            name: e.name,
            exec: e.exec,
            icon: e.icon,
        })
        .collect();
    log::info!("launcher: {} application(s)", apps.len());
    let mut shell = Shell::new(apps, true);

    // Clients need an output to be told about, and frame callbacks need one to reference.
    let output = smithay::output::Output::new(
        "spatiand-snapshot".into(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "Spatiand".into(),
            model: "Snapshot".into(),
        },
    );
    let output_mode = smithay::output::Mode {
        size: (width as i32, height as i32).into(),
        refresh: 72_000,
    };
    let _global = output.create_global::<Spatiand>(&runtime.display_handle);
    output.change_current_state(Some(output_mode), None, None, Some((0, 0).into()));
    output.set_preferred(output_mode);
    runtime.state.space.map_output(&output, (0, 0));
    let environments = Environments::discover();
    let sky_image = environments.current();
    let mut scene = Scene::new(&mut renderer, &sky_image)?;
    let mut text = TextRenderer::new();

    match view {
        View::Hud => {
            shell.handle(Intent::ToggleHud);
        }
        View::Launcher => {
            shell.handle(Intent::ToggleLauncher);
        }
        _ => {}
    }

    // --- optionally host a real application ---
    let mut windows = Vec::new();
    if let Ok(command) = std::env::var("SPATIAND_CLIENT") {
        let seconds: f32 = std::env::var("SPATIAND_CLIENT_WAIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8.0);
        log::info!("launching {command:?} into the snapshot, waiting up to {seconds}s");
        match spatiand_platform::launch(&command, &runtime.state.socket_name) {
            Ok(pid) => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f32(seconds);
                while std::time::Instant::now() < deadline {
                    // Pumping the display is what lets the client bind globals, get its
                    // configure, and commit. Without this it blocks on the first roundtrip and
                    // never draws anything.
                    display.dispatch_clients(&mut runtime.state)?;
                    display.flush_clients()?;
                    event_loop.dispatch(Some(std::time::Duration::from_millis(16)), runtime)?;

                    windows = crate::scene::collect_windows(&mut renderer, &runtime.state);
                    if !windows.is_empty() {
                        // The first buffer a toolkit commits is usually blank -- it has the
                        // right size but the UI has not been painted into it. Snapshotting
                        // there gives a black rectangle that looks like a broken import.
                        // Keep pumping so the client gets to draw itself.
                        log::info!("client mapped; letting it paint");
                        let settle = std::time::Instant::now() + std::time::Duration::from_secs(3);
                        while std::time::Instant::now() < settle {
                            for window in runtime.state.space.elements() {
                                // Frame callbacks are what tell a client it may draw the next
                                // frame. Without them most toolkits paint once and stop.
                                window.send_frame(
                                    &output,
                                    std::time::Duration::ZERO,
                                    Some(std::time::Duration::ZERO),
                                    |_, _| Some(output.clone()),
                                );
                            }
                            runtime.state.space.refresh();
                            display.dispatch_clients(&mut runtime.state)?;
                            display.flush_clients()?;
                            event_loop
                                .dispatch(Some(std::time::Duration::from_millis(16)), runtime)?;
                        }
                        windows = crate::scene::collect_windows(&mut renderer, &runtime.state);
                        break;
                    }
                }
                if windows.is_empty() {
                    log::warn!("{command:?} (pid {pid}) never committed a buffer");
                    log::warn!("  it may need a wayland flag, or it may have exited immediately");
                }
            }
            Err(e) => log::warn!("could not launch {command:?}: {e}"),
        }
    }
    {
        let ppd_now = TextRenderer::px_per_degree(width, 40.0);
        for (index, w) in windows.iter_mut().enumerate() {
            let title = runtime
                .state
                .title_for(index)
                .unwrap_or_else(|| "Untitled".to_string());
            w.title = scene.title_texture(&mut renderer, &mut text, &title, ppd_now);
        }
    }
    for w in &windows {
        log::info!(
            "window {}x{} px at yaw {:.0} deg",
            w.pixels.0,
            w.pixels.1,
            w.placement.yaw.to_degrees()
        );
    }

    // Decisive diagnostic: read the imported client texture straight back, with no scene
    // geometry involved. A black window in the world could be a bad import or a bad draw, and
    // these two look identical from outside.
    if let (Ok(path), Some(first)) = (std::env::var("SPATIAND_DUMP_WINDOW"), windows.first()) {
        let (tw, th) = first.pixels;
        let mut raw = vec![0u8; (tw * th * 4) as usize];
        let tex = first.texture;
        let status = renderer.with_context(|gl| unsafe {
            let mut fbo = 0;
            gl.GenFramebuffers(1, &mut fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                tex,
                0,
            );
            let st = gl.CheckFramebufferStatus(ffi::FRAMEBUFFER);
            if st == ffi::FRAMEBUFFER_COMPLETE {
                gl.ReadPixels(
                    0,
                    0,
                    tw as i32,
                    th as i32,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    raw.as_mut_ptr() as *mut _,
                );
            }
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            gl.DeleteFramebuffers(1, &fbo);
            st
        })?;
        let ink = raw.chunks_exact(4).filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8).count();
        log::info!(
            "window texture {tex}: fbo status {status:#x}, {tw}x{th}, {:.1}% non-black",
            ink as f32 / (tw * th) as f32 * 100.0
        );
        image::save_buffer(&path, &raw, tw, th, image::ColorType::Rgba8)?;
        log::info!("dumped the raw client texture to {path}");
    }

    let stereo = StereoConfig {
        per_eye: (width, height),
        ..Default::default()
    };
    let ppd = TextRenderer::px_per_degree(stereo.per_eye.0, stereo.h_fov_deg);
    scene.sync_apps(&mut renderer, &mut text, &shell, ppd)?;
    scene.sync_status(
        &mut renderer,
        &mut text,
        &crate::status::line(runtime.state.space.elements().count()),
        ppd,
    )?;
    scene.sync_menu(
        &mut renderer,
        &mut text,
        &crate::backend_drm::menu_text(&shell),
        ppd,
        stereo.per_eye.0.saturating_sub(160).max(64),
    )?;

    // The head-locked panel, when there is one to draw.
    let panel_text = match view {
        View::Calibrate => {
            let c = Calibration::new();
            let p = c.prompt();
            format!("{}\n\n{}\n\n{}", p.heading, p.body, p.status)
        }
        // Nothing in the middle of the view: that space belongs to the windows, and the
        // readout that used to live there is in the corner status bar now.
        View::World => String::new(),
        _ => String::new(),
    };
    let panel = if panel_text.is_empty() {
        None
    } else {
        let image = text.render(
            &panel_text,
            ppd * 1.6,
            stereo.per_eye.0.saturating_sub(120).max(64),
            [235, 240, 255, 255],
        );
        log::info!(
            "panel text {}x{} px, ink {:.1}%",
            image.width,
            image.height,
            image.ink_fraction() * 100.0
        );
        let aspect = image.width as f32 / image.height.max(1) as f32;
        Some(renderer.with_context(|gl| unsafe { (crate::gl::upload_rgba(gl, &image), aspect) })?)
    };

    // --- draw ---
    let target: GlesTexture =
        renderer.create_buffer(Fourcc::Abgr8888, (width as i32, height as i32).into())?;
    let fbo = renderer.with_context(|gl| unsafe {
        let mut fbo = 0;
        gl.GenFramebuffers(1, &mut fbo);
        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
        gl.FramebufferTexture2D(
            ffi::FRAMEBUFFER,
            ffi::COLOR_ATTACHMENT0,
            ffi::TEXTURE_2D,
            target.tex_id(),
            0,
        );
        let status = gl.CheckFramebufferStatus(ffi::FRAMEBUFFER);
        gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        (fbo, status)
    })?;
    if fbo.1 != ffi::FRAMEBUFFER_COMPLETE {
        return Err(format!("snapshot framebuffer incomplete: {:#x}", fbo.1).into());
    }
    let fbo = fbo.0;

    let orientation = DQuat::from_axis_angle(DVec3::Z, yaw_deg.to_radians());
    let shell_ref = &shell;
    let scene_ref = &scene;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    renderer.with_context(|gl| unsafe {
        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
        gl.Disable(ffi::SCISSOR_TEST);
        gl.Viewport(0, 0, width as i32, height as i32);
        gl.ClearColor(0.02, 0.02, 0.05, 1.0);
        gl.Clear(ffi::COLOR_BUFFER_BIT);

        // No flip: the readback below does it, so the PNG comes out the right way up while
        // the geometry stays in GL's own convention.
        let eye = spatiand_render::eye_for(EyeSide::Left, orientation, DVec3::ZERO, &stereo);
        scene_ref.draw_sky(gl, &eye);
        scene_ref.draw_windows(gl, &eye, &windows);
        scene_ref.draw_status(gl, &eye, orientation);
        scene_ref.draw_menu(gl, &eye, shell_ref, (stereo.h_fov_deg, stereo.v_fov_deg()));
        if let Some((tex, aspect)) = panel {
            let (pw, ph) = crate::backend_drm::fit_panel(
                aspect,
                stereo.h_fov_deg,
                stereo.v_fov_deg(),
                portrait,
            );
            let model = crate::backend_drm::head_locked_panel_sized(orientation, pw, ph, portrait);
            scene_ref
                .quads()
                .draw(gl, tex, &(eye.view_projection() * model), [1.0; 4], (0.0, 1.0));
        }

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

    // GL reads back bottom row first.
    let stride = (width * 4) as usize;
    let mut flipped = vec![0u8; pixels.len()];
    for y in 0..height as usize {
        let src = (height as usize - 1 - y) * stride;
        flipped[y * stride..(y + 1) * stride].copy_from_slice(&pixels[src..src + stride]);
    }

    image::save_buffer(&out, &flipped, width, height, image::ColorType::Rgba8)?;
    log::info!("wrote {}", out.display());
    Ok(())
}
