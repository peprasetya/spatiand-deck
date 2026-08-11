//! Taking the shake out without adding lag.
//!
//! Ported from HoloFrame's `ViewSmoothing.swift`.
//!
//! The view follows head pose one-to-one, so involuntary motion — breathing, pulse, the desk
//! being knocked — moves the image several pixels and reads as jitter. A fixed low-pass
//! filter removes it at the cost of lag on real movement, which is worse: a laggy view feels
//! detached from your head and is the classic route to discomfort.
//!
//! The One Euro filter (Casiez, Roussel & Vogel, CHI 2012) solves exactly this. Its cutoff
//! frequency rises with the speed of the signal, so it filters heavily when you are nearly
//! still — where jitter is all there is to see — and barely at all when you turn, where lag
//! would be felt and jitter would not be noticed.

/// Exponential smoothing with a cutoff that adapts to how fast the value is changing.
#[derive(Debug, Clone)]
pub struct OneEuroFilter {
    /// Cutoff in Hz while stationary. Lower means steadier but slower to start moving.
    pub min_cutoff: f64,
    /// How much the cutoff opens up with speed. Higher means less lag when turning.
    pub beta: f64,
    /// Cutoff for the speed estimate itself, so a noisy derivative cannot flap the cutoff.
    pub derivative_cutoff: f64,
    previous: Option<f64>,
    previous_derivative: f64,
}

impl OneEuroFilter {
    pub fn new(min_cutoff: f64, beta: f64) -> Self {
        Self {
            min_cutoff,
            beta,
            derivative_cutoff: 1.0,
            previous: None,
            previous_derivative: 0.0,
        }
    }

    fn alpha(cutoff: f64, dt: f64) -> f64 {
        let tau = 1.0 / (2.0 * std::f64::consts::PI * cutoff);
        1.0 / (1.0 + tau / dt)
    }

    pub fn filter(&mut self, value: f64, dt: f64) -> f64 {
        if dt <= 0.0 {
            return self.previous.unwrap_or(value);
        }
        let Some(last) = self.previous else {
            self.previous = Some(value);
            return value;
        };

        let rate = (value - last) / dt;
        let a_d = Self::alpha(self.derivative_cutoff, dt);
        let smoothed_rate = a_d * rate + (1.0 - a_d) * self.previous_derivative;
        self.previous_derivative = smoothed_rate;

        // The whole trick: open the cutoff in proportion to speed.
        let cutoff = self.min_cutoff + self.beta * smoothed_rate.abs();
        let a = Self::alpha(cutoff, dt);
        let smoothed = a * value + (1.0 - a) * last;
        self.previous = Some(smoothed);
        smoothed
    }

    pub fn reset(&mut self) {
        self.previous = None;
        self.previous_derivative = 0.0;
    }

    /// Speed of the filtered signal, in units per second.
    pub fn speed(&self) -> f64 {
        self.previous_derivative.abs()
    }
}

/// Three One Euro filters, with yaw unwrapped so crossing the ±180° seam does not register
/// as a 360°/frame lurch and blow the cutoff wide open.
#[derive(Debug, Clone)]
pub struct PoseSmoother {
    yaw: OneEuroFilter,
    pitch: OneEuroFilter,
    roll: OneEuroFilter,
    unwrapped_yaw: Option<f64>,
}

impl PoseSmoother {
    pub fn new(min_cutoff: f64, beta: f64) -> Self {
        Self {
            yaw: OneEuroFilter::new(min_cutoff, beta),
            pitch: OneEuroFilter::new(min_cutoff, beta),
            roll: OneEuroFilter::new(min_cutoff, beta),
            unwrapped_yaw: None,
        }
    }

    /// Highest angular speed across the three axes, degrees/second.
    pub fn speed(&self) -> f64 {
        self.yaw.speed().max(self.pitch.speed()).max(self.roll.speed())
    }

    /// Smooth one (yaw, pitch, roll) triple in degrees.
    pub fn filter(&mut self, yaw: f64, pitch: f64, roll: f64, dt: f64) -> (f64, f64, f64) {
        // Accumulate yaw continuously rather than filtering the wrapped value.
        let mut continuous = yaw;
        if let Some(last) = self.unwrapped_yaw {
            let mut delta = yaw - last.rem_euclid(360.0);
            while delta > 180.0 {
                delta -= 360.0;
            }
            while delta < -180.0 {
                delta += 360.0;
            }
            continuous = last + delta;
        }
        self.unwrapped_yaw = Some(continuous);

        (
            self.yaw.filter(continuous, dt),
            self.pitch.filter(pitch, dt),
            self.roll.filter(roll, dt),
        )
    }

    pub fn reset(&mut self) {
        self.yaw.reset();
        self.pitch.reset();
        self.roll.reset();
        self.unwrapped_yaw = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_passes_through_untouched() {
        let mut f = OneEuroFilter::new(1.0, 0.007);
        assert_eq!(f.filter(42.0, 0.014), 42.0);
    }

    #[test]
    fn jitter_at_rest_is_attenuated() {
        let mut f = OneEuroFilter::new(1.0, 0.007);
        let dt = 1.0 / 72.0;
        f.filter(0.0, dt);
        // Alternating noise around zero, the shape involuntary head tremor takes.
        let mut last = 0.0;
        for i in 0..200 {
            last = f.filter(if i % 2 == 0 { 0.5 } else { -0.5 }, dt);
        }
        assert!(
            last.abs() < 0.25,
            "noise should be more than halved, got {last}"
        );
    }

    #[test]
    fn a_real_turn_is_tracked_without_much_lag() {
        // The point of One Euro: fast motion must NOT be smoothed into mush.
        let mut f = OneEuroFilter::new(1.0, 0.007);
        let dt = 1.0 / 72.0;
        let mut out = 0.0;
        for i in 0..72 {
            out = f.filter(i as f64 * 2.0, dt); // 144 deg/s sweep
        }
        let truth = 71.0 * 2.0;
        assert!(
            (out - truth).abs() < 12.0,
            "expected to stay near {truth}, got {out}"
        );
    }

    #[test]
    fn yaw_seam_does_not_produce_a_lurch() {
        let mut s = PoseSmoother::new(1.0, 0.007);
        let dt = 1.0 / 72.0;
        s.filter(179.0, 0.0, 0.0, dt);
        // Crossing +180 to -179 is a 2 degree move, not a 358 degree one. If unwrapping is
        // broken the filter sees an enormous rate and opens its cutoff wide.
        let (yaw, _, _) = s.filter(-179.0, 0.0, 0.0, dt);
        assert!(
            (yaw - 181.0).abs() < 2.0,
            "unwrapped yaw should continue past 180, got {yaw}"
        );
        assert!(s.speed() < 500.0, "seam must not blow up the speed estimate");
    }

    #[test]
    fn zero_dt_is_survivable() {
        let mut f = OneEuroFilter::new(1.0, 0.007);
        f.filter(1.0, 0.014);
        assert_eq!(f.filter(2.0, 0.0), 1.0, "must not divide by zero");
    }
}
