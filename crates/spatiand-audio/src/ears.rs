//! A direction, reduced to what each ear hears.
//!
//! Everything that turns "the sound is over there" into "this is what arrives at the left ear
//! and this at the right" produces one of these, and everything downstream consumes it without
//! caring which. A measured head from a SOFA dataset and the arithmetic fallback used when
//! there is no dataset are interchangeable here on purpose: the difference between them is how
//! good it sounds, never how it is wired.

/// A short filter for each ear.
///
/// The two are the same length so the convolution can walk one delay line once. That costs a
/// few zero taps on whichever ear the sound reaches last, and buys not having to keep two
/// cursors in step in the inner loop.
#[derive(Debug, Clone, PartialEq)]
pub struct Ears {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
}

impl Ears {
    /// How many taps each ear's filter has.
    pub fn taps(&self) -> usize {
        self.left.len()
    }

    /// A pair of silent filters of the given length, ready to be written into.
    pub fn silent(taps: usize) -> Ears {
        Ears {
            left: vec![0.0; taps],
            right: vec![0.0; taps],
        }
    }

    /// Make both ears `taps` long and silent, keeping whatever memory is already here.
    ///
    /// The audio thread's way of starting a fresh filter: `resize` down then up reuses the
    /// allocation, so a head turning for a minute allocates nothing after the first block.
    pub fn blank(&mut self, taps: usize) {
        for ear in [&mut self.left, &mut self.right] {
            ear.clear();
            ear.resize(taps, 0.0);
        }
    }

    /// A filter that passes the sound to both ears unchanged.
    ///
    /// What the LFE gets, and what an empty dataset degrades to: audible, centred, and not
    /// pretending to come from anywhere.
    pub fn centred(gain: f32) -> Ears {
        Ears {
            left: vec![gain],
            right: vec![gain],
        }
    }

    /// Root-mean-square of one ear, which is the honest measure of how loud that side is.
    ///
    /// Peak would not do: an impulse response spreads its energy over the whole filter, and
    /// two directions can share a peak while differing greatly in what they deliver.
    pub fn energy(&self) -> (f32, f32) {
        let rms = |v: &[f32]| (v.iter().map(|s| s * s).sum::<f32>() / v.len().max(1) as f32).sqrt();
        (rms(&self.left), rms(&self.right))
    }

    /// Pad both ears out to `taps`, so filters from different directions can be crossfaded.
    ///
    /// A dataset returns the same length every time, but the fallback does not, and a
    /// crossfade between two filters of different lengths is a subtraction that runs off the
    /// end of the shorter one.
    pub fn pad_to(&mut self, taps: usize) {
        self.left.resize(taps.max(self.left.len()), 0.0);
        self.right.resize(taps.max(self.right.len()), 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_centred_filter_reaches_both_ears_alike() {
        let e = Ears::centred(0.5);
        let (l, r) = e.energy();
        assert_eq!(l, r);
        assert_eq!(e.taps(), 1);
    }

    #[test]
    fn padding_lengthens_without_moving_anything() {
        // The taps already there must stay at the offsets they were at: an impulse response
        // shifted by one sample is a different direction.
        let mut e = Ears {
            left: vec![1.0, 0.5],
            right: vec![0.0, 0.25],
        };
        e.pad_to(6);
        assert_eq!(e.left, vec![1.0, 0.5, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(e.right, vec![0.0, 0.25, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn blanking_reuses_the_memory_it_already_has() {
        // The reason this exists rather than just building a new pair: on the audio thread an
        // allocation is the one call that can miss a deadline.
        let mut e = Ears::silent(64);
        let before = e.left.as_ptr();
        e.left[3] = 1.0;
        e.blank(64);
        assert_eq!(e.left.as_ptr(), before, "blanking reallocated");
        assert!(e.left.iter().all(|s| *s == 0.0));
        assert_eq!(e.taps(), 64);
    }

    #[test]
    fn padding_never_shortens() {
        let mut e = Ears {
            left: vec![1.0; 8],
            right: vec![1.0; 8],
        };
        e.pad_to(2);
        assert_eq!(e.taps(), 8);
    }
}
