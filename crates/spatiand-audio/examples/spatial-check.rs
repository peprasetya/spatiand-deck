//! Drives the engine against a live audio server, so the plumbing can be heard and measured.
//!
//! Not a test, because it needs an audio server, a window's worth of audio played into it from
//! somewhere else, and something recording the result — none of which a test harness has. Run
//! it on the machine itself:
//!
//! ```text
//! spatial-check            # opens a sink and swings it left and right
//! ```
//!
//! Then, in another shell, play into `spatiand.window.1` and record whatever the sink it feeds
//! is playing. What should be visible in the recording is the sound moving from one ear to the
//! other on the beat of the swing, and nothing else changing.

use std::time::Duration;

use spatiand_audio::render::Directness;
use spatiand_audio::server::{routing_env, sink_name, Engine, Head};
use spatiand_audio::stage::{place, Layout, Stage};

const RATE: u32 = 48_000;
const SLOT: u64 = 1;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let layout = match std::env::args().nth(1).as_deref() {
        Some("5.1") => Layout::Surround51,
        Some("7.1") => Layout::Surround71,
        _ => Layout::Stereo,
    };
    // Entirely spatial, so what is measured is the placement rather than the blend.
    let engine = Engine::start(RATE, Head::Measured, Directness::SPATIAL);
    engine.open(SLOT);
    std::thread::sleep(Duration::from_millis(500));

    let (key, value) = routing_env(SLOT);
    println!("sink:  {}", sink_name(SLOT));
    println!("route: {key}={value}");
    println!(
        "play:  pw-cat --playback --target {} <file>",
        sink_name(SLOT)
    );
    println!();

    // Swing the window from hard left to hard right and back, holding at each end long enough
    // to be obvious in a recording.
    let stops = [
        ("left", 90.0f64),
        ("ahead", 0.0),
        ("right", -90.0),
        ("behind", 180.0),
    ];
    for round in 0..3 {
        for (label, yaw) in stops {
            println!("[{round}] {label} ({yaw:+.0} degrees)");
            // Step there smoothly, as a head would, so the crossfade is exercised rather than
            // stepped over.
            for step in 0..=20 {
                let at = yaw * step as f64 / 20.0;
                let stage = Stage {
                    yaw: at.to_radians(),
                    pitch: 0.0,
                    half_width: 14f64.to_radians(),
                };
                engine.aim(
                    SLOT,
                    place(layout, &stage, glam::DQuat::IDENTITY),
                    at.to_radians().abs(),
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            if let Some(status) = engine.status(SLOT) {
                println!(
                    "      connection {:?}, actually sending {:?}, peak {:.4}{}",
                    status.layout,
                    status.sounding,
                    status.peak,
                    if status.muted { ", muted" } else { "" }
                );
            }
            std::thread::sleep(Duration::from_millis(1200));
        }
    }
    println!("done");
}
