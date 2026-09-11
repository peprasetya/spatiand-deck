//! The volume buttons on the top of the Deck, and the volume keys on anything plugged in.
//!
//! The rocker is not part of the controller. It is wired to the embedded controller's keyboard
//! -- `AT Translated Set 2 keyboard`, the same device that carries the power button -- so to
//! libinput it is simply a keyboard with two keys on it, and its presses arrived exactly where
//! every other keypress did: at the focused application, as `KEY_VOLUMEUP`. Nothing turned the
//! volume. An application with focus received a key it had no use for; with nothing focused
//! the press went nowhere at all.
//!
//! So these three keys are taken before anything else sees them, and do what the sidecar's
//! volume control does. A USB or Bluetooth keyboard's media keys arrive through the same
//! device path and are handled by the same code, which is the point of doing it here rather
//! than in anything Deck-shaped.
//!
//! ## Holding a button
//!
//! libinput reports a press and a release and nothing in between -- it deliberately drops the
//! kernel's autorepeat -- so holding the rocker changed the volume by one step and then
//! stopped. Every system this is replacing repeats while the button is held, so [`Repeat`]
//! does it here, on the frame clock.
//!
//! ## Not on the frame loop
//!
//! Changing the volume means asking the audio server, and on this machine `wpctl` takes 27 ms
//! a call -- measured, every time, not a cold start. A step is a read and a write, and turning
//! it up also unmutes, so doing it where the keys are read would stall the renderer for about
//! eighty milliseconds a step: six frames at 72 Hz, with the world frozen to the head the
//! whole time, repeated for as long as the button is held. [`Mixer`] does it on a thread of
//! its own, in order, and reports the level back for the sidecar to show.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How far one press moves the volume, as a fraction of full.
///
/// Twenty presses from silence to full, which is what Plasma uses and close to what game mode
/// does, so the rocker feels the same in every mode on the machine.
pub const STEP: f32 = 0.05;

/// How long a button has to be held before it starts repeating.
///
/// Long enough that a single deliberate press never turns into two.
const REPEAT_DELAY: Duration = Duration::from_millis(400);

/// How often a held button steps once it is repeating.
///
/// Silence to full in about a second and a half: quick enough that nobody holds the button
/// wondering whether it is working, slow enough to let go on the level wanted.
const REPEAT_EVERY: Duration = Duration::from_millis(80);

/// Evdev codes, from `linux/input-event-codes.h`.
const KEY_MUTE: u32 = 113;
const KEY_VOLUMEDOWN: u32 = 114;
const KEY_VOLUMEUP: u32 = 115;

/// What a volume key asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Volume {
    Up,
    Down,
    Mute,
}

impl Volume {
    /// The volume key this evdev code is, if it is one.
    pub fn of(code: u32) -> Option<Volume> {
        match code {
            KEY_VOLUMEUP => Some(Volume::Up),
            KEY_VOLUMEDOWN => Some(Volume::Down),
            KEY_MUTE => Some(Volume::Mute),
            _ => None,
        }
    }
}

/// The level one step up or down from `current`, landing on the step grid.
///
/// On the grid rather than exactly one step away, so that a volume the sidecar's slider left at
/// 47% goes to 50% and 45% rather than 52% and 42% -- and so that a few presses always land on
/// a round number that can be found again. Clamped to 0..1: the slider will not go above unity
/// because the Deck's speakers distort there, and neither will this.
pub fn stepped(current: f32, up: bool) -> f32 {
    // How close to a grid line counts as on it, in steps. Generous on purpose: `wpctl` prints
    // two decimals, so a level set to exactly 0.50 can read back anywhere within half a
    // hundredth of it -- a tenth of a step -- and a level that is really on the grid must
    // not be taken for one a hair short of it, or pressing up does nothing visible.
    const ON_GRID: f32 = 0.15;
    let position = current.clamp(0.0, 1.0) / STEP;
    let next = if up {
        (position + ON_GRID).floor() + 1.0
    } else {
        (position - ON_GRID).ceil() - 1.0
    };
    (next * STEP).clamp(0.0, 1.0)
}

/// Which volume button is held, and when it next repeats.
#[derive(Debug, Default)]
pub struct Repeat {
    held: Option<(Volume, Instant)>,
}

impl Repeat {
    /// A volume button went down. Mute does not repeat -- holding it toggling on and off would
    /// be nonsense -- so only the two steps are remembered.
    pub fn press(&mut self, key: Volume, now: Instant) {
        self.held = match key {
            Volume::Up | Volume::Down => Some((key, now + REPEAT_DELAY)),
            Volume::Mute => None,
        };
    }

    /// A volume button came up. Only the one being held stops it: releasing the other half of
    /// the rocker, which can happen when a thumb rolls across it, leaves the held one going.
    pub fn release(&mut self, key: Volume) {
        if self.held.is_some_and(|(held, _)| held == key) {
            self.held = None;
        }
    }

