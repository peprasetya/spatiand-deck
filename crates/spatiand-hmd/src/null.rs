//! A headset that isn't there.
//!
//! This exists for two reasons, and the second matters more than the first.
//!
//! The obvious one: development without hardware. The glasses spend a lot of time unplugged
//! (they share the Deck's only USB-C port with the charger), and the renderer and shell
//! should not be blocked on that.
//!
//! The real one: it is the second implementation of [`Hmd`]. A trait with one implementation
//! is not an abstraction, it is a rename — device assumptions leak into it and nobody notices
//! until the second backend is attempted months later, by which point the leak is load
//! bearing. Writing this alongside the XREAL driver is what keeps the seam honest.
//!
//! It emits synthetic *IMU* samples rather than a finished pose, so the full
//! `spatiand-track` pipeline runs unchanged. Set `SPATIAND_NULL_SPIN=<deg/s>` to have it turn
//! slowly on the spot, which is the cheapest way to see whether the world is actually
//! head-locked or merely drawn.

use std::time::{Duration, Instant};

use glam::DVec3;

use crate::{DisplayMode, Hmd, HmdEvent, HmdInfo, ImuSample, Result};

/// Rate the synthetic stream pretends to run at. Matches the XREAL Air closely enough that
/// filter tuning carries over.
const SAMPLE_HZ: f64 = 1000.0;

pub struct NullHmd {
    info: HmdInfo,
    mode: DisplayMode,
    started: Instant,
    last_emit: Instant,
    sample_index: u64,
    spin_deg_s: f64,
}

impl NullHmd {
    pub fn new() -> Self {
        let spin_deg_s = std::env::var("SPATIAND_NULL_SPIN")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        Self {
            info: HmdInfo {
                name: "Null headset (no hardware)".into(),
                // Deliberately the Air's geometry, so switching between null and real
                // hardware does not also change the projection and confuse a comparison.
                per_eye: (1920, 1080),
                h_fov_deg: 40.0,
                default_ipd_mm: 63.0,
                supports_stereo: true,
                provides_fused_pose: false,
                // The synthetic pose is generated directly in the head frame, so there is no
                // sensor mounting to describe and nothing for calibration to discover.
                sensor_axes: None,
            },
            mode: DisplayMode::Mono,
            started: Instant::now(),
            last_emit: Instant::now(),
            sample_index: 0,
            spin_deg_s,
        }
    }
}

impl Default for NullHmd {
    fn default() -> Self {
        Self::new()
    }
}

impl Hmd for NullHmd {
    fn info(&self) -> &HmdInfo {
        &self.info
    }

    fn set_display_mode(&mut self, mode: DisplayMode) -> Result<DisplayMode> {
        self.mode = mode;
        Ok(mode)
    }

    fn display_mode(&self) -> DisplayMode {
        self.mode
    }

    fn poll(&mut self, timeout: Duration) -> Result<Option<HmdEvent>> {
        let interval = Duration::from_secs_f64(1.0 / SAMPLE_HZ);
        let due = self.last_emit + interval;
        let now = Instant::now();
        if now < due {
            let wait = (due - now).min(timeout);
            std::thread::sleep(wait);
            if Instant::now() < due {
                return Ok(None);
            }
        }
        self.last_emit = Instant::now();
        self.sample_index += 1;

        // An accelerometer at rest measures specific force, which points *up*, not down —
        // the sign error that silently breaks gravity correction. In the canonical frame
        // (X forward, Y left, Z up) that is +Z.
        let accel = DVec3::new(0.0, 0.0, 1.0);
        // A plausible field: ~0.3 G, tilted well off horizontal like the real thing, so the
        // tracker's inclination gate sees something it would accept.
        let mag = DVec3::new(0.21, 0.0, -0.21);
        let gyro = DVec3::new(0.0, 0.0, self.spin_deg_s);

        Ok(Some(HmdEvent::Imu(ImuSample {
            timestamp_ns: (self.started.elapsed().as_secs_f64() * 1e9) as u64,
            gyro,
            accel,
            mag,
            temperature_c: Some(30.0),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_samples_the_tracker_would_accept() {
        let mut hmd = NullHmd::new();
        let mut got = None;
        // The stream is rate limited, so give it a few tries rather than demanding the
        // first poll produce something.
        for _ in 0..50 {
            if let Some(HmdEvent::Imu(s)) = hmd.poll(Duration::from_millis(5)).unwrap() {
                got = Some(s);
                break;
            }
        }
        let s = got.expect("null headset should produce samples");
        assert!(s.is_plausible());
        assert!(
            (s.accel.length() - 1.0).abs() < 1e-6,
            "must read 1 g at rest"
        );
        assert!((s.mag.length() - 0.3).abs() < 0.01, "must read ~0.3 G");
    }

    #[test]
    fn stereo_is_accepted_so_the_renderer_can_be_exercised() {
        let mut hmd = NullHmd::new();
        assert_eq!(
            hmd.set_display_mode(DisplayMode::Stereo).unwrap(),
            DisplayMode::Stereo
        );
        assert_eq!(hmd.display_mode(), DisplayMode::Stereo);
    }
}
