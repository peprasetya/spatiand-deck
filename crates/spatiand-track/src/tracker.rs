//! 3DoF orientation from the glasses' IMU.
//!
//! Ported from HoloFrame's `HeadTracker.swift`. **The tuning constants below are measured,
//! not chosen** — each one has a failure mode attached, recorded in the comments. Retune only
//! with a reason and a way to check.
//!
//! A complementary filter: integrate the gyro for responsiveness, and lean on gravity to stop
//! pitch and roll drifting. Yaw has no such reference and *will* drift, so the defences are
//! gyro-bias estimation while you sit still, a deadband on what survives it, a magnetic
//! anchor, and an explicit recentre.
//!
//! Bias estimation is the single highest-value part. These glasses read roughly
//! `(+0.6, −0.9, −0.7)` deg/s at rest — measured again on this hardware, and matching what
//! HoloFrame recorded. Left uncorrected that is over 45 degrees of yaw drift per minute.

use glam::{DQuat, DVec3};
use spatiand_hmd::ImuSample;

use crate::axis::AxisMap;

/// Euler angles in degrees, already recentred.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Euler {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct TrackerConfig {
    /// How strongly gravity pulls pitch/roll back per second. Higher tracks gravity faster
    /// but makes the view swim under linear acceleration.
    pub gravity_gain: f64,
    /// A sample only counts as still below this bias-corrected rate, in deg/s.
    ///
    /// Deliberately tight. A loose gate is worse than no gate: a slow deliberate head turn
    /// sits underneath it and gets averaged in as though it were zero-offset. That closes a
    /// feedback loop — the view drifts, you turn slowly to follow it, your turn is learned as
    /// bias, the drift grows. The symptom is drift that *worsens* the longer you sit still.
    pub stillness_rate_threshold: f64,
    /// Before any bias is known the corrected rate *is* the raw rate — around 1.2 deg/s on
    /// these glasses — so the first estimate needs a looser gate or it is never made.
    pub initial_stillness_threshold: f64,
    /// ...and only if the accelerometer is within this much of 1 g.
    pub stillness_accel_tolerance: f64,
    /// Samples of continuous stillness before the estimate is trusted (~2 s at 1 kHz). Long,
    /// because the quantity being measured is a few hundredths of a deg/s.
    pub samples_to_calibrate: u32,
    /// Rates below this, in deg/s, are faded toward zero before integration.
    ///
    /// Whatever bias survives estimation still integrates, and 0.05 deg/s left running for
    /// three minutes is nine degrees of world walking away from you. Scaling by
    /// `speed²/(speed² + deadband²)` leaves the rotation *axis* untouched and only ever
    /// touches the angle: 2% down at 3 deg/s, 0.2% at 10, while crushing what sits near zero.
    pub drift_deadband: f64,

    // --- magnetic anchor ---
    /// Samples averaged before the anchor is fixed (~10 s at 1 kHz).
    pub mag_samples_to_anchor: u32,
    pub mag_strength_tolerance: f64,
    pub mag_inclination_tolerance: f64,
    pub mag_deadzone: f64,
    /// Correction authority while still, deg/s. Small enough to be invisible.
    pub mag_slow_rate: f64,
    /// ...and while the head is moving, where a correction is hidden by the motion itself.
    pub mag_fast_rate: f64,
    pub mag_gain: f64,
    pub magnetic_anchor_enabled: bool,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            gravity_gain: 0.4,
            stillness_rate_threshold: 1.0,
            initial_stillness_threshold: 2.5,
            stillness_accel_tolerance: 0.08,
            samples_to_calibrate: 2000,
            drift_deadband: 0.4,
            mag_samples_to_anchor: 10_000,
            mag_strength_tolerance: 0.15,
            mag_inclination_tolerance: 10.0,
            mag_deadzone: 0.5,
            mag_slow_rate: 0.05,
            mag_fast_rate: 2.0,
            mag_gain: 0.1,
            magnetic_anchor_enabled: true,
        }
    }
}

/// What the magnetic anchor is doing. `locked == false` with `failures > 0` means the field
/// here was too incoherent to anchor to, and the tracker is flying on gyro and gravity alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MagneticStatus {
    pub locked: bool,
    pub accepted: bool,
    pub error_deg: f64,
    pub failures: u32,
}

