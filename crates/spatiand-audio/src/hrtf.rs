//! A measured head, loaded from a SOFA dataset.
//!
//! Panning gets you left and right. It cannot get you *behind*, because the difference between
//! a sound in front and the same sound behind is not level and not timing — those are almost
//! identical for the two — it is the shape a particular pair of ears folds into the sound on
//! the way in. That shape has to be measured, and a SOFA file is a table of those measurements
//! taken all over a sphere around somebody's head.
//!
//! ## Why this binds a C library at run time rather than at build time
//!
//! Reading SOFA means reading HDF5, which is not a thing to reimplement. `libmysofa` does it,
//! and it also does the part that would otherwise be ours to get wrong: given a direction that
//! nobody measured, find the measurements around it and interpolate between them.
//!
//! It is loaded with `dlopen` and every symbol is looked up by name, so a machine without it
//! runs perfectly well — [`crate::panner`] takes over and the sound still goes where the
//! window is, it just cannot convincingly go behind you. SteamOS happens to ship both the
//! library and a dataset at [`SYSTEM_DATASET`], so on the target hardware this is the path
//! taken; making it a hard build dependency would have traded that away for nothing.
//!
//! ## The frame is already ours
//!
//! SOFA's cartesian convention for the listener is x forward, y left, z up — the same frame
//! the tracker and the renderer use. So a direction from [`crate::stage`] is passed through
//! untouched. This is worth stating because it is exactly the kind of agreement that is
//! silently violated by a later refactor, and the failure it produces is sound coming from the
//! wrong side rather than anything that looks like a bug.

use std::ffi::{c_char, c_float, c_int, c_void, CString};
use std::path::Path;

use glam::DVec3;

use crate::ears::Ears;

/// Where SteamOS keeps the dataset it ships.
///
/// Not the only place one can be: the point of naming it is that on the target hardware there
/// is already a measured head on disk, so the good path needs no download and no bundling, and
/// no question about whose measurements we are allowed to redistribute.
pub const SYSTEM_DATASET: &str = "/usr/share/libmysofa/default.sofa";

/// The library's own name, as the loader will look for it.
const LIBRARY: &str = "libmysofa.so.1";

/// How much of each measured response is worth convolving.
///
/// Measured, not guessed. The dataset SteamOS ships returns 558 taps at 48 kHz, and of the
/// energy in one of those responses the first 64 taps carry **82%**, the first 128 carry
/// **99.6%**, and the remaining 430 carry the other **0.4%** — a tail 24 dB down, which is the
/// measurement chamber rather than the head. Convolving all of it would be four and a half
/// times the arithmetic for that.
///
/// The cost is per channel per ear and it is paid twice while the head is turning, so on a
/// twelve-channel film this is the difference between something that fits comfortably beside a
/// game and something that does not.
const USEFUL_TAPS: usize = 128;

/// How many taps at the end are faded out, to avoid cutting the response off mid-swing.
///
/// Truncating a filter is multiplying it by a rectangle, and a rectangle has sharp edges that
/// show up as ripple across the whole spectrum. Sixteen taps of raised cosine is enough to
/// round that off at a cost of nothing.
const TAPER: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("libmysofa is not installed: {0}")]
    NoLibrary(String),
    #[error("libmysofa is installed but does not export {0}")]
    NoSymbol(&'static str),
    #[error("no HRTF dataset at {0}")]
    NoDataset(String),
    #[error("libmysofa could not read {path}: error {code}")]
    Unreadable { path: String, code: i32 },
    #[error("{path} has a filter length of {taps}, which is not usable")]
    Unusable { path: String, taps: i32 },
}

type OpenFn = unsafe extern "C" fn(*const c_char, c_float, *mut c_int, *mut c_int) -> *mut c_void;
type FilterFn = unsafe extern "C" fn(
    *mut c_void,
    c_float,
    c_float,
    c_float,
    *mut c_float,
    *mut c_float,
    *mut c_float,
    *mut c_float,
);
type CloseFn = unsafe extern "C" fn(*mut c_void);

/// A dataset, open and ready to be asked about a direction.
///
/// Field order is drop order, and it matters: the handle has to be closed with a function
/// pointer that is only valid while the library is still mapped, so `library` is declared last.
pub struct Hrtf {
    easy: *mut c_void,
    filter: FilterFn,
    close: CloseFn,
    /// Taps in one of the dataset's own impulse responses, before any delay is added.
    taps: usize,
    rate: u32,
    library: libloading::Library,
}

impl std::fmt::Debug for Hrtf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hrtf")
            .field("taps", &self.taps)
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

