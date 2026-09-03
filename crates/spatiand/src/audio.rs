//! Giving each window's sound a place in the room.
//!
//! The arithmetic and the audio graph both live in `spatiand-audio`. What is here is the part
//! that only the compositor knows: which window is where, which way the head is pointing, and
//! which running process belongs to which window.
//!
//! ## Matching a process to a window
//!
//! An app is launched before its window exists, so the sink has to be decided first and the
//! window attached to it afterwards. A slot is allocated at launch, its name goes into the
//! app's environment where the audio server will find it, and the process id is remembered.
//! When a window appears, its client's process id is walked up the process tree until it meets
//! one of those — which is what catches an app that forks before it opens a window.
//!
//! The environment is what does the routing, and it has to, because there is nothing to match
//! on afterwards: a sandboxed app reports the process id it has inside its own sandbox, which
//! bears no relation to anything on this side. The process walk only ties the *window* to the
//! slot; the sound was already aimed before the app started.
//!
//! Where that fails the window simply has no slot, and its app's audio goes wherever it would
//! have gone without any of this. That is the important property: a window whose sound cannot
//! be placed is an ordinary window with ordinary sound, not a silent one.
//!
//! ## The narrowest a stage may be
//!
//! A window's stereo image is as wide as the window looks, which is the right rule and has one
//! bad end: a small window across the room subtends a couple of degrees, and its left and
//! right would arrive from very nearly the same place. [`MIN_HALF_WIDTH`] stops the image
//! collapsing entirely. It is policy rather than geometry, which is why it is here and not in
//! the arithmetic.

use std::collections::HashMap;

use glam::DQuat;
use spatiand_audio::render::Directness;
use spatiand_audio::server::{routing_env, Engine, Head, Slot, Status};
use spatiand_audio::stage::{place, Layout, Stage};

use crate::window::Placement;

/// The narrowest a window's front stage is allowed to get, radians.
///
/// About fourteen degrees either side, which is what the default window subtends. Below that a
/// stereo mix starts to arrive from a point rather than from a picture, and the sense of two
/// channels is lost for no gain -- a window being small is not a reason for a song to become
/// mono.
const MIN_HALF_WIDTH: f64 = 14.0 * std::f64::consts::PI / 180.0;

/// How far up a process tree to look for the process we launched.
///
/// A shell wrapper, a launcher script and the app itself is three; browsers add another one or
/// two for their own supervisor. Eight is comfortably past anything real and stops a cycle in
/// a malformed `/proc` from being an infinite loop.
const ANCESTRY_DEPTH: usize = 8;

/// The rate everything runs at.
///
/// The graph's own rate on this hardware, so nothing is resampled on the way through.
const RATE: u32 = 48_000;

/// Every window's sound, and where it is.
pub struct Audio {
    engine: Option<Engine>,
    /// Slots given out at launch, waiting for a window to claim them.
    launched: Vec<(u32, Slot)>,
    /// Which window owns which slot.
    bound: HashMap<usize, Slot>,
    next: Slot,
}

impl Audio {
    /// Start the engine, or don't.
    ///
    /// Disabled is a real configuration rather than a failure: someone listening on the Deck's
    /// own speakers with the glasses on their forehead does not want their music moving around
    /// as they look about the room.
    pub fn new(enabled: bool, directness: Directness) -> Audio {
        let engine = enabled.then(|| Engine::start(RATE, Head::Measured, directness));
        if engine.is_none() {
            log::info!("spatial audio is off; every window's sound goes out as it arrives");
        }
        Audio {
            engine,
            launched: Vec::new(),
            bound: HashMap::new(),
            next: 1,
        }
    }

    pub fn is_on(&self) -> bool {
        self.engine.is_some()
    }

    /// Claim a sink for an app about to start, and say what to put in its environment.
    ///
    /// Returns nothing when spatial audio is off, and the caller then launches the app exactly
    /// as it always did.
    pub fn prepare_launch(&mut self) -> Option<(Slot, Vec<(String, String)>)> {
        let engine = self.engine.as_ref()?;
        let slot = self.next;
        self.next += 1;
        engine.open(slot);
        Some((slot, routing_env(slot)))
    }

    /// Remember which process was given which sink.
    pub fn launched(&mut self, pid: u32, slot: Slot) {
        self.launched.push((pid, slot));
        // Bounded, so a long session of opening and closing apps does not accumulate. The
        // oldest entries are the least likely to still be waiting for a window.
        if self.launched.len() > 64 {
            self.launched.remove(0);
        }
    }

