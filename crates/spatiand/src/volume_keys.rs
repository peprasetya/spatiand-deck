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
//!
//! ## Reading it back
//!
//! The sidecar also shows the volume as it stands, and lists the outputs and inputs, and all
//! three used to be re-read on the frame loop every two seconds: `wpctl get-volume` at 27 ms
//! and `wpctl status`, once per list, at 30 ms each. That is 87 ms every two seconds -- six
//! frames at 72 Hz, a hitch in the world as regular as a clock, for as long as the sidecar is
//! up, which on the Deck is always. The same thread does that now, and so does switching the
//! default device, which is a call of its own followed by reading the volume again.
//!
//! The same thread rather than one beside it, because a reading is only true until the next
//! change. Taken on another thread, a reading could start just before a button press and
//! land just after its report, and the slider would show the old level for the next two
//! seconds. On one thread a reading and a change cannot overlap, and a reading that something
//! was asked for during is thrown away -- see [`publish`].

use std::sync::atomic::{AtomicBool, Ordering};
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

/// How long the worker waits, after the last thing it was asked to do, before reading the
/// volume and the device lists again for the sidecar.
///
/// Soon enough that a headset plugged in, or a volume changed by something else, shows up
/// while the wearer is still looking for it. Counted from the last change rather than kept to
/// a clock, so nothing is read back in the middle of a slider drag: the finger is ahead of
/// the sound server there, and a reading would drag the handle back to where it had been.
const POLL_EVERY: Duration = Duration::from_secs(2);

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

/// Something for the worker to do.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Job {
    Volume(Change),
    /// Make this device, by PipeWire node id, the default.
    Default(u32),
}

/// What the worker hands back, each piece taken once by the frame loop.
struct Shared {
    /// The volume, as a button last set it or a poll last read it. `Some(None)` is a reading
    /// that found none to read.
    level: Mutex<Option<Option<f32>>>,
    /// The outputs and inputs, as a poll last read them.
    devices: Mutex<Option<crate::sidecar::Audio>>,
    /// Whether the lists have been looked for since the last poll. See [`Mixer::take_devices`].
    watching: AtomicBool,
}

/// The volume and the audio devices, changed and read on a thread of their own. See the
/// module notes for why.
pub struct Mixer {
    to: mpsc::Sender<Job>,
    shared: Arc<Shared>,
}

impl Mixer {
    pub fn start() -> Mixer {
        let (to, from) = mpsc::channel::<Job>();
        let shared = Arc::new(Shared {
            level: Mutex::new(None),
            devices: Mutex::new(None),
            // Set from the start, so the first poll is not skipped for want of anyone having
            // asked yet: the sidecar should have its lists when it is first drawn.
            watching: AtomicBool::new(true),
        });
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("volume".into())
            .spawn(move || work(&from, &worker));
        if let Err(e) = spawned {
            log::warn!(
                "no volume thread ({e}); volume keys will do nothing and the sidecar will \
                 show no volume or devices"
            );
        }
        Mixer { to, shared }
    }

    /// Ask for a change. Never waits.
    pub fn send(&self, change: Change) {
        self.ask(Job::Volume(change));
    }

    /// Make a device the default. Never waits.
    ///
    /// The volume is read again straight after, because the level shown belongs to whichever
    /// output is the default and has to follow the choice.
    pub fn choose_device(&self, id: u32) {
        self.ask(Job::Default(id));
        if let Ok(mut devices) = self.shared.devices.lock() {
            devices.take();
        }
    }

    /// Hand a job over, and drop any level still waiting to be picked up: it was set or read
    /// before this job, which makes it out of date the moment the job is done. The other half
    /// of [`publish`].
    fn ask(&self, job: Job) {
        let _ = self.to.send(job);
        if let Ok(mut level) = self.shared.level.lock() {
            level.take();
        }
    }

    /// The volume the worker last set or read, once, for the sidecar to show. `Some(None)`
    /// means it was read and there was none -- no sound server, or no output to be the
    /// default.
    pub fn take_level(&self) -> Option<Option<f32>> {
        self.shared.level.lock().ok()?.take()
    }

    /// The outputs and inputs as last read, once.
    ///
    /// Also what keeps them being read. The sidecar calls this every frame it is up; with no
    /// sidecar nothing does, and the worker stops asking the sound server for lists nobody
    /// will see -- otherwise a session without a second screen would run `wpctl` twice every
    /// two seconds for as long as it lasted.
    pub fn take_devices(&self) -> Option<crate::sidecar::Audio> {
        self.shared.watching.store(true, Ordering::Relaxed);
        self.shared.devices.lock().ok()?.take()
    }
}

