//! Spatiand — a 3D spatial desktop.
//!
//! Two backends, and having both from the start is deliberate:
//!
//! * **winit** — runs Spatiand as an ordinary window under an existing desktop. This is the
//!   development path. A compositor that takes DRM master on the only display you have is a
//!   machine you cannot debug when the render loop breaks.
//! * **drm** — the real session: owns the glasses in side-by-side stereo and the Deck's
//!   panel as a sidecar.
//!
//! Select with `SPATIAND_BACKEND=winit|drm`; winit is the default while the DRM path is
//! still being built.

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::{Display, DisplayHandle};

mod backend_drm;
mod backend_snapshot;
mod backend_winit;
mod calib;
mod environment;
mod gl;
mod icon;
mod input_map;
mod pointer;
mod scene;
mod state;
mod status;
mod window;

pub use state::Spatiand;

/// What the event loop carries. Split from [`Spatiand`] so the compositor state can be
/// borrowed independently of the backend during a frame.
pub struct Runtime {
    pub state: Spatiand,
    pub display_handle: DisplayHandle,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let backend = std::env::var("SPATIAND_BACKEND").unwrap_or_else(|_| "winit".into());
    log::info!("starting spatiand ({backend} backend)");

    let mut event_loop: EventLoop<Runtime> = EventLoop::try_new().expect("could not create an event loop");
    let mut display: Display<Spatiand> = Display::new().expect("could not create a wayland display");
    let display_handle = display.handle();

    let state = Spatiand::new(&mut display, &event_loop.handle());
    let socket = state.socket_name.clone();
    let mut runtime = Runtime {
        state,
        display_handle,
    };

    match backend.as_str() {
        "winit" => {
            if let Err(e) = backend_winit::run(&mut event_loop, &mut display, &mut runtime) {
                log::error!("winit backend failed: {e}");
                std::process::exit(1);
            }
        }
        "drm" => {
            if let Err(e) = backend_drm::run(&mut event_loop, &mut display, &mut runtime) {
                log::error!("drm backend failed: {e}");
                std::process::exit(1);
            }
        }
        // Renders one frame to a PNG with no display, no session and no headset. The only
        // way to see a layout bug without putting the glasses on and describing it.
        "snapshot" => {
            if let Err(e) = backend_snapshot::run(&mut event_loop, &mut display, &mut runtime) {
                log::error!("snapshot backend failed: {e}");
                std::process::exit(1);
            }
        }
        other => {
            log::error!("unknown backend {other:?}; expected winit, drm or snapshot");
            std::process::exit(1);
        }
    }

    log::info!("spatiand exited (socket was {socket})");
}

/// Input is routed by `spatiand-input` once it exists; until then the nested backend needs
/// somewhere to send winit events so the window is not inert.
pub fn input_stub(_state: &mut Spatiand, _event: smithay::backend::input::InputEvent<smithay::backend::winit::WinitInput>) {
}