pub struct HeadTracker {
    config: TrackerConfig,
    axes: AxisMap,

    /// Orientation of the head in the world frame.
    q: DQuat,
    gyro_bias: DVec3,
    bias_accumulator: DVec3,
    still_samples: u32,
    calibrated: bool,
    last_timestamp: u64,
    seeded: bool,

    /// Bias-corrected angular velocity in the body frame, rad/s. Kept so the renderer can
    /// predict forward and cancel the frame of latency between reading the pose and photons
    /// reaching the eye.
    angular_velocity: DVec3,
    /// Bias-corrected rate smoothed over ~100 ms. The stillness gate reads this rather than
    /// the instantaneous rate: at 1 kHz, noise alone trips a 1 deg/s threshold often enough
    /// that a two-second window of "continuous" stillness never completes.
    stillness_rate: DVec3,
    seconds_since_bias_update: f64,

    mag_reference: Option<DVec3>,
    mag_reference_strength: f64,
    mag_reference_inclination: f64,
    mag_accumulator: DVec3,
    mag_strength_accumulator: f64,
    mag_inclination_accumulator: f64,
    mag_samples: u32,
    mag_accepted: bool,
    mag_error: f64,
    mag_failures: u32,

    /// Seconds of continuous near-stillness. The glasses have no wear sensor, so this stands
    /// in for "taken off and put down".
    idle_accumulator: f64,

    // Subtracted from reported angles, so recentring never disturbs the filter itself.
    //
    // Pitch is included as well as yaw. Gravity fixes what "level" means in the world, but
    // not where the world should sit relative to your face: the glasses rest at an angle on
    // your nose, so looking comfortably straight ahead is several degrees off level. Roll is
    // deliberately NOT offset — you always want the horizon level with the real one.
    yaw_offset: f64,
    pitch_offset: f64,
}

impl HeadTracker {
    pub fn new(axes: AxisMap, config: TrackerConfig) -> Self {
        Self {
            config,
            axes,
            q: DQuat::IDENTITY,
            gyro_bias: DVec3::ZERO,
            bias_accumulator: DVec3::ZERO,
            still_samples: 0,
            calibrated: false,
            last_timestamp: 0,
            seeded: false,
            angular_velocity: DVec3::ZERO,
            stillness_rate: DVec3::ZERO,
            seconds_since_bias_update: 0.0,
            mag_reference: None,
            mag_reference_strength: 0.0,
            mag_reference_inclination: 0.0,
            mag_accumulator: DVec3::ZERO,
            mag_strength_accumulator: 0.0,
            mag_inclination_accumulator: 0.0,
            mag_samples: 0,
            mag_accepted: false,
            mag_error: 0.0,
            mag_failures: 0,
            idle_accumulator: 0.0,
            yaw_offset: 0.0,
            pitch_offset: 0.0,
        }
    }

    /// Adopt a freshly measured mapping without restarting. The old orientation was built in
    /// the wrong frame, so the filter is reseeded rather than carried over.
    pub fn set_axes(&mut self, axes: AxisMap) {
        let config = self.config;
        *self = Self::new(axes, config);
    }

    pub fn axes(&self) -> AxisMap {
        self.axes
    }

