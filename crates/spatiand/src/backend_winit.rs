//! Nested winit backend — Spatiand as a window on an existing desktop.
//!
//! This renders the *same* side-by-side stereo frame the glasses get, into a window instead
//! of a DRM plane. Two views of one world, side by side: what you see here is what the
//! glasses will show.
//!
//! Head pose comes from whatever [`spatiand_hmd::open_any`] finds. With no glasses attached
//! that is `NullHmd`, and `SPATIAND_NULL_SPIN=10` makes the world turn slowly on its own —
//! the cheapest way to tell a world that is genuinely head-locked from one that is merely
//! drawn.

use std::time::Duration;

use glam::{DQuat, DVec3, Mat4, Vec3};
use smithay::backend::renderer::gles::{ffi, GlesRenderer};
use smithay::backend::winit::{self, WinitEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Rectangle, Transform};

use spatiand_hmd::{DisplayMode, HmdEvent};
use spatiand_render::ray::{ray_from_pad, PointerConfig};
use spatiand_render::{EyeSide, StereoConfig, TextRenderer};
use spatiand_shell::{HudAction, Shell, ShellEvent};
use spatiand_track::{AxisMap, HeadTracker, TrackerConfig};

use crate::calib::Calibration;
use crate::environment::Environments;
use crate::gl::upload_rgba;
use crate::input_map::intent_for;
use crate::scene::{eye_centre, Scene};
use crate::{Runtime, Spatiand};

/// Half-height, so the side-by-side pair fits an ordinary screen while keeping its shape.
///
/// The default is deliberately small enough to fit the Deck's own 800x1280 panel. Asking for a
/// 1920-wide window there fails inside EGL as `BAD_ALLOC` on the window surface, and what
/// reaches the log first is `GL_INVALID_FRAMEBUFFER_OPERATION in glClear` — which reads as a
/// renderer bug rather than as a window that was never created.
const DEFAULT_DEV_WIDTH: i32 = 760;
const DEFAULT_DEV_HEIGHT: i32 = 428;

/// `SPATIAND_WINDOW=1280x720` on a larger screen.
fn dev_window_size() -> (i32, i32) {
    let Ok(spec) = std::env::var("SPATIAND_WINDOW") else {
        return (DEFAULT_DEV_WIDTH, DEFAULT_DEV_HEIGHT);
    };
    match spec.split_once(['x', 'X']) {
        Some((w, h)) => match (w.trim().parse(), h.trim().parse()) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
            _ => {
                log::warn!("SPATIAND_WINDOW={spec:?} is not WIDTHxHEIGHT; using the default");
                (DEFAULT_DEV_WIDTH, DEFAULT_DEV_HEIGHT)
            }
        },
        None => {
            log::warn!("SPATIAND_WINDOW={spec:?} is not WIDTHxHEIGHT; using the default");
            (DEFAULT_DEV_WIDTH, DEFAULT_DEV_HEIGHT)
        }
    }
}

/// How far in front of the face head-locked panels sit, metres. Close enough to read, far
/// enough that the eyes are not straining to converge on a fixed-focus display.
const PANEL_DISTANCE: f32 = 1.4;
/// Panel width in metres. 0.9 m at 1.4 m is about 36 degrees — most of one eye's 40 degree
/// field, so prompts are large without running off the edge.
const PANEL_WIDTH: f32 = 0.9;

/// How many views to draw into the window.
///
/// The nested backend usually runs on an ordinary flat screen, where a side-by-side pair
/// reads as two copies of everything and prompts become unusable. So mono is the default
/// here and stereo is opt-in via `SPATIAND_EYES=stereo` — for checking the split itself, or
/// when the window is fullscreen on glasses already in 3840x1080 mode, which gives genuine
/// stereo without the DRM backend existing yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EyeMode {
    Mono,
    Stereo,
}

impl EyeMode {
    fn from_env() -> Self {
        match std::env::var("SPATIAND_EYES").as_deref() {
            Ok("stereo") => Self::Stereo,
            _ => Self::Mono,
        }
    }