/// The worker: jobs as they arrive, and a poll once it has been [`POLL_EVERY`] since the last.
fn work(from: &mpsc::Receiver<Job>, shared: &Shared) {
    let mut backlog: Vec<Job> = Vec::new();
    // Due straight away, so the sidecar has a volume and its lists before its first frame
    // rather than two seconds into the session.
    let mut next_poll = Instant::now();
    loop {
        if backlog.is_empty() {
            match from.recv_timeout(next_poll.saturating_duration_since(Instant::now())) {
                Ok(job) => backlog.push(job),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if shared.watching.swap(false, Ordering::Relaxed) {
                        poll(from, shared, &mut backlog);
                    }
                    next_poll = Instant::now() + POLL_EVERY;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        // Everything that queued up while the last job was being done, in one go. A held
        // button sends faster than two calls to the audio server finish, and working through
        // a backlog one call at a time would carry on changing the volume after the button
        // had been let go.
        backlog.extend(from.try_iter());
        // Changes up to the next device switch, and the switch on its own pass: a step taken
        // before the switch was a step on the old output. Whatever is left stays in the
        // backlog, which is what tells `publish` that a report is already out of date.
        let run = backlog
            .iter()
            .take_while(|job| matches!(job, Job::Volume(_)))
            .count();
        if run == 0 {
            if let Job::Default(id) = backlog.remove(0) {
                crate::system::set_default_device(id);
                poll(from, shared, &mut backlog);
            }
        } else {
            let changes: Vec<Change> = backlog
                .drain(..run)
                .filter_map(|job| match job {
                    Job::Volume(change) => Some(change),
                    Job::Default(_) => None,
                })
                .collect();
            if let Some(level) = apply(&changes) {
                publish(&shared.level, Some(level), from, &mut backlog);
            }
        }
        next_poll = Instant::now() + POLL_EVERY;
    }
}

/// Read the volume and both device lists, and hand them over.
///
/// The volume first, because it is what a device switch is waiting on. Abandoned as soon as
/// anything is asked for: a button pressed mid-poll should not wait another 30 ms behind a
/// list, and whatever was read next would be out of date before it was handed over.
fn poll(from: &mpsc::Receiver<Job>, shared: &Shared, backlog: &mut Vec<Job>) {
    publish(&shared.level, crate::system::volume(), from, backlog);
    if !backlog.is_empty() {
        return;
    }
    publish(&shared.devices, crate::system::audio_devices(), from, backlog);
}

/// Hand something the worker set or read to the frame loop -- unless a job arrived while it
/// was being done, which makes it out of date.
///
/// A reading describes the sound server as it was before that job. Handed over anyway, it
/// would put back what the job replaced: a device switch would have its tick jump back to the
/// old device, and a slider set just as a poll finished would snap back to the old level and
/// stay there until the next poll. Dropping it costs little: a button reports its own level,
/// a switch is followed by a poll, a slider already shows where it is, and whatever is left
/// the next poll puts right.
///
/// Checked with the slot locked, and [`Mixer::ask`] empties the slot after sending, so a
/// stale value cannot slip past on either side: if the job was sent before this looks it is
/// seen here, and if after, the frame loop throws the value away itself.
fn publish<T>(
    slot: &Mutex<Option<T>>,
    value: T,
    from: &mpsc::Receiver<Job>,
    backlog: &mut Vec<Job>,
) {
    if let Ok(mut slot) = slot.lock() {
        backlog.extend(from.try_iter());
        if backlog.is_empty() {
            *slot = Some(value);
        }
    }
}

/// Make a batch of changes against the real audio server, and return the level to show if a
/// button moved it.
fn apply(changes: &[Change]) -> Option<f32> {
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
                return None;
            }
        }
    } else {
        0.0
    };
    let (level, toggle, unmute) = settle(current, changes);
    let mut report = None;
    if let Some(level) = level {
        crate::system::set_volume(level);
        // Reported back only when a button moved it. A slider already shows where it is, and
        // during a drag this batch is behind the finger: reporting it would snap the slider
        // back to a value it has already left, for a frame, on every batch.
        if needs_current {
            log::info!("volume {:.0}% -> {:.0}%", current * 100.0, level * 100.0);
            report = Some(level);
        }
    }
    if unmute {
        crate::system::set_muted(false);
    } else if toggle {
        crate::system::toggle_mute();
    }
    report
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

    /// A mixer with no worker behind it, so the frame loop's side can be tested without a
    /// sound server. The receiver stands in for the worker's end of the queue.
    fn idle_mixer() -> (Mixer, mpsc::Receiver<Job>) {
        let (to, from) = mpsc::channel();
        let shared = Arc::new(Shared {
            level: Mutex::new(None),
            devices: Mutex::new(None),
            watching: AtomicBool::new(false),
        });
        (Mixer { to, shared }, from)
    }

    #[test]
    fn a_reading_with_nothing_asked_for_is_handed_over() {
        let (_mixer, from) = idle_mixer();
        let slot = Mutex::new(None);
        let mut backlog = Vec::new();
        publish(&slot, Some(0.4), &from, &mut backlog);
        assert_eq!(slot.lock().unwrap().take(), Some(Some(0.4)));
    }

    #[test]
    fn a_reading_that_something_was_asked_for_during_is_dropped() {
        // The wearer taps another output while the poll is reading the volume of the old one.
        let (mixer, from) = idle_mixer();
        mixer.choose_device(7);
        let slot = Mutex::new(None);
        let mut backlog = Vec::new();
        publish(&slot, Some(0.4), &from, &mut backlog);
        assert_eq!(*slot.lock().unwrap(), None, "the old output's volume was handed over");
        assert_eq!(backlog, vec![Job::Default(7)], "the switch has to be kept to be done next");
    }

    #[test]
    fn asking_for_something_drops_a_reading_already_handed_over() {
        // The other order: the reading was published just before the tap, and has to go too.
        let (mixer, _from) = idle_mixer();
        *mixer.shared.level.lock().unwrap() = Some(Some(0.4));
        mixer.send(Change::Set(0.8));
        assert_eq!(mixer.take_level(), None, "the slider would snap back to 40%");

        *mixer.shared.level.lock().unwrap() = Some(Some(0.4));
        *mixer.shared.devices.lock().unwrap() = Some(crate::sidecar::Audio::default());
        mixer.choose_device(7);
        assert_eq!(mixer.take_level(), None);
        assert_eq!(mixer.take_devices(), None, "the tick would jump back to the old output");
    }

    #[test]
    fn only_a_sidecar_that_is_looking_keeps_the_lists_being_read() {
        let (mixer, _from) = idle_mixer();
        assert!(!mixer.shared.watching.load(Ordering::Relaxed));
        mixer.take_devices();
        assert!(mixer.shared.watching.load(Ordering::Relaxed));
    }
}
