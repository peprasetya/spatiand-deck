//! Putting the renderer into the audio graph.
//!
//! This is the only part of the crate that talks to anything outside the process, and it is
//! deliberately thin: everything it decides has already been decided in [`crate::stage`] and
//! [`crate::render`]. What it does is arrange for a window's app to have somewhere to play
//! into, for that audio to be rendered, and for the result to reach the glasses.
//!
//! ## One sink per window
//!
//! Each window that makes a sound gets its own sink, named after the window, with as many
//! channels as the app asked for. An app plays into it exactly as it would into a sound card,
//! and knows nothing about any of this — which is the point, and is why an unmodified Firefox
//! or mpv or game works.
//!
//! Getting an app's audio to *its own* window's sink is the part that is normally guesswork.
//! It is not guesswork here: an app is launched with [`ROUTING_ENV`] set, which stamps the
//! target onto every stream that process and its children open. Browsers and games that play
//! audio from a child process are the usual reason this sort of matching fails, and inheriting
//! through the environment is exactly what makes them work. See [`routing_env`].
//!
//! ## Why there is a queue in the middle
//!
//! A window's audio arrives when its app produces some; the glasses ask for audio when the
//! sound card's clock says so. Two callbacks, two schedules, neither allowed to wait for the
//! other. [`crate::ring`] sits between them and absorbs the difference, at a cost of one
//! buffer's worth of delay.
//!
//! ## What runs where
//!
//! Three threads, and the split matters because two of them have deadlines:
//!
//! * The **compositor** thread calls the methods on [`Engine`]. Everything it does is a
//!   message on a queue and returns immediately.
//! * The **loop** thread owns the PipeWire connection and creates and destroys nodes.
//! * The **data** thread is PipeWire's, and runs the process callbacks. Nothing here may
//!   block it: no allocation, no waiting on a lock. Where it needs something the loop thread
//!   also touches, it takes the lock only if it is free and skips the update otherwise —
//!   which is safe precisely because every such update is the *latest* state rather than a
//!   step in a sequence, so missing one costs a few milliseconds of staleness and nothing else.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

use crate::render::{Binaural, Directness};
use crate::ring::Ring;
use crate::stage::{Channel, Layout, Speaker};

/// Tells a **native PipeWire** client which window its sound belongs to.
///
/// PipeWire reads this when any stream is created and copies the properties onto the node, and
/// a child process inherits it — which is what makes a browser's audio process land on the
/// same sink as the tab that spawned it.
pub const ROUTING_ENV: &str = "PIPEWIRE_PROPS";

/// Tells a **PulseAudio** client the same thing.
///
/// Both are needed, and finding out why cost a debugging session. Most desktop applications do
/// not speak PipeWire: Chrome, Firefox and anything built on the usual audio libraries speak
/// the PulseAudio protocol, which PipeWire also serves — and such a client never looks at
/// [`ROUTING_ENV`] at all. Its stream shows `client.api = pipewire-pulse`, and it went
/// straight to the machine's default sink while a window's own sink sat idle beside it.
///
/// There is no identifying it after the fact either, which is why this has to be got right
/// before the app starts. A sandboxed app reports the process id it has *inside* its sandbox —
/// Chrome in a Flatpak said 271 — so there is nothing to match against a process we launched.
///
/// Verified on the hardware: it reaches inside a Flatpak sandbox, and it takes precedence over
/// PulseAudio's memory of where that application's sound went last time, which otherwise
/// quietly puts every stream back where it was.
pub const PULSE_ROUTING_ENV: &str = "PULSE_SINK";

/// How much audio the queue between the two callbacks can hold, in stereo frames.
///
/// A quarter of a second. Far more than the millisecond or two of jitter it is there to
/// absorb, because the cost of it being large is only memory, while the cost of it being too
/// small is a dropout whenever the graph hiccups.
const QUEUE_FRAMES: usize = 12_000;

/// Identifies one window's audio, for as long as that window exists.
pub type Slot = u64;

