//! A head worked out from first principles, for when there is no measured one.
//!
//! This is the fallback, and it is deliberately honest about what it can and cannot do. Two
//! of the three cues that tell you where a sound is come straight out of the geometry of
//! having a head: sound reaches the near ear sooner, and the head shadows the far one. Both
//! are computed here and both are convincing.
//!
//! The third cue is not geometry. Front and behind produce almost the same arrival time and
//! almost the same level — that is why you turn your head to find a siren — and what
//! distinguishes them is the shape your own outer ear folds into the sound. There is no
//! formula for that; it has to be measured, which is what [`crate::hrtf`] is for. What is done
//! here instead is the one part of it that generalises: an ear points forwards, so it shades
//! the high frequencies of anything behind you. That is a real cue and it is a weak one, so a
//! sound behind will read as "behind and duller" rather than convincingly behind.
//!
//! Elevation gets nothing at all, for the same reason and with no weak version available.
//! A sound above is rendered as a sound level with the ears, and saying so here is better
//! than a mystery in a listening test.
//!
//! Everything comes out as an [`Ears`] exactly as the measured path does, so nothing
//! downstream knows or cares which one is in use.

use glam::DVec3;

use crate::ears::Ears;

/// Radius of a head, metres. The usual anthropometric average, and the only body measurement
/// this needs.
const HEAD_RADIUS: f64 = 0.0875;

/// Speed of sound, metres per second, at about room temperature.
const SPEED_OF_SOUND: f64 = 343.0;

/// How much the head shadows the far ear, as a fraction of level lost when a sound is fully to
/// one side.
///
/// Broadband, which is a simplification: a real head shadows high frequencies far more than
/// low ones. The frequency-dependent part is carried by [`SHADOW_DULLING`] instead, and
/// between them they land near the 4 to 6 dB a real head produces across the spectrum.
const SHADOW_LEVEL: f64 = 0.4;

/// How much duller the far ear sounds when a sound is fully to one side, 0 for no change and
/// 0.5 for as dull as a two-tap filter can be.
const SHADOW_DULLING: f64 = 0.35;

/// How much duller a sound directly behind is than the same sound in front.
///
/// The outer ear faces forwards and shades what is behind it. Small on purpose — this is the
/// weak version of a cue that needs measurement, and overdoing it makes everything behind
/// sound like it is underwater rather than behind.
const REAR_DULLING: f64 = 0.18;

/// Where the sound is, worked out rather than measured.
#[derive(Debug, Clone, Copy)]
pub struct Panner {
    rate: u32,
}

impl Panner {
    pub fn new(rate: u32) -> Panner {
        Panner { rate }
    }

    /// The largest gap the geometry can put between the two ears, in samples.
    fn max_delay_samples(&self) -> usize {
        // Woodworth, at the extreme where the sound is directly to one side.
        let seconds = HEAD_RADIUS / SPEED_OF_SOUND * (std::f64::consts::FRAC_PI_2 + 1.0);
        (seconds * self.rate as f64).ceil() as usize
    }
}