// The handle is only ever touched from whichever thread owns this `Hrtf`, and there is exactly
// one owner: the renderer that produced it. `libmysofa`'s lookup is read-only on the handle,
// but it is not documented as thread-safe, so it is deliberately not `Sync`.
unsafe impl Send for Hrtf {}

impl Hrtf {
    /// Open a dataset, resampled to `rate`.
    ///
    /// Both failures worth distinguishing come back as distinct errors, because they call for
    /// different things: no library is a machine that never had one, while an unreadable file
    /// is a machine that has one and something is wrong with it.
    pub fn open(path: &Path, rate: u32) -> Result<Hrtf, Error> {
        if !path.exists() {
            return Err(Error::NoDataset(path.display().to_string()));
        }
        // SAFETY: loading a shared library, whose symbols are then checked one by one below.
        // Anything the loader does not have, we do not call.
        let library = unsafe { libloading::Library::new(LIBRARY) }
            .map_err(|e| Error::NoLibrary(e.to_string()))?;

        // SAFETY: each signature is transcribed from `mysofa.h`, checked against the installed
        // header rather than from memory. The pointers stay valid while `library` is alive,
        // which it is for as long as `self`.
        let (open, filter, close) = unsafe {
            let open: libloading::Symbol<OpenFn> = library
                .get(b"mysofa_open\0")
                .map_err(|_| Error::NoSymbol("mysofa_open"))?;
            let filter: libloading::Symbol<FilterFn> = library
                .get(b"mysofa_getfilter_float\0")
                .map_err(|_| Error::NoSymbol("mysofa_getfilter_float"))?;
            let close: libloading::Symbol<CloseFn> = library
                .get(b"mysofa_close\0")
                .map_err(|_| Error::NoSymbol("mysofa_close"))?;
            (*open, *filter, *close)
        };

        let c_path = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| Error::NoDataset(path.display().to_string()))?;
        let mut taps: c_int = 0;
        let mut err: c_int = 0;
        // SAFETY: `c_path` is NUL-terminated and outlives the call; both out-parameters are
        // live ints. The library resamples the dataset to `rate` as it loads.
        let easy = unsafe { open(c_path.as_ptr(), rate as c_float, &mut taps, &mut err) };
        if easy.is_null() || err != 0 {
            return Err(Error::Unreadable {
                path: path.display().to_string(),
                code: err,
            });
        }
        if taps <= 0 {
            // SAFETY: `easy` is a live handle from a successful open.
            unsafe { close(easy) };
            return Err(Error::Unusable {
                path: path.display().to_string(),
                taps,
            });
        }

        log::info!(
            "spatial audio: measured head from {} at {} Hz, {taps} taps",
            path.display(),
            rate
        );
        Ok(Hrtf {
            easy,
            filter,
            close,
            taps: taps as usize,
            rate,
            library,
        })
    }

    /// Open whatever the system has, which on the target hardware is a real dataset.
    pub fn system(rate: u32) -> Result<Hrtf, Error> {
        Hrtf::open(Path::new(SYSTEM_DATASET), rate)
    }

    /// Taps in the dataset's own responses, before the arrival delay is prepended.
    pub fn taps(&self) -> usize {
        self.taps
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }
}

impl crate::Spatialise for Hrtf {
    /// What a head does to a sound arriving from `direction`.
    ///
    /// The dataset also reports how long the sound takes to reach each ear, separately, and
    /// that difference is most of what tells you which side something is on below about a
    /// kilohertz. Measured responses usually have it already built into the impulse response
    /// and report zero here; minimum-phase ones report it and would otherwise arrive at both
    /// ears at once, which sounds like a sound inside your skull. So it is folded in either
    /// way, by writing the response after that much leading silence.
    fn ears_into(&self, direction: DVec3, out: &mut Ears) {
        let d = direction.normalize_or_zero();
        let taps = <Self as crate::Spatialise>::taps(self);
        out.blank(taps);
        // The library writes exactly `self.taps` floats, so it is only ever handed a buffer
        // that long; only the useful head of what it writes is then kept. The delay can never
        // push that past the end because `taps` above reserves a whole millisecond for it,
        // and the delay is clamped to that below.
        let kept = self.taps.min(USEFUL_TAPS);
        let room = taps - kept;
        let (mut delay_l, mut delay_r) = (0f32, 0f32);
        // A scratch pair, because the response has to be measured before it is known where in
        // the buffer it belongs -- the delay comes back from the same call that fills it.
        let mut scratch_l = vec![0f32; self.taps];
        let mut scratch_r = vec![0f32; self.taps];
        // SAFETY: `easy` is live for the lifetime of `self`; both impulse buffers are exactly
        // the length the library was told to write, and both delays are live floats.
        unsafe {
            (self.filter)(
                self.easy,
                d.x as c_float,
                d.y as c_float,
                d.z as c_float,
                scratch_l.as_mut_ptr(),
                scratch_r.as_mut_ptr(),
                &mut delay_l,
                &mut delay_r,
            );
        }
        // Seconds, per the float flavour of the call -- the short one reports samples, and
        // mixing the two up would put the ears a hundred milliseconds apart.
        let samples =
            |seconds: f32| ((seconds * self.rate as f32).round().max(0.0) as usize).min(room);
        let (at_l, at_r) = (samples(delay_l), samples(delay_r));
        out.left[at_l..at_l + kept].copy_from_slice(&scratch_l[..kept]);
        out.right[at_r..at_r + kept].copy_from_slice(&scratch_r[..kept]);
        if kept < self.taps {
            taper(&mut out.left[at_l..at_l + kept]);
            taper(&mut out.right[at_r..at_r + kept]);
        }
    }

