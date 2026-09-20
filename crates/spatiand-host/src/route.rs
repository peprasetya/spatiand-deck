//! Putting each application's sound where it belongs, whatever the application asked for.
//!
//! [`audio`](crate::audio) gives every application a sink of its own and names it in the
//! environment — `PULSE_SINK`, `PIPEWIRE_PROPS` — which is enough for the many programs that
//! open "the default output" and let the system decide what that is. It is not enough for the
//! ones that pick a device by name.
//!
//! Firestorm is one. FMOD enumerates the sound devices itself and opens the first, so with
//! `PULSE_SINK` set to its own sink it still played into the machine's line output:
//!
//! ```text
//! LLAudioEngine_FMODSTUDIO::init(): r_name="Ryzen HD Audio Controller Line Output"
//! ```
//!
//! Nothing in the environment can reach a program that does that. What can is the graph
//! itself: PipeWire will move a stream that is already playing, the same way a volume-control
//! panel does, and the session manager honours it. So this watches the graph, finds the
//! streams belonging to applications the host started, and moves any that ended up somewhere
//! else.
//!
//! **It reads `pw-dump` and writes `pw-metadata`**, both from PipeWire's own tools, which are
//! already required for [`audio`](crate::audio) and need nothing installed beyond PipeWire
//! itself. The value that works is the sink's `object.serial` — a node *name* is accepted by
//! `pw-metadata` and quietly ignored by WirePlumber, which is an hour nobody else needs to
//! spend.
//!
//! The same pass answers a second question: **is anything here recording?** An application
//! that opens a capture stream is asking for a microphone, and the only microphone worth
//! giving it is the one on the headset. That is reported back, and the session is asked for
//! the wearer's microphone only while it is true — see [`crate::microphone`].
//!
//! It runs on a thread of its own and talks in messages, because `pw-dump` forks a process and
//! the compositor may never wait for one.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

/// How often the graph is looked at.
///
/// Sound arriving in the wrong place is heard at once, so this is as slow as it can be without
/// the wrong place lasting long enough to be annoying.
const EVERY: Duration = Duration::from_secs(1);

/// How many times one stream is moved before giving up on it.
///
/// A move that does not take hold means the session manager disagrees, and repeating it for
/// ever would be a fight nobody wins and a line in the log every second.
const ATTEMPTS: u8 = 3;

/// What the compositor knows and the router needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wiring {
    /// Catalogue id → the sink node its sound belongs in.
    pub sinks: HashMap<String, String>,
    /// The node every application here should record from, once there is one.
    pub source: Option<String>,
    /// Which process belongs to which application, as the host knows it.
    pub apps: HashMap<i32, String>,
}

/// What the router has to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Word {
    /// Whether any application here has a capture stream open. Changes only.
    Recording(bool),
}

pub struct Router {
    to: Sender<Wiring>,
    from: Receiver<Word>,
}

impl Router {
    pub fn start() -> Router {
        let (to, wiring) = std::sync::mpsc::channel();
        let (say, from) = std::sync::mpsc::channel();
        if let Err(e) = std::thread::Builder::new()
            .name("sound-router".into())
            .spawn(move || watch(&wiring, &say))
        {
            log::warn!("sound: could not watch the audio graph ({e}); an application that picks its own output device will play on this computer");
        }
        Router { to, from }
    }

    /// Say what is running now. Cheap, and safe to call every time round the loop.
    pub fn wire(&self, wiring: Wiring) {
        let _ = self.to.send(wiring);
    }

    pub fn poll(&self) -> Vec<Word> {
        self.from.try_iter().collect()
    }
}

