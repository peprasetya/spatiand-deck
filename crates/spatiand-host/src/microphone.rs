//! The wearer's microphone, as a recording device on this computer.
//!
//! A remote application that wants to be spoken into — a viewer with voice chat, a call, a
//! game with a squad in it — needs a microphone, and the only one that is any use is the one
//! on the headset, in another building perhaps. So the session sends it, on a stream of its
//! own with the same header as sound coming the other way, and here it becomes a **source in
//! the audio graph**: `pw-cat --playback` declaring itself `Audio/Source`, which is the mirror
//! of the sink trick in [`crate::audio`] and needs nothing installed either.
//!
//! **The device exists from the moment the host starts, whether or not anybody is speaking.**
//! This is the same rule [`crate::pad`] follows, for the same reason, and it was learned here
//! the same way. An application reads the list of recording devices once — Firestorm does it
//! at startup, and again when its voice preferences are opened — and a device that appears
//! afterwards is one it will never offer:
//!
//! ```text
//! 15:48:28  LLWebRTCVoiceClient::addCaptureDevice : 'Ryzen HD Audio Controller Stereo Microphone'
//! 15:48:28  LLWebRTCVoiceClient::addCaptureDevice : 'Ryzen HD Audio Controller Digital Microphone'
//! 15:48:29  microphone: an application here is listening; asking the session for the wearer's
//! 15:48:29  microphone: microphone at 48000 Hz x 1
//! ```
//!
//! One second too late, every time, and necessarily so: the old arrangement only built the
//! device once something was already recording, and nothing records from a device it cannot
//! see. So the source is opened at startup and **fed silence** whenever the wearer's voice is
//! not arriving, which costs a few kilobytes a second and nothing else.
//!
//! **Silence in the graph is not an open microphone in somebody's room.** The two questions
//! are separate and stay separate: this device is always here, and the session is only asked
//! to *capture* while an application is actually recording — see [`crate::route`]. Nothing on
//! the headset is listening because a program on this machine is running.
//!
//! Applications are pointed at it the same way their sound is caught: by name in the
//! environment, and by moving the stream when an application picks its own device — see
//! [`crate::route`].
//!
//! There is nothing to clean up. `pw-cat` reads from a pipe this process holds, so when the
//! host goes the pipe closes, `pw-cat` sees the end of its input and leaves with it. A source
//! left behind would be a microphone in the desktop's sound menu that hears nothing.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use spatiand_stream::audio::RATE;

/// What applications here record from.
pub const NODE: &str = "spatiand-host.microphone";

/// One channel, which is what the session sends: a voice is one thing.
const CHANNELS: u16 = 1;

/// How long to wait for the wearer before writing silence instead.
///
/// Long enough that a lull between two lumps of speech is never mistaken for silence — the
/// link delivers in bursts, and a gap cut into a sentence is heard. While nothing is arriving
/// the only cost of waiting is that the source underruns, which is silence by another name.
const PATIENCE: Duration = Duration::from_millis(200);

/// How much silence is written at a time when there is nothing else to write: twenty
/// milliseconds, so that speech starting again is never behind more than that.
const SILENCE_BYTES: usize = (RATE as usize / 50) * CHANNELS as usize * 2;

/// How much speech may wait for the writer before some is dropped.
///
/// Reaching this means the source has stopped draining, and holding the network task until it
/// starts again would be worse than a gap. Small on purpose: see [`ALLOWANCE`].
const QUEUE: usize = 8;

/// How much sound may sit ahead of the source before it is delay rather than buffer.
///
/// **A pipe feeding a reader that consumes in real time never catches up.** Anything written
/// faster than real time stays ahead for the rest of the session: one burst — the link
/// delivering a lump it was holding, a scheduling hiccup, a retransmission — and every word
/// after it is late by that much, permanently. Nothing drains it, because the far end takes
/// exactly one second of sound per second and not a byte more.
///
/// That is what half a second of lag on a voice sounds like, and no amount of small quanta
/// anywhere else in the chain fixes it: measured on both machines, every node in this path
/// runs at a quantum of 512 samples or less, about ten milliseconds. The delay was never in
/// the audio graph. It was in the queue in front of it.
///
/// So the writer keeps its own reckoning of how much it has put in that nobody has played
/// yet, and when that passes this, it throws sound away instead of adding to it. Sixty
/// milliseconds is enough to ride out ordinary jitter and short enough that a conversation
/// does not feel like one held over a satellite.
const ALLOWANCE: Duration = Duration::from_millis(60);