    /// Fold one IMU sample into the estimate. Called at ~1 kHz.
    pub fn integrate(&mut self, raw: &ImuSample) {
        // Into the canonical frame first. The accelerometer is remapped too — gravity
        // correction must live in the same frame as the rates or the filter fights itself.
        let gyro = self.axes.apply(raw.gyro);
        let accel = self.axes.apply(raw.accel);
        let mag = self.axes.apply(raw.mag);

        // Device timestamps are nanoseconds. Guard the first sample and any hiccup.
        let mut dt = 0.001;
        if self.last_timestamp != 0 && raw.timestamp_ns > self.last_timestamp {
            let candidate = (raw.timestamp_ns - self.last_timestamp) as f64 * 1e-9;
            if candidate > 0.0 && candidate <= 0.1 {
                dt = candidate;
            }
        }
        self.last_timestamp = raw.timestamp_ns;

        // Starting from identity means several seconds of visible swim on every launch while
        // gravity pulls the estimate into place. Snap straight to the measured attitude on
        // the first usable sample instead; the filter then only maintains it. Yaw is
        // arbitrary here, which is fine — it has no reference anyway.
        if !self.seeded && (accel.length() - 1.0).abs() < 0.2 {
            self.q = quat_from_to(accel.normalize(), DVec3::Z);
            self.seeded = true;
        }

        let corrected = gyro - self.gyro_bias; // deg/s
        self.stillness_rate = self.stillness_rate.lerp(corrected, 0.01);
        self.seconds_since_bias_update += dt;

        // Anything on a head shows constant small motion; a desk does not.
        if corrected.length() > 3.0 {
            self.idle_accumulator = 0.0;
        } else {
            self.idle_accumulator += dt;
        }

        self.update_bias(gyro, accel);

        // --- predict from the gyro ---
        let mut rate = corrected * (std::f64::consts::PI / 180.0);
        let speed = rate.length();
        let deadband = self.config.drift_deadband * (std::f64::consts::PI / 180.0);
        // The denominator can never be zero, so this needs no guard for a stationary head.
        rate *= (speed * speed) / (speed * speed + deadband * deadband);

        // Lightly smoothed, so prediction is driven by real motion rather than sensor noise.
        self.angular_velocity = self.angular_velocity.lerp(rate, 0.05);
        let angle = rate.length() * dt;
        if angle > 1e-9 {
            self.q = (self.q * DQuat::from_axis_angle(rate.normalize(), angle)).normalize();
        }

        self.update_magnetic_anchor(mag, dt);

        // --- correct pitch and roll against gravity ---
        // Only when the accelerometer is plausibly measuring gravity alone; during real head
        // movement it is measuring movement too and would drag the view around.
        if (accel.length() - 1.0).abs() >= 0.2 {
            return;
        }

        // An accelerometer at rest measures specific force, which points UP, not down.
        // Comparing it against world-down gives two anti-parallel vectors whose cross product
        // is ~0, so the correction quietly does nothing and pitch/roll drift.
        let measured_up = accel.normalize();
        // Where the filter currently thinks "up" is, expressed in the body frame.
        let expected_up = self.q.inverse() * DVec3::Z;
        // The correction is applied as q' = q * R, so R must rotate measured_up onto
        // expected_up — not the other way round. Getting it backwards drives the estimate
        // away from the measurement instead of toward it.
        let axis = measured_up.cross(expected_up);
        let sin_angle = axis.length();
        if sin_angle > 1e-9 {
            let correction = sin_angle.min(1.0).asin() * self.config.gravity_gain * dt;
            self.q = (self.q * DQuat::from_axis_angle(axis / sin_angle, correction)).normalize();
        }
    }

    /// Track the gyro's zero offset whenever the glasses are held still.
    fn update_bias(&mut self, gyro: DVec3, accel: DVec3) {
        // The gate is tight once a bias is known, but relaxes the longer it has been since a
        // window was accepted. A first estimate taken while the glasses were being lifted
        // onto your face can be wrong by more than the tight gate is wide, and the gate is
        // measured against that same estimate — so without this it locks the door on its own
        // correction and drifts forever.
        let staleness = (self.seconds_since_bias_update / 60.0).min(3.0);
        let gate = if self.calibrated {
            self.config.stillness_rate_threshold * (1.0 + staleness)
        } else {
            self.config.initial_stillness_threshold
        };

        let still = self.stillness_rate.length() < gate
            && (accel.length() - 1.0).abs() < self.config.stillness_accel_tolerance;
        if !still {
            self.still_samples = 0;
            self.bias_accumulator = DVec3::ZERO;
            return;
        }
        // Raw, not corrected: bias is an absolute zero-offset, not an adjustment to the
        // estimate we already hold.
        self.bias_accumulator += gyro;
        self.still_samples += 1;
        if self.still_samples < self.config.samples_to_calibrate {
            return;
        }

        let measured = self.bias_accumulator / self.still_samples as f64;
        // Ease toward the new estimate rather than snapping, so a marginal window cannot jolt
        // the view. Heavier than it looks: windows are two seconds and the gate keeps
        // deliberate motion out of them, so each one is worth leaning on.
        self.gyro_bias = if self.calibrated {
            self.gyro_bias.lerp(measured, 0.25)
        } else {
            measured
        };
        self.calibrated = true;
        self.still_samples = 0;
        self.bias_accumulator = DVec3::ZERO;
        self.seconds_since_bias_update = 0.0;
    }