/// What the shell needs to know about a window's sound, to show it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Status {
    /// The width of the connection, once an app has negotiated one.
    pub layout: Option<Layout>,
    /// What the app is *actually* using of it, which is what a window should say about itself.
    /// `None` while it is silent.
    pub sounding: Option<Layout>,
    /// Loudest sample in the last block, before anything here touched it.
    pub peak: f32,
    pub muted: bool,
}

/// The SPA channel-position constant for one of our channels.
///
/// Transcribed from `spa/param/audio/raw.h`, which is an enum with no gaps in the part we use.
/// The app's own channel map is matched against this, so an error here does not fail loudly —
/// it puts somebody's surrounds where their mains should be.
fn spa_position(channel: Channel) -> u32 {
    match channel {
        Channel::Mono => 2,
        Channel::FrontLeft => 3,
        Channel::FrontRight => 4,
        Channel::FrontCentre => 5,
        Channel::Lfe => 6,
        Channel::SideLeft => 7,
        Channel::SideRight => 8,
        Channel::RearLeft => 12,
        Channel::RearRight => 13,
        Channel::TopFrontLeft => 15,
        Channel::TopFrontRight => 17,
        Channel::TopRearLeft => 18,
        Channel::TopRearRight => 20,
    }
}

/// What a window's sink calls itself to anything listing audio devices.
///
/// Every window's sink shares it, deliberately: they are not devices anyone chooses between,
/// and the one place they might be offered as such — the sidecar's output picker — filters
/// them out by this exact string.
pub const SINK_DESCRIPTION: &str = "Spatiand window";

/// The name a window's sink is given in the audio graph.
pub fn sink_name(slot: Slot) -> String {
    format!("spatiand.window.{slot}")
}

/// What to put in the environment of an app so its audio finds its window.
///
/// Set these on the process before it starts. Everything it and its children play is then
/// aimed at that window's sink, without anything having to work out afterwards which process
/// belonged to which window — which for a sandboxed app is not possible at all.
///
/// Two variables because there are two audio protocols in use on the same machine, and an
/// application picks one without telling anybody. See [`PULSE_ROUTING_ENV`].
pub fn routing_env(slot: Slot) -> Vec<(String, String)> {
    let sink = sink_name(slot);
    vec![
        (
            ROUTING_ENV.to_string(),
            format!("{{ target.object = \"{sink}\" }}"),
        ),
        (PULSE_ROUTING_ENV.to_string(), sink),
    ]
}

/// Everything one window's sound needs, shared between the loop and data threads.
struct SlotState {
    /// Rendered stereo, waiting to go out.
    queue: Ring,
    /// Where the sound should be. Written by the loop thread, taken by the data thread when
    /// it happens to be free — the latest one is all that matters, so a skipped update costs
    /// a few milliseconds of staleness.
    aim: Mutex<Option<(Vec<Speaker>, f64)>>,
    /// A new blend, waiting to be picked up. Same discipline as `aim`.
    directness: Mutex<Option<Directness>>,
    muted: AtomicBool,
    /// Last block's peak, as `f32` bits, so the shell can read it without a lock.
    peak: AtomicU32,
    /// What the app negotiated, as a layout code, or 0 for "not yet".
    layout: AtomicU32,
    /// What it is actually using of that, as a layout code, or 0 for silence.
    sounding: AtomicU32,
    dropped: AtomicU32,
}

impl SlotState {
    fn new() -> SlotState {
        SlotState {
            queue: Ring::new(QUEUE_FRAMES * 2),
            aim: Mutex::new(None),
            directness: Mutex::new(None),
            muted: AtomicBool::new(false),
            peak: AtomicU32::new(0),
            layout: AtomicU32::new(0),
            sounding: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
        }
    }

    fn status(&self) -> Status {
        Status {
            layout: layout_from_code(self.layout.load(Ordering::Relaxed)),
            sounding: layout_from_code(self.sounding.load(Ordering::Relaxed)),
            peak: f32::from_bits(self.peak.load(Ordering::Relaxed)),
            muted: self.muted.load(Ordering::Relaxed),
        }
    }
}

fn layout_code(layout: Layout) -> u32 {
    match layout {
        Layout::Mono => 1,
        Layout::Stereo => 2,
        Layout::Surround51 => 3,
        Layout::Surround71 => 4,
        Layout::Surround714 => 5,
        Layout::Surround514 => 6,
    }
}

