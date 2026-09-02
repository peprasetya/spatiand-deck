//! End-to-end hardware check: real glasses -> spatiand-hmd -> spatiand-track.
//!
//! This is the Rust counterpart of `tools/xr_spike.py`. The spike proved the protocol; this
//! proves our implementation of it, which is a different claim.
//!
//! Usage:
//!   cargo run --example hardware_check -p spatiand-track            # IMU only, safe
//!   cargo run --example hardware_check -p spatiand-track -- stereo  # also switches the panel

use std::time::{Duration, Instant};

use spatiand_hmd::{DisplayMode, HmdEvent};
use spatiand_track::{AxisMap, HeadTracker, TrackerConfig};

fn dp_modes() -> Vec<String> {
    std::fs::read_to_string("/sys/class/drm/card0-DP-1/modes")
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn has_stereo_mode() -> bool {
    dp_modes().iter().any(|m| m.starts_with("3840x1080"))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let test_stereo = std::env::args().any(|a| a == "stereo");

    println!("== opening ==");
    let mut hmd = match spatiand_hmd::open_any() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("could not open a headset: {e}");
            std::process::exit(1);
        }
    };
    let info = hmd.info().clone();
    println!("  {}", info.name);
    println!(
        "  per eye {}x{}, {} deg horizontal, default IPD {} mm, fused pose: {}",
        info.per_eye.0,
        info.per_eye.1,
        info.h_fov_deg,
        info.default_ipd_mm,
        info.provides_fused_pose
    );

    // --- IMU + tracker ---
    println!("\n== IMU (8 s) ==");
    println!("  hold the glasses STILL — bias estimation needs ~2 s of stillness");
    let mut tracker = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
    let (mut n, mut acc_sum, mut mag_sum) = (0u32, 0.0f64, 0.0f64);
    let start = Instant::now();
    let mut first_yaw = None;

    while start.elapsed() < Duration::from_secs(8) {
        match hmd.poll(Duration::from_millis(20)) {
            Ok(Some(HmdEvent::Imu(s))) => {
                n += 1;
                acc_sum += s.accel.length();
                mag_sum += s.mag.length();
                tracker.integrate(&s);
                if first_yaw.is_none() && tracker.is_bias_calibrated() {
                    first_yaw = Some(tracker.euler_degrees().yaw);
                    println!(
                        "  bias calibrated after {:.1} s: {:?}",
                        start.elapsed().as_secs_f64(),
                        tracker.gyro_bias()
                    );
                }
            }
            Ok(Some(HmdEvent::Disconnected)) => {
                eprintln!("  glasses disconnected");
                return;
            }
            Ok(Some(other)) => println!("  event: {other:?}"),
            Ok(None) => {}
            Err(e) => {
                eprintln!("  poll error: {e}");
                return;
            }
        }
    }

    if n == 0 {
        eprintln!("  NO SAMPLES — the stream never started");
        std::process::exit(1);
    }
    let secs = start.elapsed().as_secs_f64();
    let (acc, mag) = (acc_sum / n as f64, mag_sum / n as f64);
    println!("  samples {n} in {secs:.1} s = {:.0} Hz", n as f64 / secs);
    println!("  |accel| {acc:.3} g   (expect ~1.00)");
    println!("  |mag|   {mag:.3} G   (expect ~0.30)");
    println!("  bias    {:?} deg/s", tracker.gyro_bias());

    let e = tracker.euler_degrees();
    println!(
        "  attitude yaw {:.1} pitch {:.1} roll {:.1}",
        e.yaw, e.pitch, e.roll
    );
    let m = tracker.magnetic_status();
    println!(
        "  magnetic anchor: locked={} accepted={} err={:.2} deg failures={}",
        m.locked, m.accepted, m.error_deg, m.failures
    );
    if let Some(y0) = first_yaw {
        println!(
            "  yaw moved {:.2} deg since calibration",
            (e.yaw - y0).abs()
        );
    }

    let acc_ok = (0.9..1.1).contains(&acc);
    let mag_ok = (0.15..0.6).contains(&mag);
    println!(
        "  SANITY: accel {}  mag {}",
        if acc_ok { "PASS" } else { "FAIL" },
        if mag_ok { "PASS" } else { "FAIL" }
    );

    // --- display mode ---
    if test_stereo {
        println!("\n== stereo ==");
        println!(
            "  before: {}",
            dp_modes().first().cloned().unwrap_or_default()
        );
        match hmd.set_display_mode(DisplayMode::Stereo) {
            Ok(m) => println!("  set_display_mode -> {m:?} (acked)"),
            Err(e) => {
                eprintln!("  FAILED: {e}");
                return;
            }
        }
        // The DP link has to retrain and the kernel re-read the EDID before the new mode
        // shows up, which takes a moment.
        let mut appeared = false;
        for _ in 0..16 {
            std::thread::sleep(Duration::from_millis(500));
            if has_stereo_mode() {
                appeared = true;
                break;
            }
        }
        println!(
            "  DP-1 now: {}",
            dp_modes()
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!("  STEREO: {}", if appeared { "PASS" } else { "FAIL" });
        std::thread::sleep(Duration::from_secs(3));

        println!("\n== restoring mono ==");
        match hmd.set_display_mode(DisplayMode::Mono) {
            Ok(m) => println!("  set_display_mode -> {m:?}"),
            Err(e) => eprintln!("  restore failed: {e} — Drop will try again"),
        }
        std::thread::sleep(Duration::from_secs(2));
        println!(
            "  DP-1 now: {}",
            dp_modes().first().cloned().unwrap_or_default()
        );
    } else {
        println!("\n(skipping the display switch; pass `stereo` to include it)");
    }

    println!("\ndone");
}