    /// Nudge yaw back toward the magnetic anchor.
    ///
    /// This is not a compass and never asks where north is. It records where the field points
    /// in the WORLD frame at startup and treats that as the anchor. A field bent 40 degrees
    /// off true north by nearby electronics serves just as well, because the only thing
    /// required of it is to be the same field a minute later.
    fn update_magnetic_anchor(&mut self, mag: DVec3, dt: f64) {
        if !self.config.magnetic_anchor_enabled || !self.seeded {
            return;
        }
        let strength = mag.length();
        if strength <= 1e-9 {
            return;
        }

        let world = self.q * (mag / strength);
        let horizontal = (world.x * world.x + world.y * world.y).sqrt();
        // A near-vertical field carries almost no heading: the horizontal component is what
        // encodes yaw, and dividing by one this small amplifies noise without bound.
        if horizontal <= 0.15 {
            self.mag_accepted = false;
            return;
        }
        let inclination = (-world.z).atan2(horizontal);

        let Some(reference) = self.mag_reference else {
            // Wait for a trustworthy attitude before deciding what "the field" is — anchoring
            // to a pose the filter has not settled into bakes in that error.
            if !self.calibrated {
                return;
            }
            self.mag_accumulator += world;
            self.mag_strength_accumulator += strength;
            self.mag_inclination_accumulator += inclination;
            self.mag_samples += 1;
            if self.mag_samples < self.config.mag_samples_to_anchor {
                return;
            }

            let mean = self.mag_accumulator / self.mag_samples as f64;
            let coherence = mean.length();
            // Averaging unit vectors: a mean much shorter than the samples that formed it
            // means they disagreed about direction, so there is no single field here to
            // anchor to. Better to stay on gyro alone than to nail yaw to a fiction.
            if coherence > 0.9 {
                self.mag_reference = Some(mean / coherence);
                self.mag_reference_strength =
                    self.mag_strength_accumulator / self.mag_samples as f64;
                self.mag_reference_inclination =
                    self.mag_inclination_accumulator / self.mag_samples as f64;
            } else {
                self.mag_failures += 1;
            }
            self.mag_samples = 0;
            self.mag_accumulator = DVec3::ZERO;
            self.mag_strength_accumulator = 0.0;
            self.mag_inclination_accumulator = 0.0;
            return;
        };

        // Both gates compare against what was measured when the anchor was set, never against
        // textbook values for Earth's field. A permanently distorted but uniform field is
        // fine; a field that has CHANGED is not, and that is the real distinction.
        let strength_off =
            (strength - self.mag_reference_strength).abs() / self.mag_reference_strength;
        let inclination_off =
            (inclination - self.mag_reference_inclination).abs() * 180.0 / std::f64::consts::PI;
        if strength_off >= self.config.mag_strength_tolerance
            || inclination_off >= self.config.mag_inclination_tolerance
        {
            self.mag_accepted = false;
            return;
        }
        self.mag_accepted = true;

        let mut error = reference.y.atan2(reference.x) - world.y.atan2(world.x);
        while error > std::f64::consts::PI {
            error -= 2.0 * std::f64::consts::PI;
        }
        while error < -std::f64::consts::PI {
            error += 2.0 * std::f64::consts::PI;
        }
        self.mag_error = error * 180.0 / std::f64::consts::PI;
        if self.mag_error.abs() <= self.config.mag_deadzone {
            return;
        }

        // Correct faster while the head is moving. A degree per second of yaw correction is
        // invisible mid-turn and glaring when you are holding still on a line of text, so the
        // authority follows the motion that hides it — which also means ordinary use erases
        // accumulated error long before it becomes visible.
        let speed = self.angular_velocity.length() * 180.0 / std::f64::consts::PI;
        let limit = (self.config.mag_slow_rate
            + (self.config.mag_fast_rate - self.config.mag_slow_rate) * (speed / 20.0).min(1.0))
            * std::f64::consts::PI
            / 180.0;
        let step = (error * self.config.mag_gain * dt).clamp(-limit * dt, limit * dt);
        // Pre-multiplied: this is a rotation about the WORLD's vertical, not the head's.
        // Post-multiplying would tilt the horizon whenever you were not upright.
        self.q = (DQuat::from_axis_angle(DVec3::Z, step) * self.q).normalize();
    }

