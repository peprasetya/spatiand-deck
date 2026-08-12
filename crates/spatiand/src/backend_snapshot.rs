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

use std::path::PathBuf;

use glam::{DQuat, DVec3};
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::{ffi, GlesRenderer, GlesTexture};
use smithay::backend::renderer::Offscreen;
use smithay::utils::DeviceFd;

use spatiand_render::{EyeSide, StereoConfig, TextRenderer};
use spatiand_shell::{Intent, Shell};

use crate::calib::Calibration;
use crate::environment::Environments;
use crate::scene::Scene;

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

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
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

    let stereo = StereoConfig {
        per_eye: (width, height),
        ..Default::default()
    };
    let ppd = TextRenderer::px_per_degree(stereo.per_eye.0, stereo.h_fov_deg);
    scene.sync_apps(&mut renderer, &mut text, &shell, ppd)?;
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
        View::World => "Spatiand\n\nyaw 0   pitch 0   roll 0\n\n0 window(s)\n\nSTEAM settings    ... apps".into(),
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
