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
use spatiand_render::{EyeSide, StereoConfig, TextRenderer};
use spatiand_track::{AxisMap, HeadTracker, TrackerConfig};

use crate::calib::Calibration;
use crate::gl::{upload_rgba, QuadPipeline};
use crate::{Runtime, Spatiand};

/// Half-height, so the side-by-side pair fits an ordinary screen while keeping its shape.
const DEV_WIDTH: i32 = 1920;
const DEV_HEIGHT: i32 = 540;

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

    let mode = Mode {
        size: (DEV_WIDTH, DEV_HEIGHT).into(),
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

    // A stored calibration means this headset's axes are already known. Without one, run the
    // in-world flow rather than silently trusting a guess — the guess is usually right, but
    // "usually" produces a world that nods when it should pan and no clue why.
    let stored = spatiand_track::config::load_axes();
    let mut calibration = if stored.is_none() && hmd.is_some() {
        log::info!("no stored axis calibration — starting the in-world flow");
        Some(Calibration::new())
    } else {
        None
    };
    let mut tracker = HeadTracker::new(stored.unwrap_or(AxisMap::IDENTITY), TrackerConfig::default());

    // --- gpu resources ---
    let pipeline = QuadPipeline::new(backend.renderer())?;
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

        // Re-rasterise only when the words change. At 72 Hz, re-uploading an unchanged string
        // every frame is pure waste — the same reasoning that let HoloFrame idle at 0 fps
        // captured while panning.
        if prompt_text != last_prompt {
            last_prompt = prompt_text.clone();
            let ppd = TextRenderer::px_per_degree(eye_w.max(1) as u32, stereo.h_fov_deg);
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
        renderer.with_context(|gl| unsafe {
            gl.Disable(ffi::SCISSOR_TEST);
            gl.Viewport(0, 0, size.w, size.h);
            gl.ClearColor(0.02, 0.02, 0.05, 1.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);

            let Some((tex, aspect)) = panel_snapshot else {
                return;
            };
            for (side, x, w) in &viewports {
                gl.Viewport(*x, 0, *w, eye_h);

                let eye = spatiand_render::eye_for(*side, orientation, DVec3::ZERO, &stereo);
                let model = head_locked_panel(orientation, aspect);
                let mvp = eye.projection * eye.view * model;
                pipeline.draw(gl, tex, &mvp, [1.0, 1.0, 1.0, 1.0], (0.0, 1.0));
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
