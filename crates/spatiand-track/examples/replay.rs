//! Replay an IMU recording through the tracker and say what it would have done.
//!
//! ```text
//! replay <imu.bin> [--every 30]
//! ```
//!
//! The recording is what `SPATIAND_IMU_RECORD` writes (HoloFrame's format): ten little-endian
//! f64 per sample, raw. It is run under the XREAL Air's axis map, and every `--every` seconds
//! prints the yaw, the bias, how the magnetometer fit stands and its best arrangements.

use glam::DVec3;
use spatiand_hmd::ImuSample;
use spatiand_track::hard_iron::HardIronFit;
use spatiand_track::{AxisMap, HeadTracker, TrackerConfig};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("replay <imu.bin> [--every seconds]");
        std::process::exit(2);
    };
    let every: f64 = args
        .iter()
        .position(|a| a == "--every")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(30.0);
    let bytes = std::fs::read(path).expect("readable recording");
    let samples: Vec<ImuSample> = bytes
        .chunks_exact(80)
        .map(|c| {
            let f = |i: usize| f64::from_le_bytes(c[i * 8..i * 8 + 8].try_into().unwrap());
            ImuSample {
                timestamp_ns: f(0) as u64,
                gyro: DVec3::new(f(1), f(2), f(3)),
                accel: DVec3::new(f(4), f(5), f(6)),
                mag: DVec3::new(f(7), f(8), f(9)),
                temperature_c: None,
            }
        })
        .collect();
    let first = samples.first().map(|s| s.timestamp_ns).unwrap_or(0);
    let last = samples.last().map(|s| s.timestamp_ns).unwrap_or(0);
    println!(
        "{} samples over {:.1} s",
        samples.len(),
        (last - first) as f64 * 1e-9
    );

    let axes = AxisMap::XREAL_AIR;
    let mut tracker = HeadTracker::new(axes, TrackerConfig::default());
    let mut fit = HardIronFit::new();
    let mut next = every;
    let mut previous = first;
    let mut mag_min = DVec3::splat(f64::MAX);
    let mut mag_max = DVec3::splat(f64::MIN);
    for s in &samples {
        tracker.integrate(s);
        let dt = if s.timestamp_ns > previous {
            ((s.timestamp_ns - previous) as f64 * 1e-9).min(0.1)
        } else {
            0.001
        };
        previous = s.timestamp_ns;
        let mag = axes.apply(s.mag);
        mag_min = mag_min.min(mag);
        mag_max = mag_max.max(mag);
        if tracker.is_bias_calibrated() {
            let rate = (axes.apply(s.gyro) - tracker.gyro_bias()).length();
            fit.feed(tracker.orientation(), rate, mag, dt);
        }
        let t = (s.timestamp_ns - first) as f64 * 1e-9;
        if t >= next {
            next += every;
            let e = tracker.euler_degrees();
            let b = tracker.gyro_bias();
            println!(
                "\n{t:5.0} s  yaw {:+7.1} pitch {:+6.1} roll {:+6.1}  bias {:+.3} {:+.3} {:+.3}  |mag| {:.3}",
                e.yaw,
                e.pitch,
                e.roll,
                b.x,
                b.y,
                b.z,
                mag.length()
            );
            println!("       fit: {:?}", fit.state());
            if let Some((coverage, ranking)) = fit.ranking() {
                println!("       coverage {coverage:.4}");
                for (share, arrangement, field) in ranking.iter().take(4) {
                    println!(
                        "       {:>8}  {:5.1}% left  field {:.3} G  offset {:+.3} {:+.3} {:+.3}",
                        arrangement.axes_summary(),
                        share * 100.0,
                        field,
                        arrangement.offset.x,
                        arrangement.offset.y,
                        arrangement.offset.z
                    );
                }
            }
        }
    }
    println!(
        "\nraw magnetometer range (tracker frame): x {:+.3}..{:+.3}  y {:+.3}..{:+.3}  z {:+.3}..{:+.3}",
        mag_min.x, mag_max.x, mag_min.y, mag_max.y, mag_min.z, mag_max.z
    );
}
