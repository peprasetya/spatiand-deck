//! The sound a key makes.
//!
//! Neither keyboard has any travel. In the world you press a key by pointing a ray at it from
//! two metres away; on the panel you press it through a sheet of glass with a thumb that is
//! covering the thing it just pressed. In both cases the only confirmation is visual, and in
//! both cases you are looking at what you are typing *into* rather than at the keyboard. A
//! click is the one confirmation that reaches you where you are actually looking, which is why
//! it is on by default — and why it is a wearer's choice rather than a constant, since a sound
//! on every keystroke is also the fastest way to make a quiet room unpleasant.
//!
//! ## Why a long-lived `pw-cat` and not one process per press
//!
//! Playing a file with `pw-play` costs a fork, an exec, and a stream negotiation before the
//! first sample — tens of milliseconds at best, and unbounded when the machine is busy. A
//! click that arrives 80 ms after the key does not read as confirmation, it reads as an echo.
//! So one `pw-cat` is started on the first press and kept, with the waveform written into its
//! stdin each time. That is one stream negotiation for the session instead of one per letter.
//!
//! `wpctl` is already how [`crate::system`] talks to PipeWire, so this adds no dependency —
//! and going through the sound server rather than a library means the click follows the
//! default sink, which is the device the sidecar's own picker sets.
//!
//! ## Why the stream is fed continuously, and paced
//!
//! `pw-cat` plays a *stream*. It was first driven the obvious way for a one-shot — write nine
//! milliseconds when a key goes down, write nothing until the next one — and that loses clicks.
//! Measured on the Deck against a recording of the sink's own monitor: **ten of twelve** clicks
//! arrived, and several of those that did were cut to under half their length. Between presses
//! the stream starves, and what a starved stream does on being fed again is not "carry on".
//!
//! Padding each click with silence to exceed a buffer is worse, not better — five of twelve,
//! all fragments. That result is what rules out the tempting explanation, that a write shorter
//! than one quantum sits waiting for the rest of a buffer. It is not about the size of a write.
//!
//! So the stream is fed without gaps: a chunk every few milliseconds, silence when there is
//! nothing to say, the waveform spliced in where a click belongs. Measured the same way, that
//! is **twelve of twelve**, every one identical in length — where the varying lengths before
//! were the artefacts of restarting, not the click.
//!
//! Paced, because feeding it as fast as the pipe will take leaves a backlog of silence sitting
//! in front of the next click: unpaced, the same rig put roughly nine tenths of a second
//! between the press and the sound. Each chunk is written only once it is nearly due, so what
//! is buffered ahead of real time is never more than [`LEAD`].
//!
//! ## Why a thread
//!
//! The render loop must never block on this. A pipe whose reader has stalled fills up and the
//! next write sleeps, which would be a compositor that stutters when the sound server hiccups.
//! The loop instead drops a message into a small bounded queue and moves on; if the queue is
//! full the click is dropped, which nobody can detect, whereas a dropped frame is the most
//! visible thing in a headset.
//!
//! ## Why it is not simply left running
//!
//! A continuously fed stream is a wakeup every few milliseconds and an audio device that never
//! idles, which is not a thing to leave switched on for a session that may go hours without
//! anyone typing. So the shell says when a keyboard is actually up, and only then is the
//! stream held open — warm for every key of the burst it exists to serve, and let go the moment
//! the keyboard is put away.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

/// Sample rate of the waveform, and what `pw-cat` is told to expect.
pub const RATE: u32 = 48_000;

/// How long the click lasts. Long enough to have a body, short enough that holding a key down
/// at any plausible repeat rate never overlaps itself.
const LENGTH: Duration = Duration::from_micros(9_000);

/// Peak amplitude, as a fraction of full scale.
///
/// Quiet on purpose. This plays on every keystroke and mixes with whatever the wearer is
/// actually listening to; a click loud enough to be impressive on its own is one that has to
/// be turned off within a minute.
const PEAK: f32 = 0.22;

/// The queue between the render loop and the thread that does the writing.
///
/// Two thumbs can press keys in the same frame, so one slot would drop a real press. Much more
/// than that and a stalled sound server would bank up a burst of clicks to play all at once
/// when it recovered, which is worse than having lost them.
const QUEUE: usize = 4;

/// How long to wait before trying to start `pw-cat` again after it has failed.
///
/// Without this, typing into a session with no sound server would fork a process per keystroke.
const RETRY: Duration = Duration::from_secs(5);
/// How long to wait between checks while backing off, so the thread is not spinning.
const RETRY_POLL: Duration = Duration::from_millis(100);

