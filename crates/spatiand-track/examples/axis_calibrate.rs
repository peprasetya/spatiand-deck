//! Interactive axis calibration over the terminal.
//!
//! A bring-up tool. The real flow will show these prompts on the glasses themselves — nobody
//! should need a terminal to make head tracking work — but the measurement logic in
//! `spatiand_track::calibration` is the same either way, so what this produces is the map the
//! shell will use.
//!
//!   cargo run --release --example axis_calibrate -p spatiand-track
//!   cargo run --release --example axis_calibrate -p spatiand-track -- --show

use std::io::Write;
use std::time::{Duration, Instant};

use spatiand_hmd::{Hmd, HmdEvent};
use spatiand_track::calibration::{self, Phase, PhaseCollector};
use spatiand_track::{config, AxisMap};

const COLLECT_SECONDS: f64 = 4.0;
const MAX_ATTEMPTS: u32 = 4;

/// Drain samples for `seconds`, optionally accumulating them, returning the peak rate seen.
fn pump(
    hmd: &mut Box<dyn Hmd>,
    seconds: f64,
    collector: Option<&mut PhaseCollector>,
) -> f64 {
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    let mut peak: f64 = 0.0;
    let mut sink = collector;
    while Instant::now() < end {
        match hmd.poll(Duration::from_millis(10)) {
            Ok(Some(HmdEvent::Imu(s))) => {
                peak = peak.max(s.gyro.length());
                if let Some(c) = sink.as_deref_mut() {
                    c.feed(&s);
                }
            }
            Ok(Some(HmdEvent::Disconnected)) => {
                eprintln!("\nglasses disconnected");
                std::process::exit(1);
            }
            Ok(_) | Err(_) => {}
        }
    }
    peak
}

fn countdown(hmd: &mut Box<dyn Hmd>, phase: Phase, step: usize) {
    for n in (1..=3).rev() {
        print!("\r  [{step}/3] {} — get ready {n}...   ", phase.heading());
        let _ = std::io::stdout().flush();
        pump(hmd, 1.0, None);
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    if std::env::args().any(|a| a == "--show") {
        match config::load_axes() {
            Some(m) => println!("{}\n  stored at {}", m.summary(), config::axes_path().display()),
            None => println!("no stored calibration (would use identity: {})", AxisMap::IDENTITY.summary()),
        }
        return;
    }

    let mut hmd = match spatiand_hmd::open_any() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("could not open a headset: {e}");
            std::process::exit(1);
        }
    };
    println!("{}\n", hmd.info().name);
    println!("Three movements. Move SLOWLY and hold each one.");
    println!("Wear the glasses — this measures head motion, not hand motion.\n");

    // Wait for the glasses to be picked up, so the first phase does not start while they are
    // still sitting on the desk.
    print!("  Put the glasses on, then shake your head to begin... ");
    let _ = std::io::stdout().flush();
    let started = Instant::now();
    loop {
        if pump(&mut hmd, 0.1, None) > 60.0 {
            println!("got it.\n");
            break;
        }
        if started.elapsed() > Duration::from_secs(120) {
            eprintln!("\ntimed out waiting for movement.");
            std::process::exit(1);
        }
    }
    pump(&mut hmd, 1.5, None); // let the shake settle

    let mut measured = Vec::new();
    for (index, phase) in Phase::ALL.iter().enumerate() {
        let step = index + 1;
        let mut accepted = None;

        for attempt in 1..=MAX_ATTEMPTS {
            countdown(&mut hmd, *phase, step);
            print!("\r  [{step}/3] {} — MOVE NOW          ", phase.heading());
            let _ = std::io::stdout().flush();

            let mut collector = PhaseCollector::new();
            pump(&mut hmd, COLLECT_SECONDS, Some(&mut collector));

            let v = collector.integral();
            let (axis, value) = collector.dominant();
            println!(
                "\r  [{step}/3] {:<10} X {:+7.1}  Y {:+7.1}  Z {:+7.1}  ->  {} {}",
                phase.heading(),
                v.x,
                v.y,
                v.z,
                ["X", "Y", "Z"][axis],
                if value < 0.0 { "negative" } else { "positive" }
            );

            if let Some(a) = collector.accepted() {
                accepted = Some(a);
                break;
            }
            // Retry rather than abort — the usual cause is simply not having started to move
            // yet, and discarding good phases for that is needless.
            println!(
                "        too small (need {:.0} deg){}",
                calibration::MINIMUM_DEGREES,
                if attempt < MAX_ATTEMPTS { " — again" } else { "" }
            );
        }

        match accepted {
            Some(a) => measured.push(a),
            None => {
                eprintln!("\ngave up on \"{}\". Nothing saved.", phase.heading());
                std::process::exit(1);
            }
        }
        println!("        good.\n");
    }

    match calibration::build(&measured) {
        Ok(map) => {
            println!("Result: {}", map.summary());
            match config::save_axes(&map) {
                Ok(p) => println!("Saved to {}", p.display()),
                Err(e) => eprintln!("Could not save: {e}"),
            }
            if map == AxisMap::IDENTITY {
                println!("\n(That matches the built-in default, so the identity guess was right.)");
            } else {
                println!("\n(This differs from the built-in default — the guess was wrong, which is\n why this step exists.)");
            }
        }
        Err(e) => {
            eprintln!("\nCalibration failed: {e}");
            eprintln!("Nothing was saved. Run it again, keeping each movement clean and separate.");
            std::process::exit(1);
        }
    }
}