    // --- output ---

    /// Orientation in the world frame, without recentring applied.
    pub fn orientation(&self) -> DQuat {
        self.q
    }

    /// Orientation with recentring applied — what the renderer should use.
    pub fn recentred_orientation(&self) -> DQuat {
        self.apply_offsets(self.q)
    }

    fn apply_offsets(&self, q: DQuat) -> DQuat {
        // Yaw is a world-vertical rotation (pre-multiply); pitch is about the head's own
        // right axis (post-multiply). Mixing the two up tilts the horizon.
        let yaw = DQuat::from_axis_angle(DVec3::Z, -self.yaw_offset.to_radians());
        let pitch = DQuat::from_axis_angle(DVec3::Y, self.pitch_offset.to_radians());
        (yaw * q * pitch).normalize()
    }

    pub fn magnetic_status(&self) -> MagneticStatus {
        MagneticStatus {
            locked: self.mag_reference.is_some(),
            accepted: self.mag_accepted,
            error_deg: self.mag_error,
            failures: self.mag_failures,
        }
    }

    pub fn is_bias_calibrated(&self) -> bool {
        self.calibrated
    }

    pub fn gyro_bias(&self) -> DVec3 {
        self.gyro_bias
    }

    /// Seconds the glasses have been essentially motionless.
    pub fn idle_seconds(&self) -> f64 {
        self.idle_accumulator
    }

    /// False until the first usable sample has been folded in.
    pub fn has_samples(&self) -> bool {
        self.seeded
    }

    pub fn euler_degrees(&self) -> Euler {
        self.euler_of(self.q)
    }

    /// Where the head will be `seconds` from now, assuming it keeps turning at the current
    /// rate.
    ///
    /// There is roughly a frame between latching the pose and photons arriving, and over that
    /// gap a real head turn has moved on. Extrapolating closes most of it, which is what makes
    /// the world feel nailed to the room rather than dragged behind you. Capped so a fast
    /// flick cannot overshoot wildly — overshoot reads far worse than a little lag.
    pub fn predicted_orientation(&self, seconds: f64, max_degrees: f64) -> DQuat {
        let rate = self.angular_velocity;
        let speed = rate.length();
        if speed <= 1e-6 || seconds <= 0.0 {
            return self.apply_offsets(self.q);
        }
        let limit = max_degrees.to_radians();
        let angle = (speed * seconds).min(limit);
        let predicted = (self.q * DQuat::from_axis_angle(rate / speed, angle)).normalize();
        self.apply_offsets(predicted)
    }

    fn euler_of(&self, o: DQuat) -> Euler {
        let (w, x, y, z) = (o.w, o.x, o.y, o.z);
        // Frame is X forward, Y left, Z up (right-handed). By the right-hand rule that makes
        // rotation about +Y a pitch DOWN, so it is negated here to report the more natural
        // "positive means looking up".
        let yaw = (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z));
        let sin_pitch = 2.0 * (w * y - z * x);
        let pitch_down = if sin_pitch.abs() >= 1.0 {
            (std::f64::consts::FRAC_PI_2).copysign(sin_pitch)
        } else {
            sin_pitch.asin()
        };
        let roll = (2.0 * (w * x + y * z)).atan2(1.0 - 2.0 * (x * x + y * y));