/// The one microphone, made when the host starts and lasting as long as it does.
///
/// There is only ever one: one node name, one device in every application's list.
pub fn shared() -> &'static Mutex<Microphone> {
    static MICROPHONE: OnceLock<Mutex<Microphone>> = OnceLock::new();
    MICROPHONE.get_or_init(|| Mutex::new(Microphone::new()))
}

/// Open the device, before any application looks for one. See the module notes.
pub fn start() {
    let _ = shared();
}

/// The wearer's microphone, as this machine sees it.
pub struct Microphone {
    live: Option<Live>,
    /// Said once, not once for every piece that cannot be written.
    complained: bool,
}

/// An open source, and the way to put sound into it.
struct Live {
    /// What shape it was opened in, so a stream that arrives in another is answered with a
    /// new one.
    shape: (u32, u16),
    pcm: SyncSender<Vec<u8>>,
}

impl Microphone {
    /// Open the source at the shape the session sends, and start feeding it.
    pub fn new() -> Microphone {
        Microphone {
            live: open(RATE, CHANNELS),
            complained: false,
        }
    }

    /// Play a piece of what the wearer said into the graph.
    pub fn feed(&mut self, pcm: &[u8], rate: u32, channels: u16) {
        if self.live.as_ref().is_some_and(|live| live.shape != (rate, channels)) {
            log::info!("microphone: the wearer's is {rate} Hz x {channels}; opening it again");
            self.live = None;
        }
        if self.live.is_none() {
            self.live = open(rate, channels);
        }
        let Some(live) = self.live.as_ref() else {
            return;
        };
        match live.pcm.try_send(pcm.to_vec()) {
            Ok(()) => self.complained = false,
            Err(TrySendError::Full(_)) => {
                if !self.complained {
                    self.complained = true;
                    log::warn!("microphone: the source is not keeping up; some speech is lost");
                }
            }
            // The writer has gone, and with it the device. The next piece opens another.
            Err(TrySendError::Disconnected(_)) => {
                log::warn!("microphone: the source stopped listening");
                self.live = None;
            }
        }
    }

    /// The session has stopped sending. **The device stays**; it goes quiet, which is what a
    /// real microphone in a silent room does, and an application recording from it carries on
    /// without noticing that anything happened.
    pub fn quiet(&mut self) {}
}

impl Default for Microphone {
    fn default() -> Microphone {
        Microphone::new()
    }
}

