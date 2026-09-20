//! The magnetometer's own offset, measured while the glasses are worn.
//!
//! A magnetometer in a pair of glasses reads Earth's field plus the field of everything bolted
//! to it — the speaker magnets above all. That second part turns with the head, so it is a
//! constant in the *head* frame (a "hard-iron" offset), and on XREAL glasses it is bigger than
//! Earth's field: HoloFrame fitted about 0.25 G against a true field of about 0.16 G. Left in,
//! the apparent heading bends by tens of degrees depending on where you face, and a magnetic
//! anchor then steers yaw toward a heading that is simply wrong — measured at 23–32 degrees on a
//! head that had not drifted. So the anchor must not run until this offset is known.
//!
//! ## How it is fitted
//!
//! Over a short window the gyro's orientation `R` is good to a fraction of a degree, and the
//! field in the world `F` is constant, so every sample says
//!
//! ```text
//!     R · (P·m − c) = F
//! ```
//!
//! with `m` the raw reading, `c` the offset and `P` how the magnetometer's axes sit relative to
//! the gyro's. For a fixed `P` that is linear in `c` and `F`, and eliminating `F` leaves a 3×3
//! system for `c` alone, built from running sums. Each window gets its own `F`, so slow yaw drift
//! between windows costs nothing: only the rotation *within* a window is trusted.
//!
//! `P` is not assumed. The magnetometer is a separate chip and nothing says its axes are the
//! gyro's; one axis swapped or flipped makes every fit wrong in a way no offset can repair. The
//! sums are kept in a form that lets every signed axis permutation be tried at solve time, and
//! the one that explains the data is kept — clearly better than the rest, or not at all.
//!
//! ## What it needs from the wearer
//!
//! Nothing deliberate, but some variety. Turning only left and right leaves the offset along
//! the vertical unmeasured, so the fit waits until the head has also looked up and down, which
//! ordinary use does within a minute or two. Until it is satisfied it says why, and the anchor
//! stays off.

use std::collections::VecDeque;

use glam::{DMat3, DQuat, DVec3};

/// One sample every this many seconds. The field changes slowly; 50 Hz is plenty and keeps
/// the sums from being dominated by any one still moment.
const SAMPLE_EVERY_S: f64 = 0.02;
/// A window is this long. Short enough that the gyro alone holds orientation to well under a
/// degree across it, long enough to contain some head movement.
const WINDOW_S: f64 = 20.0;
/// Windows remembered: twenty minutes. Old enough ones fall out, so a fit follows a change.
const WINDOWS: usize = 60;
/// Samples taken while the head turns faster than this are skipped: the magnetometer and the
/// gyro are not sampled at exactly the same instant, and mid-flick that skew is an error.
const MAX_RATE_DPS: f64 = 90.0;
/// Readings stronger than this are glitches, not fields. Earth's is about 0.5 G at most, and
/// the glasses' own magnets add a few tenths.
pub const PLAUSIBLE_GAUSS: f64 = 2.0;
/// At least a minute of samples before any answer.
const MIN_SAMPLES: f64 = 3000.0;
/// How much the head must have turned about its least-turned axis, within windows. 0 means
/// never, 1 means every direction equally. Only turning left and right gives 0; glancing up
/// and down by fifteen degrees or so now and then, as at a desk, gives about 0.01.
const MIN_COVERAGE: f64 = 0.006;
/// The field that is left must be steady to within this share of its own size.
const MAX_RESIDUAL_SHARE: f64 = 0.15;
/// ...and the best axis arrangement must beat the next best by this factor.
const DECISIVE: f64 = 0.6;

/// A fitted offset and how the magnetometer's axes map onto the gyro's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HardIron {
    /// Row `i` names which raw axis becomes axis `i`, and with which sign: `(axis, sign)`.
    pub axes: [(usize, f64); 3],
    /// Gauss, in the gyro's frame, after `axes` is applied.
    pub offset: DVec3,
}

