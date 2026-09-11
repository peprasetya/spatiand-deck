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
use spatiand_audio::stage::{place, Layout, Stage, NOMINAL_HALF_STAGE};

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
        // A launched app is reaped when it exits, so its pid is free to be handed out again --
        // and an app that never opened a window leaves an entry here that `forget` never
        // clears. If a new launch is given that pid, the lookup must find the new slot, not
        // the dead app's.
        self.launched.retain(|(p, _)| *p != pid);
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

    /// Say where every window's sound is now. Called once a frame with the whole room.
    ///
    /// Takes all of them together rather than one at a time, because **a sink belongs to a
    /// process, not to a window**, and a media player is two windows: the film and the bar of
    /// controls in front of it. Both are adopted onto the same slot, so aiming them one by one
    /// aims the same sound twice and whichever came last in the list wins. That is how the
    /// film's sound came to be arriving from wherever the transport bar happened to be
    /// standing -- an ordinary window, placed by the fan, with no relation to the picture.
    ///
    /// So each sink is aimed exactly once, from whichever of its windows is the thing making
    /// the sound. See [`Source::rank`] for what that means.
    pub fn aim_all(&self, sources: &[Source], head: DQuat) {
        // Best source per sink, in one pass. Small maps: an app with more than a handful of
        // windows is unusual and one with more than a handful of *sinks* is impossible.
        let mut best: HashMap<Slot, &Source> = HashMap::new();
        for source in sources {
            let Some(slot) = self.bound.get(&source.window) else {
                continue;
            };
            best.entry(*slot)
                .and_modify(|held| {
                    if source.rank() > held.rank() {
                        *held = source;
                    }
                })
                .or_insert(source);
        }
        for (slot, source) in best {
            self.point(slot, source.stage(), head);
        }
    }

    /// Hand one sink's stage to the engine.
    fn point(&self, slot: Slot, stage: Stage, head: DQuat) {
        let Some(engine) = &self.engine else {
            return;
        };
        // How far the sound's front is from straight ahead, which decides how much of the
        // plain stereo fold is kept.
        let front = Placement {
            yaw: stage.yaw,
            pitch: stage.pitch,
            radius: 1.0,
            ..Placement::default()
        };
        let ahead = (head.inverse() * front.position().normalize())
            .x
            .clamp(-1.0, 1.0);
        // Every window's sink is the widest layout, whatever the app is using of it; the
        // renderer skips whatever is silent.
        engine.aim(slot, place(Layout::Surround714, &stage, head), ahead.acos());
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

/// One window, and what its sound would be if it were the one making it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Source {
    /// The window's id, the same one the sink was bound to.
    pub window: usize,
    pub kind: Kind,
}

/// What kind of thing a window is, for the purpose of deciding where its app's sound is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// An ordinary window: the sound is the size and place of the panel.
    Window(Placement),
    /// A surface that has taken the room. Carries the world yaw its picture is centred on --
    /// [`crate::xr::XrState::sky_yaw_urad`] in radians, which is the same frame a placement's
    /// yaw is in, so the sound and the picture agree by construction and a recentre that turns
    /// one turns the other.
    Environment { yaw: f64 },
}

impl Source {
    /// How strong this window's claim to be what the app's sound is coming from.
    ///
    /// Higher wins. The order:
    ///
    /// * **An environment beats everything.** An app that has taken the room is showing a
    ///   film; anything else it has open is a control sitting in front of that film, and a
    ///   control is not where a soundtrack comes from.
    /// * **Otherwise the biggest wins**, by solid angle. Not the focused one -- clicking a
    ///   preferences panel should not move the music -- and not the newest, which is the
    ///   accident this replaces. A player's video panel is much larger than its transport
    ///   bar, a browser's page much larger than its popup, so in every case that prompted
    ///   this the biggest window *is* the one with the picture in it.
    ///
    /// Ties go to the lower window id, purely so that two identical windows do not make the
    /// aim depend on the order a hash map happened to yield.
    fn rank(&self) -> (u8, OrderedSize, std::cmp::Reverse<usize>) {
        let (tier, size) = match self.kind {
            Kind::Environment { .. } => (1, 0.0),
            // Width over radius: how big it looks, not how big it is.
            Kind::Window(p) => (0, p.width / p.radius.max(1e-6)),
        };
        (tier, OrderedSize(size), std::cmp::Reverse(self.window))
    }

    /// Where this window's sound would come from, and how wide.
    fn stage(&self) -> Stage {
        match self.kind {
            // An immersive film is pointed, not stretched. It has no edges to size a stage
            // to, and the obvious reading -- the picture is a hundred and eighty degrees
            // wide, so the stage is too -- would put the front pair at the wearer's ears and
            // hollow out the middle of every mix. An immersive track already describes a
            // room; there is nothing to infer from the picture that the mix has not said.
            Kind::Environment { yaw } => Stage {
                yaw,
                pitch: 0.0,
                half_width: NOMINAL_HALF_STAGE,
            },
            Kind::Window(p) => Stage {
                yaw: p.yaw,
                pitch: p.pitch,
                // How wide the window looks from here, which is how wide its stage should be.
                half_width: (p.width * 0.5).atan2(p.radius).max(MIN_HALF_WIDTH),
            },
        }
    }
}

