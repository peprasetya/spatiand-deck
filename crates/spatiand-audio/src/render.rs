//! Turning a stream's channels into two ears.
//!
//! Everything upstream decides *where* each channel is. This applies that decision to actual
//! samples, and it is the only part of the crate with a deadline: it runs on the audio thread,
//! where being late is a click and there is no second chance.
//!
//! ## Two paths, mixed
//!
//! A sound is rendered twice and the results are blended.
//!
//! The **spatial path** convolves each channel with the pair of filters for its direction.
//! This is what puts a sound behind you, and it is also what makes a stranger's ears colour
//! everything you hear: the measurements come from one particular head, and where they
//! disagree with yours the difference arrives as a mild tint.
//!
//! The **plain path** is the standard fold of the layout down to two channels — what any
//! player does when you listen on headphones. It carries no direction at all, and it sounds
//! exactly like the material is supposed to sound.
//!
//! [`Directness`] sets how much of each. This exists because a window straight ahead does not
//! need much help finding it, and the tint costs more than the placement is worth there; a
//! window behind you needs all the help it can get. So the blend follows how far off-centre
//! the window is. **The defaults are a starting point and not a result** — how much colouring
//! a generic head puts on a particular listener is a fact about that listener, and the only
//! instrument for it is their ears.
//!
//! ## Why the filters crossfade
//!
//! A filter is swapped whenever the head has turned far enough to be worth a new one. Swapping
//! it between one sample and the next steps the output, and a step is a click — dozens a
//! second while the head is moving. So the old filter and the new one are both applied for one
//! block and mixed across it, which costs a second convolution on the blocks where the head is
//! actually turning and nothing at all when it is still.

use glam::DVec3;

use crate::ears::Ears;
use crate::stage::{Channel, Layout, Speaker};
use crate::Spatialise;

/// How far the head must turn before a fresh pair of filters is fetched, radians.
///
/// Every fetch costs a crossfade, so this trades work against how finely the sound tracks the
/// head. A degree and a half is below what anyone can place a sound to — localisation blur for
/// a real sound source in front is a few degrees at best — so nothing is given up by not
/// chasing smaller movements than this.
const AIM_STEP: f64 = 1.5 * std::f64::consts::PI / 180.0;

/// How quickly a level change settles, seconds.
///
/// Long enough that muting is a fade rather than a step -- a step in level is a click, exactly
/// as a step in filter is -- and short enough to read as immediate.
const LEVEL_SETTLE: f32 = 0.012;

/// How much of the plain stereo fold to keep, and how that changes as the window moves off to
/// one side.
///
/// See the module note: these are the tuning knobs for how *processed* the result sounds, and
/// they are the one part of this crate that cannot be settled by a test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Directness {
    /// Fraction of plain fold when the window is dead ahead.
    pub centred: f32,
    /// Fraction of plain fold once the window is well round to one side.
    pub off_axis: f32,
    /// How far off-centre the window has to be for the change to be complete, radians.
    pub fade_by: f64,
}

impl Default for Directness {
    fn default() -> Self {
        Self {
            // Enough to keep a window in front sounding like itself rather than like a
            // recording of itself, and not so much that it stops being placed.
            centred: 0.35,
            // Off to the side there is real work for the filters to do, and a plain fold has
            // nothing to say about which side a sound is on beyond louder and quieter.
            off_axis: 0.10,
            fade_by: 40.0 * std::f64::consts::PI / 180.0,
        }
    }
}

impl Directness {
    /// Entirely spatial, with no plain fold mixed back in.
    pub const SPATIAL: Directness = Directness {
        centred: 0.0,
        off_axis: 0.0,
        fade_by: 1.0,
    };

    /// Entirely plain: the fold any headphone listener would get, placed nowhere.
    pub const PLAIN: Directness = Directness {
        centred: 1.0,
        off_axis: 1.0,
        fade_by: 1.0,
    };