    /// Viewports to draw, as (eye, x, width).
    fn viewports(self, width: i32) -> Vec<(EyeSide, i32, i32)> {
        match self {
            // One centred view. The left eye is used rather than a special cyclopean camera
            // so that what you see is exactly one of the two real views, half an IPD off
            // centre — honest about the geometry rather than a third rendering path.
            Self::Mono => vec![(EyeSide::Left, 0, width)],
            Self::Stereo => vec![(EyeSide::Left, 0, width / 2), (EyeSide::Right, width / 2, width / 2)],
        }
    }
}

pub fn run(
    event_loop: &mut EventLoop<'static, Runtime>,
    display: &mut Display<Spatiand>,
    runtime: &mut Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit_source) = winit::init::<GlesRenderer>()?;

    let (dev_w, dev_h) = dev_window_size();
    log::info!("nested window {dev_w}x{dev_h} (set SPATIAND_WINDOW=WxH to change)");
    let mode = Mode {
        size: (dev_w, dev_h).into(),
        refresh: 72_000,
    };
    let output = Output::new(
        "spatiand-dev".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Spatiand".into(),
            model: "Nested".into(),
        },
    );
    let _global = output.create_global::<Spatiand>(&runtime.display_handle);
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
    runtime.state.space.map_output(&output, (0, 0));

    // --- head tracking ---
    let mut hmd = match spatiand_hmd::open_any() {
        Ok(h) => {
            log::info!("head tracking: {}", h.info().name);
            Some(h)
        }
        Err(e) => {
            log::warn!("no headset ({e}); the world will not be head-locked");
            None
        }
    };
    let eye_mode = EyeMode::from_env();
    log::info!("eye mode: {eye_mode:?} (set SPATIAND_EYES=stereo for the side-by-side pair)");
    let stereo = StereoConfig {
        h_fov_deg: hmd.as_ref().map(|h| h.info().h_fov_deg).unwrap_or(40.0),
        ipd_m: hmd.as_ref().map(|h| h.info().default_ipd_mm).unwrap_or(63.0) / 1000.0,
        ..Default::default()
    };

    // A headset that states its own IMU mounting has already answered this; calibration is
    // only for hardware whose mounting nobody has measured. See `backend_drm::settle_axes`
    // for why that order matters — it is the fix for pitch and roll coming back swapped after
    // every restart.
    let stored = spatiand_track::config::load_axes();
    let mut calibration: Option<Calibration> = None;
    let mut tracker = HeadTracker::new(stored.unwrap_or(AxisMap::XREAL_AIR), TrackerConfig::default());
    if let Some(h) = hmd.as_ref() {
        crate::backend_drm::settle_axes(h.info(), stored, &mut tracker, &mut calibration);
    }

    // --- the shell ---
    //
    // The same objects the DRM backend builds. Running them here is the point of having a
    // nested backend at all: the launcher, the HUD and the environment can be looked at
    // without taking over the only display on the machine.
    //
    // Note the controller *is* usable from here, but only with Steam stopped -- it configures
    // the device for itself and every payload field then reads zero.
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
    let mut controller = spatiand_input::DeckController::open();
    let mut gesture = spatiand_input::TwoPadGesture::new();
    let pointer_config = PointerConfig::default();

    // --- gpu resources ---
    let mut scene = Scene::new(backend.renderer(), &sky_image)?;
    let mut text = TextRenderer::new();
    let mut panel: Option<PanelTexture> = None;
    let mut last_prompt = String::new();

    let resize_output = output.clone();
    event_loop
        .handle()
        .insert_source(winit_source, move |event, _, runtime| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode {
                    size,
                    refresh: 72_000,
                };
                resize_output.change_current_state(Some(mode), None, None, None);
                resize_output.set_preferred(mode);
            }
            WinitEvent::Input(event) => crate::input_stub(&mut runtime.state, event),
            WinitEvent::CloseRequested => runtime.state.running = false,
            _ => {}
        })
        .map_err(|e| format!("could not register the winit source: {e}"))?;

    log::info!(
        "run a client with:  WAYLAND_DISPLAY={} <app>",
        runtime.state.socket_name
    );

    while runtime.state.running {
        // Drain every sample since the last frame: the stream runs at ~1 kHz and the display
        // at 72 Hz, so feeding all of them keeps the filter's dt honest.
        if let Some(h) = hmd.as_mut() {
            while let Ok(Some(event)) = h.poll(Duration::ZERO) {
                match event {
                    HmdEvent::Imu(sample) => {
                        // Calibration sees the raw sample: it is what discovers the mapping,
                        // so it must not be fed already-remapped axes.
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

        // --- input ---
        let mut pointer: Option<(f32, f32, bool)> = None;
        if let Some(c) = controller.as_mut() {
            c.poll();
            let mut events: Vec<ShellEvent> = Vec::new();
            for control in c.pressed() {
                if let Some(intent) = intent_for(*control) {
                    if let Some(event) = shell.handle(intent) {
                        events.push(event);
                    }
                }
            }
            let input = *c.state();
            let two_handed = gesture.update(&input.left_pad, &input.right_pad);
            if two_handed.is_none() && !shell.menu_is_open() && input.right_pad.touched {
                pointer = Some((input.right_pad.x, input.right_pad.y, input.right_pad.clicked));
            }

            for event in events {
                match event {
                    ShellEvent::ModeChanged(mode) => {
                        if mode == spatiand_shell::Mode::World {
                            scene.forget_anchor();
                        } else {
                            scene.anchor_menu(mode, tracker.euler_degrees().yaw.to_radians() as f32);
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
                        HudAction::Calibrate => calibration = Some(Calibration::new()),
                        HudAction::OpenEnvironments => {
                            environments.refresh();
                            shell.set_environments(environments.entries(), environments.choice());
                        }
                        HudAction::OpenSystemSettings(module) => {
                            let command = format!("kcmshell6 {module}");
                            if let Err(e) =
                                spatiand_platform::launch(&command, &runtime.state.socket_name)
                            {
                                log::warn!("could not open {module}: {e}");
                            }
                        }
                        // Nothing to hand back in a window on someone else's desktop.
                        HudAction::ToggleKeyboard | HudAction::ReturnToDesktop
                        | HudAction::Screenshot => {
                            log::info!("{action:?} does nothing in the nested backend");
                        }
                        HudAction::Dismiss => {}
                    },
                }
            }
        }

        // Adopt a freshly measured mapping the moment it lands, so the world becomes
        // correctly head-locked without a restart.
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

        let size = backend.window_size();
        // A Wayland surface has no size until the compositor has configured it, and winit
        // reports 0x0 until then. Binding and drawing into that produces an incomplete
        // framebuffer and then EGL BAD_ALLOC when the swap tries to allocate a zero-sized
        // buffer -- which surfaces as `GL_INVALID_FRAMEBUFFER_OPERATION in glClear` and reads
        // as a renderer fault rather than as a window that does not exist yet.
        if size.w <= 0 || size.h <= 0 {
            event_loop.dispatch(Some(Duration::from_millis(16)), runtime)?;
            continue;
        }
        let viewports = eye_mode.viewports(size.w);
        let (eye_w, eye_h) = (viewports[0].2, size.h);

        // Late-latch: read the pose as close to drawing as possible, and predict one frame
        // forward to cancel the gap between reading it and photons arriving.
        let orientation = tracker.predicted_orientation(
            spatiand_track::DEFAULT_PREDICTION_SECONDS,
            spatiand_track::DEFAULT_PREDICTION_MAX_DEGREES,
        );

        let prompt_text = match calibration.as_ref() {
            Some(c) => {
                let p = c.prompt();
                format!("{}\n\n{}\n\n{}", p.heading, p.body, p.status)
            }
            None if shell.menu_is_open() => String::new(),
            None => {
                let e = tracker.euler_degrees();
                format!(
                    "Spatiand\n\nyaw {:.0}   pitch {:.0}   roll {:.0}\n\n{} window(s)",
                    e.yaw,
                    e.pitch,
                    e.roll,
                    runtime.state.space.elements().count()
                )
            }
        };

        let (renderer, framebuffer) = backend.bind()?;

        let ppd = TextRenderer::px_per_degree(eye_w.max(1) as u32, stereo.h_fov_deg);
        if sky_dirty {
            sky_dirty = false;
            let image = &sky_image;
            renderer.with_context(|gl| unsafe { scene.set_sky(gl, image) })?;
            log::info!("environment now {}", environments.describe());
        }
        scene.sync_apps(renderer, &mut text, &shell, ppd)?;
        scene.sync_menu(
            renderer,
            &mut text,
            crate::menu::model(&shell).as_ref(),
            ppd,
            (stereo.h_fov_deg, stereo.v_fov_deg()),
        )?;

        // Re-rasterise only when the words change. At 72 Hz, re-uploading an unchanged string
        // every frame is pure waste — the same reasoning that let HoloFrame idle at 0 fps
        // captured while panning.
        if prompt_text.is_empty() {
            if let Some(p) = panel.take() {
                renderer.with_context(|gl| unsafe { gl.DeleteTextures(1, &p.id) })?;
            }
            last_prompt.clear();
        } else if prompt_text != last_prompt {
            last_prompt = prompt_text.clone();
            // ~1.6 degrees tall: comfortably readable across a 40 degree field.
            let image = text.render(
                &prompt_text,
                ppd * 1.6,
                (eye_w as u32).saturating_sub(80).max(64),
                [235, 240, 255, 255],
            );
            let old = panel.take();
            let uploaded = renderer.with_context(|gl| unsafe {
                if let Some(o) = old {
                    gl.DeleteTextures(1, &o.id);
                }
                PanelTexture {
                    id: upload_rgba(gl, &image),
                    aspect: image.width as f32 / image.height.max(1) as f32,
                }
            })?;
            panel = Some(uploaded);
        }

        let panel_snapshot = panel.as_ref().map(|p| (p.id, p.aspect));
        // Where the pointer is aiming, latched with the pose this frame.
        let pointer_ray = pointer
            .map(|(px, py, _)| ray_from_pad(px, py, orientation, eye_centre(orientation, &stereo), &pointer_config));
        let scene = &scene;
        let shell = &shell;
        renderer.with_context(|gl| unsafe {
            gl.Disable(ffi::SCISSOR_TEST);
            gl.Viewport(0, 0, size.w, size.h);
            gl.ClearColor(0.02, 0.02, 0.05, 1.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);

            for (side, x, w) in &viewports {
                gl.Viewport(*x, 0, *w, eye_h);

                // No vertical flip here, unlike the DRM path. This draws straight into a GL
                // surface that is presented with GL's own bottom-left convention; the flip
                // exists there only because the frame goes through an offscreen texture.
                let eye = spatiand_render::eye_for(*side, orientation, DVec3::ZERO, &stereo);

                scene.draw_sky(gl, &eye);
                scene.draw_menu(gl, &eye, shell, (stereo.h_fov_deg, stereo.v_fov_deg()));
                if let Some(ray) = pointer_ray {
                    scene.draw_pointer(gl, &eye, &ray, None, true, crate::scene::Cursor::Point);
                }
                if let Some((tex, aspect)) = panel_snapshot {
                    let model = head_locked_panel(orientation, aspect);
                    scene.quads().draw(
                        gl,
                        tex,
                        &(eye.view_projection() * model),
                        [1.0, 1.0, 1.0, 1.0],
                        (0.0, 1.0),
                    );
                }
            }
        })?;

        drop(framebuffer);
        backend.submit(Some(&[Rectangle::from_size(size)]))?;

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

    if let Some(h) = hmd.as_mut() {
        let _ = h.set_display_mode(DisplayMode::Mono);
    }
    Ok(())
}

struct PanelTexture {
    id: u32,
    aspect: f32,
}

/// Transform placing a panel a fixed distance in front of the head, facing the viewer.
///
/// Head-locked, which is the opposite of everything else Spatiand draws — and deliberate.
/// This is what shows calibration prompts, and calibration exists precisely because the world
/// frame is not yet trustworthy; a world-locked prompt would swim around exactly when it most
/// needs to be readable.
///
/// The quad is authored in its own XY plane spanning -0.5..0.5. In the canonical frame
/// (+X forward, +Y left, +Z up) that means its local right maps to world **-Y** and its local
/// up to world **+Z**, sitting out at +X. Building the basis explicitly rather than composing
/// Euler rotations keeps that mapping legible — and mirrored text is the symptom of getting
/// the right-vector sign wrong.
fn head_locked_panel(orientation: DQuat, aspect: f32) -> Mat4 {
    let height = PANEL_WIDTH / aspect.max(0.01);
    let basis = Mat4::from_cols(
        (-Vec3::Y * PANEL_WIDTH).extend(0.0),
        (Vec3::Z * height).extend(0.0),
        Vec3::X.extend(0.0),
        (Vec3::X * PANEL_DISTANCE).extend(1.0),
    );
    Mat4::from_quat(orientation.as_quat()) * basis
}