impl HardIron {
    /// Nothing moved and nothing subtracted. Not a measurement; what the anchor did before.
    pub const NONE: HardIron = HardIron {
        axes: [(0, 1.0), (1, 1.0), (2, 1.0)],
        offset: DVec3::ZERO,
    };

    /// The raw reading in the gyro's frame, with the glasses' own field taken out.
    pub fn apply(&self, raw: DVec3) -> DVec3 {
        let arranged = DVec3::new(
            raw[self.axes[0].0] * self.axes[0].1,
            raw[self.axes[1].0] * self.axes[1].1,
            raw[self.axes[2].0] * self.axes[2].1,
        );
        arranged - self.offset
    }

    fn matrix(&self) -> DMat3 {
        let mut cols = [DVec3::ZERO; 3];
        for (row, (axis, sign)) in self.axes.iter().enumerate() {
            cols[*axis][row] = *sign;
        }
        DMat3::from_cols(cols[0], cols[1], cols[2])
    }

    /// `"x+ y+ z+"`, for the log.
    pub fn axes_summary(&self) -> String {
        self.axes
            .iter()
            .map(|(a, s)| format!("{}{}", ["x", "y", "z"][*a], if *s > 0.0 { "+" } else { "-" }))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// How the fit stands, for the log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FitState {
    /// Not enough samples yet.
    Gathering { seconds: f64 },
    /// Enough samples but not enough variety: the head has not looked around enough.
    NarrowView { coverage: f64 },
    /// Plenty of everything, and no arrangement explains the readings. Something in the room
    /// is changing the field, or the magnetometer is not what this assumes.
    NoFit { residual_share: f64 },
    /// Two arrangements explain it about equally.
    Undecided,
    Fitted {
        fit: HardIron,
        /// Earth's field as measured here, gauss.
        field: f64,
        residual_share: f64,
        coverage: f64,
    },
}

#[derive(Debug, Clone, Copy)]
struct Window {
    n: f64,
    /// Σ R
    sum_r: DMat3,
    /// Σ R[a][i]·m[j], so that Σ R·P·m can be formed for any P afterwards.
    t: [[[f64; 3]; 3]; 3],
    sum_m: DVec3,
    sum_mm: f64,
}

// Written out, not derived: glam's `DMat3::default()` is the identity, not zero, and a sum
// that starts at the identity biased every fit by a hundredth of a gauss.
impl Default for Window {
    fn default() -> Self {
        Window {
            n: 0.0,
            sum_r: DMat3::ZERO,
            t: [[[0.0; 3]; 3]; 3],
            sum_m: DVec3::ZERO,
            sum_mm: 0.0,
        }
    }
}

impl Window {
    fn add(&mut self, r: DMat3, m: DVec3) {
        self.n += 1.0;
        self.sum_r += r;
        for a in 0..3 {
            for i in 0..3 {
                let rai = r.col(i)[a];
                for j in 0..3 {
                    self.t[a][i][j] += rai * m[j];
                }
            }
        }
        self.sum_m += m;
        self.sum_mm += m.length_squared();
    }

    /// Σ R·P·m and Σ P·m for this arrangement.
    fn sums(&self, p: DMat3) -> (DVec3, DVec3) {
        let mut rm = DVec3::ZERO;
        for a in 0..3 {
            let mut v = 0.0;
            for i in 0..3 {
                for j in 0..3 {
                    let pij = p.col(j)[i];
                    if pij != 0.0 {
                        v += pij * self.t[a][i][j];
                    }
                }
            }
            rm[a] = v;
        }
        (rm, p * self.sum_m)
    }
}

/// Every signed permutation of three axes that could be told apart: 6 orders × 4 sign patterns.
///
/// Not 8. An arrangement and its exact negative explain any reading equally well — the field
/// simply comes out pointing the other way, with the offset negated — so nothing can tell
/// them apart. Nothing needs to: the anchor compares headings, and both turn together. So the
/// first axis always keeps its sign.
fn arrangements() -> Vec<HardIron> {
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut out = Vec::with_capacity(24);
    for order in ORDERS {
        for signs in (0..8).step_by(2) {
            let sign = |bit: usize| if signs & (1 << bit) == 0 { 1.0 } else { -1.0 };
            out.push(HardIron {
                axes: [(order[0], sign(0)), (order[1], sign(1)), (order[2], sign(2))],
                offset: DVec3::ZERO,
            });
        }
    }
    out
}

pub struct HardIronFit {
    windows: VecDeque<Window>,
    current: Window,
    window_age: f64,
    since_sample: f64,
    state: FitState,
    arrangements: Vec<HardIron>,
}

impl Default for HardIronFit {
    fn default() -> Self {
        Self::new()
    }
}

impl HardIronFit {
    pub fn new() -> Self {
        Self {
            windows: VecDeque::new(),
            current: Window::default(),
            window_age: 0.0,
            since_sample: 0.0,
            state: FitState::Gathering { seconds: 0.0 },
            arrangements: arrangements(),
        }
    }