fn layout_from_code(code: u32) -> Option<Layout> {
    Some(match code {
        1 => Layout::Mono,
        2 => Layout::Stereo,
        3 => Layout::Surround51,
        4 => Layout::Surround71,
        5 => Layout::Surround714,
        6 => Layout::Surround514,
        _ => return None,
    })
}

enum Command {
    Open(Slot),
    Close(Slot),
    Aim {
        slot: Slot,
        speakers: Vec<Speaker>,
        off_axis: f64,
    },
    Mute {
        slot: Slot,
        muted: bool,
    },
    Directness(Directness),
    Stop,
}

/// How the head is made: measured if the machine has a dataset, worked out if not.
#[derive(Debug, Clone, Copy)]
pub enum Head {
    /// Load a SOFA dataset, falling back to the arithmetic head if it cannot be read.
    Measured,
    /// Always the arithmetic head. For a machine with no dataset, and for telling the two
    /// apart when something sounds wrong.
    Reasoned,
}

/// The audio engine: a PipeWire connection, and one sink per window that makes a sound.
pub struct Engine {
    to_loop: pw::channel::Sender<Command>,
    slots: Arc<Mutex<HashMap<Slot, Arc<SlotState>>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    rate: u32,
}

impl Engine {
    /// Start the engine on its own thread.
    ///
    /// Returns as soon as the thread is running; whether PipeWire could actually be reached is
    /// reported in the log rather than here, because a desktop with no sound server should
    /// still be a working desktop.
    pub fn start(rate: u32, head: Head, directness: Directness) -> Engine {
        let (to_loop, from_engine) = pw::channel::channel();
        let slots: Arc<Mutex<HashMap<Slot, Arc<SlotState>>>> = Arc::default();
        let thread = {
            let slots = Arc::clone(&slots);
            std::thread::Builder::new()
                .name("spatiand-audio".into())
                .spawn(move || {
                    if let Err(e) = run(rate, head, directness, slots, from_engine) {
                        log::error!("spatial audio stopped: {e}");
                    }
                })
                .ok()
        };
        Engine {
            to_loop,
            slots,
            thread,
            rate,
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Give a window somewhere to play.
    ///
    /// The sink appears in the audio graph named [`sink_name`]; an app launched with
    /// [`routing_env`] will find it.
    pub fn open(&self, slot: Slot) {
        let _ = self.to_loop.send(Command::Open(slot));
    }

    /// Take a window's sink away, when the window has gone.
    pub fn close(&self, slot: Slot) {
        let _ = self.to_loop.send(Command::Close(slot));
    }

    /// Say where a window's sound is now. Called every frame; cheap when nothing has moved.
    pub fn aim(&self, slot: Slot, speakers: Vec<Speaker>, off_axis: f64) {
        let _ = self.to_loop.send(Command::Aim {
            slot,
            speakers,
            off_axis,
        });
    }

    pub fn set_muted(&self, slot: Slot, muted: bool) {
        let _ = self.to_loop.send(Command::Mute { slot, muted });
    }

    pub fn set_directness(&self, directness: Directness) {
        let _ = self.to_loop.send(Command::Directness(directness));
    }

    /// What a window's sound is doing, for the shell to show. `None` if it has no sink.
    pub fn status(&self, slot: Slot) -> Option<Status> {
        self.slots.lock().ok()?.get(&slot).map(|s| s.status())
    }

    /// Every window currently making a sound, loudest first.
    pub fn sounding(&self, floor: f32) -> Vec<(Slot, Status)> {
        let Ok(slots) = self.slots.lock() else {
            return Vec::new();
        };
        let mut out: Vec<(Slot, Status)> = slots
            .iter()
            .map(|(k, v)| (*k, v.status()))
            .filter(|(_, s)| s.peak >= floor)
            .collect();
        out.sort_by(|a, b| b.1.peak.total_cmp(&a.1.peak));
        out
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.to_loop.send(Command::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Everything the loop thread keeps, for as long as a slot exists.
struct Node {
    state: Arc<SlotState>,
    /// Both streams are kept alive by being held here; dropping them removes the nodes.
    _sink: pw::stream::Stream,
    _sink_listener: pw::stream::StreamListener<SinkData>,
    _out: pw::stream::Stream,
    _out_listener: pw::stream::StreamListener<OutData>,
}

fn run(
    rate: u32,
    head: Head,
    directness: Directness,
    slots: Arc<Mutex<HashMap<Slot, Arc<SlotState>>>>,
    commands: pw::channel::Receiver<Command>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None)?;
    let context = pw::context::Context::new(&mainloop)?;
    let core = context.connect(None)?;

    let nodes: Rc<RefCell<HashMap<Slot, Node>>> = Rc::default();
    let directness = Rc::new(RefCell::new(directness));

    let _receiver = commands.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let nodes = Rc::clone(&nodes);
        let slots = Arc::clone(&slots);
        let directness = Rc::clone(&directness);
        let core = core.clone();
        move |command| match command {
            Command::Stop => mainloop.quit(),
            Command::Open(slot) => {
                if nodes.borrow().contains_key(&slot) {
                    return;
                }
                match open_slot(&core, slot, rate, head, *directness.borrow()) {
                    Ok(node) => {
                        if let Ok(mut s) = slots.lock() {
                            s.insert(slot, Arc::clone(&node.state));
                        }
                        nodes.borrow_mut().insert(slot, node);
                        log::info!("spatial audio: opened {}", sink_name(slot));
                    }
                    Err(e) => log::warn!("spatial audio: could not open {}: {e}", sink_name(slot)),
                }
            }
            Command::Close(slot) => {
                nodes.borrow_mut().remove(&slot);
                if let Ok(mut s) = slots.lock() {
                    s.remove(&slot);
                }
            }
            Command::Aim {
                slot,
                speakers,
                off_axis,
            } => {
                if let Some(node) = nodes.borrow().get(&slot) {
                    if let Ok(mut aim) = node.state.aim.lock() {
                        *aim = Some((speakers, off_axis));
                    }
                }
            }
            Command::Mute { slot, muted } => {
                if let Some(node) = nodes.borrow().get(&slot) {
                    node.state.muted.store(muted, Ordering::Relaxed);
                }
            }
            Command::Directness(d) => {
                *directness.borrow_mut() = d;
                for node in nodes.borrow().values() {
                    if let Ok(mut pending) = node.state.directness.lock() {
                        *pending = Some(d);
                    }
                }
            }
        }
    });

    log::info!("spatial audio: running at {rate} Hz");
    mainloop.run();
    Ok(())
}

fn make_head(head: &Head, rate: u32) -> Box<dyn crate::Spatialise> {
    match head {
        Head::Measured => match crate::hrtf::Hrtf::system(rate) {
            Ok(h) => Box::new(h),
            Err(e) => {
                log::warn!(
                    "spatial audio: no measured head ({e}); \
                     sounds will be placed but not convincingly behind you"
                );
                Box::new(crate::Panner::new(rate))
            }
        },
        Head::Reasoned => Box::new(crate::Panner::new(rate)),
    }
}

/// Build the format the sink offers: float, at the graph's rate, in the given layout.
fn format_pod(layout: Layout, rate: u32) -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(layout.count() as u32);
    let mut position = [0u32; 64];
    for (i, channel) in layout.channels().iter().enumerate() {
        position[i] = spa_position(*channel);
    }
    info.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .expect("an audio format is always serialisable")
    .0
    .into_inner()
}

/// Everything the sink's callbacks own. Touched only by PipeWire's own threads, which is what
/// lets the renderer live here rather than behind a lock.
struct SinkData {
    state: Arc<SlotState>,
    render: Option<Binaural>,
    head: Head,
    rate: u32,
    directness: Directness,
    /// De-interleaved input and rendered output, sized once the format is known.
    input: Vec<f32>,
    output: Vec<f32>,
}

/// Everything the output stream's callbacks own.
struct OutData {
    state: Arc<SlotState>,
    scratch: Vec<f32>,
}

fn open_slot(
    core: &pw::core::Core,
    slot: Slot,
    rate: u32,
    head: Head,
    directness: Directness,
) -> Result<Node, pw::Error> {
    let state = Arc::new(SlotState::new());
    let name = sink_name(slot);

    // ---- The sink the app plays into.
    //
    // `Audio/Sink` is what makes this a device as far as every application is concerned, so
    // nothing has to be taught about any of this. It is deliberately not a candidate for the
    // machine's default sink: a window's sink is for that window's app, and anything that
    // landed here by accident would be placed at a window it has nothing to do with.
    let sink = pw::stream::Stream::new(
        core,
        &name,
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => name.as_str(),
            *pw::keys::NODE_DESCRIPTION => SINK_DESCRIPTION,
            *pw::keys::NODE_VIRTUAL => "true",
            // The sink is as wide as the widest thing we can place, and an app narrower than
            // that must be left alone rather than spread to fill it. Without this PipeWire
            // helpfully upmixes a stereo song into twelve speakers, and what then gets placed
            // is its idea of a surround mix rather than the two channels the app sent -- the
            // opposite of keeping the stereo. Silent channels cost nothing downstream, so the
            // one wide sink serves a song and a film equally.
            "channelmix.upmix" => "false",
        },
    )?;