    /// The step due this frame, if one is.
    ///
    /// At most one per call, however long the frame was: a stall should not turn into a jump,
    /// and the next frame is never more than a few milliseconds away.
    pub fn due(&mut self, now: Instant) -> Option<Volume> {
        let (key, next) = self.held?;
        if now < next {
            return None;
        }
        // Keep the cadence when on time; start it afresh from now when a frame has run long,
        // rather than scheduling the next step in the past and paying it out immediately.
        let following = if next + REPEAT_EVERY > now {
            next + REPEAT_EVERY
        } else {
            now + REPEAT_EVERY
        };
        self.held = Some((key, following));
        Some(key)
    }
}

/// Something for the volume worker to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Change {
    /// A volume key.
    Key(Volume),
    /// Go to this level, as the sidecar's slider does.
    Set(f32),
}

/// What a run of changes adds up to, starting from `current`.
///
/// Returned as the level to set -- `None` when nothing touched it -- and whether to flip the
/// mute and whether to clear it. Pure, so that what the worker does with a backlog can be
/// tested without an audio server.
///
/// Changes are applied in order rather than netted, because a step lands on the grid: up then
/// down from 47% is 45%, not 47%, and only walking them reproduces that.
pub fn settle(current: f32, changes: &[Change]) -> (Option<f32>, bool, bool) {
    let mut level = None;
    let mut toggle = false;
    let mut unmute = false;
    for change in changes {
        match *change {
            Change::Key(Volume::Mute) => toggle = !toggle,
            Change::Key(key) => {
                let up = key == Volume::Up;
                level = Some(stepped(level.unwrap_or(current), up));
                // Turning it up is a request to hear something; still muted while the number
                // climbs is the one outcome that is certainly not what was meant. It also
                // overrides a mute flip earlier in the same backlog, which it has outlived.
                if up {
                    unmute = true;
                    toggle = false;
                }
            }
            Change::Set(value) => level = Some(value.clamp(0.0, 1.0)),
        }
    }
    (level, toggle, unmute)
}

/// The volume, changed on a thread of its own. See the module notes for why.
pub struct Mixer {
    to: mpsc::Sender<Change>,
    /// The level most recently set, waiting to be picked up by the sidecar.
    level: Arc<Mutex<Option<f32>>>,
}

impl Mixer {
    pub fn start() -> Mixer {
        let (to, from) = mpsc::channel::<Change>();
        let level = Arc::new(Mutex::new(None));
        let reported = level.clone();
        let spawned = std::thread::Builder::new()
            .name("volume".into())
            .spawn(move || {
                while let Ok(first) = from.recv() {
                    // Everything that queued up while the last change was being made, in one
                    // go. A held button sends faster than two calls to the audio server
                    // finish, and working through a backlog one call at a time would carry
                    // on changing the volume after the button had been let go.
                    let mut changes = vec![first];
                    changes.extend(from.try_iter());
                    apply(&changes, &reported);
                }
            });
        if let Err(e) = spawned {
            log::warn!("no volume thread ({e}); volume keys will do nothing");
        }
        Mixer { to, level }
    }

    /// Ask for a change. Never waits.
    pub fn send(&self, change: Change) {
        let _ = self.to.send(change);
    }

    /// The level the worker last set, once, for the sidecar to show.
    pub fn take_level(&self) -> Option<f32> {
        self.level.lock().ok()?.take()
    }
}