    pub fn state(&self) -> FitState {
        self.state
    }

    /// Feed one IMU sample: the orientation the gyro believes, how fast the head is turning,
    /// and the raw magnetometer reading in the gyro's canonical frame. Returns true when a
    /// window closed and the state may have changed.
    pub fn feed(&mut self, q: DQuat, rate_dps: f64, raw_mag: DVec3, dt: f64) -> bool {
        self.window_age += dt;
        self.since_sample += dt;
        if self.since_sample >= SAMPLE_EVERY_S {
            self.since_sample = 0.0;
            // A reading outside anything a magnetometer near a head can see is a glitch — the
            // XREAL Air sends (−16, −16, −16) G now and then, and one such sample outweighs a
            // window's worth of real ones in a least-squares fit.
            let plausible = (0.02..PLAUSIBLE_GAUSS).contains(&raw_mag.length());
            if rate_dps < MAX_RATE_DPS && plausible {
                self.current.add(DMat3::from_quat(q), raw_mag);
            }
        }
        if self.window_age < WINDOW_S {
            return false;
        }
        self.window_age = 0.0;
        let closed = std::mem::take(&mut self.current);
        if closed.n >= 10.0 {
            self.windows.push_back(closed);
            while self.windows.len() > WINDOWS {
                self.windows.pop_front();
            }
        }
        self.state = self.solve();
        true
    }

    fn solve(&self) -> FitState {
        let total: f64 = self.windows.iter().map(|w| w.n).sum();
        if total < MIN_SAMPLES {
            return FitState::Gathering {
                seconds: total * SAMPLE_EVERY_S,
            };
        }
        // How much the head has turned is the same whatever the arrangement.
        let mut info = DMat3::ZERO;
        for w in &self.windows {
            info += DMat3::IDENTITY * w.n - w.sum_r.transpose() * w.sum_r / w.n;
        }
        let coverage = min_eigenvalue(&info) / total;
        if coverage < MIN_COVERAGE {
            return FitState::NarrowView { coverage };
        }
        let Some(inverse) = invert(&info) else {
            return FitState::NarrowView { coverage };
        };

        let scored = self.scores(&inverse, total);
        let (share, fit, field) = scored[0];
        if share > MAX_RESIDUAL_SHARE || !(0.05..=1.0).contains(&field) {
            return FitState::NoFit {
                residual_share: share,
            };
        }
        if share > DECISIVE * scored[1].0 {
            return FitState::Undecided;
        }
        FitState::Fitted {
            fit,
            field,
            residual_share: share,
            coverage,
        }
    }