    let sink_listener = sink
        .add_local_listener_with_user_data(SinkData {
            state: Arc::clone(&state),
            render: None,
            head,
            rate,
            directness,
            input: Vec::new(),
            output: Vec::new(),
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::audio::AudioInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            // What the app actually settled on, which need not be what was offered: a stereo
            // app connecting to a 7.1 sink negotiates stereo, and rendering it as though it
            // were 7.1 would read six channels of somebody else's memory.
            let Some(layout) = Layout::from_count(info.channels() as usize) else {
                log::warn!(
                    "spatial audio: {} channels is not a layout we can place",
                    info.channels()
                );
                return;
            };
            log::info!("spatial audio: a window's app is sending {layout:?}");
            data.state
                .layout
                .store(layout_code(layout), Ordering::Relaxed);
            data.render = Some(Binaural::new(
                layout,
                make_head(&data.head, data.rate),
                data.directness,
                data.rate,
            ));
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(render) = data.render.as_mut() else {
                return;
            };
            let channels = render.layout().count();
            let datas = buffer.datas_mut();
            let Some(first) = datas.first_mut() else {
                return;
            };
            let size = first.chunk().size() as usize;
            let Some(bytes) = first.data() else { return };
            let bytes = &bytes[..size.min(bytes.len())];
            let frames = bytes.len() / (4 * channels);
            if frames == 0 {
                return;
            }

            // Latest geometry, if the loop thread is not in the middle of writing it. Skipping
            // one is harmless -- it is the current state rather than a step in a sequence --
            // and this is a real-time callback where waiting is a dropout.
            if let Ok(mut aim) = data.state.aim.try_lock() {
                if let Some((speakers, off_axis)) = aim.take() {
                    render.aim(&speakers, off_axis);
                }
            }
            if let Ok(mut pending) = data.state.directness.try_lock() {
                if let Some(d) = pending.take() {
                    render.set_directness(d);
                }
            }
            render.set_muted(data.state.muted.load(Ordering::Relaxed));

            data.input.resize(frames * channels, 0.0);
            data.output.resize(frames * 2, 0.0);
            for (i, slot) in data.input.iter_mut().enumerate() {
                let at = i * 4;
                *slot =
                    f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
            }
            render.render(&data.input, &mut data.output);
            data.state
                .peak
                .store(render.peak().to_bits(), Ordering::Relaxed);
            data.state.sounding.store(
                render.sounding_layout().map(layout_code).unwrap_or(0),
                Ordering::Relaxed,
            );

            let dropped = data.state.queue.write(&data.output);
            if dropped > 0 {
                let total = data.state.dropped.fetch_add(1, Ordering::Relaxed);
                // Only occasionally, because a genuinely diverged clock would otherwise fill
                // the log faster than anything else could be read in it.
                if total % 200 == 0 {
                    log::warn!(
                        "spatial audio: a window is producing faster than the glasses consume"
                    );
                }
            }
        })
        .register()?;