    fn taps(&self) -> usize {
        // The useful part of the dataset's response, plus a millisecond of arrival delay --
        // more than the roughly 700 microseconds a head can put between one ear and the other.
        self.taps.min(USEFUL_TAPS) + self.rate as usize / 1000
    }
}

/// Fade the last few taps of a truncated response down to nothing.
fn taper(response: &mut [f32]) {
    let n = response.len().min(TAPER);
    if n == 0 {
        return;
    }
    let start = response.len() - n;
    for (i, tap) in response[start..].iter_mut().enumerate() {
        let t = (i + 1) as f32 / n as f32;
        *tap *= 0.5 + 0.5 * (t * std::f32::consts::PI).cos();
    }
}

impl Drop for Hrtf {
    fn drop(&mut self) {
        // SAFETY: `easy` came from a successful `mysofa_open` and is closed exactly once.
        unsafe { (self.close)(self.easy) };
        // Named so the drop order above is not mistaken for an unused field.
        let _ = &self.library;
    }
}

/// Tests against the dataset the machine actually has.
///
/// Ignored by default because they need both `libmysofa` and a SOFA file, which a development
/// machine may well not have — the fallback in [`crate::panner`] exists for exactly that case
/// and is tested unconditionally. Run them where the hardware is:
///
/// ```text
/// cargo test -p spatiand-audio -- --ignored --nocapture
/// ```
///
/// What they assert is physics rather than numbers: a sound on the left must reach the left
/// ear first and louder, and front must differ from behind. That last one is the whole reason
/// this module exists, and it is the one claim panning cannot make.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Spatialise;

    const RATE: u32 = 48_000;

    fn head() -> Hrtf {
        Hrtf::system(RATE).expect("no measured head on this machine")
    }

    fn from(x: f64, y: f64, z: f64) -> Ears {
        head().ears(DVec3::new(x, y, z))
    }

    /// How many samples later the right ear hears the same sound, by cross-correlation.
    ///
    /// The textbook estimator for the interaural time difference, and it has to be this rather
    /// than anything simpler. The obvious measure -- where each response's energy sits -- is
    /// not a delay: the shadowed ear is smeared as well as delayed, which drags its energy
    /// later and reports an interaural gap larger than a head can physically produce. That is
    /// a property of the metric, not of the head, and it cost one wrong assertion to find.
    fn interaural_delay(left: &[f32], right: &[f32]) -> i64 {
        // A head can put at most about 700 microseconds between the ears, which is 34 samples
        // at 48 kHz. Twice that is plenty of room to find the peak inside.
        const SEARCH: i64 = 68;
        let mut best = (0i64, f64::MIN);
        for lag in -SEARCH..=SEARCH {
            let score: f64 = left
                .iter()
                .enumerate()
                .filter_map(|(i, l)| {
                    let j = i as i64 + lag;
                    (j >= 0 && (j as usize) < right.len())
                        .then(|| *l as f64 * right[j as usize] as f64)
                })
                .sum();
            if score > best.1 {
                best = (lag, score);
            }
        }
        // Positive means the right ear's copy lines up later, so the sound is on the left.
        best.0
    }

    /// How much of a response lives above roughly a quarter of the sample rate.
    ///
    /// A one-pole difference is a crude high-pass, and crude is the right tool: what is being
    /// compared is two responses from the same dataset, so anything monotone in frequency
    /// separates them.
    fn treble(v: &[f32]) -> f64 {
        let high: f64 = v.windows(2).map(|w| ((w[1] - w[0]) as f64).powi(2)).sum();
        let all: f64 = v.iter().map(|s| (*s as f64).powi(2)).sum();
        if all > 0.0 {
            high / all
        } else {
            0.0
        }
    }

    #[test]
    #[ignore = "needs libmysofa and a SOFA dataset"]
    fn the_system_dataset_loads() {
        let h = head();
        assert!(h.taps() > 16, "a {}-tap response is not a head", h.taps());
        assert_eq!(h.rate(), RATE);
        println!("dataset: {} taps at {} Hz", h.taps(), h.rate());
        // How much of that length actually carries the sound, which decides how much of it is
        // worth convolving. Printed rather than asserted: it is a measurement of the dataset,
        // and what to do about it is a decision made once, in `USEFUL_TAPS`.
        let e = h.ears(DVec3::new(0.3, 0.9, 0.2));
        let total: f64 = e.left.iter().map(|s| (*s as f64).powi(2)).sum();
        let mut running = 0.0;
        let mut marks = Vec::new();
        for (i, s) in e.left.iter().enumerate() {
            running += (*s as f64).powi(2);
            if [64, 128, 192, 256, 384, 512].contains(&(i + 1)) {
                marks.push(format!("{}: {:.2}%", i + 1, 100.0 * running / total));
            }
        }
        println!(
            "energy captured by the first N taps -- {}",
            marks.join(", ")
        );
    }

    #[test]
    #[ignore = "needs libmysofa and a SOFA dataset"]
    fn a_sound_on_the_left_is_louder_in_the_left_ear() {
        // Also the test that catches the axes being swapped. SOFA's listener frame is x
        // forward, y left, z up, which is ours -- but "it is the same frame" is exactly the
        // kind of agreement that a later change breaks silently, and the symptom would be
        // sound coming from the wrong side rather than anything that looks like a fault.
        let (l, r) = from(0.0, 1.0, 0.0).energy();
        println!("from the left: left ear {l:.4}, right ear {r:.4}");
        assert!(l > r * 1.2, "left {l}, right {r}");
    }

    #[test]
    #[ignore = "needs libmysofa and a SOFA dataset"]
    fn a_sound_on_the_right_is_louder_in_the_right_ear() {
        let (l, r) = from(0.0, -1.0, 0.0).energy();
        assert!(r > l * 1.2, "left {l}, right {r}");
    }

    #[test]
    #[ignore = "needs libmysofa and a SOFA dataset"]
    fn a_sound_on_the_left_reaches_the_left_ear_first() {
        // The interaural time difference, which is most of what tells you which side a sound
        // is on below about a kilohertz. A head can put at most about 700 microseconds between
        // the ears, which is 34 samples at this rate.
        let e = from(0.0, 1.0, 0.0);
        let gap = interaural_delay(&e.left, &e.right);
        println!("the ears are {gap} samples apart");
        assert!(gap > 4, "the left ear did not hear it first: {gap} samples");
        // Bounded by physics: sound crosses the widest head in about 750 microseconds, which
        // is 36 samples here. The dataset is measured on a mannequin whose head is at the
        // large end, and it comes back at 35.
        assert!(
            gap <= 40,
            "the ears cannot be {gap} samples apart on one head"
        );
    }

    #[test]
    #[ignore = "needs libmysofa and a SOFA dataset"]
    fn a_sound_ahead_arrives_at_both_ears_together() {
        let e = from(1.0, 0.0, 0.0);
        let gap = interaural_delay(&e.left, &e.right).abs();
        assert!(
            gap < 3,
            "a sound straight ahead was {gap} samples off-centre"
        );
        let (l, r) = e.energy();
        assert!((l - r).abs() < l.max(r) * 0.25, "left {l}, right {r}");
    }

    #[test]
    #[ignore = "needs libmysofa and a SOFA dataset"]
    fn front_and_behind_are_not_the_same_sound() {
        // The claim the whole module exists for. Front and behind have nearly the same level
        // and nearly the same timing -- that is why you turn your head to find a siren -- so
        // a panner cannot tell them apart at all. A measured head can, because the outer ear
        // folds a different shape into each, and here that difference has to be real.
        let front = from(1.0, 0.0, 0.0);
        let back = from(-1.0, 0.0, 0.0);
        let (fl, _) = front.energy();
        let (bl, _) = back.energy();
        println!(
            "front: level {fl:.4} treble {:.4}   behind: level {bl:.4} treble {:.4}",
            treble(&front.left),
            treble(&back.left)
        );
        // Levels close enough that level alone could not distinguish them...
        assert!(
            (fl - bl).abs() < fl.max(bl) * 0.6,
            "front and behind differed mostly in level: {fl} vs {bl}"
        );
        // ...but the responses themselves plainly different.
        let difference: f32 = front
            .left
            .iter()
            .zip(back.left.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        let size: f32 = front.left.iter().map(|s| s.abs()).sum();
        assert!(
            difference > size * 0.3,
            "front and behind came back nearly identical: {difference} against {size}"
        );
    }

    #[test]
    #[ignore = "needs libmysofa and a SOFA dataset"]
    fn every_direction_gives_a_usable_filter() {
        // Walked over the whole sphere, because a dataset with a gap in it returns something
        // for a direction nobody measured, and what it returns had better still be a filter.
        let h = head();
        let taps = <Hrtf as Spatialise>::taps(&h);
        for az in (0..360).step_by(15) {
            for el in (-80..=80).step_by(20) {
                let (a, e) = ((az as f64).to_radians(), (el as f64).to_radians());
                let d = DVec3::new(e.cos() * a.cos(), e.cos() * a.sin(), e.sin());
                let ears = h.ears(d);
                assert_eq!(ears.taps(), taps, "at {az},{el}");
                assert!(
                    ears.left
                        .iter()
                        .chain(ears.right.iter())
                        .all(|s| s.is_finite()),
                    "a filter full of nonsense at {az},{el}"
                );
                let (l, r) = ears.energy();
                assert!(l > 0.0 && r > 0.0, "a silent ear at {az},{el}");
            }
        }
    }

    #[test]
    #[ignore = "needs libmysofa and a SOFA dataset"]
    fn nearby_directions_give_nearby_filters() {
        // The filters are swapped as the head turns, and a crossfade only hides a small step.
        // If two directions a degree apart returned unrelated filters, every head movement
        // would be heard as a texture crawling over the sound.
        let h = head();
        let a = h.ears(DVec3::new(1.0, 0.0, 0.0));
        let b = h.ears(DVec3::new(
            1.0f64.to_radians().cos(),
            1.0f64.to_radians().sin(),
            0.0,
        ));
        let difference: f32 = a
            .left
            .iter()
            .zip(b.left.iter())
            .map(|(x, y)| (x - y).abs())
            .sum();
        let size: f32 = a.left.iter().map(|s| s.abs()).sum();
        assert!(
            difference < size * 0.5,
            "a degree of movement changed the filter by {difference} against {size}"
        );
    }

    #[test]
    #[ignore = "needs libmysofa and a SOFA dataset"]
    fn the_kept_part_of_a_response_is_nearly_all_of_it() {
        // The justification for `USEFUL_TAPS`, kept as a test so that swapping the dataset
        // for one with a longer tail cannot quietly start throwing sound away.
        let h = head();
        let e = h.ears(DVec3::new(0.3, 0.9, 0.2));
        assert!(
            e.taps() <= USEFUL_TAPS + RATE as usize / 1000,
            "the response was not truncated: {} taps",
            e.taps()
        );
        // Against the whole thing, measured the long way round: the untruncated response is
        // what the library wrote, and what is kept must carry essentially all of its energy.
        let full: f64 = {
            let mut scratch = vec![0f32; h.taps()];
            let mut other = vec![0f32; h.taps()];
            let (mut a, mut b) = (0f32, 0f32);
            unsafe {
                (h.filter)(
                    h.easy,
                    0.3,
                    0.9,
                    0.2,
                    scratch.as_mut_ptr(),
                    other.as_mut_ptr(),
                    &mut a,
                    &mut b,
                );
            }
            scratch.iter().map(|s| (*s as f64).powi(2)).sum()
        };
        let kept: f64 = e.left.iter().map(|s| (*s as f64).powi(2)).sum();
        println!("kept {:.2}% of the response's energy", 100.0 * kept / full);
        assert!(
            kept / full > 0.98,
            "truncation kept only {:.1}%",
            100.0 * kept / full
        );
    }

    #[test]
    fn a_missing_dataset_is_a_clear_error_rather_than_a_panic() {
        // Runs everywhere, including where there is no libmysofa at all: the point is that
        // both absences are reported rather than crashed on, because the fallback needs to
        // know to take over.
        let err = Hrtf::open(Path::new("/nonexistent/head.sofa"), RATE).unwrap_err();
        assert!(matches!(err, Error::NoDataset(_)), "got {err:?}");
    }
}
