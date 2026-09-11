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
//! `SPATIAND_VIEW` is `world`, `hud`, `environment`, `files`, `launcher`, `keyboard`,
//! `calibrate` or `sidecar`; `SPATIAND_SNAPSHOT_SIZE` is
//! `WIDTHxHEIGHT` and defaults to one eye of the glasses (1920x1080). `SPATIAND_SNAPSHOT_YAW`
//! turns the head and `SPATIAND_SNAPSHOT_PITCH` tips it down, both in degrees -- which is how
//! the arc's edges and anything hanging below the eye line get checked.
//!
//! `sidecar` draws the Deck's own panel instead of the glasses, at its real 800x1280, and
//! takes `SPATIAND_VIEW_PAGE=keyboard` for its second page and `SPATIAND_VIEW_EXIT=0..1` to
//! catch the exit button part-way through its hold.
//!
//! `SPATIAND_VIEW_CLICK=off` draws either keyboard with its sound turned off, which is the
//! only way to look at the muted speaker without a headset and a finger.
//!
//! `SPATIAND_VIEW=waiting` draws the screen shown when there is nothing to put the world on,
//! with `SPATIAND_VIEW_MISSING=picture` for the half-connected case — glasses answering over
//! USB with no display behind them. Both are states that need broken hardware to reach, which
//! is exactly why they are worth being able to look at without it.
//!
//! `SPATIAND_SNAPSHOT_EYE=right` draws the right eye instead of the left. Rendering both and
//! comparing them is how a stereoscopic surface is checked without a headset on.
//!
//! `SPATIAND_VIEW=sidecar SPATIAND_VIEW_PAGE=startup` draws the panel as it looks while a
//! session is still coming up, with `SPATIAND_VIEW_STAGE=link|stereo|world` for which stage.
//! That screen only exists during startup on real hardware, so this is the only way to look at
//! it without restarting a session and being quick with a camera.
//!
//! `SPATIAND_SNAPSHOT_YAW=150` looks 150 degrees round, and tells the compositor the wearer
//! is facing that way — so windows open there and an immersive layer is centred there, which
//! is what makes "the film arrived behind me" reproducible without a headset.
//!
//! `SPATIAND_IDLE_SECONDS=n` winds the idle-fade clock forward by n seconds of nobody touching
//! anything, which is how a surface that asked for `set_idle_fade` is seen fading without a
//! wearer.
//!
//! `SPATIAND_CLICK=u,v` clicks the client's window at that fraction across it once it has
//! painted, and `SPATIAND_CLICK_BUTTON=right` uses the other button. This is how a menu gets
//! opened without a person: a menu is opened *by* a click, so a harness that cannot click can
//! only photograph an application sitting there with no menu open, which proves nothing.
//! `tools/popup-probe.c` is the matching minimal client.
//!
//! `SPATIAND_CLOSE=1` then does what the title bar's close button does, and says whether the
//! window went. The button did nothing for X11 windows and only a headset could show it; with
//! an X11 client this is that same check, over SSH.
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
use spatiand_shell::{DesktopPanels, Intent, Shell};

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
    /// The screen shown when there is no world to put anywhere. `SPATIAND_VIEW_MISSING=picture`
    /// picks the half-connected case; the default is glasses that are simply not plugged in.
    Waiting,
    Keyboard,
    Environment,
    Files,
    /// The Deck's own panel rather than the glasses, so the sidecar can be looked at without
    /// a Deck to look at. Honours `SPATIAND_VIEW_PAGE=keyboard` for its second page.
    Sidecar,
}