fn watch(wiring: &Receiver<Wiring>, say: &Sender<Word>) {
    let mut current = Wiring::default();
    let mut tried: HashMap<u32, (u64, u8)> = HashMap::new();
    let mut recording = false;
    let mut complained = false;
    loop {
        // Everything that has been said since the last look; the newest is the truth.
        loop {
            match wiring.try_recv() {
                Ok(next) => current = next,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if !current.sinks.is_empty() || current.source.is_some() {
            match dump() {
                Ok(graph) => {
                    complained = false;
                    let now = act(&graph, &current, &mut tried);
                    if now != recording {
                        recording = now;
                        if say.send(Word::Recording(now)).is_err() {
                            return;
                        }
                    }
                    // Streams that have gone take their history with them.
                    tried.retain(|id, _| graph.streams.iter().any(|s| s.node == *id));
                }
                Err(e) => {
                    if !complained {
                        complained = true;
                        log::warn!("sound: could not read the audio graph ({e}); an application that picks its own output device will play on this computer");
                    }
                }
            }
        }
        std::thread::sleep(EVERY);
    }
}

/// Move whatever is in the wrong place, and say whether anything is recording.
fn act(graph: &Graph, wiring: &Wiring, tried: &mut HashMap<u32, (u64, u8)>) -> bool {
    let mut recording = false;
    for stream in &graph.streams {
        // The host's own sinks and sources are not applications.
        if stream.name.starts_with("spatiand-host.") {
            continue;
        }
        let Some(pid) = stream.pid else { continue };
        let Some(app) = app_of(pid, &wiring.apps) else {
            continue;
        };
        let wanted = if stream.capture {
            recording = true;
            wiring.source.clone()
        } else {
            wiring.sinks.get(&app).cloned()
        };
        let Some(wanted) = wanted else { continue };
        let Some(&serial) = graph.serials.get(&wanted) else {
            continue;
        };
        let Some(&device) = graph.ids.get(&wanted) else {
            continue;
        };
        if graph.linked_to(stream.node, device) {
            tried.remove(&stream.node);
            continue;
        }
        let attempts = match tried.get(&stream.node) {
            Some((was, n)) if *was == serial => *n,
            _ => 0,
        };
        if attempts >= ATTEMPTS {
            continue;
        }
        tried.insert(stream.node, (serial, attempts + 1));
        if attempts + 1 == ATTEMPTS {
            log::warn!(
                "sound: {app}'s {} will not move to {wanted}; it stays where it is",
                if stream.capture { "recording" } else { "sound" }
            );
        } else {
            log::info!(
                "sound: moving {app}'s {} to {wanted}",
                if stream.capture { "recording" } else { "sound" }
            );
        }
        move_to(stream.node, serial);
    }
    recording
}

/// Ask the session manager to move one stream, by the target's serial.
fn move_to(node: u32, serial: u64) {
    let done = Command::new("pw-metadata")
        .args([
            "-n",
            "default",
            &node.to_string(),
            "target.object",
            &serial.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Err(e) = done {
        log::warn!("sound: could not move a stream ({e})");
    }
}

/// Walk up from a process looking for one the host started, as the compositor does for windows.
fn app_of(pid: i32, apps: &HashMap<i32, String>) -> Option<String> {
    let mut pid = pid;
    for _ in 0..8 {
        if let Some(app) = apps.get(&pid) {
            return Some(app.clone());
        }
        match crate::state::parent_of(pid) {
            Some(parent) if parent > 1 => pid = parent,
            _ => return None,
        }
    }
    None
}

/// One playing or recording stream in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub node: u32,
    pub name: String,
    pub pid: Option<i32>,
    /// True for a stream that is recording rather than playing.
    pub capture: bool,
}

/// As much of the audio graph as this needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Graph {
    /// Sink and source node names → the serial the session manager wants to be given.
    pub serials: HashMap<String, u64>,
    /// The same, by node id, for reading the links.
    pub ids: HashMap<String, u32>,
    pub streams: Vec<Stream>,
    /// Output node → input node, for every link.
    pub links: HashSet<(u32, u32)>,
}

impl Graph {
    fn linked_to(&self, stream: u32, device: u32) -> bool {
        self.links.contains(&(stream, device)) || self.links.contains(&(device, stream))
    }
}

fn dump() -> Result<Graph, String> {
    let out = Command::new("pw-dump")
        .output()
        .map_err(|e| format!("pw-dump: {e}"))?;
    if !out.status.success() {
        return Err(format!("pw-dump: {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse(&text)
}

/// Read what `pw-dump` printed.
///
/// Its shape is one array of objects, each with an `id`, a `type` and an `info.props`. Only
/// two kinds matter: a node, which is a device or a stream depending on its `media.class`, and
/// a link, which says what is joined to what.
pub fn parse(text: &str) -> Result<Graph, String> {
    let objects: Vec<serde_json::Value> =
        serde_json::from_str(text).map_err(|e| format!("pw-dump said something unreadable: {e}"))?;
    let mut graph = Graph::default();
    for object in &objects {
        let kind = object.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let id = object.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let props = object.pointer("/info/props");
        let Some(props) = props else { continue };
        let string = |key: &str| props.get(key).and_then(|v| v.as_str());
        // A number in `pw-dump` may be a number or a string; both appear, in the same dump.
        let number = |key: &str| {
            props
                .get(key)
                .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        };
        match kind {
            "PipeWire:Interface:Node" => {
                let class = string("media.class").unwrap_or("");
                let name = string("node.name").unwrap_or("").to_string();
                match class {
                    "Audio/Sink" | "Audio/Source" => {
                        if let Some(serial) = number("object.serial") {
                            graph.serials.insert(name.clone(), serial);
                            graph.ids.insert(name, id);
                        }
                    }
                    "Stream/Output/Audio" | "Stream/Input/Audio" => graph.streams.push(Stream {
                        node: id,
                        name,
                        pid: number("application.process.id").map(|p| p as i32),
                        capture: class == "Stream/Input/Audio",
                    }),
                    _ => {}
                }
            }
            "PipeWire:Interface:Link" => {
                if let (Some(from), Some(to)) = (number("link.output.node"), number("link.input.node"))
                {
                    graph.links.insert((from as u32, to as u32));
                }
            }
            _ => {}
        }
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"[
      { "id": 111, "type": "PipeWire:Interface:Node",
        "info": { "props": { "media.class": "Audio/Sink", "node.name": "spatiand-host.firestorm",
                             "object.serial": 450 } } },
      { "id": 56, "type": "PipeWire:Interface:Node",
        "info": { "props": { "media.class": "Audio/Sink", "node.name": "alsa_output.line",
                             "object.serial": 20 } } },
      { "id": 96, "type": "PipeWire:Interface:Node",
        "info": { "props": { "media.class": "Audio/Source", "node.name": "spatiand-host.microphone",
                             "object.serial": 470 } } },
      { "id": 80, "type": "PipeWire:Interface:Node",
        "info": { "props": { "media.class": "Stream/Output/Audio", "node.name": "Firestorm",
                             "application.process.id": "4242" } } },
      { "id": 81, "type": "PipeWire:Interface:Node",
        "info": { "props": { "media.class": "Stream/Input/Audio", "node.name": "Firestorm voice",
                             "application.process.id": 4242 } } },
      { "id": 900, "type": "PipeWire:Interface:Link",
        "info": { "props": { "link.output.node": 80, "link.input.node": 56 } } }
    ]"#;

    fn wiring() -> Wiring {
        Wiring {
            sinks: [("firestorm".to_string(), "spatiand-host.firestorm".to_string())]
                .into_iter()
                .collect(),
            source: Some("spatiand-host.microphone".into()),
            apps: [(4242, "firestorm".to_string())].into_iter().collect(),
        }
    }

    #[test]
    fn a_dump_yields_devices_streams_and_links() {
        let graph = parse(DUMP).expect("readable");
        assert_eq!(graph.serials.get("spatiand-host.firestorm"), Some(&450));
        assert_eq!(graph.serials.get("spatiand-host.microphone"), Some(&470));
        assert_eq!(graph.streams.len(), 2);
        assert!(graph.streams.iter().any(|s| s.node == 81 && s.capture));
        // A process id that came as a string is a process id.
        assert_eq!(graph.streams[0].pid, Some(4242));
        assert!(graph.linked_to(80, 56));
        assert!(!graph.linked_to(80, 111));
    }

    #[test]
    fn a_stream_in_the_wrong_place_is_moved_and_recording_is_noticed() {
        let graph = parse(DUMP).expect("readable");
        let mut tried = HashMap::new();
        // Both streams are somewhere other than where they belong, so both are moved -- and
        // the capture one means the wearer's microphone is wanted.
        assert!(act(&graph, &wiring(), &mut tried));
        assert_eq!(tried.get(&80), Some(&(450, 1)));
        assert_eq!(tried.get(&81), Some(&(470, 1)));
    }

    #[test]
    fn a_stream_already_in_the_right_place_is_left_alone() {
        let text = DUMP.replace("\"link.input.node\": 56", "\"link.input.node\": 111");
        let graph = parse(&text).expect("readable");
        let mut tried = HashMap::new();
        act(&graph, &wiring(), &mut tried);
        assert_eq!(tried.get(&80), None);
    }

    #[test]
    fn a_stream_that_will_not_move_is_given_up_on() {
        let graph = parse(DUMP).expect("readable");
        let mut tried = HashMap::new();
        for _ in 0..ATTEMPTS + 2 {
            act(&graph, &wiring(), &mut tried);
        }
        assert_eq!(tried.get(&80), Some(&(450, ATTEMPTS)));
    }

    #[test]
    fn nothing_of_anybody_elses_is_touched() {
        // A stream whose process the host never started belongs to whoever is using this
        // computer, and moving their music into a headset in another room would be rude.
        let graph = parse(DUMP).expect("readable");
        let mut tried = HashMap::new();
        let theirs = Wiring {
            apps: HashMap::new(),
            ..wiring()
        };
        assert!(!act(&graph, &theirs, &mut tried));
        assert!(tried.is_empty());
    }
}