    /// Every arrangement, scored by how steady the field it leaves is: (share of the field
    /// left unexplained, the fit, Earth's field), best first.
    fn scores(&self, inverse: &DMat3, total: f64) -> Vec<(f64, HardIron, f64)> {
        let mut scored: Vec<(f64, HardIron, f64)> = self
            .arrangements
            .iter()
            .map(|arrangement| {
                let p = arrangement.matrix();
                let sums: Vec<(DVec3, DVec3)> = self.windows.iter().map(|w| w.sums(p)).collect();
                let mut rhs = DVec3::ZERO;
                for (w, (rm, m)) in self.windows.iter().zip(&sums) {
                    rhs += *m - w.sum_r.transpose() * *rm / w.n;
                }
                let c = *inverse * rhs;
                let mut squares = 0.0;
                let mut field = 0.0;
                for (w, (rm, m)) in self.windows.iter().zip(&sums) {
                    let f = (*rm - w.sum_r * c) / w.n;
                    squares += w.sum_mm - 2.0 * c.dot(*m) + w.n * c.length_squared()
                        - w.n * f.length_squared();
                    field += f.length() * w.n;
                }
                let field = field / total;
                let rms = (squares.max(0.0) / total).sqrt();
                (
                    rms / field.max(1e-6),
                    HardIron {
                        offset: c,
                        ..*arrangement
                    },
                    field,
                )
            })
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored
    }

