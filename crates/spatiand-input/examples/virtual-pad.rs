//! Holds the virtual gamepad up so something else can be asked whether it can see it.
//!
//! The pad a game sees is created by the compositor and lives as long as the session, which
//! makes it awkward to test anything against: a unit test creates one for a few milliseconds
//! and a session needs a headset. This holds one for as long as you like, from a terminal.
//!
//! ```text
//! virtual-pad            # holds it for 30 seconds, pressing A once a second
//! virtual-pad 120 quiet  # two minutes, no buttons
//! ```
//!
//! What it is for, in order of usefulness:
//!
//! * open a game's controller-binding screen and watch whether A arrives;
//! * run `jstest /dev/input/jsN` or `evtest` against it;
//! * check that a game started with the environment it prints sees this pad **and nothing
//!   else** — which is the whole point of the virtual pad, and not something the compositor
//!   can prove about itself.

use std::time::{Duration, Instant};

use spatiand_input::virtual_pad::{hide_other_controllers, Identity, Report, VirtualPad};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let seconds: u64 = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(30);
    let quiet = args.next().as_deref() == Some("quiet");

    // `SPATIAND_PAD=xbox` wears the identity this used to have, which is the experiment that
    // answers "why does this game not see the controller": run one of each while a game is up
    // and watch which one `winedevice.exe` opens.
    let mut identity = if std::env::var("SPATIAND_PAD").as_deref() == Ok("xbox") {
        Identity::xbox_360()
    } else {
        Identity::default()
    };
    // Any identity at all, because which ones a runtime accepts is a question only the runtime
    // can answer: `SPATIAND_PAD_NAME`, `SPATIAND_PAD_VID`, `SPATIAND_PAD_PID`,
    // `SPATIAND_PAD_VER`, the numbers in hex.
    if let Ok(name) = std::env::var("SPATIAND_PAD_NAME") {
        identity.name = name;
    }
    for (key, field) in [
        ("SPATIAND_PAD_VID", &mut identity.vendor as *mut u16),
        ("SPATIAND_PAD_PID", &mut identity.product as *mut u16),
        ("SPATIAND_PAD_VER", &mut identity.version as *mut u16),
    ] {
        if let Some(value) = std::env::var(key)
            .ok()
            .and_then(|v| u16::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        {
            // SAFETY: three distinct fields of a local, written one at a time.
            unsafe { *field = value };
        }
    }
    let mut pad = match VirtualPad::create_as(&identity) {
        Ok(pad) => pad,
        Err(e) => {
            eprintln!("could not create the pad: {e}");
            std::process::exit(1);
        }
    };
    println!("holding the pad for {seconds}s");
    println!("a game started with these sees this pad and no other:");
    for (key, value) in hide_other_controllers() {
        println!("  {key}={value}");
    }
    if !quiet {
        println!("pressing A once a second, and sweeping the left stick");
    }

    let began = Instant::now();
    let mut pressed = false;
    while began.elapsed() < Duration::from_secs(seconds) {
        let t = began.elapsed().as_secs_f32();
        let report = if quiet {
            Report::default()
        } else {
            pressed = t.rem_euclid(1.0) < 0.2;
            Report {
                // Bit 0 is A -- see `PadButton::bit`, which the compositor uses to build this.
                buttons: if pressed { 1 } else { 0 },
                left: ((t * 0.7).sin(), (t * 0.5).cos()),
                ..Default::default()
            }
        };
        if let Err(e) = pad.send(&report) {
            eprintln!("the pad stopped taking reports: {e}");
            break;
        }
        // Rumble a game asks for, which is the other direction and worth seeing arrive.
        if let Some((strong, weak)) = pad.poll_rumble() {
            println!("rumble asked for: strong {strong}, weak {weak}");
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    println!("done");
}