impl View {
    fn from_env() -> Self {
        match std::env::var("SPATIAND_VIEW").as_deref() {
            Ok("hud") => Self::Hud,
            Ok("environment") => Self::Environment,
            Ok("files") => Self::Files,
            Ok("launcher") => Self::Launcher,
            Ok("calibrate") => Self::Calibrate,
            Ok("waiting") => Self::Waiting,
            Ok("keyboard") => Self::Keyboard,
            Ok("sidecar") => Self::Sidecar,
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
    // Positive pitches the view DOWN, so anything that hangs below the eye line -- the
    // keyboard especially -- can be looked at without a headset. Without this the only way to
    // check something placed off the horizon is to put the glasses on, which is exactly the
    // sort of thing this backend exists to avoid.
    let pitch_deg: f64 = std::env::var("SPATIAND_SNAPSHOT_PITCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    // The Deck's own panel is portrait and its content is rolled a quarter turn, which is
    // exactly the case where panel sizing has gone wrong before.
    let portrait = std::env::var("SPATIAND_SNAPSHOT_PORTRAIT").is_ok();
    log::info!("snapshot: {view:?} at {width}x{height}, yaw {yaw_deg}, pitch {pitch_deg}, portrait {portrait} -> {}", out.display());
    // Where the wearer is looking, as far as everything that places things is concerned. The
    // session refreshes this from the tracker every frame; here it is the one number the
    // harness was given, set before any client can connect.
    //
    // Without it a snapshot at yaw 150 drew the world from over there while every window and
    // every immersive layer was still placed at world zero -- which is not a picture of
    // anything the session would ever show, and hid exactly the fault this exists to catch.
    runtime.state.spawn_yaw = yaw_deg.to_radians();

    // --- a GL context with no display attached ---
    let node =
        std::env::var("SPATIAND_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)?;
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
            categories: e.categories,
        })
        .collect();
    log::info!("launcher: {} application(s)", apps.len());
    // The snapshot always draws the settled arrangement, which is the one people will see for
    // all but the first session.
    let calibrated = true;
    let mut shell = Shell::new(apps, DesktopPanels::ALL, calibrated);

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
    // Not advertised: clients are told about `state.screen`. See `Spatiand::screen`.
    output.change_current_state(Some(output_mode), None, None, Some((0, 0).into()));
    output.set_preferred(output_mode);
    runtime.state.set_screen_refresh(output_mode.refresh);
    crate::dmabuf::advertise(&mut runtime.state, &renderer);
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
        // Both are reached by walking the real HUD rather than by setting a mode directly, so
        // a snapshot cannot show a state the wearer could not get to.
        View::Environment | View::Files => {
            shell.handle(Intent::ToggleHud);
            // Navigating never reports anything back, so the walk is bounded by the list's
            // own length rather than by waiting for it to stop moving.
            for _ in 0..shell.hud().items().len() {
                if shell.hud().activate() == spatiand_shell::HudAction::OpenEnvironments {
                    break;
                }
                shell.handle(Intent::Navigate(spatiand_shell::NavDirection::Down));
            }
            shell.handle(Intent::Accept);
            shell.set_environments(environments.entries(), environments.choice());
            if view == View::Files {
                // The browse row is always the last one.
                for _ in 0..shell.environments().rows().len() {
                    shell.handle(Intent::Navigate(spatiand_shell::NavDirection::Down));
                }
                shell.handle(Intent::Accept);
                let browser = crate::environment::Browser::new();
                shell.show_directory(browser.label(), browser.entries());
            }
        }
        _ => {}
    }

    let mut keyboard = spatiand_shell::Keyboard::default();
    if view == View::Keyboard {
        keyboard.open = true;
    }
    if std::env::var("SPATIAND_VIEW_CLICK").as_deref() == Ok("off") {
        keyboard.click = false;
    }

    // --- optionally host a real application ---
    let mut windows = Vec::new();
    if let Ok(command) = std::env::var("SPATIAND_CLIENT") {
        let seconds: f32 = std::env::var("SPATIAND_CLIENT_WAIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8.0);
        // An X server, so an X11 application can be photographed too. Without one the harness
        // could only ever test half the applications on the machine -- and the half it could
        // not test is the half whose bugs were being fixed by reasoning. `SPATIAND_XWAYLAND=off`
        // still turns it off, which is how a Wayland-only run is asked for.
        let x_display = crate::xwayland::start(&runtime.display_handle, &event_loop.handle());
        let environment = crate::xwayland::client_environment(x_display);
        log::info!("launching {command:?} into the snapshot, waiting up to {seconds}s");
        match spatiand_platform::launch(&command, &runtime.state.socket_name, &environment) {
            Ok(pid) => {
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs_f32(seconds);
                // Whether it ever drew, which is not the same as whether it is drawing now: a
                // window that was asked to close and did has no window left to count.
                let mut mapped = false;
                while std::time::Instant::now() < deadline {
                    // Pumping the display is what lets the client bind globals, get its
                    // configure, and commit. Without this it blocks on the first roundtrip and
                    // never draws anything.
                    display.dispatch_clients(&mut runtime.state)?;
                    display.flush_clients()?;
                    event_loop.dispatch(Some(std::time::Duration::from_millis(16)), runtime)?;

                    crate::dmabuf::settle(&mut runtime.state, &mut renderer);
                    // Noticing a shape change needs to have seen the shape before, so this runs
                    // on every pump here exactly as it does every frame in a session.
                    crate::window::apply_resize_anchors(&mut runtime.state);
                    windows = crate::scene::collect_windows(&mut renderer, &runtime.state);
                    if !windows.is_empty() {
                        // The first buffer a toolkit commits is usually blank -- it has the
                        // right size but the UI has not been painted into it. Snapshotting
                        // there gives a black rectangle that looks like a broken import.
                        // Keep pumping so the client gets to draw itself.
                        log::info!("client mapped; letting it paint");
                        let settle = std::time::Instant::now() + std::time::Duration::from_secs(3);
                        while std::time::Instant::now() < settle {
                            // Frame callbacks are what tell a client it may draw the next
                            // frame. Without them most toolkits paint once and stop.
                            let screen = runtime.state.screen.clone();
                            runtime.state.send_frames(&screen, std::time::Duration::ZERO);
                            runtime.state.space.refresh();
                            display.dispatch_clients(&mut runtime.state)?;
                            display.flush_clients()?;
                            event_loop
                                .dispatch(Some(std::time::Duration::from_millis(16)), runtime)?;
                            crate::dmabuf::settle(&mut runtime.state, &mut renderer);
                            // Noticing a shape change needs to have seen the shape before, so this runs
                            // on every pump here exactly as it does every frame in a session.
                            crate::window::apply_resize_anchors(&mut runtime.state);
                        }
                        windows = crate::scene::collect_windows(&mut renderer, &runtime.state);
                        // A click, if one was asked for, and then time to answer it. A menu
                        // is two round trips away: the client has to be told, create the
                        // popup, hear its configure, and commit a buffer. A close is the same
                        // shape -- asked, then answered in the client's own time -- so it
                        // gets the same wait rather than a pump of its own.
                        let steps = requested_steps();
                        let asked_to_close = steps.iter().any(|s| matches!(s, Step::Close));
                        for step in steps {
                            match step {
                                Step::Click(u, v) => {
                                    click_on_the_window(&mut runtime.state, &windows, (u, v))
                                }
                                Step::Close => close_the_window(&runtime.state, &windows),
                            }
                            let answered =
                                std::time::Instant::now() + std::time::Duration::from_secs(3);
                            while std::time::Instant::now() < answered {
                                let screen = runtime.state.screen.clone();
                                runtime.state.send_frames(&screen, std::time::Duration::ZERO);
                                runtime.state.space.refresh();
                                display.dispatch_clients(&mut runtime.state)?;
                                display.flush_clients()?;
                                event_loop
                                    .dispatch(Some(std::time::Duration::from_millis(16)), runtime)?;
                                // The same two as the pumps above, and the missing pair was a
                                // real gap rather than a tidy-up: a scripted client changes
                                // shape during *this* phase -- a click is what drives it to
                                // the player -- so a resize anchor could not be seen working
                                // from a script at all, and a dmabuf committed in reply to a
                                // click was never imported. All three pumps must stay alike.
                                crate::dmabuf::settle(&mut runtime.state, &mut renderer);
                                crate::window::apply_resize_anchors(&mut runtime.state);
                            }
                            windows = crate::scene::collect_windows(&mut renderer, &runtime.state);
                        }
                        // Said either way, because a window still standing looks exactly like
                        // one that was never asked -- and the answer is the point of asking.
                        if asked_to_close {
                            if windows.is_empty() {
                                log::info!("the window closed when asked to");
                            } else {
                                log::warn!("asked the window to close; it is still open");
                            }
                        }
                        mapped = true;
                        break;
                    }
                }
                if !mapped {
                    log::warn!("{command:?} (pid {pid}) never committed a buffer");
                    log::warn!("  it may need a wayland flag, or it may have exited immediately");
                }
            }
            Err(e) => log::warn!("could not launch {command:?}: {e}"),
        }
    }
    // Idle fading, without a wearer to sit still for it.
    //
    // `SPATIAND_IDLE_SECONDS=n` winds the attention clock forward by n seconds of nobody
    // touching anything, which is the one input a harness with no headset cannot produce by waiting.
    // It is how the compositor-side fade in `crate::attention` gets looked at at all.
    if let Ok(raw) = std::env::var("SPATIAND_IDLE_SECONDS") {
        let seconds: f32 = raw.parse().unwrap_or(0.0);
        // `true` for "there is a headset": the harness has none, and without pretending
        // otherwise nothing would ever fade and there would be nothing to photograph.
        let step = std::time::Duration::from_millis(14);
        let mut left = std::time::Duration::from_secs_f32(seconds.max(0.0));
        while !left.is_zero() {
            let dt = step.min(left);
            runtime.state.attention.tick(true, dt);
            left -= dt;
        }
        for quad in windows.iter_mut() {
            quad.fade = crate::xr::fade_of(&quad.surface, &runtime.state.attention);
            // Per surface, because the threshold is per surface: reporting the default here
            // said nothing about a window that had asked for its own, which is exactly the
            // case worth looking at.
            if quad.xr.idle_fade {
                log::info!(
                    "idle for {seconds}s: a surface asking to fade after {}ms is at alpha {:.2}",
                    crate::attention::idle_after(quad.xr.idle_after_ms).as_millis(),
                    quad.fade
                );
            }
        }
    }

    // Whether an application has taken the environment. Asked once here, because a snapshot
    // is one frame -- in the session this is asked every frame. See `scene::sky_surface`.
    let sky_from_client = crate::scene::sky_surface(&mut renderer, &runtime.state);
    if sky_from_client.is_some() {
        log::info!("an application is the environment for this frame");
    }
    scene.set_sky_override(sky_from_client);
    {
        let ppd_now = TextRenderer::px_per_degree(width, 40.0);
        for quad in windows.iter_mut() {
            let title = runtime.state.display_title(&quad.window);
            quad.title = scene.title_texture(&mut renderer, &mut text, &title, ppd_now);
            if let Some(app_id) = runtime.state.app_id_of(&quad.window) {
                quad.icon = scene.window_icon(&mut renderer, &app_id);
            }
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
        let ink = raw
            .chunks_exact(4)
            .filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8)
            .count();
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
    // Icons load on a thread of their own and arrive a frame or two after they are asked for.
    // A snapshot is the only frame there is, so it waits for them rather than drawing initials
    // where they go -- which would be a picture of something the session never looks like.
    scene.wait_for_icons(&mut renderer, std::time::Duration::from_secs(10));
    scene.sync_apps(&mut renderer, &mut text, &shell, ppd)?;
    for quad in windows.iter_mut() {
        if let Some(app_id) = runtime.state.app_id_of(&quad.window) {
            quad.icon = scene.window_icon(&mut renderer, &app_id);
        }
    }
    if keyboard.open {
        scene.sync_keyboard(&mut renderer, &mut text, &keyboard, ppd)?;
    }
    scene.sync_status(
        &mut renderer,
        &mut text,
        &crate::status::line(runtime.state.window_count()),
        ppd,
    )?;
    scene.sync_menu(
        &mut renderer,
        &mut text,
        crate::menu::model(&shell).as_ref(),
        ppd,
        (stereo.h_fov_deg, stereo.v_fov_deg()),
    )?;

    // The head-locked panel, when there is one to draw.
    let panel_text = match view {
        View::Calibrate => {
            let c = Calibration::new();
            let p = c.prompt();
            format!("{}\n\n{}\n\n{}", p.heading, p.body, p.status)
        }
        View::Waiting => {
            // The same words the DRM backend shows, from the same place, so that looking at
            // this is actually looking at what the wearer gets.
            let missing = match std::env::var("SPATIAND_VIEW_MISSING").as_deref() {
                Ok("picture") => crate::waiting::Missing::Picture,
                _ => crate::waiting::Missing::Headset,
            };
            // `had_headset` follows the case: the half-connected one has, by definition, just
            // opened a headset, while "not plugged in" is the first-run screen.
            let had = missing == crate::waiting::Missing::Picture;
            crate::waiting::message(missing, had, crate::waiting::exit_hint(true, had))
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

    // The panel is a different surface with a different size and its own 2D projection, so it
    // short-circuits the whole stereo path rather than being drawn into it.
    if view == View::Sidecar {
        return draw_sidecar(&mut renderer, &mut text, &mut scene, &keyboard, &out);
    }

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

    // Yaw about up, then pitch about the rotated left axis -- the same order `Placement`
    // uses, so a snapshot frames things the way the world actually builds them.
    let orientation = DQuat::from_axis_angle(DVec3::Z, yaw_deg.to_radians())
        * DQuat::from_axis_angle(DVec3::Y, pitch_deg.to_radians());
    // `SPATIAND_VIEW_HOVER=g` draws that key raised, which is the only way to check the
    // hover state without a controller in hand.
    let hovered_keys: Vec<&'static spatiand_shell::keyboard::Key> =
        match std::env::var("SPATIAND_VIEW_HOVER") {
            Ok(want) if !want.is_empty() => spatiand_shell::keyboard::ROWS
                .iter()
                .flat_map(|r| r.iter())
                .find(|k| k.label == want)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        };
    for key in &hovered_keys {
        scene.sync_key_cap(&mut renderer, &mut text, &keyboard, key)?;
    }
    let shell_ref = &shell;
    let scene_ref = &scene;
    let keyboard_open = keyboard.open;
    let keyboard_state = &keyboard;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    renderer.with_context(|gl| unsafe {
        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
        gl.Disable(ffi::SCISSOR_TEST);
        gl.Viewport(0, 0, width as i32, height as i32);
        gl.ClearColor(0.02, 0.02, 0.05, 1.0);
        gl.Clear(ffi::COLOR_BUFFER_BIT);

        // No flip: the readback below does it, so the PNG comes out the right way up while
        // the geometry stays in GL's own convention.
        // Which eye to draw. Left unless asked otherwise -- and being able to ask is the whole
        // way a stereoscopic surface gets checked without a headset: render both, and the two
        // pictures either differ in the right way or they do not.
        let side = match std::env::var("SPATIAND_SNAPSHOT_EYE").as_deref() {
            Ok("right") => EyeSide::Right,
            _ => EyeSide::Left,
        };
        let eye = spatiand_render::eye_for(side, orientation, DVec3::ZERO, &stereo);
        // The waiting screen is not a place, so it gets the same bare backdrop the DRM backend
        // gives it. Drawing a world behind it here would make this view a picture of something
        // that never ships -- and the whole reason for the view is to see what does.
        if view != View::Waiting {
            scene_ref.draw_sky(gl, &eye);
            scene_ref.draw_windows(gl, &eye, &windows);
            if !shell_ref.menu_is_open() {
                scene_ref.draw_status(gl, &eye, orientation);
            }
        }
        if keyboard_open {
            let focus = windows.iter().find(|w| w.focused);
            scene_ref.draw_keyboard(
                gl,
                &eye,
                focus.map(|w| &w.placement),
                focus.map(|w| w.pixels).unwrap_or((16, 9)),
                orientation,
                (stereo.h_fov_deg, stereo.v_fov_deg()),
                keyboard_state,
                false,
                &hovered_keys,
            );
        }
        scene_ref.draw_menu(gl, &eye, shell_ref, (stereo.h_fov_deg, stereo.v_fov_deg()));
        if let Some((tex, aspect)) = panel {
            let (pw, ph) = crate::backend_drm::fit_panel(
                aspect,
                stereo.h_fov_deg,
                stereo.v_fov_deg(),
                portrait,
            );
            let model = crate::backend_drm::head_locked_panel_sized(orientation, pw, ph, portrait);
            scene_ref.quads().draw(
                gl,
                tex,
                &(eye.view_projection() * model),
                [1.0; 4],
                (0.0, 1.0),
            );
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

/// Render the Deck's own panel to a PNG.
///
/// The sidecar draws in the panel's pixels with its own quarter turn, so there is nothing here
/// to share with the stereo path above -- and every layout bug it has ever had was one that
/// only showed up as a picture.
fn draw_sidecar(
    renderer: &mut GlesRenderer,
    text: &mut TextRenderer,
    scene: &mut Scene,
    keyboard: &spatiand_shell::Keyboard,
    out: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // The Deck's panel, as it actually reports itself.
    let panel = (800u32, 1280u32);
    let mut ui = crate::sidecar::Sidecar::new(scene.white(), panel);
    if std::env::var("SPATIAND_VIEW_PAGE").as_deref() == Ok("keyboard") {
        ui.show(crate::sidecar::Page::Keyboard);
    }
    // `SPATIAND_VIEW_EXIT=0.6` draws the exit button part-way through its hold, which is the
    // only way to see the fill without a Deck and a spare finger.
    if let Ok(progress) = std::env::var("SPATIAND_VIEW_EXIT") {
        if let Ok(progress) = progress.parse::<f32>() {
            ui.pose_exit_hold(progress);
        }
    }
    let levels = crate::sidecar::Levels {
        screen: Some(0.62),
        glasses: Some(0.40),
        volume: Some(0.75),
    };
    let audio = crate::sidecar::Audio::default();
    let mut monitors = crate::system::Monitors::new();
    monitors.tick();

    // The startup screen is drawn instead of the running one, not as a page of it: none of
    // what the sidecar normally shows exists while a session is still coming up.
    // `SPATIAND_VIEW_PAGE=startup` picks it, and `SPATIAND_VIEW_STAGE=stereo` picks which
    // stage -- otherwise the long one, which is the one worth looking at.
    let startup = (std::env::var("SPATIAND_VIEW_PAGE").as_deref() == Ok("startup")).then(|| {
        let stage = match std::env::var("SPATIAND_VIEW_STAGE").as_deref() {
            Ok("link") => crate::startup::Stage::Link,
            Ok("world") => crate::startup::Stage::World,
            _ => crate::startup::Stage::Stereo,
        };
        let label = scene
            .title_texture(renderer, text, stage.label(), 80.0)
            .map(|t| (t.id, t.aspect));
        (stage, label)
    });

    let prepared = ui.prepare(renderer, text, &monitors, "17:04", levels, &audio, keyboard);

    let target: GlesTexture =
        renderer.create_buffer(Fourcc::Abgr8888, (panel.0 as i32, panel.1 as i32).into())?;
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
        return Err(format!("sidecar framebuffer incomplete: {:#x}", fbo.1).into());
    }
    let fbo = fbo.0;

    let mut pixels = vec![0u8; (panel.0 * panel.1 * 4) as usize];
    let quads = scene.quads();
    let rounded = scene.rounded();
    renderer.with_context(|gl| unsafe {
        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
        gl.Disable(ffi::SCISSOR_TEST);
        gl.Viewport(0, 0, panel.0 as i32, panel.1 as i32);
        gl.ClearColor(0.02, 0.03, 0.05, 1.0);
        gl.Clear(ffi::COLOR_BUFFER_BIT);
        match &startup {
            Some((stage, label)) => ui.draw_startup(
                gl,
                quads,
                rounded,
                *label,
                stage.step(),
                crate::startup::STAGES.len(),
            ),
            None => ui.draw(
                gl, quads, rounded, &monitors, levels, &audio, keyboard, &prepared,
            ),
        }
        gl.ReadPixels(
            0,
            0,
            panel.0 as i32,
            panel.1 as i32,
            ffi::RGBA,
            ffi::UNSIGNED_BYTE,
            pixels.as_mut_ptr() as *mut _,
        );
        gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
    })?;

    // No flip on the way out, unlike the stereo path above. The sidecar's own projection
    // already carries the Y flip -- see `Sidecar::projection`, which explains at length why it
    // lives in `Layout::rect` rather than in the projection itself. Flipping again here
    // mirrors every label while leaving the layout looking plausible, which is precisely the
    // failure that comment was written about.
    image::save_buffer(out, &pixels, panel.0, panel.1, image::ColorType::Rgba8)?;
    log::info!("wrote {}", out.display());
    Ok(())
}

/// Where a synthetic click should land, as a fraction across the window.
///
/// `SPATIAND_CLICK=u,v`, both 0..1 -- so `0.5,0.5` is the middle of the client's surface and
/// `0.06,0.09` is a toolbar button near the top left. `SPATIAND_CLICK_BUTTON=right` sends the
/// other one. Several may be given, separated by `;`, with time to answer between each: a
/// menu item is two clicks away, and a dialog behind it is three.
///
/// This exists because of one bug, and it is worth saying which: menus in real applications
/// did not appear, and the only way to reproduce it was to put the glasses on and click
/// something. A menu is opened *by* a click, so a harness that cannot click cannot see the
/// thing at all -- it can launch an application and photograph it sitting there with no menu
/// open, which proves nothing. Pressing a real toolbar button and photographing what happens
/// next is the difference between reading the code again and knowing.
fn requested_clicks() -> Vec<(f64, f64)> {
    let Ok(raw) = std::env::var("SPATIAND_CLICK") else {
        return Vec::new();
    };
    raw.split(';')
        .filter_map(|step| {
            let (u, v) = step.split_once(',')?;
            let u: f64 = u.trim().parse().ok()?;
            let v: f64 = v.trim().parse().ok()?;
            Some((u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)))
        })
        .collect()
}

/// One scripted thing to do to the client's window, each followed by time for it to answer.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    /// `SPATIAND_CLICK`: a fraction across the window. See [`requested_clicks`].
    Click(f64, f64),
    /// `SPATIAND_CLOSE=1`: what the title bar's close button does. Always last, because
    /// nothing after it has a window to happen to.
    Close,
}

/// The clicks asked for, then the close if one was.
fn requested_steps() -> Vec<Step> {
    let mut steps: Vec<Step> = requested_clicks()
        .into_iter()
        .map(|(u, v)| Step::Click(u, v))
        .collect();
    if std::env::var("SPATIAND_CLOSE").as_deref() == Ok("1") {
        steps.push(Step::Close);
    }
    steps
}

/// Do what the close button does to the first window, without a title bar or a finger.
///
/// Through [`Spatiand::close_window`], which is all the button does once its hit test has
/// passed -- and the hit test is geometry, already tested as geometry in `pointer`. What could
/// not be tested was the half after it: whether the application hears the request at all. For
/// an X11 window it did not, and nothing short of a headset could show that.
fn close_the_window(state: &Spatiand, windows: &[crate::scene::WindowQuad]) {
    let Some(window) = windows.first() else {
        log::warn!("asked to close a window with no window to close");
        return;
    };
    log::info!("asking {:?} to close", state.display_title(&window.window));
    state.close_window(&window.window);
}

/// Press and release a mouse button on the first window.
///
/// Coordinates stay the window's own even when a menu is open, because a menu here *is* drawn
/// on the window's surface -- so a menu item is addressed the same way a toolbar button is.
///
/// Deliberately *not* routed through the ray caster. There is no head, no pad and no aim here;
/// what is being tested is what a client does when the pointer arrives, and going through the
/// 3D pointer would be testing the ray maths instead. These are the same events it ends up
/// sending: enter, motion in surface pixels, press, release.
fn click_on_the_window(
    state: &mut Spatiand,
    windows: &[crate::scene::WindowQuad],
    at: (f64, f64),
) {
    use smithay::input::pointer::{ButtonEvent, MotionEvent};
    use smithay::utils::{Point, SERIAL_COUNTER};

    let Some(window) = windows.first() else {
        log::warn!("asked for a click with no window to click on");
        return;
    };
    let button = match std::env::var("SPATIAND_CLICK_BUTTON").as_deref() {
        Ok("right") => crate::pointer::BTN_RIGHT,
        Ok("middle") => crate::pointer::BTN_MIDDLE,
        _ => crate::pointer::BTN_LEFT,
    };
    let location = Point::from((at.0 * window.pixels.0 as f64, at.1 * window.pixels.1 as f64));
    log::info!(
        "clicking button {button:#x} at {:.0},{:.0} of {}x{}",
        location.x,
        location.y,
        window.pixels.0,
        window.pixels.1
    );
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let surface = window.surface.clone();
    // Keyboard focus as well as pointer focus: several toolkits open a menu from the focused
    // widget rather than from what is under the cursor.
    let window = window.window.clone();
    state.focus_window(&window);

    // The second element is the surface's ORIGIN, not the position within it -- smithay sends
    // the client `location - origin`. Our space is one surface at a time, so it is zero.
    pointer.motion(
        state,
        Some((surface, Point::from((0.0, 0.0)))),
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time: 100,
        },
    );
    pointer.frame(state);
    for (pressed, time) in [(true, 110u32), (false, 190u32)] {
        pointer.button(
            state,
            &ButtonEvent {
                button,
                state: if pressed {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(state);
    }
}