    /// For looking at a recording: coverage so far and every arrangement's score, whatever the
    /// thresholds would decide. `None` until there are samples and some rotation at all.
    pub fn ranking(&self) -> Option<(f64, Vec<(f64, HardIron, f64)>)> {
        let total: f64 = self.windows.iter().map(|w| w.n).sum();
        if total < 1.0 {
            return None;
        }
        let mut info = DMat3::ZERO;
        for w in &self.windows {
            info += DMat3::IDENTITY * w.n - w.sum_r.transpose() * w.sum_r / w.n;
        }
        let inverse = invert(&info)?;
        Some((min_eigenvalue(&info) / total, self.scores(&inverse, total)))
    }
}

/// The smallest eigenvalue of a symmetric 3×3 matrix, in closed form.
fn min_eigenvalue(m: &DMat3) -> f64 {
    let a = |r: usize, c: usize| m.col(c)[r];
    let p1 = a(0, 1).powi(2) + a(0, 2).powi(2) + a(1, 2).powi(2);
    if p1 < 1e-18 {
        return a(0, 0).min(a(1, 1)).min(a(2, 2));
    }
    let q = (a(0, 0) + a(1, 1) + a(2, 2)) / 3.0;
    let p2 = (a(0, 0) - q).powi(2) + (a(1, 1) - q).powi(2) + (a(2, 2) - q).powi(2) + 2.0 * p1;
    let p = (p2 / 6.0).sqrt();
    let b = (*m - DMat3::IDENTITY * q) * (1.0 / p);
    let r = (b.determinant() / 2.0).clamp(-1.0, 1.0);
    let phi = r.acos() / 3.0;
    // The three are q + 2p·cos(phi + 2πk/3); the smallest is k = 1.
    q + 2.0 * p * (phi + 2.0 * std::f64::consts::PI / 3.0).cos()
}

fn invert(m: &DMat3) -> Option<DMat3> {
    let det = m.determinant();
    (det.abs() > 1e-12).then(|| m.inverse())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A head that looks around: yaw sweeping ±60°, pitch ±20° at another rate, a little roll.
    fn head(t: f64) -> DQuat {
        let yaw = 60f64.to_radians() * (t * 0.21).sin();
        let pitch = 20f64.to_radians() * (t * 0.13).sin();
        let roll = 5f64.to_radians() * (t * 0.37).sin();
        DQuat::from_rotation_z(yaw) * DQuat::from_rotation_y(pitch) * DQuat::from_rotation_x(roll)
    }

    /// What a magnetometer arranged by `truth` reads at `q`, with the glasses' own field added.
    fn reading(q: DQuat, truth: &HardIron, field: DVec3) -> DVec3 {
        let arranged = q.inverse() * field + truth.offset;
        // Undo the arrangement: raw[axis] = arranged[row] * sign.
        let mut raw = DVec3::ZERO;
        for (row, (axis, sign)) in truth.axes.iter().enumerate() {
            raw[*axis] = arranged[row] * sign;
        }
        raw
    }

    fn run(fit: &mut HardIronFit, truth: &HardIron, seconds: f64, motion: impl Fn(f64) -> DQuat) {
        let field = DVec3::new(0.14, 0.02, -0.08);
        let dt = 0.001;
        let mut noise = 0x2545F4914F6CDD1Du64;
        let mut jitter = || {
            noise ^= noise << 13;
            noise ^= noise >> 7;
            noise ^= noise << 17;
            (noise as f64 / u64::MAX as f64 - 0.5) * 0.004
        };
        let steps = (seconds / dt) as usize;
        for i in 0..steps {
            let t = i as f64 * dt;
            let q = motion(t);
            let m = reading(q, truth, field) + DVec3::new(jitter(), jitter(), jitter());
            fit.feed(q, 10.0, m, dt);
        }
    }

    #[test]
    fn an_offset_bigger_than_the_field_is_found() {
        // HoloFrame's measurement: about 0.25 G of offset against about 0.16 G of field.
        let truth = HardIron {
            axes: [(0, 1.0), (1, 1.0), (2, 1.0)],
            offset: DVec3::new(0.18, -0.12, 0.13),
        };
        let mut fit = HardIronFit::new();
        run(&mut fit, &truth, 240.0, head);
        match fit.state() {
            FitState::Fitted { fit, field, .. } => {
                assert_eq!(fit.axes, truth.axes);
                assert!(
                    (fit.offset - truth.offset).length() < 0.002,
                    "fitted {:?}, truth {:?}",
                    fit.offset,
                    truth.offset
                );
                assert!((field - 0.162).abs() < 0.01, "field {field}");
            }
            other => panic!("expected a fit, got {other:?}"),
        }
    }

    #[test]
    fn axes_that_are_not_the_gyros_are_found_too() {
        let truth = HardIron {
            axes: [(1, -1.0), (0, 1.0), (2, -1.0)],
            offset: DVec3::new(-0.2, 0.05, 0.1),
        };
        let mut fit = HardIronFit::new();
        run(&mut fit, &truth, 240.0, head);
        match fit.state() {
            FitState::Fitted { fit, .. } => {
                // Found as its negative, which is the same thing: see `arrangements`.
                assert_eq!(
                    fit.axes,
                    [(1, 1.0), (0, -1.0), (2, 1.0)],
                    "found {}",
                    fit.axes_summary()
                );
                assert!((fit.offset + truth.offset).length() < 0.01);
            }
            other => panic!("expected a fit, got {other:?}"),
        }
    }

    #[test]
    fn only_turning_left_and_right_is_not_enough() {
        let truth = HardIron {
            offset: DVec3::new(0.1, 0.1, 0.1),
            ..HardIron::NONE
        };
        let mut fit = HardIronFit::new();
        run(&mut fit, &truth, 240.0, |t| {
            DQuat::from_rotation_z(60f64.to_radians() * (t * 0.21).sin())
        });
        assert!(
            matches!(fit.state(), FitState::NarrowView { .. }),
            "got {:?}",
            fit.state()
        );
    }

    #[test]
    fn a_field_that_turns_with_nothing_is_not_fitted() {
        // Readings that follow no field at all: every arrangement leaves a mess.
        let mut fit = HardIronFit::new();
        let mut i = 0u64;
        let dt = 0.001;
        for step in 0..240_000 {
            let t = step as f64 * dt;
            i = i.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let r = |s: u32| ((i >> s) & 0xffff) as f64 / 65535.0 - 0.5;
            fit.feed(head(t), 10.0, DVec3::new(r(0), r(16), r(32)) * 0.6, dt);
        }
        assert!(
            matches!(fit.state(), FitState::NoFit { .. } | FitState::Undecided),
            "got {:?}",
            fit.state()
        );
    }

    #[test]
    fn the_smallest_eigenvalue_is_right() {
        let m = DMat3::from_cols(
            DVec3::new(2.0, 1.0, 0.0),
            DVec3::new(1.0, 2.0, 0.0),
            DVec3::new(0.0, 0.0, 5.0),
        );
        assert!((min_eigenvalue(&m) - 1.0).abs() < 1e-9);
    }
}