    let format = format_pod(Layout::Surround714, rate);
    let mut params = [Pod::from_bytes(&format).expect("a serialised format is a pod")];
    sink.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    // ---- The stereo that comes back out, into whatever the glasses are.
    let out = pw::stream::Stream::new(
        core,
        &format!("{name}.out"),
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_NAME => format!("{name}.out").as_str(),
            *pw::keys::NODE_DESCRIPTION => "Spatiand window, placed",
        },
    )?;

    let out_listener = out
        .add_local_listener_with_user_data(OutData {
            state: Arc::clone(&state),
            scratch: Vec::new(),
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(first) = datas.first_mut() else {
                return;
            };
            const STRIDE: usize = 8; // two channels of f32
            let frames = match first.data() {
                Some(slice) => {
                    let frames = slice.len() / STRIDE;
                    data.scratch.resize(frames * 2, 0.0);
                    // Short reads come back as silence rather than as a stall: making the
                    // sound card wait would take out every other window's audio too.
                    data.state.queue.read(&mut data.scratch);
                    for (i, sample) in data.scratch.iter().enumerate() {
                        let at = i * 4;
                        slice[at..at + 4].copy_from_slice(&sample.to_le_bytes());
                    }
                    frames
                }
                None => 0,
            };
            let chunk = first.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = STRIDE as _;
            *chunk.size_mut() = (STRIDE * frames) as _;
        })
        .register()?;

    let out_format = format_pod(Layout::Stereo, rate);
    let mut out_params = [Pod::from_bytes(&out_format).expect("a serialised format is a pod")];
    out.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut out_params,
    )?;

    Ok(Node {
        state,
        _sink: sink,
        _sink_listener: sink_listener,
        _out: out,
        _out_listener: out_listener,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_told_where_to_send_its_sound_in_both_languages() {
        // The whole stream-to-window matching problem, solved by inheritance rather than by
        // guessing afterwards. Worth a test because the format is a contract with an audio
        // server: a stray quote or brace silently stops routing without failing anything.
        //
        // Both, because an application picks its audio protocol without telling anyone, and
        // the one most desktop applications pick ignores the PipeWire variable entirely. This
        // was found the hard way -- a browser's sound went to the machine's default sink
        // while its window's own sink sat idle beside it.
        let env = routing_env(42);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("nothing set {k}"))
        };
        assert_eq!(
            get("PIPEWIRE_PROPS"),
            "{ target.object = \"spatiand.window.42\" }"
        );
        assert_eq!(get("PULSE_SINK"), "spatiand.window.42");
        assert_eq!(get("PULSE_SINK"), sink_name(42));
    }

    #[test]
    fn a_layout_survives_the_trip_through_an_atomic() {
        for layout in [
            Layout::Mono,
            Layout::Stereo,
            Layout::Surround51,
            Layout::Surround71,
            Layout::Surround714,
        ] {
            assert_eq!(layout_from_code(layout_code(layout)), Some(layout));
        }
        assert_eq!(layout_from_code(0), None, "nothing negotiated yet");
    }

    #[test]
    fn every_channel_has_a_distinct_place_in_the_audio_server() {
        // Two channels sharing a position is how somebody's surrounds end up where their
        // mains should be, and the audio server would accept it without a word.
        let mut seen = std::collections::HashSet::new();
        for layout in [Layout::Surround714, Layout::Surround51, Layout::Mono] {
            for channel in layout.channels() {
                let position = spa_position(*channel);
                assert!(
                    position > 2 || *channel == Channel::Mono,
                    "{channel:?} is unnamed"
                );
                assert!(
                    seen.insert((*channel, position)) || seen.contains(&(*channel, position)),
                    "{channel:?} moved"
                );
            }
        }
        let positions: std::collections::HashSet<u32> = Layout::Surround714
            .channels()
            .iter()
            .map(|c| spa_position(*c))
            .collect();
        assert_eq!(
            positions.len(),
            Layout::Surround714.count(),
            "two channels share a position"
        );
    }
}