/// The click, as mono samples at [`RATE`].
///
/// Synthesised rather than shipped as a file: it is nine milliseconds of sound, the generator
/// is shorter than the asset would be, and an asset is one more thing that can fail to install.
///
/// Three parts, which is what a real key is. A noise burst that decays almost immediately is
/// the strike itself — the broadband transient the ear localises and reads as *contact*. Two
/// damped sines under it are the body: without them the strike alone is a tick of static that
/// sounds like a fault in the audio path rather than like something being pressed.
pub fn waveform() -> Vec<i16> {
    let n = (LENGTH.as_secs_f32() * RATE as f32) as usize;
    // A fixed seed, so the click is the same sound every time. Fresh noise per press shimmers,
    // and a keyboard whose keys each sound slightly different reads as broken rather than as
    // organic.
    let mut rng: u32 = 0x1357_9BDF;
    let mut noise = || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        (rng as f32 / u32::MAX as f32) * 2.0 - 1.0
    };

    let mut raw = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / RATE as f32;
        let strike = noise() * (-t / 0.0009).exp();
        let body = (std::f32::consts::TAU * 2_200.0 * t).sin() * (-t / 0.0022).exp();
        let ring = (std::f32::consts::TAU * 5_200.0 * t).sin() * (-t / 0.0011).exp();
        let mut s = 0.55 * strike + 0.35 * body + 0.18 * ring;
        // Both ends have to reach exactly zero, and for the same reason: this is written into
        // a stream that is silence on either side of it, and a waveform that starts or stops
        // on a non-zero sample puts a step there. A step *is* a click — so a sound whose whole
        // job is to be one click would arrive as three, and the two spurious ones are the
        // brittle digital ticks that make an audio path sound broken.
        //
        // The attack ramp is four samples, about eighty microseconds. Far too short for the
        // ear to hear as a slope, which is the point: it removes the discontinuity without
        // softening the transient that makes this read as contact rather than as a tone.
        let attack = 4.0;
        if (i as f32) < attack {
            s *= 0.5 - 0.5 * (std::f32::consts::PI * i as f32 / attack).cos();
        }
        // The tail is a millisecond, because the exponential envelopes decay towards zero
        // without ever arriving.
        let tail = 0.001 * RATE as f32;
        let left = (n - 1 - i) as f32;
        if left < tail {
            s *= 0.5 - 0.5 * (std::f32::consts::PI * left / tail).cos();
        }
        raw.push(s);
    }

    // Normalise to a known peak so the level is a decision rather than an accident of how the
    // three parts happened to line up.
    let peak = raw.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-6);
    let gain = PEAK / peak;
    raw.iter()
        .map(|s| (s * gain * i16::MAX as f32).round() as i16)
        .collect()
}

/// How much audio each write carries.
///
/// The click cannot start until the chunk it falls in is written, so this is the floor on how
/// late it can be — and it is also how often the thread wakes, so it cannot simply be made
/// tiny. Five milliseconds is a third of a frame at 72 Hz and two hundred wakeups a second,
/// which is a fair trade against a keypress you can hear the delay on.
const CHUNK: usize = (RATE as usize) / 200;

/// How far ahead of real time the stream is allowed to run.
///
/// The whole reason for pacing. Fed as fast as the pipe accepts it, the silence between presses
/// banks up in front of the next click: measured that way, roughly nine tenths of a second
/// passed between the press and the sound. Writing each chunk only once it is nearly due caps
/// what is queued ahead at this, and the cost of a smaller number is an underrun the first time
/// the thread is scheduled late.
const LEAD: Duration = Duration::from_millis(30);

/// What the thread is being asked to do.
enum Msg {
    Play,
    /// Whether a keyboard is up. The stream is only held open while one is, so a session
    /// nobody types in never opens the audio device at all.
    Wanted(bool),
}

/// A handle the render loop can click with, cheaply and without ever blocking.
pub struct Clicks {
    tx: SyncSender<Msg>,
    /// The last thing [`Clicks::wanted`] was told, so that being told it every frame costs
    /// nothing. A `Cell` because this is the render loop's own handle and never leaves it.
    wanted: std::cell::Cell<bool>,
}

impl Default for Clicks {
    fn default() -> Self {
        Self::new()
    }
}

impl Clicks {
    pub fn new() -> Self {
        let (tx, rx) = sync_channel(QUEUE);
        std::thread::Builder::new()
            .name("spatiand-click".into())
            .spawn(move || run(rx))
            .ok();
        Self {
            tx,
            wanted: std::cell::Cell::new(false),
        }
    }

