//! A remote application's sound, played where its window is.
//!
//! The host sends each application's sound on a stream of its own (see
//! `spatiand_stream::audio`). Here each stream gets a player — a `pw-cat` fed on its standard
//! input — aimed at the sink of that application's window, which is the same kind of sink a
//! local application plays into, and so is placed in the room the same way: a video playing in
//! a remote browser sounds from the browser.
//!
//! Which sink that is, is the compositor's to say, because the compositor is what gives a window
//! its sink. It says so through [`set_sinks`]; a player looks it up and moves when it changes —
//! the sound usually starts before the window has been placed. With nowhere to aim, or spatial
//! sound turned off, it plays on the default output, which is still better than on a computer in
//! another room.
//!
//! **The pipe is the buffer.** Sound over WiFi arrives in bursts, so something has to hold a
//! little back. That something is the pipe into `pw-cat`, sized at [`PIPE_BYTES`]: whatever
//! has arrived goes straight into it, and `pw-cat` draws on it at its own steady rate. An
//! earlier attempt gathered the sound here instead, waiting for 60 ms every time the queue
//! emptied — but the queue empties constantly, because everything in it is pushed into the
//! pipe at once, so it waited every few milliseconds and stuttered far worse than the jitter
//! it was meant to absorb: "ran dry 99 times in 10 s" in the log with a video playing.
//!
//! What is left here is the delay bound. A link that stalls delivers late and all at once, and
//! played as it came the sound would lag the picture by the length of the stall for ever
//! after; so nothing older than [`MAX_QUEUED_MS`] waits.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use spatiand_stream::audio::{AudioHeader, CHANNELS, FRAME_BYTES, RATE};

/// The most sound that may wait to be played, in milliseconds.
const MAX_QUEUED_MS: usize = 250;
/// What a burst may leave waiting in the pipe: 170 ms, rounded to whole pages.
const PIPE_BYTES: i32 = 32 * 1024;
const fn bytes_for(ms: usize) -> usize {
    RATE as usize * FRAME_BYTES * ms / 1000 / FRAME_BYTES * FRAME_BYTES
}
const MAX_QUEUED_BYTES: usize = bytes_for(MAX_QUEUED_MS);
/// Kept back when the queue is trimmed, so trimming does not leave it empty.
const KEEP_BYTES: usize = bytes_for(60);
/// A player with nothing to play for this long lets go of its output.
///
/// Long, because the host sends nothing during silence, and every pause in a video used to
/// end in a fresh `pw-cat` — whose start is itself a glitch — when the sound came back.
const IDLE: Duration = Duration::from_secs(60);

fn sinks() -> &'static Mutex<HashMap<String, String>> {
    static SINKS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    SINKS.get_or_init(Default::default)
}

/// Which sink each remote application's sound belongs in, by app id. The compositor's call.
pub fn set_sinks(map: HashMap<String, String>) {
    if let Ok(mut s) = sinks().lock() {
        *s = map;
    }
}

fn sink_for(app_id: &str) -> Option<String> {
    sinks().lock().ok().and_then(|s| s.get(app_id).cloned())
}

