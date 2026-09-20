//! Each application's sound, caught on its own and sent to the session.
//!
//! An application launched here is given a sink of its own — `spatiand-host.<app>` — and told
//! to play into it, through the environment, the same way Spatiand routes a local app to its
//! window: `PULSE_SINK` for the many apps that speak PulseAudio (Chrome, Firefox, most games),
//! `PIPEWIRE_PROPS` for native PipeWire ones. Children inherit both, which is what catches a
//! browser's separate audio process.
//!
//! **The sink is `pw-record`.** A PipeWire capture stream that declares itself an
//! `Audio/Sink` *is* a sink: applications can pick it, and whatever they play into it comes
//! out of `pw-record` on standard output. So one small process per application is the whole
//! of the capture side, with nothing linked into the host and nothing to install on any
//! desktop that runs PipeWire. It lives as long as the host does, and goes when the host goes —
//! a sink left behind by a crash would be a device in the desktop's sound menu that plays into
//! nothing.
//!
//! Silence is dropped here, so an app that is not playing costs nothing on the link.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use spatiand_stream::audio::{is_silent, CHANNELS, FRAME_BYTES, RATE};
use spatiand_stream::App;
use tokio::sync::mpsc::UnboundedSender;

use crate::net::ToSession;

/// Ten milliseconds of sound: what is read and sent at a time.
const CHUNK_BYTES: usize = (RATE as usize / 100) * FRAME_BYTES;

pub struct Sounds {
    out: UnboundedSender<ToSession>,
    attached: Arc<AtomicBool>,
    sinks: HashMap<String, Child>,
    /// Set once `pw-record` is found missing, so it is said once.
    unavailable: bool,
}

impl Sounds {
    pub fn new(out: UnboundedSender<ToSession>) -> Sounds {
        Sounds {
            out,
            attached: Arc::new(AtomicBool::new(false)),
            sinks: HashMap::new(),
            unavailable: false,
        }
    }

    /// A session came or went. Sound is only sent while one is here.
    pub fn set_attached(&self, attached: bool) {
        self.attached.store(attached, Ordering::Relaxed);
    }

    /// Make sure `app` has a sink, and say what to put in its environment so it plays there.
    /// Empty when there is no way to catch its sound; it then plays wherever it would have.
    pub fn prepare(&mut self, app: &App) -> Vec<(String, String)> {
        if self.unavailable {
            return Vec::new();
        }
        let node = sink_name(&app.id);
        let alive = self
            .sinks
            .get_mut(&app.id)
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        if !alive {
            match self.start(app, &node) {
                Ok(child) => {
                    self.sinks.insert(app.id.clone(), child);
                    // Long enough for the node to be in the graph before the application
                    // looks for it; one that is not there yet sends the app to the default.
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                Err(e) => {
                    log::warn!(
                        "sound: could not start pw-record ({e}); applications here will play \
                         on this computer instead of in the headset"
                    );
                    self.unavailable = true;
                    return Vec::new();
                }
            }
        }
        vec![
            ("PULSE_SINK".into(), node.clone()),
            (
                "PIPEWIRE_PROPS".into(),
                format!("{{ target.object = \"{node}\" }}"),
            ),
        ]
    }

    /// Which sink each application has, by catalogue id. What [`crate::route`] aims at.
    pub fn sinks(&self) -> std::collections::HashMap<String, String> {
        self.sinks
            .keys()
            .map(|app| (app.clone(), sink_name(app)))
            .collect()
    }

    fn start(&self, app: &App, node: &str) -> std::io::Result<Child> {
        let description = format!("{} (Spatiand)", app.name.replace('"', "'"));
        let properties = format!(
            "{{ media.class = Audio/Sink node.name = \"{node}\" node.description = \
             \"{description}\" audio.position = [ FL FR ] }}"
        );
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
                "10ms",
                "-P",
                &properties,
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdout = child.stdout.take().expect("piped");
        let out = self.out.clone();
        let attached = self.attached.clone();
        let id = app.id.clone();
        std::thread::Builder::new()
            .name(format!("sound-{id}"))
            .spawn(move || {
                let mut chunk = vec![0u8; CHUNK_BYTES];
                loop {
                    if stdout.read_exact(&mut chunk).is_err() {
                        log::info!("sound: {id}'s sink has closed");
                        return;
                    }
                    if !attached.load(Ordering::Relaxed) || is_silent(&chunk) {
                        continue;
                    }
                    let sent = out.send(ToSession::Audio {
                        app: id.clone(),
                        pcm: chunk.clone(),
                    });
                    if sent.is_err() {
                        return;
                    }
                }
            })?;
        log::info!("sound: {} plays into {node}", app.id);
        Ok(child)
    }
}

impl Drop for Sounds {
    fn drop(&mut self) {
        for child in self.sinks.values_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The sink's name in the audio graph.
pub fn sink_name(app: &str) -> String {
    let safe: String = app
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    format!("spatiand-host.{safe}")
}