    /// How much plain fold to use for a window this far off-centre.
    pub fn at(&self, off_axis: f64) -> f32 {
        let t = (off_axis.abs() / self.fade_by.max(1e-6)).clamp(0.0, 1.0);
        // Raised cosine, so there is no corner at either end of the movement -- a corner in
        // the blend is audible as the sound "arriving" at a place rather than reaching it.
        let w = (0.5 - 0.5 * (t * std::f64::consts::PI).cos()) as f32;
        self.centred + (self.off_axis - self.centred) * w
    }
}

/// How a channel folds into plain stereo, as (into left, into right).
///
/// The standard fold: the mains stay put, the centre goes to both at equal power, and
/// everything to one side of the room goes to that side. The LFE is kept rather than dropped —
/// a film mix puts real content down there and a listener on headphones has nothing else to
/// hear it with.
fn plain_fold(channel: Channel) -> (f32, f32) {
    const HALF_POWER: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match channel {
        Channel::FrontLeft => (1.0, 0.0),
        Channel::FrontRight => (0.0, 1.0),
        Channel::Mono | Channel::FrontCentre | Channel::Lfe => (HALF_POWER, HALF_POWER),
        Channel::SideLeft | Channel::RearLeft | Channel::TopFrontLeft | Channel::TopRearLeft => {
            (HALF_POWER, 0.0)
        }
        Channel::SideRight
        | Channel::RearRight
        | Channel::TopFrontRight
        | Channel::TopRearRight => (0.0, HALF_POWER),
    }
}

/// What to divide a layout's summed output by so it is no louder than stereo would have been.
///
/// Twelve channels summed are louder than two, and a listener should not have to reach for the
/// volume because a film happens to be mixed for more speakers. Uncorrelated signals add as
/// the root of the sum of squares, which is what this is.
fn layout_norm(layout: Layout) -> f32 {
    let power: f32 = layout
        .channels()
        .iter()
        .map(|&c| {
            let (l, r) = plain_fold(c);
            l * l + r * r
        })
        .sum::<f32>()
        / 2.0;
    if power > 0.0 {
        power.sqrt().recip()
    } else {
        1.0
    }
}

/// One channel: a delay line, and the filters currently pointed at it.
struct Voice {
    /// Twice the filter length, with every sample written twice, so the inner loop reads a
    /// straight run of memory and never takes a modulo.
    history: Vec<f32>,
    write: usize,
    current: Ears,
    /// The filter being mixed out of, valid only while `crossfading`.
    fading: Ears,
    crossfading: bool,
    /// The direction `current` was fetched for, or `None` for a channel with no direction.
    aimed: Option<DVec3>,
    fold: (f32, f32),
}

impl Voice {
    fn new(channel: Channel, taps: usize) -> Voice {
        Voice {
            history: vec![0.0; taps * 2],
            write: 0,
            current: Ears::silent(taps),
            fading: Ears::silent(taps),
            crossfading: false,
            aimed: None,
            fold: plain_fold(channel),
        }
    }

    fn taps(&self) -> usize {
        self.history.len() / 2
    }

    /// Add a sample and return what the two ears hear, `t` being how far through a crossfade
    /// this sample is.
    #[inline]
    fn step(&mut self, x: f32, t: f32) -> (f32, f32) {
        let taps = self.taps();
        self.history[self.write] = x;
        self.history[self.write + taps] = x;
        // The newest sample sits at `write + taps`, and walking back from there walks back
        // through time without ever leaving the buffer.
        let newest = self.write + taps;
        let window = &self.history[newest + 1 - taps..=newest];

        let (mut left, mut right) = dot2(window, &self.current);
        if self.crossfading {
            let (fl, fr) = dot2(window, &self.fading);
            left = fl + (left - fl) * t;
            right = fr + (right - fr) * t;
        }

        self.write += 1;
        if self.write == taps {
            self.write = 0;
        }
        (left, right)
    }
}

/// Both ears' worth of convolution over one window of history.
///
/// `window` is oldest-first and the filters are newest-first, so the filter is walked
/// backwards. Written as one loop over both ears because they share the sample read, which is
/// the expensive part once this is in cache.
#[inline]
fn dot2(window: &[f32], ears: &Ears) -> (f32, f32) {
    let mut left = 0.0f32;
    let mut right = 0.0f32;
    let n = window.len();
    for (k, (&hl, &hr)) in ears.left.iter().zip(ears.right.iter()).enumerate() {
        let x = window[n - 1 - k];
        left += hl * x;
        right += hr * x;
    }
    (left, right)
}