fn open(rate: u32, channels: u16) -> Option<Live> {
    let properties = format!(
        "{{ media.class = Audio/Source node.name = \"{NODE}\" node.description = \
         \"Headset Microphone (Spatiand)\" }}"
    );
    let child = Command::new("pw-cat")
        .args([
            "--playback",
            "--raw",
            "--format",
            "s16",
            "--rate",
            &rate.to_string(),
            "--channels",
            &channels.to_string(),
            "--latency",
            "20ms",
            "-P",
            &properties,
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            log::warn!("microphone: could not start pw-cat ({e}); nothing here can be spoken into");
            return None;
        }
    };
    let Some(stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let (pcm, waiting) = sync_channel(QUEUE);
    let per_second = f64::from(rate) * f64::from(channels) * 2.0;
    let started = std::thread::Builder::new()
        .name("microphone".into())
        .spawn(move || {
            write(stdin, &waiting, per_second);
            let _ = child.kill();
            let _ = child.wait();
        });
    if let Err(e) = started {
        log::warn!("microphone: could not feed the source ({e}); nothing here can be spoken into");
        return None;
    }
    log::info!(
        "microphone: applications here will see one recording device, {NODE}, \
         quiet until the wearer speaks"
    );
    Some(Live {
        shape: (rate, channels),
        pcm,
    })
}

/// Keep the source fed: what the wearer said when there is any, silence when there is not,
/// and nothing at all when what is already in front of it would be heard as delay.
///
/// `ahead` is how much has been written that the far end has not played yet, in bytes. It
/// grows by every write and falls with the clock, because the far end plays in real time and
/// at no other speed. See [`ALLOWANCE`] for why this is kept rather than trusting the pipe to
/// push back.
fn write(mut stdin: impl Write, waiting: &Receiver<Vec<u8>>, per_second: f64) {
    let silence = vec![0u8; SILENCE_BYTES];
    let allowance = per_second * ALLOWANCE.as_secs_f64();
    let mut ahead = 0.0f64;
    let mut last = Instant::now();
    let mut thrown_away = 0usize;
    let mut complained = false;
    loop {
        let said = match waiting.recv_timeout(PATIENCE) {
            Ok(said) => Some(said),
            Err(RecvTimeoutError::Timeout) => None,
            // Nobody will ever send again: the microphone itself has been dropped.
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let now = Instant::now();
        ahead = (ahead - now.duration_since(last).as_secs_f64() * per_second).max(0.0);
        last = now;
        if ahead > allowance {
            // Late sound is worse than missing sound: a voice half a second behind a face is
            // harder to talk over than one with a gap in it.
            thrown_away += said.as_deref().map_or(0, <[u8]>::len);
            if !complained && thrown_away as f64 > per_second {
                complained = true;
                log::warn!(
                    "microphone: arriving faster than it can be played; catching up by \
                     dropping some of it"
                );
            }
            continue;
        }
        let bytes = said.as_deref().unwrap_or(&silence);
        if stdin.write_all(bytes).is_err() {
            return;
        }
        ahead += bytes.len() as f64;
    }
}

/// What to put in an application's environment so it records from the headset.
///
/// `PULSE_SOURCE` only; the PipeWire equivalent is `target.object`, which is one property for
/// every stream an application has and is already spoken for by its sink. An application that
/// speaks PipeWire natively and picks its own input is moved instead — see [`crate::route`].
pub fn environment() -> Vec<(String, String)> {
    vec![("PULSE_SOURCE".to_string(), NODE.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn a_quiet_link_still_feeds_the_device() {
        // Nothing to say, and the source is still written to -- which is what keeps it in the
        // graph for an application that has not started yet.
        let (pcm, waiting) = sync_channel::<Vec<u8>>(QUEUE);
        let written = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = Recorder(written.clone());
        let writer = std::thread::spawn(move || write(sink, &waiting, PER_SECOND));
        std::thread::sleep(PATIENCE * 2);
        drop(pcm);
        writer.join().expect("the writer finished");
        let written = written.lock().expect("nothing panicked");
        assert!(!written.is_empty(), "silence was written");
        assert!(written.iter().all(|b| *b == 0), "and it was silence");
    }

    #[test]
    fn what_the_wearer_says_is_written_whole_and_in_order() {
        let (pcm, waiting) = sync_channel::<Vec<u8>>(QUEUE);
        let written = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = Recorder(written.clone());
        let writer = std::thread::spawn(move || write(sink, &waiting, PER_SECOND));
        pcm.send(vec![1, 2, 3, 4]).expect("the writer is there");
        pcm.send(vec![5, 6]).expect("the writer is there");
        // Well inside PATIENCE, so no silence can be mistaken for part of it.
        std::thread::sleep(Duration::from_millis(20));
        drop(pcm);
        writer.join().expect("the writer finished");
        assert_eq!(*written.lock().expect("nothing panicked"), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn going_quiet_does_not_take_the_device_away() {
        // The failure this whole module is about: an application enumerating devices after the
        // wearer stopped speaking must still find one.
        let mut microphone = Microphone {
            live: Some(Live {
                shape: (RATE, CHANNELS),
                pcm: sync_channel(QUEUE).0,
            }),
            complained: false,
        };
        microphone.quiet();
        assert!(microphone.live.is_some());
    }

    /// 48 kHz, one channel, two bytes a sample: what the session sends.
    const PER_SECOND: f64 = 96_000.0;

    #[test]
    fn a_burst_is_thrown_away_rather_than_turned_into_delay() {
        // The half-second lag, as a test. A second of speech arrives all at once -- a lump the
        // link was holding. Writing all of it would put a second of sound in front of a reader
        // that plays one second per second, and every word after it would be a second late for
        // as long as the session lasted. What must come out the other side is the allowance
        // and not much more.
        let (pcm, waiting) = sync_channel::<Vec<u8>>(QUEUE);
        let written = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = Recorder(written.clone());
        let writer = std::thread::spawn(move || write(sink, &waiting, PER_SECOND));
        let chunk = vec![7u8; 1920];
        for _ in 0..50 {
            pcm.send(chunk.clone()).expect("the writer is there");
        }
        drop(pcm);
        writer.join().expect("the writer finished");
        let written = written.lock().expect("nothing panicked").len();
        let allowance = (PER_SECOND * ALLOWANCE.as_secs_f64()) as usize;
        assert!(
            written <= allowance + chunk.len(),
            "wrote {written} bytes, which is more than the {allowance} allowed to be waiting"
        );
        assert!(written > 0, "it must still write what it can play");
    }

    struct Recorder(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for Recorder {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("nothing panicked").extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
