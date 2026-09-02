//! Axis calibration, shown in the world rather than in a terminal.
//!
//! HoloFrame's note is the requirement: *"nobody should have to open a terminal to make the
//! thing work."* The measurement itself lives in `spatiand_track::calibration`; this is the
//! part that decides what the wearer sees and when, driven by the render loop.
//!
//! Prompts are **head-locked** — they follow your head rather than staying put in the world.
//! That is the opposite of what every other surface in Spatiand does, and it is deliberate:
//! the whole point of this flow is that the world frame is not yet trustworthy, so anything
//! world-locked would swim around exactly when it needs to be readable.

use std::time::{Duration, Instant};

use spatiand_hmd::ImuSample;
use spatiand_track::calibration::{self, Phase, PhaseCollector, MINIMUM_DEGREES};
use spatiand_track::AxisMap;

const COUNTDOWN: Duration = Duration::from_secs(3);
const COLLECT: Duration = Duration::from_secs(4);
const CONFIRM: Duration = Duration::from_millis(1200);
const MAX_ATTEMPTS: u32 = 4;
/// Angular rate that counts as "shake to begin", deg/s. Well above anything a head does
/// while merely being still, so it cannot trigger by accident.
const SHAKE_THRESHOLD: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Waiting for the wearer to shake their head.
    WaitingForShake,
    Countdown,
    Collecting,
    /// The phase was measured; showing "good" briefly.
    Accepted,
    /// The movement was too small; ask again.
    TooSmall,
    Done,
    Failed,
    /// Abandoned by the wearer.
    Cancelled,
}

/// What the renderer should put in front of the wearer this frame.
pub struct Prompt {
    pub heading: String,
    pub body: String,
    pub status: String,
}

pub struct Calibration {
    stage: Stage,
    phase_index: usize,
    attempt: u32,
    since: Instant,
    collector: PhaseCollector,
    measured: Vec<(usize, f64)>,
    result: Option<AxisMap>,
    error: Option<String>,
}

impl Calibration {
    pub fn new() -> Self {
        Self {
            stage: Stage::WaitingForShake,
            phase_index: 0,
            attempt: 1,
            since: Instant::now(),
            collector: PhaseCollector::new(),
            measured: Vec::new(),
            result: None,
            error: None,
        }
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Abandon the run, keeping whatever mapping was already stored.
    ///
    /// There has to be a way out. The flow waits for a shake to begin and then runs on its own
    /// timers, so someone who starts it by accident -- or whose glasses will not produce a
    /// clean measurement -- was previously stuck in it with no exit at all, which is worse
    /// than never offering calibration.
    pub fn cancel(&mut self) {
        log::info!("calibration cancelled; the stored axes are unchanged");
        self.result = None;
        self.enter(Stage::Cancelled);
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.stage, Stage::Done | Stage::Failed | Stage::Cancelled)
    }

    pub fn result(&self) -> Option<AxisMap> {
        self.result
    }

    fn phase(&self) -> Phase {
        Phase::ALL[self.phase_index.min(Phase::ALL.len() - 1)]
    }

    fn enter(&mut self, stage: Stage) {
        self.stage = stage;
        self.since = Instant::now();
    }

    /// Fold in one raw IMU sample. Must be the **un-remapped** sample: calibration is what
    /// discovers the mapping, so it has to see the sensor's own axes.
    pub fn feed(&mut self, sample: &ImuSample) {
        match self.stage {
            Stage::WaitingForShake => {
                if sample.gyro.length() > SHAKE_THRESHOLD {
                    self.enter(Stage::Countdown);
                }
            }
            Stage::Collecting => self.collector.feed(sample),
            _ => {}
        }
    }