impl crate::Spatialise for Panner {
    fn ears_into(&self, direction: DVec3, out: &mut Ears) {
        let d = direction.normalize_or_zero();
        // How far round to one side, -1 fully right to +1 fully left.
        let lateral = d.y.clamp(-1.0, 1.0);
        // How far behind, -1 directly behind to +1 directly ahead.
        let frontal = d.x.clamp(-1.0, 1.0);

        // Woodworth's approximation: the extra distance round a sphere to the far ear.
        let theta = lateral.asin();
        let itd = HEAD_RADIUS / SPEED_OF_SOUND * (theta + theta.sin());
        let to_samples = |seconds: f64| (seconds * self.rate as f64).round().max(0.0) as usize;
        // Positive `itd` means the sound is to the left, so the right ear waits.
        let (delay_l, delay_r) = if itd >= 0.0 {
            (0, to_samples(itd))
        } else {
            (to_samples(-itd), 0)
        };

        // The far ear is quieter and duller; how far each ear is from the sound decides which.
        let shadow = |away: f64| 1.0 - SHADOW_LEVEL * away.max(0.0);
        let (gain_l, gain_r) = (shadow(-lateral), shadow(lateral));

        // Behind dulls both ears equally; the head shadow dulls only the far one.
        let behind = ((1.0 - frontal) / 2.0) * REAR_DULLING;
        let dull_l = (behind + SHADOW_DULLING * (-lateral).max(0.0)).clamp(0.0, 0.5);
        let dull_r = (behind + SHADOW_DULLING * lateral.max(0.0)).clamp(0.0, 0.5);

        // Constant power as the sound goes round, so nothing swells or dips just from moving.
        // Without this a sound crossing the front would be heard to change loudness, which
        // reads as a fault in the audio rather than as movement.
        //
        // The dulling has to be counted here, not just the gain. A two-tap filter that leaves
        // the level alone at DC still carries away energy across the band -- that is what a
        // lowpass is -- so an ear that is both shadowed and dulled loses twice while the
        // arithmetic only saw one of them. Left uncorrected it cost nearly two decibels
        // between a sound in front and the same sound to one side, which is heard as the
        // sound dipping as it passes.
        let amplitude = |gain: f64, dull: f64| gain * two_tap_energy(dull);
        let power = (amplitude(gain_l, dull_l).powi(2) + amplitude(gain_r, dull_r).powi(2)).sqrt();
        let norm = if power > 1e-9 {
            std::f64::consts::SQRT_2 / power
        } else {
            0.0
        };

        let taps = <Self as crate::Spatialise>::taps(self);
        out.blank(taps);
        two_tap(&mut out.left, delay_l, gain_l * norm, dull_l);
        two_tap(&mut out.right, delay_r, gain_r * norm, dull_r);
    }

    fn taps(&self) -> usize {
        // The widest the ears can be apart, plus the two taps of the dulling filter.
        self.max_delay_samples() + 2
    }
}

/// How much of a signal a two-tap filter with this much dulling passes, as a fraction.
///
/// Its taps are `1 - dull` and `dull`, so for anything but a pure tone the energy it passes is
/// the root of the sum of their squares. At full dulling that is 0.707, which is a whole
/// decibel and a half -- far too much to leave out of a level calculation.
fn two_tap_energy(dull: f64) -> f64 {
    ((1.0 - dull) * (1.0 - dull) + dull * dull).sqrt()
}

