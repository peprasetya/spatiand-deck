//! The wearer's microphone, sent up to a host that is listening.
//!
//! A remote application with voice in it — a viewer, a call, a game with a squad — needs a
//! microphone, and the useful one is here, on the headset. So this captures whatever the Deck
//! is set to record from and sends it on a stream of its own, with the same header sound
//! coming the other way carries (see `spatiand_stream::audio`): mono, because a voice is one
//! thing and mono halves what it costs.
//!
//! **Only while the host is listening.** The host asks for it when one of its applications
//! opens a capture stream and withdraws the request when the last one closes — so a
//! microphone in this room is not open because a program on another machine happens to be
//! running. If the wearer would rather it never was, `remote_microphone = false` in the
//! preferences settles it and nothing here ever starts.
//!
//! Capture is `pw-record` on the default source, which is whatever the wearer has chosen in
//! the desktop's own sound settings — the Deck's built-in microphone, the glasses, or a
//! headset. There is nothing to configure here that is not already configured there.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use spatiand_stream::audio::{AudioHeader, RATE};

/// One channel: a microphone is a voice, and stereo would be the same voice twice.
const CHANNELS: u16 = 1;
/// Ten milliseconds, to match the sound coming the other way.
const CHUNK_BYTES: usize = (RATE as usize / 100) * CHANNELS as usize * 2;

/// The wearer's microphone, if it is being sent.
#[derive(Default)]
pub struct Microphone {
    sending: Option<Arc<AtomicBool>>,
}

impl Microphone {
    /// Start or stop sending, as the host asks. Doing either twice is harmless.
    pub fn want(&mut self, connection: &quinn::Connection, host: &str, wanted: bool) {
        match (wanted, self.sending.is_some()) {
            (true, false) => self.start(connection, host),
            (false, true) => self.stop(),
            _ => {}
        }
    }

    /// The link has gone; whatever was being captured stops with it.
    pub fn stop(&mut self) {
        if let Some(stop) = self.sending.take() {
            stop.store(true, Ordering::Relaxed);
            log::info!("remote: the microphone is no longer being sent");
        }
    }

    fn start(&mut self, connection: &quinn::Connection, host: &str) {
        let stop = Arc::new(AtomicBool::new(false));
        self.sending = Some(stop.clone());
        let connection = connection.clone();
        let host = host.to_string();
        tokio::spawn(async move {
            if let Err(e) = send(&connection, &stop).await {
                log::warn!("remote {host}: the microphone stopped ({e})");
            }
        });
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn send(connection: &quinn::Connection, stop: &AtomicBool) -> Result<(), String> {
    let mut stream = connection
        .open_uni()
        .await
        .map_err(|e| format!("no stream for it: {e}"))?;
    let header = AudioHeader {
        app: "microphone".into(),
        rate: RATE,
        channels: CHANNELS,
    };
    stream
        .write_all(&header.encode())
        .await
        .map_err(|e| format!("could not say what it is: {e}"))?;

    // `pw-record` reads whatever the desktop calls the default source. Its output is read on a
    // thread of its own -- a child's pipe is a blocking read, and blocking here would stop
    // everything else this runtime is doing.
    let mut child = Command::new("pw-record")
        .args([
            "--raw",
            "--format",
            "s16",
            "--rate",
            &RATE.to_string(),
            "--channels",
            &CHANNELS.to_string(),
            "--latency",
            "20ms",
            "-P",
            "{ media.name = \"Spatiand (remote)\" }",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start pw-record: {e}"))?;
    let mut out = child.stdout.take().ok_or("pw-record has no output")?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    std::thread::Builder::new()
        .name("microphone".into())
        .spawn(move || {
            let mut chunk = vec![0u8; CHUNK_BYTES];
            while out.read_exact(&mut chunk).is_ok() {
                // Blocking on purpose: the recording is a steady trickle, and a full queue
                // means the link has stopped, which the next write will say properly.
                if tx.blocking_send(chunk.clone()).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| format!("could not read the microphone: {e}"))?;

    log::info!("remote: sending the microphone");
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
        let pcm = match waited {
            // Nothing for a moment: only the chance to notice that the host has stopped asking.
            Err(_) => continue,
            Ok(None) => break Err("the microphone closed".to_string()),
            Ok(Some(pcm)) => pcm,
        };
        if stream.write_all(&pcm).await.is_err() {
            break Err("the host stopped listening".to_string());
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    let _ = stream.finish();
    result
}