/// An angular size that can be sorted.
///
/// A window's size is a float and floats are not `Ord`; it is also finite by construction
/// here, which is what makes this sound rather than a shortcut.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
struct OrderedSize(f64);

impl Eq for OrderedSize {}

#[allow(clippy::derive_ord_xor_partial_ord)]
impl Ord for OrderedSize {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
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

    fn win(window: usize, yaw: f64, width: f64) -> Source {
        Source {
            window,
            kind: Kind::Window(Placement {
                yaw,
                width,
                ..Default::default()
            }),
        }
    }

    fn sky(window: usize, yaw: f64) -> Source {
        Source {
            window,
            kind: Kind::Environment { yaw },
        }
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
        audio.aim_all(&[win(7, 0.0, 1.1), sky(8, 1.0)], DQuat::IDENTITY);
    }

    #[test]
    fn a_reused_pid_finds_the_new_launch_not_the_dead_one() {
        // Launched apps are reaped now, so a pid can come round again while an entry for its
        // last owner -- an app that exited without ever opening a window -- is still here.
        // Finding the first match would send the new app's sound to the dead app's sink.
        let mut audio = Audio::new(false, Directness::default());
        audio.launched(4242, 1);
        audio.launched(4242, 2);
        assert_eq!(audio.slot_of_process(4242), Some(2));
    }

    /// The reported fault, as a rule about which window an app's sound follows.
    ///
    /// A player is a film and a bar of controls, on one sink because a sink belongs to the
    /// process. The bar is an ordinary window placed by the fan, so it can be anywhere; the
    /// film is the picture. Aiming per window let the bar win and the soundtrack arrived from
    /// wherever the bar was standing, which is exactly what "the sound is rotated" is.
    #[test]
    fn a_transport_bar_does_not_decide_where_the_film_sounds_from() {
        let film = sky(1, 90f64.to_radians());
        let bar = win(2, 0.0, 1.1);
        for order in [vec![film, bar], vec![bar, film]] {
            let chosen = order
                .iter()
                .max_by_key(|s| s.rank())
                .expect("something to choose");
            assert_eq!(*chosen, film, "the bar won with the sources in this order");
            assert!((chosen.stage().yaw - 90f64.to_radians()).abs() < 1e-12);
        }
    }

    /// The film's sound points where the film's picture is.
    ///
    /// Two different pieces of code turn the sky's anchor into a direction -- the shader
    /// negates it, because a window at yaw θ reads as azimuth −θ in the equirect projection,
    /// and this one does not -- so the only thing keeping them together is that both are
    /// handed the same number in the same frame. This asserts the audio half: a film anchored
    /// at yaw θ sounds like a window placed at yaw θ, which is what the wearer is comparing
    /// it against when they say the sound is not where the picture is.
    #[test]
    fn the_film_sounds_from_where_the_film_is() {
        use spatiand_audio::stage::Channel;

        for degrees in [0.0f64, 90.0, 180.0, -90.0] {
            let yaw = degrees.to_radians();
            let centre = place(Layout::Surround714, &sky(1, yaw).stage(), DQuat::IDENTITY)
                .into_iter()
                .find(|s| s.channel == Channel::FrontCentre)
                .and_then(|s| s.direction)
                .expect("the centre channel has a direction");
            let picture = Placement {
                yaw,
                radius: 1.0,
                ..Default::default()
            }
            .position()
            .normalize();
            assert!(
                centre.distance(picture) < 1e-9,
                "at {degrees} deg the sound's front is {centre:?} and the picture is {picture:?}"
            );
        }
    }

    /// An immersive mix is pointed, not stretched.
    #[test]
    fn a_film_that_fills_the_sky_does_not_put_its_front_pair_at_your_ears() {
        use spatiand_audio::stage::Channel;

        let left = place(Layout::Surround714, &sky(1, 0.0).stage(), DQuat::IDENTITY)
            .into_iter()
            .find(|s| s.channel == Channel::FrontLeft)
            .and_then(|s| s.direction)
            .expect("the front left channel has a direction");
        // Thirty degrees to the left, where the mix put it -- not ninety, which is what
        // sizing the stage to a 180 degree picture would have given.
        assert!((left.y.atan2(left.x).to_degrees() - 30.0).abs() < 1e-6);
    }

    /// With no film, the biggest window is the one with the picture in it.
    #[test]
    fn an_ordinary_app_sounds_from_its_main_window_not_its_popup() {
        let page = win(1, 0.0, 1.6);
        let popup = win(2, 0.9, 0.3);
        assert_eq!(
            *[popup, page].iter().max_by_key(|s| s.rank()).unwrap(),
            page
        );
        // And a single window is still simply itself.
        assert_eq!(*[page].iter().max_by_key(|s| s.rank()).unwrap(), page);
    }

    /// Two windows the same size must not let a hash map decide the answer.
    #[test]
    fn a_tie_is_broken_the_same_way_every_time() {
        let a = win(3, 0.0, 1.1);
        let b = win(9, 1.0, 1.1);
        assert_eq!(*[a, b].iter().max_by_key(|s| s.rank()).unwrap(), a);
        assert_eq!(*[b, a].iter().max_by_key(|s| s.rank()).unwrap(), a);
    }
}