    /// Advance timers. Call once per frame, after `feed`.
    pub fn tick(&mut self) {
        let elapsed = self.since.elapsed();
        match self.stage {
            Stage::Countdown if elapsed >= COUNTDOWN => {
                self.collector = PhaseCollector::new();
                self.enter(Stage::Collecting);
            }
            Stage::Collecting if elapsed >= COLLECT => {
                if let Some(measurement) = self.collector.accepted() {
                    self.measured.push(measurement);
                    self.enter(Stage::Accepted);
                } else if self.attempt >= MAX_ATTEMPTS {
                    self.error = Some(format!(
                        "Could not measure \"{}\". Nothing was saved.",
                        self.phase().heading()
                    ));
                    self.enter(Stage::Failed);
                } else {
                    // Retry rather than abort: the usual cause is simply not having started
                    // to move yet, and discarding good phases for that is needless.
                    self.attempt += 1;
                    self.enter(Stage::TooSmall);
                }
            }
            Stage::Accepted if elapsed >= CONFIRM => {
                self.phase_index += 1;
                self.attempt = 1;
                if self.phase_index >= Phase::ALL.len() {
                    self.finish();
                } else {
                    self.enter(Stage::Countdown);
                }
            }
            Stage::TooSmall if elapsed >= CONFIRM => self.enter(Stage::Countdown),
            _ => {}
        }
    }

    fn finish(&mut self) {
        // The raw measurements, before anything interprets them.
        //
        // Pitch and roll have come out exchanged since the beginning of this project, and
        // every attempt to reason it out from the conventions has been wrong, including
        // several of mine. The three numbers below are the ground truth: which sensor axis
        // dominated during each prompted motion, and by how much. With them, and with the
        // variant the wearer ends up choosing, the correct mapping is arithmetic rather than
        // argument -- and the argument has cost more than the measurement ever would.
        for (phase, (axis, degrees)) in Phase::ALL.iter().zip(self.measured.iter()) {
            log::info!(
                "calibration measured {:?} (\"{}\") -> sensor axis {axis}, {degrees:+.1} deg",
                phase,
                phase.heading()
            );
        }
        match calibration::build(&self.measured) {
            Ok(map) => {
                log::info!("calibration built: {}", map.summary());
                self.result = Some(map);
                match spatiand_track::config::save_axes(&map) {
                    Ok(path) => {
                        log::info!("calibration saved to {}: {}", path.display(), map.summary())
                    }
                    Err(e) => log::warn!("calibrated but could not save: {e}"),
                }
                self.enter(Stage::Done);
            }
            Err(e) => {
                // A mirrored or degenerate result is refused rather than stored: saving it
                // makes the filter fight itself in ways that look like drift, not like a bad
                // calibration.
                self.error = Some(format!("{e}"));
                self.enter(Stage::Failed);
            }
        }
    }

    /// What to draw this frame.
    pub fn prompt(&self) -> Prompt {
        let step = format!("{} of {}", self.phase_index + 1, Phase::ALL.len());
        match self.stage {
            Stage::WaitingForShake => Prompt {
                heading: "Set up head tracking".into(),
                body: "Put the glasses on,\nthen shake your head to begin.".into(),
                status: "B to cancel".into(),
            },
            Stage::Countdown => {
                let left = COUNTDOWN.saturating_sub(self.since.elapsed()).as_secs() + 1;
                Prompt {
                    heading: self.phase().heading().into(),
                    body: self.phase().instruction().into(),
                    status: format!("{step}  ·  get ready {}  ·  B to cancel", left.min(3)),
                }
            }
            Stage::Collecting => Prompt {
                heading: self.phase().heading().into(),
                body: self.phase().instruction().into(),
                status: format!("{step}  ·  MOVE NOW  ·  B to cancel"),
            },
            Stage::Accepted => Prompt {
                heading: self.phase().heading().into(),
                body: "Good.".into(),
                status: format!("{step}   ·   done"),
            },
            Stage::TooSmall => Prompt {
                heading: "Bigger movement".into(),
                body: self.phase().instruction().into(),
                status: format!("needs about {MINIMUM_DEGREES:.0} degrees — again"),
            },
            Stage::Done => Prompt {
                heading: "All set".into(),
                body: "Head tracking is calibrated.".into(),
                status: self.result.map(|m| m.summary()).unwrap_or_default(),
            },
            Stage::Cancelled => Prompt {
                heading: "Calibration cancelled".into(),
                body: "Nothing was changed.".into(),
                status: "B again to close".into(),
            },
            Stage::Failed => Prompt {
                heading: "Didn't work".into(),
                body: self
                    .error
                    .clone()
                    .unwrap_or_else(|| "Try again, keeping each movement clean.".into()),
                status: "nothing was saved".into(),
            },
        }
    }
}

