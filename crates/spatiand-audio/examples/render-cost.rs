//! How long a second of sound takes to place, per layout.
//!
//! The renderer runs in a real-time callback with a whole quantum to finish in — about ten
//! milliseconds at 512 frames — and it does one convolution per channel per ear. A measured
//! head is 558 taps, so twelve channels is twelve times the work of one, and whether that fits
//! is a question about this machine rather than about the code.
//!
//! It exists because a game answered it the hard way: Stumble Guys saw a twelve-channel sink,
//! mixed for twelve channels, and its music broke up.
//!
//! ```text
//! cargo run --release -p spatiand-audio --example render-cost
//! ```
//!
//! Read the last column. Anything near or above 1.0 cannot keep up at all; anything above
//! about 0.2 is asking for trouble on a machine that is also running a game.

use std::time::Instant;

use spatiand_audio::hrtf::Hrtf;
use spatiand_audio::render::{Binaural, Directness};
use spatiand_audio::stage::{place, Layout, Stage};
use spatiand_audio::Panner;

const RATE: u32 = 48_000;
const SECONDS: usize = 4;

fn main() {
    match Hrtf::system(RATE) {
        Ok(h) => println!("measured head: {} taps", h.taps()),
        Err(e) => println!("no measured head on this machine ({e}); the arithmetic one only"),
    }
    println!();
    println!("{:<14} {:>9} {:>10} {:>12} {:>9}", "layout", "channels", "head", "seconds/4s", "of real time");

    for layout in [
        Layout::Mono,
        Layout::Stereo,
        Layout::Surround51,
        Layout::Surround71,
        Layout::Surround514,
        Layout::Surround714,
    ] {
        for name in ["reasoned", "measured"] {
            // Opened per run rather than shared: the dataset holds a library handle and is not
            // something to clone, and opening it is not what is being timed.
            let spatialiser: Box<dyn spatiand_audio::Spatialise> = if name == "measured" {
                match Hrtf::system(RATE) {
                    Ok(h) => Box::new(h),
                    Err(_) => continue,
                }
            } else {
                Box::new(Panner::new(RATE))
            };
            let channels = layout.count();
            let mut binaural = Binaural::new(layout, spatialiser, Directness::SPATIAL, RATE);
            binaural.aim(
                &place(
                    layout,
                    &Stage {
                        yaw: 0.3,
                        pitch: 0.0,
                        half_width: 0.25,
                    },
                    glam::DQuat::IDENTITY,
                ),
                0.3,
            );
            // A quantum at a time, as the audio thread gets it.
            let quantum = 512;
            let input: Vec<f32> = (0..quantum * channels)
                .map(|i| ((i % 97) as f32 / 97.0) - 0.5)
                .collect();
            let mut out = vec![0.0; quantum * 2];
            let blocks = RATE as usize * SECONDS / quantum;
            let began = Instant::now();
            for _ in 0..blocks {
                binaural.render(&input, &mut out);
            }
            let spent = began.elapsed().as_secs_f64();
            println!(
                "{:<14} {:>9} {:>10} {:>12.3} {:>8.1}%",
                format!("{layout:?}"),
                channels,
                name,
                spent,
                spent / SECONDS as f64 * 100.0
            );
        }
    }
}
