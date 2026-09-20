//! What the head tracker has learned about the glasses' sensors, kept between sessions, and a
//! line in the log about how the magnetic anchor is doing.
//!
//! Two things are worth keeping. The gyro's resting offset: the first estimate of a session is
//! the fragile one, taken on a head that is never still, and this sensor's offset barely moves
//! from one day to the next. And the magnetometer's own field: without it the anchor does not
//! run at all, and measuring it takes a minute or two of looking around. See
//! `spatiand_track::hard_iron`.
//!
//! The log line is there because yaw drift is otherwise impossible to reason about after the
//! fact: whether the anchor was holding, still measuring, or refusing a field that made no
//! sense are three different problems that feel identical from inside the glasses.

use std::time::{Duration, Instant};

use spatiand_track::config::Remembered;
use spatiand_track::{FitState, HeadTracker};

const LOG_EVERY: Duration = Duration::from_secs(30);
const SAVE_EVERY: Duration = Duration::from_secs(60);

pub struct SensorMemory {
    device: Option<String>,
    saved: Remembered,
    last_log: Instant,
    last_save: Instant,
}

impl Default for SensorMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl SensorMemory {
    pub fn new() -> Self {
        Self {
            device: None,
            saved: Remembered::default(),
            last_log: Instant::now(),
            last_save: Instant::now(),
        }
    }

    /// Hand the tracker what was learned last time. Call after the axis map is settled: a new
    /// map resets the tracker, and what is remembered belongs to one map.
    pub fn restore(&mut self, device: &str, tracker: &mut HeadTracker) {
        let remembered = spatiand_track::config::load_sensors(device, &tracker.axes());
        if let Some(bias) = remembered.gyro_bias {
            tracker.preset_bias(bias);
            log::info!(
                "gyro bias {:+.3} {:+.3} {:+.3} deg/s, from last time",
                bias.x,
                bias.y,
                bias.z
            );
        }
        match remembered.hard_iron {
            Some(fit) => {
                tracker.set_hard_iron(Some(fit));
                log::info!(
                    "magnetometer: axes {}, own field {:.3} G, from last time — the yaw anchor \
                     runs",
                    fit.axes_summary(),
                    fit.offset.length()
                );
            }
            None => log::info!(
                "magnetometer: its own field is not measured yet, so the yaw anchor waits; \
                 look around, up and down as well as sideways, for a minute or two"
            ),
        }
        self.device = Some(device.to_string());
        self.saved = remembered;
    }

    /// Once a frame. Logs now and then, and saves what changed at most once a minute.
    pub fn tick(&mut self, tracker: &HeadTracker) {
        if !tracker.has_samples() {
            return;
        }
        if self.last_log.elapsed() >= LOG_EVERY {
            self.last_log = Instant::now();
            log::info!("{}", describe(tracker));
        }
        let Some(device) = self.device.as_deref() else {
            return;
        };
        if self.last_save.elapsed() < SAVE_EVERY {
            return;
        }
        self.last_save = Instant::now();
        let now = Remembered {
            gyro_bias: tracker.desk_bias().or(self.saved.gyro_bias),
            hard_iron: tracker.hard_iron().or(self.saved.hard_iron),
        };
        if now == self.saved {
            return;
        }
        if now.hard_iron != self.saved.hard_iron {
            if let Some(fit) = now.hard_iron {
                log::info!(
                    "magnetometer measured: axes {}, own field {:+.3} {:+.3} {:+.3} G — the yaw \
                     anchor runs from now on",
                    fit.axes_summary(),
                    fit.offset.x,
                    fit.offset.y,
                    fit.offset.z
                );
            }
        }
        match spatiand_track::config::save_sensors(device, &tracker.axes(), &now) {
            Ok(()) => self.saved = now,
            Err(e) => log::warn!("could not remember the sensor calibration: {e}"),
        }
    }
}

/// One line: the bias, whether the magnetometer is measured, and what the anchor is doing.
fn describe(tracker: &HeadTracker) -> String {
    let bias = tracker.gyro_bias();
    let status = tracker.magnetic_status();
    let fit = match status.fit {
        FitState::Gathering { seconds } => format!("measuring ({seconds:.0} s of samples)"),
        FitState::NarrowView { coverage } => {
            format!("waiting for the head to look up and down too (coverage {coverage:.3})")
        }
        FitState::NoFit { residual_share } => format!(
            "the field makes no sense here ({:.0}% left over) — something magnetic nearby?",
            residual_share * 100.0
        ),
        FitState::Undecided => "cannot yet tell how its axes sit".into(),
        FitState::Fitted {
            fit,
            field,
            residual_share,
            coverage,
        } => format!(
            "fitted: axes {}, own field {:.3} G, Earth {:.3} G, {:.0}% left over, coverage \
             {coverage:.3}",
            fit.axes_summary(),
            fit.offset.length(),
            field,
            residual_share * 100.0
        ),
    };
    let anchor = if status.hard_iron.is_none() {
        "off until the magnetometer is measured".to_string()
    } else if !status.locked {
        "learning where the field points".to_string()
    } else if status.accepted {
        format!("holding, {:+.1} deg off", status.error_deg)
    } else {
        "paused: the field here differs from where it was learned".to_string()
    };
    // The anchor's trim is said separately from the bias it sits on top of, because they are
    // learned from different things and only one of them is ever remembered. A trim that has
    // pinned itself at its limit is the anchor fighting a reference it should not believe.
    let trim = tracker.mag_trim();
    let trim = if trim.length() < 1e-4 {
        String::new()
    } else {
        format!(
            " (+{:+.3} {:+.3} {:+.3} from the anchor)",
            trim.x, trim.y, trim.z
        )
    };
    format!(
        "tracker: bias {:+.3} {:+.3} {:+.3} deg/s{trim}; magnetometer {fit}; yaw anchor {anchor}",
        bias.x, bias.y, bias.z
    )
}
