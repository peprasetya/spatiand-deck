//! The wearer's microphone, as a recording device on this computer.
//!
//! A remote application that wants to be spoken into — a viewer with voice chat, a call, a
//! game with a squad in it — needs a microphone, and the only one that is any use is the one
//! on the headset, in another building perhaps. So the session sends it, on a stream of its
//! own with the same header as sound coming the other way, and here it becomes a **source in
//! the audio graph**: `pw-cat --playback` declaring itself `Audio/Source`, which is the mirror
//! of the sink trick in [`crate::audio`] and needs nothing installed either.
//!
//! Applications are pointed at it the same way their sound is caught: by name in the
//! environment, and by moving the stream when an application picks its own device — see
//! [`crate::route`].
//!
//! **It only exists while it is being used.** The host asks for the microphone when one of its
//! applications opens a capture stream, and the session stops sending when it stops asking, so
//! a microphone in someone's room is not open because a program on another machine is running.

use std::io::Write;
use std::process::{Child, Command, Stdio};

/// What applications here record from.
pub const NODE: &str = "spatiand-host.microphone";

/// The wearer's microphone, once some of it has arrived.
pub struct Microphone {
    child: Option<Child>,
    /// What shape the source was opened in, so a stream that changes is answered with a new one.
    shape: (u32, u16),
}

impl Microphone {
    pub fn new() -> Microphone {
        Microphone {
            child: None,
            shape: (0, 0),
        }
    }

    /// Play a piece of what the wearer said into the graph, starting the source if needed.
    pub fn feed(&mut self, pcm: &[u8], rate: u32, channels: u16) {
        if self.child.is_some() && self.shape != (rate, channels) {
            self.stop();
        }
        if self.child.is_none() {
            self.shape = (rate, channels);
            self.child = start(rate, channels);
        }
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let written = child
            .stdin
            .as_mut()
            .map(|stdin| stdin.write_all(pcm).is_ok())
            .unwrap_or(false);
        if !written {
            log::warn!("microphone: the source stopped listening");
            self.stop();
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take());
            let _ = child.kill();
            let _ = child.wait();
            log::info!("microphone: the wearer's microphone has gone");
        }
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        self.stop();
    }
}

fn start(rate: u32, channels: u16) -> Option<Child> {
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
    match child {
        Ok(child) => {
            log::info!("microphone: the wearer's microphone is here as {NODE}");
            Some(child)
        }
        Err(e) => {
            log::warn!("microphone: could not start pw-cat ({e}); nothing here can be spoken into");
            None
        }
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