/// Make a batch of changes against the real audio server.
fn apply(changes: &[Change], reported: &Mutex<Option<f32>>) {
    // Read only when a step needs somewhere to start from. A slider position is absolute and a
    // mute flip does not care, and each read is another 27 ms.
    let needs_current = changes
        .iter()
        .any(|c| matches!(c, Change::Key(Volume::Up | Volume::Down)));
    let current = if needs_current {
        match crate::system::volume() {
            Some(v) => v,
            None => {
                log::warn!("a volume key was pressed but the volume cannot be read");
                return;
            }
        }
    } else {
        0.0
    };
    let (level, toggle, unmute) = settle(current, changes);
    if let Some(level) = level {
        crate::system::set_volume(level);
        // Reported back only when a button moved it. A slider already shows where it is, and
        // during a drag this batch is behind the finger: reporting it would snap the slider
        // back to a value it has already left, for a frame, on every batch.
        if needs_current {
            log::info!("volume {:.0}% -> {:.0}%", current * 100.0, level * 100.0);
            if let Ok(mut slot) = reported.lock() {
                *slot = Some(level);
            }
        }
    }
    if unmute {
        crate::system::set_muted(false);
    } else if toggle {
        crate::system::toggle_mute();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn only_the_three_volume_keys_are_taken() {
        assert_eq!(Volume::of(115), Some(Volume::Up));
        assert_eq!(Volume::of(114), Some(Volume::Down));
        assert_eq!(Volume::of(113), Some(Volume::Mute));
        // The power button is on the same device, and a letter is what a keyboard is for.
        for other in [116, 30, 1, 0] {
            assert_eq!(Volume::of(other), None, "code {other} was taken");
        }
    }

    #[test]
    fn a_step_lands_on_the_grid() {
        assert!(approx(stepped(0.47, true), 0.50));
        assert!(approx(stepped(0.47, false), 0.45));
    }

    #[test]
    fn a_level_already_on_the_grid_moves_by_exactly_one_step() {
        // Including when wpctl reads it back a hair short, which is how 0.5 comes back.
        for level in [0.50, 0.4999, 0.5001, 0.496, 0.504] {
            assert!(approx(stepped(level, true), 0.55), "{level} up");
            assert!(approx(stepped(level, false), 0.45), "{level} down");
        }
    }

    #[test]
    fn the_ends_hold() {
        assert!(approx(stepped(1.0, true), 1.0));
        assert!(approx(stepped(0.98, true), 1.0));
        assert!(approx(stepped(0.0, false), 0.0));
        assert!(approx(stepped(0.02, false), 0.0));
        // Above unity -- something else set it there -- comes down onto the scale.
        assert!(approx(stepped(1.3, false), 0.95));
    }

    #[test]
    fn twenty_presses_is_silence_to_full() {
        let mut level = 0.0;
        for _ in 0..20 {
            level = stepped(level, true);
        }
        assert!(approx(level, 1.0));
    }

    #[test]
    fn a_quick_press_does_not_repeat() {
        let t = Instant::now();
        let mut r = Repeat::default();
        r.press(Volume::Up, t);
        assert_eq!(r.due(t + Duration::from_millis(100)), None);
        r.release(Volume::Up);
        assert_eq!(r.due(t + Duration::from_secs(5)), None);
    }

    #[test]
    fn holding_repeats_after_the_delay_and_then_steadily() {
        let t = Instant::now();
        let mut r = Repeat::default();
        r.press(Volume::Down, t);
        assert_eq!(r.due(t + REPEAT_DELAY - Duration::from_millis(1)), None);
        assert_eq!(r.due(t + REPEAT_DELAY), Some(Volume::Down));
        assert_eq!(r.due(t + REPEAT_DELAY + Duration::from_millis(10)), None);
        assert_eq!(r.due(t + REPEAT_DELAY + REPEAT_EVERY), Some(Volume::Down));
    }

    #[test]
    fn a_long_frame_is_one_step_not_a_jump() {
        let t = Instant::now();
        let mut r = Repeat::default();
        r.press(Volume::Up, t);
        let late = t + REPEAT_DELAY + REPEAT_EVERY * 10;
        assert_eq!(r.due(late), Some(Volume::Up));
        assert_eq!(r.due(late), None, "a stalled frame paid out more than once");
    }

    #[test]
    fn mute_never_repeats() {
        let t = Instant::now();
        let mut r = Repeat::default();
        r.press(Volume::Mute, t);
        assert_eq!(r.due(t + Duration::from_secs(5)), None);
    }

    #[test]
    fn releasing_the_other_half_of_the_rocker_does_not_stop_the_held_one() {
        let t = Instant::now();
        let mut r = Repeat::default();
        r.press(Volume::Up, t);
        r.release(Volume::Down);
        assert_eq!(r.due(t + REPEAT_DELAY), Some(Volume::Up));
    }

    #[test]
    fn a_backlog_is_walked_in_order_not_netted() {
        let up = Change::Key(Volume::Up);
        let down = Change::Key(Volume::Down);
        // Up then down from 47% is 45%: the first step lands on 50, the second on 45.
        let (level, _, _) = settle(0.47, &[up, down]);
        assert!(approx(level.unwrap(), 0.45));
    }

    #[test]
    fn a_held_button_backlog_adds_up() {
        let up = Change::Key(Volume::Up);
        let (level, _, unmute) = settle(0.20, &[up, up, up]);
        assert!(approx(level.unwrap(), 0.35));
        assert!(unmute, "turning it up should unmute");
    }

    #[test]
    fn turning_it_down_leaves_a_mute_alone() {
        let (level, toggle, unmute) = settle(0.5, &[Change::Key(Volume::Down)]);
        assert!(approx(level.unwrap(), 0.45));
        assert!(!toggle && !unmute);
    }

    #[test]
    fn two_mute_presses_cancel() {
        let mute = Change::Key(Volume::Mute);
        assert_eq!(settle(0.5, &[mute]), (None, true, false));
        assert_eq!(settle(0.5, &[mute, mute]), (None, false, false));
    }

    #[test]
    fn turning_it_up_after_muting_is_heard() {
        let (_, toggle, unmute) = settle(0.5, &[Change::Key(Volume::Mute), Change::Key(Volume::Up)]);
        assert!(unmute && !toggle);
    }

    #[test]
    fn the_slider_and_the_buttons_are_one_path() {
        // A slider position is absolute; a step after it starts from there, not from whatever
        // was read before the batch.
        let (level, _, _) = settle(0.9, &[Change::Set(0.30), Change::Key(Volume::Up)]);
        assert!(approx(level.unwrap(), 0.35));
        let (level, _, _) = settle(0.9, &[Change::Set(1.7)]);
        assert!(approx(level.unwrap(), 1.0), "the slider cannot go past unity either");
    }
}
