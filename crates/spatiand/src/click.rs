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
//! ## Why a thread
//!
//! The render loop must never block on this. A pipe whose reader has stalled fills up and the
//! next write sleeps, which would be a compositor that stutters when the sound server hiccups.
//! The loop instead drops a message into a small bounded queue and moves on; if the queue is
//! full the click is dropped, which nobody can detect, whereas a dropped frame is the most
//! visible thing in a headset.

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

/// What the thread is being asked to do.
enum Msg {
    Play,
    /// Let go of the sound device. Sent when the wearer turns the click off, so that choosing
    /// silence actually releases the stream rather than leaving an idle one in the mixer.
    Release,
}

/// A handle the render loop can click with, cheaply and without ever blocking.
pub struct Clicks {
    tx: SyncSender<Msg>,
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
            .spawn(move || {
                let pcm: Vec<u8> = waveform().iter().flat_map(|s| s.to_le_bytes()).collect();
                let mut player: Option<Player> = None;
                let mut next_try = Instant::now();
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Release => player = None,
                        Msg::Play => {
                            // Anything else already queued is the same click again. Play it
                            // once: two presses in one frame are one sound, and this is also
                            // what stops a backlog turning into a rattle.
                            loop {
                                match rx.try_recv() {
                                    Ok(Msg::Play) => continue,
                                    Ok(Msg::Release) => {
                                        player = None;
                                        break;
                                    }
                                    Err(TryRecvError::Empty) => break,
                                    Err(TryRecvError::Disconnected) => return,
                                }
                            }
                            if player.is_none() {
                                if Instant::now() < next_try {
                                    continue;
                                }
                                player = Player::start();
                                if player.is_none() {
                                    next_try = Instant::now() + RETRY;
                                }
                            }
                            if let Some(p) = player.as_mut() {
                                if p.write(&pcm).is_err() {
                                    // The sound server went away, or was restarted. Drop this
                                    // one and let the next press start a new stream.
                                    log::debug!("the click stream closed; will restart it");
                                    player = None;
                                    next_try = Instant::now() + RETRY;
                                }
                            }
                        }
                    }
                }
            })
            .ok();
        Self { tx }
    }

    /// Make the sound, if there is room in the queue to ask for it.
    ///
    /// Never blocks and never fails: a click that could not be queued is a click nobody will
    /// miss, and the alternative is stalling the frame.
    pub fn play(&self) {
        let _ = self.tx.try_send(Msg::Play);
    }

    /// Give the sound device back.
    pub fn release(&self) {
        let _ = self.tx.try_send(Msg::Release);
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
                // The default is 100 ms, which would put the click a tenth of a second behind
                // the key. This is about one frame at 72 Hz.
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
        assert!(seconds > 0.004, "the click is {seconds}s long — too short to hear as a click");
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
        let attack = w.iter().position(|s| s.unsigned_abs() as f32 > 0.5 * i16::MAX as f32 * PEAK);
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
        assert!((PEAK - fraction).abs() < 0.01, "peak is {fraction}, wanted {PEAK}");
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
        let clicks = Clicks { tx };
        let start = Instant::now();
        for _ in 0..10_000 {
            clicks.play();
        }
        assert!(start.elapsed() < Duration::from_millis(200), "play() blocked");
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
        for _ in 0..6 {
            clicks.play();
            std::thread::sleep(Duration::from_millis(300));
        }
        // The stream is closed when `clicks` drops, so give the last one time to be heard.
        std::thread::sleep(Duration::from_millis(500));
    }
}