        let to_deg = 180.0 / std::f64::consts::PI;
        // Wrap yaw into -180..180 so crossing the seam does not fling the view across.
        let mut relative_yaw = yaw * to_deg - self.yaw_offset;
        while relative_yaw > 180.0 {
            relative_yaw -= 360.0;
        }
        while relative_yaw < -180.0 {
            relative_yaw += 360.0;
        }
        Euler {
            yaw: relative_yaw,
            pitch: -pitch_down * to_deg - self.pitch_offset,
            roll: roll * to_deg,
        }
    }

    /// Make wherever you are looking now the centre of the world.
    ///
    /// Takes yaw and pitch, so this fixes both "the world is off to one side" and "the world
    /// sits too low". Roll is left alone — the horizon should stay level with the real one.
    ///
    /// Deliberately manual: doing it automatically would fight you the moment you wanted to
    /// hold your gaze off-centre, for example on a window at the edge of the view.
    pub fn recenter(&mut self) {
        self.yaw_offset = 0.0;
        self.pitch_offset = 0.0;
        let e = self.euler_of(self.q);
        self.yaw_offset = e.yaw;
        self.pitch_offset = e.pitch;
    }
}

/// Shortest-arc rotation taking `from` onto `to`. Both must be unit vectors.
fn quat_from_to(from: DVec3, to: DVec3) -> DQuat {
    let dot = from.dot(to).clamp(-1.0, 1.0);
    if dot > 1.0 - 1e-12 {
        return DQuat::IDENTITY;
    }
    if dot < -1.0 + 1e-12 {
        // Antiparallel: any perpendicular axis is a valid 180 degree rotation, but it must
        // actually be perpendicular, so pick whichever basis vector is least aligned.
        let axis = if from.x.abs() < 0.9 {
            DVec3::X
        } else {
            DVec3::Y
        };
        let perp = from.cross(axis).normalize();
        return DQuat::from_axis_angle(perp, std::f64::consts::PI);
    }
    let axis = from.cross(to);
    DQuat::from_xyzw(axis.x, axis.y, axis.z, 1.0 + dot).normalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t_ns: u64, gyro: DVec3, accel: DVec3, mag: DVec3) -> ImuSample {
        ImuSample {
            timestamp_ns: t_ns,
            gyro,
            accel,
            mag,
            temperature_c: None,
        }
    }

    /// A still headset: gravity up, a plausible field, and a constant gyro bias.
    fn still_stream(tracker: &mut HeadTracker, bias: DVec3, count: u64) {
        for i in 0..count {
            tracker.integrate(&sample(
                i * 1_000_000,
                bias,
                DVec3::new(0.0, 0.0, 1.0),
                DVec3::new(0.21, 0.0, -0.21),
            ));
        }
    }

    #[test]
    fn seeds_immediately_from_gravity() {
        let mut t = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
        assert!(!t.has_samples());
        t.integrate(&sample(
            0,
            DVec3::ZERO,
            DVec3::new(0.0, 0.0, 1.0),
            DVec3::ZERO,
        ));
        assert!(t.has_samples(), "must not need seconds of swim to settle");
    }

    #[test]
    fn learns_the_gyro_bias_and_stops_drifting() {
        // This is the highest-value behaviour in the whole crate: the real hardware reads
        // about (+0.6, -0.9, -0.7) deg/s at rest, which is 45+ deg/min of yaw if ignored.
        let bias = DVec3::new(0.64, -0.86, -0.73);
        let mut t = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
        still_stream(&mut t, bias, 3000);
        assert!(t.is_bias_calibrated(), "2 s of stillness should calibrate");
        let learned = t.gyro_bias();
        assert!(
            (learned - bias).length() < 0.05,
            "learned {learned:?} but the truth is {bias:?}"
        );
    }

    #[test]
    fn residual_bias_does_not_walk_the_world_away() {
        // The end-to-end property that matters. With bias estimation and the deadband, a
        // still headset must stay put over a long run.
        let bias = DVec3::new(0.64, -0.86, -0.73);
        let mut t = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
        still_stream(&mut t, bias, 3000);
        let before = t.euler_degrees().yaw;
        still_stream(&mut t, bias, 60_000); // a further minute at 1 kHz
        let drift = (t.euler_degrees().yaw - before).abs();
        assert!(drift < 1.0, "yaw drifted {drift} deg in a minute at rest");
    }

    #[test]
    fn integrates_a_real_turn_at_the_right_rate() {
        let mut t = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
        // Turning left at 30 deg/s about +Z for one second.
        for i in 0..1000u64 {
            t.integrate(&sample(
                i * 1_000_000,
                DVec3::new(0.0, 0.0, 30.0),
                DVec3::new(0.0, 0.0, 1.0),
                DVec3::ZERO,
            ));
        }
        let yaw = t.euler_degrees().yaw;
        assert!(
            (yaw - 30.0).abs() < 2.0,
            "expected ~30 deg of yaw, got {yaw}"
        );
    }

    #[test]
    fn gravity_pulls_pitch_back_rather_than_pushing_it_away() {
        // The sign trap: comparing specific force against world-DOWN makes the correction
        // quietly do nothing, and getting the inverse backwards drives the estimate away.
        let mut t = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
        t.integrate(&sample(
            0,
            DVec3::ZERO,
            DVec3::new(0.0, 0.0, 1.0),
            DVec3::ZERO,
        ));
        // Inject a false pitch by integrating a burst, then feed level gravity and check the
        // error shrinks rather than grows.
        for i in 1..200u64 {
            t.integrate(&sample(
                i * 1_000_000,
                DVec3::new(0.0, 50.0, 0.0),
                DVec3::new(0.0, 0.0, 1.0),
                DVec3::ZERO,
            ));
        }
        let tilted = t.euler_degrees().pitch.abs();
        for i in 200..20_000u64 {
            t.integrate(&sample(
                i * 1_000_000,
                DVec3::ZERO,
                DVec3::new(0.0, 0.0, 1.0),
                DVec3::ZERO,
            ));
        }
        let settled = t.euler_degrees().pitch.abs();
        assert!(
            settled < tilted * 0.5,
            "gravity should pull pitch back: {tilted} -> {settled}"
        );
    }

    #[test]
    fn recenter_zeroes_yaw_and_pitch_but_not_roll() {
        let mut t = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
        for i in 0..1000u64 {
            t.integrate(&sample(
                i * 1_000_000,
                DVec3::new(0.0, 0.0, 30.0),
                DVec3::new(0.0, 0.0, 1.0),
                DVec3::ZERO,
            ));
        }
        assert!(t.euler_degrees().yaw.abs() > 20.0);
        t.recenter();
        let e = t.euler_degrees();
        assert!(e.yaw.abs() < 1e-6, "yaw should be zeroed, got {}", e.yaw);
        assert!(
            e.pitch.abs() < 1e-6,
            "pitch should be zeroed, got {}",
            e.pitch
        );
    }

    #[test]
    fn prediction_is_capped_so_a_flick_cannot_overshoot() {
        let mut t = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
        for i in 0..1000u64 {
            t.integrate(&sample(
                i * 1_000_000,
                DVec3::new(0.0, 0.0, 400.0), // a violent flick
                DVec3::new(0.0, 0.0, 1.0),
                DVec3::ZERO,
            ));
        }
        let now = t.recentred_orientation();
        let ahead = t.predicted_orientation(0.5, 8.0); // absurd lookahead, tight cap
        let delta = now.inverse() * ahead;
        let angle = 2.0 * delta.w.abs().clamp(-1.0, 1.0).acos();
        assert!(
            angle.to_degrees() <= 8.5,
            "prediction exceeded its cap: {} deg",
            angle.to_degrees()
        );
    }

    #[test]
    fn zero_prediction_is_the_current_pose() {
        let mut t = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
        still_stream(&mut t, DVec3::ZERO, 100);
        let a = t.predicted_orientation(0.0, 8.0);
        let b = t.recentred_orientation();
        assert!((a.dot(b).abs() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn quat_from_to_handles_the_antiparallel_case() {
        // The degenerate case that produces NaN if the axis is not chosen carefully.
        let q = quat_from_to(DVec3::Z, -DVec3::Z);
        let rotated = q * DVec3::Z;
        assert!(
            (rotated + DVec3::Z).length() < 1e-9,
            "should rotate Z onto -Z, got {rotated:?}"
        );
        assert!(q.is_finite());
    }

    #[test]
    fn incoherent_field_is_refused_rather_than_anchored_to() {
        let cfg = TrackerConfig {
            // Shortened from 10 000 so the test does not have to simulate ten seconds.
            mag_samples_to_anchor: 200,
            ..TrackerConfig::default()
        };
        let mut t = HeadTracker::new(AxisMap::IDENTITY, cfg);
        still_stream(&mut t, DVec3::ZERO, 2100); // get calibrated first
                                                 // A field pointing somewhere different every sample averages to nothing.
        for i in 0..400u64 {
            let a = i as f64;
            t.integrate(&sample(
                (2100 + i) * 1_000_000,
                DVec3::ZERO,
                DVec3::new(0.0, 0.0, 1.0),
                DVec3::new(0.3 * a.cos(), 0.3 * a.sin(), 0.0),
            ));
        }
        let s = t.magnetic_status();
        assert!(
            !s.locked,
            "must not anchor to a field with no single direction"
        );
        assert!(
            s.failures > 0,
            "and it should say so rather than stay silent"
        );
    }
}

#[cfg(test)]
mod path_equivalence_tests {
    use super::*;
    use crate::axis::AxisMap;

    /// A stream of samples for a steady rotation about one sensor axis.
    fn stream(axis: usize, rate_dps: f64, seconds: f64) -> Vec<ImuSample> {
        let n = (seconds * 1000.0) as u64;
        (0..n)
            .map(|i| {
                let mut gyro = DVec3::ZERO;
                gyro[axis] = rate_dps;
                ImuSample {
                    timestamp_ns: i * 1_000_000,
                    gyro,
                    // Resting gravity on sensor +Z, the measured orientation of these glasses.
                    accel: DVec3::new(0.0, 0.0, 1.0),
                    mag: DVec3::ZERO,
                    temperature_c: None,
                }
            })
            .collect()
    }

    fn run(mut tracker: HeadTracker, samples: &[ImuSample]) -> Euler {
        for s in samples {
            tracker.integrate(s);
        }
        tracker.euler_degrees()
    }

    /// The measured map for the XREAL Air: yaw on sensor Z, pitch on X, roll on Y.
    fn measured() -> AxisMap {
        AxisMap {
            version: crate::axis::CURRENT_VERSION,
            yaw_axis: 2,
            yaw_sign: 1.0,
            pitch_axis: 0,
            pitch_sign: 1.0,
            roll_axis: 1,
            roll_sign: 1.0,
        }
    }

    #[test]
    fn adopting_a_map_matches_starting_with_it() {
        // The reported symptom: head tracking behaves correctly immediately after calibration
        // and is wrong on the next launch, with the same map in both cases. That can only be
        // true if these two paths disagree -- calibration reaches the tracker through
        // set_axes, a restart through new() -- so this pins them together.
        for axis in 0..3 {
            let samples = stream(axis, 30.0, 1.0);

            let adopted = {
                let mut t = HeadTracker::new(AxisMap::IDENTITY, TrackerConfig::default());
                // Some history first, as there would be during calibration itself.
                for s in stream(axis, 5.0, 0.2).iter() {
                    t.integrate(s);
                }
                t.set_axes(measured());
                run(t, &samples)
            };
            let fresh = run(
                HeadTracker::new(measured(), TrackerConfig::default()),
                &samples,
            );

            let close = |a: f64, b: f64| (a - b).abs() < 0.5;
            assert!(
                close(adopted.yaw, fresh.yaw)
                    && close(adopted.pitch, fresh.pitch)
                    && close(adopted.roll, fresh.roll),
                "sensor axis {axis}: adopted {adopted:?} but fresh {fresh:?}"
            );
        }
    }

    #[test]
    fn a_map_survives_being_written_and_read_back() {
        // The other half of the same question: the map that gets saved has to be the map that
        // comes back, field for field, or the restart uses something else entirely.
        let map = measured();
        let text = toml::to_string_pretty(&map).expect("serialises");
        let back: AxisMap = toml::from_str(&text).expect("parses");
        assert_eq!(map, back, "round trip changed the map:\n{text}");

        for axis in 0..3 {
            let samples = stream(axis, 30.0, 1.0);
            let before = run(HeadTracker::new(map, TrackerConfig::default()), &samples);
            let after = run(HeadTracker::new(back, TrackerConfig::default()), &samples);
            assert_eq!(
                (
                    before.yaw.round(),
                    before.pitch.round(),
                    before.roll.round()
                ),
                (after.yaw.round(), after.pitch.round(), after.roll.round()),
                "sensor axis {axis} behaves differently after a round trip"
            );
        }
    }
}
