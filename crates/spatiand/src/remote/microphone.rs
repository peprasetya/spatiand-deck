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

use spatiand_stream::audio::{AudioHeader, Coding, RATE};

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
    // The encoder is made before the header is sent, so that a machine whose ffmpeg cannot
    // do Opus says so here and carries on with samples rather than opening a stream the other
    // end will wait on for ever.
    let mut opus = match spatiand_video::voice::Encoder::new(RATE, CHANNELS) {
        Ok(encoder) => Some(encoder),
        Err(e) => {
            log::warn!("remote: sending the microphone uncompressed ({e})");
            None
        }
    };
    let header = AudioHeader {
        app: "microphone".into(),
        rate: RATE,
        channels: CHANNELS,
        coding: if opus.is_some() { Coding::Opus } else { Coding::Pcm },
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
    // Four chunks, forty milliseconds. A voice is only worth sending while it is still
    // roughly now: a queue deep enough to ride out a stall is also deep enough to put every
    // word behind it late by the length of the stall, and unlike a file this never catches
    // up. So it is kept short and the overflow is dropped rather than waited on.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    std::thread::Builder::new()
        .name("microphone".into())
        .spawn(move || {
            let mut chunk = vec![0u8; CHUNK_BYTES];
            let mut lost = 0usize;
            let mut complained = false;
            while out.read_exact(&mut chunk).is_ok() {
                match tx.try_send(chunk.clone()) {
                    Ok(()) => {}
                    // The link has stopped taking it. Speaking into a queue would only make
                    // the delay it comes back with longer.
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        lost += 1;
                        if !complained && lost > 100 {
                            complained = true;
                            log::warn!(
                                "remote: the microphone is going out slower than it comes in; \
                                 dropping some of it rather than falling behind"
                            );
                        }
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        })
        .map_err(|e| format!("could not read the microphone: {e}"))?;

    log::info!("remote: sending the microphone");
    // Whether any of it is sound. A capture that yields nothing but digital silence looks
    // exactly like a working microphone from both ends -- the stream opens, the header
    // arrives, the host builds its source -- and the only way anybody found out last time was
    // by measuring the graph from another machine. So it is said here, once, either way.
    let mut heard = false;
    let mut silent_for = 0usize;
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
        // What actually goes out: Opus packets behind their lengths, or the samples
        // themselves when this machine has no encoder.
        let outgoing = match opus.as_mut() {
            None => pcm.clone(),
            Some(opus) => {
                let samples: Vec<i16> = pcm
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                match opus.encode(&samples) {
                    Ok(packets) => packets.iter().flat_map(|p| spatiand_stream::audio::frame(p)).collect(),
                    Err(e) => break Err(format!("the voice encoder stopped: {e}")),
                }
            }
        };
        if !heard {
            if pcm.iter().any(|b| *b != 0) {
                heard = true;
                log::info!("remote: the microphone is picking up sound");
            } else {
                silent_for += pcm.len();
                // Three seconds of perfect zeroes is not a quiet room; a quiet room has a
                // noise floor. It is a device nothing is coming out of.
                if silent_for >= CHUNK_BYTES * 300 {
                    silent_for = 0;
                    log::warn!(
                        "remote: the microphone is sending digital silence -- the chosen \
                         recording device is capturing nothing. Pick another in the sound \
                         settings."
                    );
                }
            }
        }
        // A frame that is not yet whole makes no packet, and there is nothing to send.
        if !outgoing.is_empty() && stream.write_all(&outgoing).await.is_err() {
            break Err("the host stopped listening".to_string());
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    let _ = stream.finish();
    result
}