    /// Attach a newly mapped window to whichever sink its process was given.
    pub fn adopt(&mut self, window: usize, client_pid: Option<u32>) {
        if self.engine.is_none() || self.bound.contains_key(&window) {
            return;
        }
        let Some(pid) = client_pid else {
            log::debug!("a window arrived without a process; its sound stays where it is");
            return;
        };
        let Some(slot) = self.slot_of_process(pid) else {
            log::debug!("no sink was claimed for pid {pid}; its sound stays where it is");
            return;
        };
        self.bound.insert(window, slot);
        log::info!("window {window} sounds through slot {slot}");
    }

    /// Walk up from `pid` looking for a process we launched.
    fn slot_of_process(&self, pid: u32) -> Option<Slot> {
        let mut at = pid;
        for _ in 0..ANCESTRY_DEPTH {
            if let Some((_, slot)) = self.launched.iter().find(|(p, _)| *p == at) {
                return Some(*slot);
            }
            at = parent_of(at)?;
            if at <= 1 {
                return None;
            }
        }
        None
    }

    /// The window has gone; take its sink with it.
    pub fn forget(&mut self, window: usize) {
        if let Some(slot) = self.bound.remove(&window) {
            self.launched.retain(|(_, s)| *s != slot);
            if let Some(engine) = &self.engine {
                engine.close(slot);
            }
        }
    }

    /// Say where a window's sound is now. Called every frame.
    pub fn aim(&self, window: usize, placement: &Placement, head: DQuat) {
        let (Some(engine), Some(slot)) = (&self.engine, self.bound.get(&window)) else {
            return;
        };
        // How wide the window looks from here, which is how wide its front stage should be.
        let half_width = (placement.width * 0.5).atan2(placement.radius);
        let stage = Stage {
            yaw: placement.yaw,
            pitch: placement.pitch,
            half_width: half_width.max(MIN_HALF_WIDTH),
        };
        // How far the window is from straight ahead, which decides how much of the plain
        // stereo fold is kept.
        let ahead = (head.inverse() * placement.position().normalize())
            .x
            .clamp(-1.0, 1.0);
        // Every window's sink is the widest layout, whatever the app is using of it; the
        // renderer skips whatever is silent.
        engine.aim(
            *slot,
            place(Layout::Surround714, &stage, head),
            ahead.acos(),
        );
    }

    /// What a window's sound is doing, for its title bar to show.
    pub fn status(&self, window: usize) -> Option<Status> {
        let engine = self.engine.as_ref()?;
        engine.status(*self.bound.get(&window)?)
    }

    /// Silence one window, or bring it back.
    pub fn set_muted(&self, window: usize, muted: bool) {
        if let (Some(engine), Some(slot)) = (&self.engine, self.bound.get(&window)) {
            engine.set_muted(*slot, muted);
        }
    }
}

/// The parent of a process, from `/proc`.
///
/// The name field can contain anything, spaces and brackets included, so it is skipped by
/// finding the *last* `)` rather than by splitting on whitespace -- a process called
/// `foo) 1 2 3 (bar` is unusual but entirely legal, and splitting naively reads the wrong
/// field and walks off to some unrelated process.
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 1..];
    // What follows is " R <ppid> ...".
    after_name.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_name_full_of_punctuation_does_not_derail_the_walk() {
        // Not hypothetical: a process can name itself anything, and the obvious way to read
        // this file -- split on spaces, take field four -- reads the wrong number and then
        // walks up somebody else's process tree.
        let line = "42 (evil) 1 2 3 (name) S 7 42 42 0 -1 4194304";
        let after = &line[line.rfind(')').unwrap() + 1..];
        let ppid: u32 = after.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert_eq!(ppid, 7);
    }

    #[test]
    fn the_stage_never_collapses_however_small_the_window_is() {
        // A window pushed far away still has two channels, and they still have to arrive from
        // two places. This is the one place the geometry is deliberately not obeyed.
        let tiny = Placement {
            width: 0.05,
            radius: 8.0,
            ..Default::default()
        };
        let honest = (tiny.width * 0.5).atan2(tiny.radius);
        assert!(
            honest < MIN_HALF_WIDTH,
            "the test window is not small enough"
        );
        assert!(honest.max(MIN_HALF_WIDTH) >= MIN_HALF_WIDTH);
    }

    #[test]
    fn a_window_with_no_sink_is_silent_about_it() {
        // Everything has to be safe to call for a window that never claimed a sink, because
        // most windows never will -- and the failure mode of getting this wrong is a panic in
        // the render loop.
        let audio = Audio::new(false, Directness::default());
        assert!(!audio.is_on());
        assert_eq!(audio.status(7), None);
        audio.set_muted(7, true);
        audio.aim(7, &Placement::default(), DQuat::IDENTITY);
    }
}