    /// Make the sound, if there is room in the queue to ask for it.
    ///
    /// Never blocks and never fails: a click that could not be queued is a click nobody will
    /// miss, and the alternative is stalling the frame.
    pub fn play(&self) {
        let _ = self.tx.try_send(Msg::Play);
    }

    /// Say whether a keyboard is up, and so whether the sound device should be held open.
    ///
    /// Called every frame; only a change is sent. Passing `false` is what gives the device
    /// back — there is no separate way to release it, because two ways to say the same thing
    /// is how they come to disagree.
    pub fn wanted(&self, wanted: bool) {
        if self.wanted.replace(wanted) != wanted {
            let _ = self.tx.try_send(Msg::Wanted(wanted));
        }
    }
}

/// The thread: keep a stream fed for as long as one is wanted, and splice clicks into it.
fn run(rx: std::sync::mpsc::Receiver<Msg>) {
    let pcm = waveform();
    let mut player: Option<Player> = None;
    // Where in the waveform the click being played has reached, if one is.
    let mut at: Option<usize> = None;
    let mut wanted = false;
    let mut retry_at = Instant::now();
    // Samples written since this stream opened, and when it opened. Together they say when the
    // next chunk falls due.
    let (mut written, mut epoch) = (0u64, Instant::now());

    loop {
        loop {
            match rx.try_recv() {
                Ok(Msg::Play) => at = Some(0),
                Ok(Msg::Wanted(w)) => wanted = w,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Nothing to feed. Give the device back and sleep on the channel — an idle session
        // should cost nothing, which is the whole point of being told when a keyboard is up.
        if !wanted && at.is_none() {
            player = None;
            match rx.recv() {
                Ok(Msg::Play) => at = Some(0),
                Ok(Msg::Wanted(w)) => wanted = w,
                Err(_) => return,
            }
            continue;
        }

        if player.is_none() {
            if Instant::now() < retry_at {
                // No stream and not yet time to try again. Drop the click rather than saving
                // it: a sound that arrives seconds after the key is worse than none.
                at = None;
                std::thread::sleep(RETRY_POLL);
                continue;
            }
            player = Player::start();
            match player {
                Some(_) => {
                    written = 0;
                    epoch = Instant::now();
                }
                None => {
                    retry_at = Instant::now() + RETRY;
                    at = None;
                    continue;
                }
            }
        }

        // One chunk: the next of the waveform where a click is playing, silence elsewhere.
        let mut chunk = [0u8; CHUNK * 2];
        if let Some(pos) = at.as_mut() {
            for slot in chunk.chunks_exact_mut(2) {
                match pcm.get(*pos) {
                    Some(sample) => {
                        slot.copy_from_slice(&sample.to_le_bytes());
                        *pos += 1;
                    }
                    None => break,
                }
            }
            if *pos >= pcm.len() {
                at = None;
            }
        }

        if let Some(p) = player.as_mut() {
            if p.write(&chunk).is_err() {
                // The sound server went away, or was restarted. Let the next request open a
                // new stream rather than trying to rescue this one.
                log::debug!("the click stream closed; will reopen it");
                player = None;
                retry_at = Instant::now() + RETRY;
                continue;
            }
        }
        written += CHUNK as u64;

        // Sleep until this much audio is nearly due, so no more than `LEAD` of it is ever
        // queued ahead of real time. Behind rather than ahead, the sleep is skipped and the
        // next chunks catch up.
        let due = epoch + Duration::from_secs_f64(written as f64 / RATE as f64);
        if let Some(wait) = due
            .checked_sub(LEAD)
            .and_then(|d| d.checked_duration_since(Instant::now()))
        {
            std::thread::sleep(wait);
        }
    }
}

/// One `pw-cat` and the pipe into it.
struct Player {
    child: Child,
}

impl Player {
    fn start() -> Option<Self> {
        let child = Command::new("pw-cat")
            .args([
                "--playback",
                "--raw",
                "--format",
                "s16",
                "--rate",
                &RATE.to_string(),
                "--channels",
                "1",
                // Its own buffering, on top of what this thread keeps ahead of real time.
                "--latency",
                "15ms",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| log::info!("no keyboard click: could not start pw-cat ({e})"))
            .ok()?;
        child.stdin.as_ref()?;
        Some(Self { child })
    }

    fn write(&mut self, pcm: &[u8]) -> std::io::Result<()> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        stdin.write_all(pcm)?;
        stdin.flush()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // Closing the pipe is what tells `pw-cat` to finish; killing it as well is what stops
        // a stuck one outliving the session it belonged to.
        self.child.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_click_is_short_enough_to_keep_up_with_typing() {
        // Held keys repeat at about 30 a second once the delay is past. A click longer than
        // that interval overlaps itself into a buzz.
        let samples = waveform().len();
        let seconds = samples as f32 / RATE as f32;
        assert!(seconds < 1.0 / 30.0, "the click is {seconds}s long");
        assert!(
            seconds > 0.004,
            "the click is {seconds}s long — too short to hear as a click"
        );
    }

    #[test]
    fn it_starts_and_ends_at_silence() {
        // Not fussiness. A stream that begins or ends on a non-zero sample puts a step into the
        // audio, which is itself a click -- so the sound would be bracketed by two spurious
        // ones, and the whole thing reads as a fault in the audio path.
        let w = waveform();
        assert_eq!(*w.last().unwrap(), 0, "the click ends on a step");
        assert_eq!(w[0], 0, "the click starts on a step");
        // The attack is still abrupt: nothing is allowed to soften it into a fade-in, which is
        // the difference between a key being pressed and a tone being played.
        let attack = w
            .iter()
            .position(|s| s.unsigned_abs() as f32 > 0.5 * i16::MAX as f32 * PEAK);
        assert!(
            attack.is_some_and(|n| n < (0.0005 * RATE as f32) as usize),
            "the click takes too long to get loud: {attack:?}"
        );
    }

    #[test]
    fn it_is_quiet_enough_to_live_under_something_else() {
        // This plays on every keystroke, over whatever the wearer is listening to.
        let peak = waveform().iter().map(|s| s.unsigned_abs()).max().unwrap();
        let fraction = peak as f32 / i16::MAX as f32;
        assert!(
            (PEAK - fraction).abs() < 0.01,
            "peak is {fraction}, wanted {PEAK}"
        );
    }

    #[test]
    fn it_is_the_same_sound_every_time() {
        // Fresh noise per press would make every key sound slightly different, which reads as
        // a fault rather than as character.
        assert_eq!(waveform(), waveform());
    }

    #[test]
    fn it_has_a_strike_and_a_body_rather_than_being_one_or_the_other() {
        // The first half-millisecond is the transient; what follows is the part that makes it
        // sound like a key rather than like static. Losing either is a real regression and
        // neither is visible in a length or a peak.
        let w = waveform();
        let split = (0.0005 * RATE as f32) as usize;
        let strike: i64 = w[..split].iter().map(|s| (*s as i64).abs()).sum();
        let body: i64 = w[split..].iter().map(|s| (*s as i64).abs()).sum();
        assert!(strike > 0 && body > 0, "strike {strike}, body {body}");
        // The body is longer, so it carries more total energy; the strike is louder per sample.
        let strike_rms = strike / split as i64;
        let body_rms = body / (w.len() - split) as i64;
        assert!(strike_rms > body_rms, "the strike is not the loud part");
    }

    #[test]
    fn asking_for_a_click_never_blocks_even_with_nobody_listening() {
        // The property the render loop depends on. `play` is called from the frame; if it can
        // ever wait on a full queue or a stalled pipe, the compositor stutters when the sound
        // server does.
        let (tx, _rx) = sync_channel::<Msg>(QUEUE);
        let clicks = Clicks {
            tx,
            wanted: std::cell::Cell::new(true),
        };
        let start = Instant::now();
        for _ in 0..10_000 {
            clicks.play();
        }
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "play() blocked"
        );
    }
}

#[cfg(test)]
mod hardware {
    //! Play the click on a real machine, for ears rather than for assertions.
    //!
    //! Ignored by default, because it needs a live sound server: run it with
    //! `cargo test --bin spatiand -- --ignored audible_on_this_machine`. Everything above tests
    //! the *shape* of the waveform, which is what regressions show up in; whether it is a
    //! pleasant sound is not something an assertion can settle.
    //!
    //! Run it **on the host**, not inside the build container — `pw-cat` is part of PipeWire
    //! and the container has no reason to carry it, so in there this passes while playing
    //! nothing at all. `target/debug/deps/spatiand-* --ignored audible_on_this_machine`.
    use super::*;

    #[test]
    #[ignore]
    fn audible_on_this_machine() {
        let clicks = Clicks::new();
        // Without this there is no keyboard up, so no stream is held open and nothing plays.
        clicks.wanted(true);
        std::thread::sleep(Duration::from_millis(400));
        for _ in 0..6 {
            clicks.play();
            std::thread::sleep(Duration::from_millis(300));
        }
        // The stream is closed when `clicks` drops, so give the last one time to be heard.
        std::thread::sleep(Duration::from_millis(500));
    }
}