/// One stream, rendered to two ears.
pub struct Binaural {
    layout: Layout,
    spatialiser: Box<dyn Spatialise>,
    voices: Vec<Voice>,
    directness: Directness,
    norm: f32,
    /// Where the blend and the level are now, and where they are heading. Both are smoothed
    /// per sample, because both are gain and a step in gain is a click.
    dry: f32,
    dry_target: f32,
    level: f32,
    level_target: f32,
    settle: f32,
    peak: f32,
    /// Loudest sample in the last block, per channel. What the shell shows as "this window is
    /// sending 5.1", and what decides which voices are worth convolving.
    channel_peak: Vec<f32>,
    /// How many samples each channel has been silent for. A voice whose delay line has had
    /// time to empty contributes nothing, and convolving it is arithmetic spent on zero.
    quiet_for: Vec<usize>,
}

impl Binaural {
    /// Set up a renderer for one stream.
    ///
    /// Everything that allocates happens here: the delay lines, both filter pairs per channel,
    /// and nothing after. What runs later runs on the audio thread.
    pub fn new(
        layout: Layout,
        spatialiser: Box<dyn Spatialise>,
        directness: Directness,
        rate: u32,
    ) -> Binaural {
        let taps = spatialiser.taps().max(1);
        let voices = layout
            .channels()
            .iter()
            .map(|&c| Voice::new(c, taps))
            .collect();
        let dry = directness.at(0.0);
        Binaural {
            layout,
            spatialiser,
            voices,
            directness,
            norm: layout_norm(layout),
            dry,
            dry_target: dry,
            level: 1.0,
            level_target: 1.0,
            settle: 1.0 - (-1.0 / (LEVEL_SETTLE * rate as f32)).exp(),
            peak: 0.0,
            channel_peak: vec![0.0; layout.count()],
            // Start silent, so a stream that never uses its surrounds never pays for them.
            quiet_for: vec![usize::MAX / 2; layout.count()],
        }
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// The loudest sample seen in the last block, before any of this touched it.
    ///
    /// What the window chrome shows: whether this window is making a sound at all, and
    /// roughly how much. Taken from the input so it reads the app's own output rather than
    /// what muting it has done.
    pub fn peak(&self) -> f32 {
        self.peak
    }

    /// The loudest sample in the last block for each channel, in the layout's own order.
    ///
    /// This is how a window can say *which* channels an app is actually using, rather than
    /// which it has room for -- the difference between "7.1.4" and "a stereo mix through a
    /// 7.1.4 connection", which is the thing worth showing on the window.
    pub fn channel_peaks(&self) -> &[f32] {
        &self.channel_peak
    }

    /// Which channels have carried sound recently enough to still matter.
    pub fn live_channels(&self) -> usize {
        self.quiet_for.iter().filter(|q| **q < self.taps()).count()
    }

    /// The smallest standard layout that covers what the app is *actually* sending.
    ///
    /// Not the same as [`Binaural::layout`], and the difference is the thing worth putting on
    /// a window. The connection is as wide as the widest thing we can carry, so a music player
    /// and a film both arrive through the same twelve channels; what tells them apart is how
    /// many of those channels have anything in them. `None` means silence.
    pub fn sounding_layout(&self) -> Option<Layout> {
        let taps = self.taps();
        let live: Vec<Channel> = self
            .layout
            .channels()
            .iter()
            .zip(self.quiet_for.iter())
            .filter(|(_, quiet)| **quiet < taps)
            .map(|(c, _)| *c)
            .collect();
        if live.is_empty() {
            return None;
        }
        // Smallest first, so a stereo mix down a twelve-channel pipe reads as stereo.
        [
            Layout::Mono,
            Layout::Stereo,
            Layout::Surround51,
            Layout::Surround71,
            Layout::Surround714,
        ]
        .into_iter()
        .find(|candidate| live.iter().all(|c| candidate.channels().contains(c)))
        .or(Some(Layout::Surround714))
    }

    fn taps(&self) -> usize {
        self.voices.first().map(|v| v.taps()).unwrap_or(0)
    }

    /// Silence this stream, or bring it back. Faded, not switched.
    pub fn set_muted(&mut self, muted: bool) {
        self.level_target = if muted { 0.0 } else { 1.0 };
    }

    pub fn is_muted(&self) -> bool {
        self.level_target == 0.0
    }

    pub fn set_directness(&mut self, directness: Directness) {
        self.directness = directness;
    }

    /// Point the voices at where the sound is now.
    ///
    /// `off_axis` is how far the window is from straight ahead, which decides how much plain
    /// fold to keep. Cheap when nothing has moved: a direction that has not changed by
    /// [`AIM_STEP`] leaves its voice entirely alone.
    pub fn aim(&mut self, speakers: &[Speaker], off_axis: f64) {
        self.dry_target = self.directness.at(off_axis);
        for (voice, speaker) in self.voices.iter_mut().zip(speakers.iter()) {
            let Some(direction) = speaker.direction else {
                // A channel with no direction -- the LFE -- is never convolved and never
                // aimed. It reaches both ears through the plain fold and nothing else.
                voice.aimed = None;
                continue;
            };
            let moved = match voice.aimed {
                None => true,
                Some(was) => was.dot(direction) < AIM_STEP.cos(),
            };
            if !moved {
                continue;
            }
            let first = voice.aimed.is_none();
            std::mem::swap(&mut voice.current, &mut voice.fading);
            self.spatialiser.ears_into(direction, &mut voice.current);
            // The first aim has nothing to fade from, and fading up from silence would make
            // every stream start with a short swell.
            voice.crossfading = !first;
            voice.aimed = Some(direction);
        }
    }

    /// Render one block.
    ///
    /// `input` is interleaved in the layout's channel order; `out` is interleaved stereo and
    /// is written, not added to. Whichever of the two runs out first decides how many frames
    /// are done, so a short or ragged block is a quiet one rather than a panic.
    pub fn render(&mut self, input: &[f32], out: &mut [f32]) {
        let channels = self.layout.count();
        let frames = (input.len() / channels).min(out.len() / 2);
        self.peak = 0.0;
        if frames == 0 {
            self.channel_peak.iter_mut().for_each(|p| *p = 0.0);
            return;
        }
        let step = (frames as f32).recip();

        // Which channels are carrying anything. A stereo app connected to a twelve-channel
        // sink leaves ten of them at exactly zero, and convolving those is most of the work
        // for none of the sound -- so this is what makes one sink layout serve every app
        // without charging a song the price of a film.
        let taps = self.taps();
        for (c, peak) in self.channel_peak.iter_mut().enumerate() {
            let mut loudest = 0.0f32;
            for f in 0..frames {
                let x = input[f * channels + c];
                if x.is_finite() && x.abs() > loudest {
                    loudest = x.abs();
                }
            }
            *peak = loudest;
            if loudest > 0.0 {
                self.quiet_for[c] = 0;
            } else {
                self.quiet_for[c] = self.quiet_for[c].saturating_add(frames);
            }
            if loudest > self.peak {
                self.peak = loudest;
            }
        }

        for f in 0..frames {
            // How far through the crossfade this sample is. Reaching exactly 1 on the last
            // sample of the block is what lets the old filter be dropped at the end of it.
            let t = (f + 1) as f32 * step;
            let frame = &input[f * channels..(f + 1) * channels];

            let (mut wet_l, mut wet_r) = (0.0f32, 0.0f32);
            let (mut dry_l, mut dry_r) = (0.0f32, 0.0f32);
            for ((c, voice), &x) in self.voices.iter_mut().enumerate().zip(frame.iter()) {
                // A channel silent for longer than its own filter has nothing left in the
                // delay line to come out, so there is nothing to compute.
                if self.quiet_for[c] >= taps {
                    continue;
                }
                let x = if x.is_finite() { x } else { 0.0 };
                dry_l += x * voice.fold.0;
                dry_r += x * voice.fold.1;
                if voice.aimed.is_some() {
                    let (l, r) = voice.step(x, t);
                    wet_l += l;
                    wet_r += r;
                } else {
                    // Undirected, so it arrives the same way in both paths and the blend
                    // below cannot change it. This is what keeps the bass steady while the
                    // head turns.
                    wet_l += x * voice.fold.0;
                    wet_r += x * voice.fold.1;
                }
            }

            self.dry += (self.dry_target - self.dry) * self.settle;
            self.level += (self.level_target - self.level) * self.settle;
            let gain = self.level * self.norm;
            out[f * 2] = gain * (wet_l + (dry_l - wet_l) * self.dry);
            out[f * 2 + 1] = gain * (wet_r + (dry_r - wet_r) * self.dry);
        }

        // The crossfade finished at the end of the block, by construction.
        for voice in &mut self.voices {
            voice.crossfading = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage::{place, Stage};

    const RATE: u32 = 48_000;

    /// A spatialiser with no physics in it, so a test can say exactly what should come out.
    ///
    /// One tap per ear, panned by how far to the left the sound is. Deliberately not a head:
    /// what is being tested here is the plumbing — routing, folding, blending, crossfading —
    /// and a real head's filters would hide an error in any of it behind plausible numbers.
    struct Panned;

    impl Spatialise for Panned {
        fn ears_into(&self, d: DVec3, out: &mut Ears) {
            out.blank(<Self as Spatialise>::taps(self));
            let left = ((1.0 + d.normalize_or_zero().y) / 2.0) as f32;
            out.left[0] = left;
            out.right[0] = 1.0 - left;
        }
        fn taps(&self) -> usize {
            4
        }
    }

    fn stereo(directness: Directness) -> Binaural {
        Binaural::new(Layout::Stereo, Box::new(Panned), directness, RATE)
    }

    fn ahead() -> Vec<Speaker> {
        place(
            Layout::Stereo,
            &Stage {
                yaw: 0.0,
                pitch: 0.0,
                half_width: 0.25,
            },
            glam::DQuat::IDENTITY,
        )
    }

    /// Render a block and return it as (left, right) pairs.
    fn run(b: &mut Binaural, input: &[f32]) -> Vec<(f32, f32)> {
        let frames = input.len() / b.layout().count();
        let mut out = vec![0.0; frames * 2];
        b.render(input, &mut out);
        out.chunks(2).map(|c| (c[0], c[1])).collect()
    }

    #[test]
    fn silence_in_is_silence_out() {
        let mut b = stereo(Directness::default());
        b.aim(&ahead(), 0.0);
        for (l, r) in run(&mut b, &vec![0.0; 256]) {
            assert_eq!((l, r), (0.0, 0.0));
        }
    }

    #[test]
    fn the_plain_fold_of_stereo_is_the_stereo_itself() {
        // The fold has to be transparent for two channels, or every window would be quietly
        // reprocessed even with the spatial path turned off entirely.
        let mut b = stereo(Directness::PLAIN);
        b.aim(&ahead(), 0.0);
        let input = [1.0, 0.0, 0.0, 1.0, 0.5, -0.5];
        let out = run(&mut b, &input);
        for (got, want) in out.iter().zip([(1.0, 0.0), (0.0, 1.0), (0.5, -0.5)]) {
            assert!((got.0 - want.0).abs() < 1e-3, "{got:?} wanted {want:?}");
            assert!((got.1 - want.1).abs() < 1e-3, "{got:?} wanted {want:?}");
        }
    }

    #[test]
    fn a_channel_lands_in_the_ear_its_direction_points_at() {
        // The left channel of a window in front is a little to the left, so it should be a
        // little louder on the left. With the plain fold mixed out, this is the spatial path
        // on its own.
        let mut b = stereo(Directness::SPATIAL);
        b.aim(&ahead(), 0.0);
        let mut input = vec![0.0; 64];
        input[0] = 1.0; // one impulse, left channel only
        let out = run(&mut b, &input);
        let (l, r) = out.iter().fold((0.0f32, 0.0f32), |a, s| {
            (a.0.max(s.0.abs()), a.1.max(s.1.abs()))
        });
        assert!(
            l > r,
            "the left channel was not louder on the left: {l} vs {r}"
        );
    }

    #[test]
    fn muting_fades_rather_than_cuts() {
        // A step to silence is a click. The whole point of a mute button is that it is not
        // heard doing anything except stopping the sound.
        let mut b = stereo(Directness::PLAIN);
        b.aim(&ahead(), 0.0);
        run(&mut b, &vec![1.0; 512]);
        b.set_muted(true);
        let out = run(&mut b, &vec![1.0; 4096]);
        let biggest_step = out
            .windows(2)
            .map(|w| (w[1].0 - w[0].0).abs())
            .fold(0.0f32, f32::max);
        assert!(biggest_step < 0.01, "muting stepped by {biggest_step}");
        assert!(out.last().unwrap().0.abs() < 0.05, "it never went quiet");
    }

    #[test]
    fn unmuting_comes_back() {
        let mut b = stereo(Directness::PLAIN);
        b.aim(&ahead(), 0.0);
        b.set_muted(true);
        run(&mut b, &vec![1.0; 4096]);
        assert!(b.is_muted());
        b.set_muted(false);
        let out = run(&mut b, &vec![1.0; 4096]);
        assert!(out.last().unwrap().0 > 0.9, "it did not come back");
        assert!(!b.is_muted());
    }

    #[test]
    fn a_filter_swap_does_not_click() {
        // The reason the crossfade exists. A head turning swaps filters dozens of times a
        // second, and each swap steps the output unless it is mixed across the block.
        //
        // Stated as: the join between two blocks is no rougher than the signal on either side
        // of it. That comparison is the whole trick here, and it took two wrong versions to
        // arrive at. Comparing against a fixed baseline does not work, because swinging the
        // window also legitimately changes the level -- the tone really is steeper when it is
        // louder, and the first version of this test was measuring that and calling it a
        // click. Measuring the seam against its own neighbours is immune to level, so what is
        // left can only be discontinuity.
        const BLOCK: usize = 480;
        let tone = |n: usize| -> Vec<f32> {
            (0..BLOCK)
                .flat_map(|i| {
                    let v = ((n * BLOCK + i) as f32 * 0.05).sin();
                    [v, v]
                })
                .collect()
        };
        let swung_at = |yaw_deg: f64| {
            place(
                Layout::Stereo,
                &Stage {
                    yaw: yaw_deg.to_radians(),
                    pitch: 0.0,
                    half_width: 0.25,
                },
                glam::DQuat::IDENTITY,
            )
        };

        let mut b = stereo(Directness::SPATIAL);
        let mut previous: Option<(f32, f32)> = None;
        // A head swinging through a right angle between one block and the next, which is
        // faster than a neck moves.
        for (n, yaw) in [0.0, 20.0, -20.0, 60.0, -60.0, 0.0].iter().enumerate() {
            b.aim(&swung_at(*yaw), yaw.to_radians().abs());
            let out = run(&mut b, &tone(n));
            if let (Some(last), Some(first)) = (previous, out.first()) {
                let seam = (first.0 - last.0).abs();
                // The roughness of the signal itself, right where the join is.
                let local = out
                    .windows(2)
                    .take(32)
                    .map(|w| (w[1].0 - w[0].0).abs())
                    .fold(0.0f32, f32::max);
                assert!(
                    seam <= local * 1.5,
                    "at {yaw} degrees the join stepped by {seam}, against {local} either side"
                );
            }
            previous = out.last().copied();
        }
    }

    #[test]
    fn a_film_is_no_louder_than_a_song() {
        // Twelve channels summed are louder than two. A listener should not have to reach for
        // the volume because a mix happens to have been made for more speakers.
        let loudness = |layout: Layout| {
            let mut b = Binaural::new(layout, Box::new(Panned), Directness::PLAIN, RATE);
            let speakers = place(
                layout,
                &Stage {
                    yaw: 0.0,
                    pitch: 0.0,
                    half_width: 0.25,
                },
                glam::DQuat::IDENTITY,
            );
            b.aim(&speakers, 0.0);
            // Genuinely independent content in every channel, which is what the
            // root-sum-of-squares normalisation is derived for and roughly what a real mix
            // is. An earlier version stepped a single sequence across the channels, which
            // made them deterministically related -- they then partly cancelled, and the
            // test was measuring that rather than the normalisation.
            let noise = |frame: usize, channel: usize| {
                let mut x = (frame as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    ^ (channel as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                x ^= x >> 30;
                x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
                x ^= x >> 27;
                x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
                x ^= x >> 31;
                // Twelve bits, spanning -1 to 1. Taking more than twelve leaves a large
                // positive offset on every channel, which correlates them all and makes this
                // measure their agreement rather than the normalisation.
                (x >> 52) as f32 / 2048.0 - 1.0
            };
            let input: Vec<f32> = (0..2048)
                .flat_map(|f| (0..layout.count()).map(move |c| noise(f, c)))
                .collect();
            let out = run(&mut b, &input);
            (out.iter().map(|(l, _)| l * l).sum::<f32>() / out.len() as f32).sqrt()
        };
        let two = loudness(Layout::Stereo);
        for layout in [Layout::Surround51, Layout::Surround71, Layout::Surround714] {
            let many = loudness(layout);
            assert!(
                (many / two) < 1.4 && (many / two) > 0.6,
                "{layout:?} came out at {:.2}x stereo",
                many / two
            );
        }
    }

    #[test]
    fn the_bass_does_not_move_when_the_head_does() {
        // The LFE has no direction, so nothing about turning your head may change it. If it
        // ever went through the spatial path it would be filtered by a pair of ears, and the
        // one thing a listener would notice is the bass level breathing as they looked around.
        let mut b = Binaural::new(
            Layout::Surround51,
            Box::new(Panned),
            Directness::SPATIAL,
            RATE,
        );
        let lfe = Layout::Surround51
            .channels()
            .iter()
            .position(|c| *c == Channel::Lfe)
            .unwrap();
        let mut input = vec![0.0; Layout::Surround51.count() * 64];
        for f in 0..64 {
            input[f * Layout::Surround51.count() + lfe] = 1.0;
        }
        let sample_at = |b: &mut Binaural, yaw: f64| {
            let speakers = place(
                Layout::Surround51,
                &Stage {
                    yaw,
                    pitch: 0.0,
                    half_width: 0.25,
                },
                glam::DQuat::IDENTITY,
            );
            b.aim(&speakers, yaw.abs());
            run(b, &input).last().copied().unwrap()
        };
        let front = sample_at(&mut b, 0.0);
        let side = sample_at(&mut b, 1.5);
        assert!((front.0 - side.0).abs() < 1e-6, "{front:?} vs {side:?}");
        assert!((front.1 - side.1).abs() < 1e-6, "{front:?} vs {side:?}");
    }

    #[test]
    fn the_blend_follows_the_window_off_to_the_side() {
        let d = Directness::default();
        assert!((d.at(0.0) - d.centred).abs() < 1e-6);
        assert!((d.at(std::f64::consts::PI) - d.off_axis).abs() < 1e-6);
        // Monotone in between, so nothing swims as a window is dragged round.
        let mut last = d.at(0.0);
        for step in 1..=40 {
            let now = d.at(d.fade_by * step as f64 / 40.0);
            assert!(
                now <= last + 1e-6,
                "the blend went back on itself at step {step}"
            );
            last = now;
        }
    }

    #[test]
    fn a_stereo_mix_down_a_wide_pipe_still_reads_as_stereo() {
        // Every window's sink is as wide as the widest thing that can be carried, so a music
        // player and a film arrive through the same twelve channels. What tells them apart --
        // and what a window should say about itself -- is how many of those channels have
        // anything in them.
        let mut b = Binaural::new(
            Layout::Surround714,
            Box::new(Panned),
            Directness::PLAIN,
            RATE,
        );
        let speakers = place(
            Layout::Surround714,
            &Stage {
                yaw: 0.0,
                pitch: 0.0,
                half_width: 0.25,
            },
            glam::DQuat::IDENTITY,
        );
        b.aim(&speakers, 0.0);
        assert_eq!(b.sounding_layout(), None, "silence is not a layout");

        // Only the front pair carries anything, as an ordinary stereo app would leave it.
        let channels = Layout::Surround714.count();
        let mut input = vec![0.0; channels * 128];
        for f in 0..128 {
            input[f * channels] = 0.5;
            input[f * channels + 1] = -0.5;
        }
        run(&mut b, &input);
        assert_eq!(b.sounding_layout(), Some(Layout::Stereo));
        assert_eq!(b.live_channels(), 2);

        // Now the centre and the surrounds join in, and it is a film.
        for f in 0..128 {
            for c in 0..6 {
                input[f * channels + c] = 0.25;
            }
        }
        run(&mut b, &input);
        assert_eq!(b.sounding_layout(), Some(Layout::Surround51));
    }

    #[test]
    fn a_silent_channel_costs_nothing_and_changes_nothing() {
        // The optimisation that lets one sink layout serve every app: ten silent channels of
        // a twelve-channel connection are skipped entirely. What must not change is the
        // sound, so the same content through a wide pipe and a narrow one has to match.
        let stage = Stage {
            yaw: 0.4,
            pitch: 0.0,
            half_width: 0.25,
        };
        let narrow = {
            let mut b = Binaural::new(Layout::Stereo, Box::new(Panned), Directness::SPATIAL, RATE);
            b.aim(&place(Layout::Stereo, &stage, glam::DQuat::IDENTITY), 0.4);
            let input: Vec<f32> = (0..256)
                .flat_map(|i| {
                    let v = (i as f32 * 0.07).sin();
                    [v, -v]
                })
                .collect();
            run(&mut b, &input)
        };
        let wide = {
            let mut b = Binaural::new(
                Layout::Surround714,
                Box::new(Panned),
                Directness::SPATIAL,
                RATE,
            );
            b.aim(
                &place(Layout::Surround714, &stage, glam::DQuat::IDENTITY),
                0.4,
            );
            let channels = Layout::Surround714.count();
            let mut input = vec![0.0; channels * 256];
            for i in 0..256 {
                let v = (i as f32 * 0.07).sin();
                input[i * channels] = v;
                input[i * channels + 1] = -v;
            }
            run(&mut b, &input)
        };
        // The wide one is scaled down by its layout's normalisation, which is the honest
        // difference between the two; the shape has to be identical.
        let scale = wide[200].0 / narrow[200].0;
        for (n, w) in narrow.iter().zip(wide.iter()).skip(64) {
            assert!(
                (n.0 * scale - w.0).abs() < 1e-4,
                "the silent channels changed the sound: {} vs {}",
                n.0 * scale,
                w.0
            );
        }
    }

    #[test]
    fn a_ragged_block_is_quiet_rather_than_a_panic() {
        // Buffers come from an audio server and are not always the size arithmetic says.
        let mut b = stereo(Directness::default());
        b.aim(&ahead(), 0.0);
        let mut out = vec![0.0; 8];
        b.render(&[1.0, 1.0, 1.0], &mut out); // one and a half frames
        assert!(out.iter().all(|s| s.is_finite()));
        b.render(&[], &mut out);
        b.render(&[1.0; 64], &mut []);
    }

    #[test]
    fn a_stream_full_of_nonsense_does_not_poison_the_mix() {
        // One NaN convolved into a delay line stays there, and every sample after it is NaN
        // too -- silence for as long as the window is open, from one bad sample.
        let mut b = stereo(Directness::default());
        b.aim(&ahead(), 0.0);
        let input = vec![f32::NAN, f32::INFINITY, 0.5, -0.5];
        let out = run(&mut b, &input);
        assert!(out.iter().all(|(l, r)| l.is_finite() && r.is_finite()));
        let after = run(&mut b, &vec![0.25; 64]);
        assert!(after.iter().all(|(l, r)| l.is_finite() && r.is_finite()));
    }
}