/// Read one sound stream from the host until it ends, playing it as it comes.
pub async fn receive(mut stream: quinn::RecvStream, host: String) {
    let mut first = [0u8; 8];
    if stream.read_exact(&mut first).await.is_err() {
        return;
    }
    let Some(length) = AudioHeader::length(&first) else {
        log::warn!("remote {host}: a stream that is not sound; ignoring it");
        return;
    };
    let mut body = vec![0u8; length];
    if stream.read_exact(&mut body).await.is_err() {
        return;
    }
    let Some(header) = AudioHeader::decode(&body) else {
        log::warn!("remote {host}: a sound stream with an unreadable header");
        return;
    };
    if header.rate != RATE || header.channels != CHANNELS {
        log::warn!(
            "remote {host}: {} sends {} Hz × {}, which this cannot play yet",
            header.app,
            header.rate,
            header.channels
        );
        return;
    }
    let app_id = super::app_id(&host, &header.app);
    log::info!("remote {host}: sound from {}", header.app);
    let player = Player::start(app_id);
    let mut buffer = vec![0u8; 8192];
    // Bytes of an incomplete frame, held until the rest arrives: dropping or queueing half a
    // frame would swap left and right from then on.
    let mut carry: Vec<u8> = Vec::new();
    loop {
        match stream.read(&mut buffer).await {
            Ok(Some(n)) => {
                carry.extend_from_slice(&buffer[..n]);
                let whole = carry.len() / FRAME_BYTES * FRAME_BYTES;
                if whole > 0 {
                    player.push(&carry[..whole]);
                    carry.drain(..whole);
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    log::info!("remote {host}: sound from {} ended", header.app);
}

struct Shared {
    queue: Mutex<VecDeque<u8>>,
    stop: AtomicBool,
}

/// One application's sound, on its way out through `pw-cat`.
struct Player {
    shared: Arc<Shared>,
}

impl Player {
    fn start(app_id: String) -> Player {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::with_capacity(MAX_QUEUED_BYTES * 2)),
            stop: AtomicBool::new(false),
        });
        let thread = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("remote-sound".into())
            .spawn(move || play(&app_id, &thread));
        if let Err(e) = spawned {
            log::warn!("remote sound: could not start a player: {e}");
        }
        Player { shared }
    }

    fn push(&self, pcm: &[u8]) {
        let Ok(mut queue) = self.shared.queue.lock() else {
            return;
        };
        queue.extend(pcm);
        if queue.len() > MAX_QUEUED_BYTES {
            // The oldest goes, in whole frames.
            let excess = queue.len() - KEEP_BYTES;
            queue.drain(..excess);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

struct Output {
    child: Child,
    target: Option<String>,
}

impl Output {
    fn start(app_id: &str, target: Option<String>) -> Option<Output> {
        let mut command = Command::new("pw-cat");
        command.args([
            "--playback",
            "--raw",
            "--format",
            "s16",
            "--rate",
            &RATE.to_string(),
            "--channels",
            &CHANNELS.to_string(),
            // What pw-cat itself holds. Together with the pipe this is the jitter budget.
            "--latency",
            "40ms",
            "-P",
            &format!("{{ media.name = \"{app_id}\" }}"),
        ]);
        if let Some(sink) = &target {
            command.args(["--target", sink]);
        }
        command.arg("-");
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => {
                // The pipe is where a burst waits; see the module notes.
                if let Some(stdin) = child.stdin.as_ref() {
                    use std::os::fd::AsRawFd;
                    unsafe {
                        libc::fcntl(stdin.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_BYTES);
                    }
                }
                log::info!(
                    "remote sound: {app_id} plays into {}",
                    target.as_deref().unwrap_or("the default output")
                );
                Some(Output { child, target })
            }
            Err(e) => {
                log::warn!("remote sound: could not start pw-cat: {e}");
                None
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn play(app_id: &str, shared: &Shared) {
    let mut output: Option<Output> = None;
    let mut last_sound = Instant::now();
    let mut checked = Instant::now();
    let mut chunk = Vec::with_capacity(8192);
    // Gaps long enough to hear, for the log: the evidence when someone reports a flicker.
    let mut dropouts = 0u32;
    let mut said = Instant::now();
    let mut playing = false;
    while !shared.stop.load(Ordering::Relaxed) {
        chunk.clear();
        if let Ok(mut queue) = shared.queue.lock() {
            let n = queue.len().min(8192);
            chunk.extend(queue.drain(..n));
        }
        if dropouts > 0 && said.elapsed() > Duration::from_secs(10) {
            log::info!("remote sound: {app_id} was interrupted {dropouts} time(s) in 10 s");
            dropouts = 0;
            said = Instant::now();
        }
        if chunk.is_empty() {
            // Nothing for longer than the pipe can cover is a gap that was heard. Anything
            // shorter is ordinary: what arrived has already gone into the pipe.
            let quiet = last_sound.elapsed();
            if playing && quiet > Duration::from_millis(250) {
                dropouts += 1;
                playing = false;
            }
            if output.is_some() && quiet > IDLE {
                output = None;
            }
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        last_sound = Instant::now();
        playing = true;
        // Follow the window's sink: it is often given one only after the sound has started.
        if output.is_none() || checked.elapsed() > Duration::from_millis(500) {
            checked = Instant::now();
            let target = sink_for(app_id);
            if output.as_ref().map(|o| &o.target) != Some(&target) {
                output = Output::start(app_id, target);
            }
        }
        let Some(out) = output.as_mut() else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        // Blocks once the pipe is full, which is what paces this loop.
        let written = out
            .child
            .stdin
            .as_mut()
            .map(|stdin| stdin.write_all(&chunk).is_ok())
            .unwrap_or(false);
        if !written {
            output = None;
        }
    }
}