/// A filter that waits, then applies a gain and as much dulling as asked for.
///
/// The two taps sum to one before the gain, so `dull` changes the tone without changing the
/// level -- otherwise turning the treble down would also turn the sound down, and a sound
/// moving behind you would seem to recede rather than to pass.
fn two_tap(v: &mut [f32], delay: usize, gain: f64, dull: f64) {
    let taps = v.len();
    let g = gain as f32;
    let a = dull as f32;
    if delay < taps {
        v[delay] = g * (1.0 - a);
    }
    if delay + 1 < taps {
        v[delay + 1] = g * a;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Spatialise;

    const RATE: u32 = 48_000;

    fn ears(x: f64, y: f64, z: f64) -> Ears {
        Panner::new(RATE).ears(DVec3::new(x, y, z))
    }

    /// The offset of the first tap that carries anything, which is when the sound arrives.
    fn arrival(v: &[f32]) -> usize {
        v.iter().position(|s| s.abs() > 1e-9).expect("silent ear")
    }

    /// How much of the filter survives at the top of the audible range, relative to the bottom.
    ///
    /// A filter's response at Nyquist is its taps summed with alternating sign, and at DC it
    /// is simply their sum -- so this is "how much treble, per unit of bass", which is exactly
    /// what dulling reduces.
    fn brightness(v: &[f32]) -> f32 {
        let dc: f32 = v.iter().sum();
        let nyquist: f32 = v
            .iter()
            .enumerate()
            .map(|(i, s)| if i % 2 == 0 { *s } else { -*s })
            .sum();
        (nyquist / dc).abs()
    }

    #[test]
    fn a_sound_on_the_left_is_louder_in_the_left_ear() {
        let e = ears(0.0, 1.0, 0.0);
        let (l, r) = e.energy();
        assert!(l > r * 1.3, "left {l}, right {r}");
    }

    #[test]
    fn a_sound_on_the_left_reaches_the_left_ear_first() {
        // The cue that does most of the work below a kilohertz, and the one a plain
        // amplitude pan leaves out entirely.
        let e = ears(0.0, 1.0, 0.0);
        assert_eq!(arrival(&e.left), 0);
        let gap = arrival(&e.right);
        // Woodworth at full lateral: about 655 microseconds, which is 31 samples at 48 kHz.
        assert!(
            (30..=33).contains(&gap),
            "the ears were {gap} samples apart"
        );
    }

    #[test]
    fn a_sound_ahead_arrives_at_both_ears_at_once_and_alike() {
        let e = ears(1.0, 0.0, 0.0);
        assert_eq!(arrival(&e.left), arrival(&e.right));
        let (l, r) = e.energy();
        assert!((l - r).abs() < 1e-6, "left {l}, right {r}");
    }

    #[test]
    fn a_sound_behind_is_duller_than_the_same_sound_in_front() {
        // The weak version of the one cue that really needs measuring. It should exist, and
        // it should be small -- large enough to read as a difference, not so large that
        // everything behind sounds broken.
        let front = brightness(&ears(1.0, 0.0, 0.0).left);
        let back = brightness(&ears(-1.0, 0.0, 0.0).left);
        assert!(back < front, "behind was not duller: {back} vs {front}");
        assert!(
            back > front * 0.5,
            "behind was muffled, not shaded: {back} vs {front}"
        );
    }

    #[test]
    fn the_far_ear_is_duller_than_the_near_one() {
        let e = ears(0.0, 1.0, 0.0);
        assert!(
            brightness(&e.right) < brightness(&e.left),
            "the shadowed ear kept its treble"
        );
    }

    #[test]
    fn nothing_swells_or_dips_as_it_goes_round() {
        // A sound circling the listener must not be heard to change loudness: that reads as a
        // fault in the audio rather than as movement. This is what the constant-power
        // normalisation is for, and it is the kind of thing that is obvious in a listening
        // test and invisible in code review.
        let mut powers = Vec::new();
        for step in 0..72 {
            let angle = (step as f64) * std::f64::consts::TAU / 72.0;
            let e = ears(angle.cos(), angle.sin(), 0.0);
            let (l, r) = e.energy();
            powers.push((l * l + r * r).sqrt());
        }
        let lo = powers.iter().cloned().fold(f32::MAX, f32::min);
        let hi = powers.iter().cloned().fold(0.0, f32::max);
        // Within a quarter of a decibel all the way round -- comfortably below the roughly
        // one decibel a listener can pick out on a moving sound.
        assert!(hi / lo < 1.03, "level swung from {lo} to {hi} going round");
    }

    #[test]
    fn both_ears_are_the_same_length_so_they_can_be_crossfaded() {
        for (x, y, z) in [(1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (-0.3, -0.9, 0.3)] {
            let e = ears(x, y, z);
            assert_eq!(e.left.len(), e.right.len());
            assert_eq!(e.left.len(), Panner::new(RATE).taps());
        }
    }

    #[test]
    fn a_direction_of_nothing_is_not_a_panic() {
        // A zero vector should not reach here, but it costs one clamp to make sure that a bug
        // upstream is a centred sound rather than a NaN sprayed through the mix.
        let e = ears(0.0, 0.0, 0.0);
        assert!(e.left.iter().all(|s| s.is_finite()));
        assert!(e.right.iter().all(|s| s.is_finite()));
    }
}