impl Default for Calibration {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn sample(t_ns: u64, gyro: DVec3) -> ImuSample {
        ImuSample {
            timestamp_ns: t_ns,
            gyro,
            accel: DVec3::new(0.0, 0.0, 1.0),
            mag: DVec3::ZERO,
            temperature_c: None,
        }
    }

    #[test]
    fn stillness_does_not_start_it() {
        let mut c = Calibration::new();
        for i in 0..500u64 {
            c.feed(&sample(i * 1_000_000, DVec3::new(0.1, -0.2, 0.05)));
            c.tick();
        }
        assert_eq!(
            c.stage(),
            Stage::WaitingForShake,
            "resting gyro noise must not be mistaken for a shake"
        );
    }

    #[test]
    fn a_shake_begins_the_sequence() {
        let mut c = Calibration::new();
        c.feed(&sample(0, DVec3::new(0.0, 0.0, 120.0)));
        assert_eq!(c.stage(), Stage::Countdown);
    }

    #[test]
    fn prompts_exist_for_every_stage() {
        // A stage with an empty prompt shows the wearer a blank world and looks like a hang.
        for stage in [
            Stage::WaitingForShake,
            Stage::Countdown,
            Stage::Collecting,
            Stage::Accepted,
            Stage::TooSmall,
            Stage::Done,
            Stage::Failed,
            Stage::Cancelled,
        ] {
            let mut c = Calibration::new();
            c.stage = stage;
            let p = c.prompt();
            assert!(!p.heading.is_empty(), "{stage:?} had no heading");
            assert!(!p.body.is_empty(), "{stage:?} had no body");
        }
    }

    #[test]
    fn samples_are_only_collected_during_the_move_window() {
        let mut c = Calibration::new();
        c.feed(&sample(0, DVec3::new(0.0, 0.0, 120.0))); // shake -> countdown
                                                         // Movement during the countdown must not count, or an eager wearer poisons the
                                                         // measurement before it has started.
        for i in 1..100u64 {
            c.feed(&sample(i * 1_000_000, DVec3::new(0.0, 0.0, 50.0)));
        }
        assert_eq!(c.collector.samples(), 0);
    }

    #[test]
    fn there_is_always_a_way_out() {
        // From every stage, cancelling must finish the flow and change nothing. Before this
        // existed the only escape from a calibration you did not mean to start was killing
        // the session.
        for stage in [
            Stage::WaitingForShake,
            Stage::Countdown,
            Stage::Collecting,
            Stage::Accepted,
            Stage::TooSmall,
        ] {
            let mut c = Calibration::new();
            c.stage = stage;
            c.cancel();
            assert!(c.is_finished(), "{stage:?} could not be cancelled");
            assert!(c.result().is_none(), "{stage:?} left a result behind");
        }
    }

    #[test]
    fn every_prompt_before_the_end_says_how_to_get_out() {
        // In an otherwise empty world there is no other clue that B does anything.
        for stage in [Stage::WaitingForShake, Stage::Countdown, Stage::Collecting] {
            let mut c = Calibration::new();
            c.stage = stage;
            let p = c.prompt();
            assert!(
                p.status.contains('B') || p.body.contains('B'),
                "{stage:?} never mentions the way out: {p:?}",
                p = p.status
            );
        }
    }

    #[test]
    fn a_completed_run_yields_a_right_handed_map() {
        let mut c = Calibration::new();
        c.stage = Stage::Collecting;
        // Feed the three ideal motions straight into the builder path.
        c.measured = vec![(2, 60.0), (1, 45.0), (0, -30.0)];
        c.phase_index = Phase::ALL.len();
        c.finish();
        assert_eq!(c.stage(), Stage::Done);
        let map = c.result().expect("should have produced a map");
        assert!(map.is_right_handed() && map.is_usable());
    }

    #[test]
    fn a_mirrored_run_fails_loudly_instead_of_saving() {
        let mut c = Calibration::new();
        c.measured = vec![(2, -60.0), (1, 45.0), (0, -30.0)];
        c.finish();
        assert_eq!(c.stage(), Stage::Failed);
        assert!(c.result().is_none(), "a mirrored map must never be stored");
        assert!(c.prompt().body.len() > 4);
    }
}
