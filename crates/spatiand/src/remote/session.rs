//! One host, on one thread: the link, the decoders and the windows they feed.
//!
//! The loop here is deliberately plain. It polls the network, decodes whatever arrived, and
//! hands the pictures to surfaces; nothing in it waits for anything else. What keeps it honest
//! is the two rules at the bottom of it:
//!
//! * **Decode every frame; show only the newest.** A compressed frame is not a picture, it is
//!   the *difference* from the one before, so skipping one to save time corrupts every frame
//!   after it until the next keyframe. This loop once did exactly that — "the newest frame
//!   wins" applied to frames still to be decoded — and the result was a window of flat grey
//!   with faint blocks in it: what an HEVC decoder draws when every frame is a change to a
//!   reference it never had. It passed for a buffer-sharing fault for a whole day, because the
//!   one picture ever read back to check was the first, which is a keyframe. What may be
//!   skipped is *showing* a picture that a newer one has already overtaken.
//! * **Never feed a decoder a frame with a hole in it**, or anything after one. A missing piece
//!   loses its frame and asks the host for a keyframe, and until that keyframe arrives nothing
//!   is decoded at all: the window keeps its last good picture rather than a broken new one.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spatiand_stream::link::{hear, resolve};
use spatiand_stream::video::{Arrival, Packet, Reassembler};
use spatiand_stream::{ClientMessage, Codec, Fingerprint, HostMessage, Identity, WindowId};
use spatiand_video::{Converter, Decoder};
use tokio::sync::mpsc::UnboundedReceiver;
use wayland_client::EventQueue;

use super::client::{self, Client};
use super::{Command, HostView, Link, RemoteApp};

/// Which host to show, and how to reach it.
#[derive(Debug, Clone)]
pub struct Config {
    /// What to connect to: a name or address, with a port.
    pub host: String,
    /// The host's certificate, from `spatiand-host --fingerprint`.
    pub fingerprint: Fingerprint,
    /// Where this session keeps its own identity, so a host only has to pair with it once.
    pub identity_dir: std::path::PathBuf,
    pub render_node: String,
    /// Started as soon as the link is up. Empty means "show whatever the host already has".
    pub launch: Vec<String>,
    /// Whether this session will send its microphone when a host asks for one. See
    /// [`super::microphone`].
    pub microphone: bool,
}

/// One remote window's decoder and its conversion, made when the host says what it is sending.
struct Stream {
    decoder: Decoder,
    converter: Option<Converter>,
    /// Every frame that has arrived and not been decoded yet, oldest first. All of them get
    /// decoded; see the module notes.
    queue: std::collections::VecDeque<(u64, Vec<u8>)>,
    /// Something was lost, so nothing can be decoded correctly until a keyframe arrives.
    broken: bool,
    /// The number the next frame should carry. The host numbers each window's frames one
    /// after another, so a frame carrying any other number means one went missing whole —
    /// every piece lost, which the reassembler cannot see, since it never heard of it.
    expected: Option<u32>,
    /// Frames found missing that way.
    gaps: u64,
    /// When a keyframe was last asked for, so a stream waiting for one keeps asking.
    asked: Option<Instant>,
    /// The newest decoded picture, not yet shown because the compositor had no room. Kept so
    /// a window that goes still still ends on its last frame.
    unshown: Option<spatiand_video::Picture>,
    /// Pictures decoded and never shown, because a newer one overtook them.
    dropped: u64,
    /// Frames thrown away undecoded while waiting for a keyframe.
    skipped: u64,
    shown: u64,
}

/// How often a window being dragged may tell the host its new size.
const RESIZE_EVERY: Duration = Duration::from_millis(150);

/// How many frames may wait to be decoded before it is cheaper to start again from a
/// keyframe. Half a second at the glasses' rate: beyond that the wearer is watching the past.
const BACKLOG: usize = 36;

/// What a remote window's title bar says: what the application calls it, and which computer
/// it is on — a browser here and a browser there are otherwise indistinguishable.
fn window_title(view: &Mutex<HostView>, app: &str, title: &str) -> String {
    let (host, name) = view
        .lock()
        .map(|v| {
            (
                v.name.clone(),
                v.apps.iter().find(|a| a.id == app).map(|a| a.name.clone()),
            )
        })
        .unwrap_or((None, None));
    let what = if title.trim().is_empty() {
        name.unwrap_or_else(|| app.to_string())
    } else {
        title.trim().to_string()
    };
    match host {
        Some(host) => format!("{what} — {host}"),
        None => what,
    }
}

/// Why a connection ended.
enum Ended {
    /// The session is closing; do not come back.
    Stopped,
    /// The link went. Try again.
    Lost(String),
    /// The host answered and would not have this headset.
    Refused(String),
    /// The compositor side is gone, so there is nothing to show windows on.
    NoCompositor(String),
}

/// Keep a host's windows in the room for as long as the session runs.
///
/// **Reconnects for ever**, with a growing pause. A host that is asleep, rebooting or out of
/// range is the normal case for something that runs on another machine, and the wearer should
/// not have to do anything for its windows to come back when it does: its applications never
/// stopped, so neither should this.
pub fn run(
    stream: UnixStream,
    config: Config,
    stop: Arc<AtomicBool>,
    view: Arc<Mutex<HostView>>,
    mut commands: UnboundedReceiver<Command>,
) {
    let (mut client, mut queue) = match Client::new(stream) {
        Ok(pair) => pair,
        Err(e) => {
            log::error!("remote: {e}");
            return;
        }
    };

    let identity = match Identity::load_or_create(&config.identity_dir) {
        Ok(identity) => identity,
        Err(e) => {
            log::error!("remote: {e}");
            return;
        }
    };
    log::info!(
        "remote: this session is {}, looking for {}",
        identity.fingerprint().short(),
        config.host
    );

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            log::error!("remote: could not start a network runtime: {e}");
            return;
        }
    };

    let set = |link: Link| {
        if let Ok(mut v) = view.lock() {
            v.link = link;
        }
    };

    runtime.block_on(async {
        let mut pause = Duration::from_secs(1);
        let mut last_said = String::new();
        while !stop.load(Ordering::Relaxed) {
            set(Link::Connecting);
            let ended = match connect(&identity, &config).await {
                Ok(connection) => {
                    log::info!("remote: connected to {}", config.host);
                    pause = Duration::from_secs(1);
                    last_said.clear();
                    let ended = serve(
                        &connection,
                        &config,
                        &mut client,
                        &mut queue,
                        &stop,
                        &view,
                        &mut commands,
                    )
                    .await;
                    connection.close(0u32.into(), b"session ended");
                    ended
                }
                Err(e) => Ended::Lost(e),
            };
            client.close_all();
            let reason = match ended {
                Ended::Stopped => break,
                Ended::NoCompositor(e) => {
                    log::error!("remote: {e}");
                    return;
                }
                Ended::Refused(reason) => {
                    set(Link::Refused(reason.clone()));
                    reason
                }
                Ended::Lost(reason) => {
                    set(Link::Offline(reason.clone()));
                    reason
                }
            };
            // Said once per reason, not once per attempt: a host that is off for a night would
            // otherwise write a line every twenty seconds until morning.
            if reason != last_said {
                log::info!("remote {}: {reason}; will keep trying", config.host);
                last_said = reason;
            }

            // Wait, still answering the compositor — an unanswered client is one it may decide
            // has hung — and still hearing the launcher, so a launch of an app on a host that
            // is off can at least say why nothing happened.
            let until = Instant::now() + pause;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                    command = commands.recv() => {
                        if let Some(Command::Launch(app) | Command::ForceQuit(app)) = command {
                            log::info!(
                                "remote: cannot reach {app}: {} is not connected",
                                config.host
                            );
                        }
                    }
                }
                if let Err(e) = client::pump(&mut client, &mut queue) {
                    log::error!("remote: {e}");
                    return;
                }
            }
            pause = (pause * 2).min(Duration::from_secs(20));
        }
    });
}

/// One connection, from hello to whatever ends it.
async fn serve(
    connection: &quinn::Connection,
    config: &Config,
    client: &mut Client,
    queue: &mut EventQueue<Client>,
    stop: &AtomicBool,
    view: &Mutex<HostView>,
    commands: &mut UnboundedReceiver<Command>,
) -> Ended {
    let mut streams: HashMap<u32, Stream> = HashMap::new();
    let mut windows: HashMap<u32, Reassembler> = HashMap::new();
    let mut apps: HashMap<u32, String> = HashMap::new();
    // Resizes, held back a little. A drag configures the window on every frame, and the host
    // builds a new encoder for every size it is told — so the whole drag would be a stream of
    // rebuilt encoders and keyframes. One every `RESIZE_EVERY`, and the last one always.
    let mut resize_sent: HashMap<u32, Instant> = HashMap::new();
    let mut resize_pending: HashMap<u32, (u32, u32)> = HashMap::new();
    // Sent only while the host says something there is listening, and never if the wearer
    // has said no.
    let mut microphone = super::microphone::Microphone::default();
    // One ordered stream for everything this end says; see `talk`.
    let (out, outbox) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();
    let writer = tokio::spawn(talk(connection.clone(), outbox));
    let mut said = Instant::now();
    // What the link had carried by the last report, so each one can say what happened since
    // rather than since the session began. See the cadence line below.
    let mut carried = connection.stats();
    let mut worked = Instant::now();
    let mut frames = 0u64;
    let mut decode_ms: Vec<f32> = Vec::new();
    {
        say(&out,
            ClientMessage::Hello {
                version: spatiand_stream::VERSION,
                codecs: vec![Codec::H265, Codec::H264],
                max_size: (3840, 2160),
                refresh_mhz: 72_000,
                session: "spatiand".into(),
            },
        );
        for app in &config.launch {
            say(&out, ClientMessage::Launch { app: app.clone() });
        }

        let control = match connection.accept_uni().await {
            Ok(stream) => stream,
            // A host that does not know this headset lets the handshake finish and refuses at
            // the first exchange, which is here. See `transport`'s tests for why.
            Err(e) => {
                return Ended::Refused(format!(
                    "the host would not talk to this headset ({e}); pair it again"
                ))
            }
        };
        let mut control = Box::pin(hear(control));

        while !stop.load(Ordering::Relaxed) {
            tokio::select! {
                message = &mut control => {
                    match message {
                        Some((message, rest)) => {
                            control = Box::pin(hear(rest));
                            match message {
                                HostMessage::Welcome { host, reattached, .. } => {
                                    log::info!(
                                        "remote: {host} says hello{}",
                                        if reattached { ", with windows already open" } else { "" }
                                    );
                                    if let Ok(mut v) = view.lock() {
                                        v.name = Some(host);
                                        v.link = Link::Online;
                                    }
                                }
                                HostMessage::Opened(info) => {
                                    log::info!(
                                        "remote: window {} is {} on {}",
                                        info.window.0, info.app, config.host
                                    );
                                    apps.insert(info.window.0, info.app.clone());
                                    let title = window_title(view, &info.app, &info.title);
                                    client.open(
                                        info.window.0,
                                        &super::app_id(&config.host, &info.app),
                                        &title,
                                    );
                                }
                                HostMessage::Retitled { window, title } => {
                                    if let Some(app) = apps.get(&window.0) {
                                        let title = window_title(view, app, &title);
                                        client.retitle(window.0, &title);
                                    }
                                }
                                HostMessage::Stream { window, codec, width, height, .. } => {
                                    match Decoder::new(&config.render_node, codec) {
                                        Ok(decoder) => {
                                            log::info!(
                                                "remote: window {} streams {} at {width}x{height}",
                                                window.0, codec.label()
                                            );
                                            streams.insert(window.0, Stream {
                                                decoder,
                                                converter: None,
                                                queue: Default::default(),
                                                // Nothing has been decoded yet, so the first
                                                // thing decodable is a keyframe.
                                                broken: true,
                                                expected: None,
                                                gaps: 0,
                                                asked: None,
                                                unshown: None,
                                                dropped: 0,
                                                skipped: 0,
                                                shown: 0,
                                            });
                                            windows.insert(window.0, Reassembler::new());
                                        }
                                        Err(e) => log::error!("remote: window {}: {e}", window.0),
                                    }
                                }
                                HostMessage::Clipboard(what) => {
                                    // Queued for the compositor, which owns the selection and
                                    // the only pipes an application is reading.
                                    if let Ok(mut v) = view.lock() {
                                        // A cap, because a host that talked to a session that
                                        // was not listening would otherwise grow this for ever.
                                        if v.clipboard.len() >= 16 {
                                            v.clipboard.remove(0);
                                        }
                                        v.clipboard.push(what);
                                    }
                                }
                                HostMessage::Rumble { strong, weak } => {
                                    // The motors are the compositor's; it takes this on its
                                    // next frame. Only the newest matters -- a rumble that
                                    // was overtaken was never felt.
                                    if let Ok(mut v) = view.lock() {
                                        v.rumble = Some((strong, weak));
                                    }
                                }
                                HostMessage::Microphone { wanted } => {
                                    if wanted && !config.microphone {
                                        log::info!(
                                            "remote {}: something there is listening, but this \
                                             session does not send its microphone",
                                            config.host
                                        );
                                    } else {
                                        microphone.want(connection, &config.host, wanted);
                                    }
                                }
                                HostMessage::Closed { window } => {
                                    client.close(window.0);
                                    streams.remove(&window.0);
                                    windows.remove(&window.0);
                                }
                                HostMessage::Catalog { apps } => {
                                    log::info!("remote: {} application(s) offered", apps.len());
                                    let listed: Vec<RemoteApp> = apps
                                        .iter()
                                        .map(|a| RemoteApp {
                                            id: a.id.clone(),
                                            name: a.name.clone(),
                                            icon: a.icon_png.as_deref().and_then(|png| {
                                                super::store_icon(
                                                    &super::app_id(&config.host, &a.id),
                                                    png,
                                                )
                                            }),
                                        })
                                        .collect();
                                    if let Ok(mut v) = view.lock() {
                                        v.apps = listed;
                                    }
                                }
                                HostMessage::Refused { reason, .. } => {
                                    log::error!("remote: the host refused this session: {reason}");
                                    return Ended::Refused(reason);
                                }
                                other => log::debug!("remote: {other:?}"),
                            }
                        }
                        None => {
                            // Say which it was. "The host closed the link" was a guess, and
                            // it read the same whether the host had gone, refused us, or the
                            // link had simply timed out -- three different faults with three
                            // different fixes, and hours spent telling them apart by hand.
                            return Ended::Lost(match connection.close_reason() {
                                Some(quinn::ConnectionError::TimedOut) => format!(
                                    "the link went quiet for {} s and was given up on",
                                    spatiand_stream::transport::IDLE_TIMEOUT_MS / 1000
                                ),
                                Some(reason) => format!("the host closed the link: {reason}"),
                                None => "the host closed the control stream".into(),
                            });
                        }
                    }
                }
                // Each application's sound comes on a stream of its own; see `sound`.
                incoming = connection.accept_uni() => {
                    match incoming {
                        Ok(stream) => {
                            tokio::spawn(super::sound::receive(stream, config.host.clone()));
                        }
                        Err(_) => return Ended::Lost("the link went quiet".into()),
                    }
                }
                datagram = connection.read_datagram() => {
                    let Ok(datagram) = datagram else {
                        return Ended::Lost("the link went quiet".into());
                    };
                    let Some(packet) = Packet::read(&datagram) else { continue };
                    let window = packet.window as u32;
                    let Some(reassembler) = windows.get_mut(&window) else { continue };
                    match reassembler.accept(&packet) {
                        Arrival::Frame(frame) => {
                            if let Some(stream) = streams.get_mut(&window) {
                                // A whole frame lost is a hole in the numbering. Decoding the
                                // next one anyway is what drew the grey: HEVC with a missing
                                // reference predicts from mid-grey, and only the changes show.
                                let gap = stream.expected.is_some_and(|e| e != frame.frame);
                                stream.expected = Some(frame.frame.wrapping_add(1));
                                if gap && !frame.keyframe && !stream.broken {
                                    stream.gaps += 1;
                                    stream.skipped += stream.queue.len() as u64;
                                    stream.queue.clear();
                                    stream.broken = true;
                                    stream.asked = Some(Instant::now());
                                    say(&out, ClientMessage::WantKeyframe {
                                        window: WindowId(window),
                                    });
                                }
                                if frame.keyframe {
                                    // Everything before a keyframe is irrelevant to everything
                                    // after it, so a backlog can go.
                                    stream.skipped += stream.queue.len() as u64;
                                    stream.queue.clear();
                                    stream.broken = false;
                                }
                                if stream.broken {
                                    stream.skipped += 1;
                                } else if stream.queue.len() >= BACKLOG {
                                    // Too far behind to catch up by decoding: start again.
                                    log::warn!(
                                        "remote: window {window} is {} frames behind; asking \
                                         for a keyframe",
                                        stream.queue.len()
                                    );
                                    stream.skipped += stream.queue.len() as u64 + 1;
                                    stream.queue.clear();
                                    stream.broken = true;
                                    say(&out, ClientMessage::WantKeyframe {
                                        window: WindowId(window),
                                    });
                                } else {
                                    stream.queue.push_back((frame.captured_us, frame.bytes));
                                }
                            }
                        }
                        Arrival::Lost(_) => {
                            if let Some(stream) = streams.get_mut(&window) {
                                stream.skipped += stream.queue.len() as u64;
                                stream.queue.clear();
                                stream.broken = true;
                            }
                            say(&out, ClientMessage::WantKeyframe {
                                window: WindowId(window),
                            });
                        }
                        Arrival::Partial | Arrival::Stale => {}
                    }
                }
                command = commands.recv() => {
                    match command {
                        Some(Command::Launch(app)) => {
                            log::info!("remote: asking {} to start {app}", config.host);
                            say(&out, ClientMessage::Launch { app });
                        }
                        Some(Command::ForceQuit(app)) => {
                            log::warn!("remote: asking {} to kill {app}", config.host);
                            say(&out, ClientMessage::ForceQuit { app });
                        }
                        Some(Command::Pad(state)) => {
                            say(&out, ClientMessage::Pad(state));
                        }
                        Some(Command::Clipboard(what)) => {
                            say(&out, ClientMessage::Clipboard(what));
                        }
                        None => {}
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(2)) => {}
            }

            // --- decode and show whatever is waiting ---
            //
            // Not on every datagram. A frame arrives in a couple of hundred of them, and doing
            // the compositor's round of syscalls after each one is most of a core spent on
            // nothing. Either there is a frame to decode, or it has been long enough that the
            // compositor deserves a word.
            // **A stream without a keyframe asks for one, and keeps asking.** On a reattach
            // the host sends one keyframe of a still window and then, rightly, nothing — and
            // that keyframe can overtake the message saying the stream exists, arrive for a
            // window this session does not know yet, and be thrown away. Waiting for the next
            // one then waits for ever: the window sat grey, or showed only the parts that
            // changed afterwards. Asking costs one small message every half second, and only
            // while a picture is actually missing.
            for (id, stream) in streams.iter_mut() {
                if stream.broken
                    && stream.asked.is_none_or(|at| at.elapsed() >= Duration::from_millis(500))
                {
                    stream.asked = Some(Instant::now());
                    say(&out, ClientMessage::WantKeyframe {
                        window: WindowId(*id),
                    });
                }
            }

            let anything_to_decode = streams
                .values()
                .any(|s| !s.queue.is_empty() || s.unshown.is_some());
            if !anything_to_decode && worked.elapsed() < Duration::from_millis(4) {
                continue;
            }
            worked = Instant::now();

            for (id, stream) in streams.iter_mut() {
                // Decode everything that has arrived, in order. Only the newest picture is
                // worth showing; the ones it overtakes are still decoded, because the next
                // frame is built on them.
                let began = Instant::now();
                let decoded_any = !stream.queue.is_empty();
                while let Some((captured, bytes)) = stream.queue.pop_front() {
                    match stream.decoder.decode(captured as i64, &bytes) {
                        Ok(pictures) => {
                            for picture in pictures {
                                if stream.unshown.replace(picture).is_some() {
                                    stream.dropped += 1;
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("remote: window {id}: {e}");
                            stream.skipped += stream.queue.len() as u64;
                            stream.queue.clear();
                            stream.broken = true;
                            say(&out, ClientMessage::WantKeyframe {
                                window: WindowId(*id),
                            });
                        }
                    }
                }
                if decoded_any {
                    decode_ms.push(began.elapsed().as_secs_f32() * 1000.0);
                }

                // While the compositor is still holding two of this window's buffers, another
                // picture would only queue behind them — so wait, and **keep the picture**.
                // For a window that has just gone still no other is coming, and dropping this
                // one would leave the wearer looking at the one before, for ever.
                if client.in_flight(*id) >= 2 {
                    continue;
                }
                let Some(picture) = stream.unshown.take() else {
                    continue;
                };
                let pictures = [picture];
                for picture in pictures {
                    // A way to take the colour conversion out of the picture, literally: hand
                    // the compositor the decoder's own NV12 buffer. The colours will be wrong
                    // if it shows anything at all — and *that* is the point, because it says
                    // whether the handover works or the conversion's output does not.
                    if std::env::var("SPATIAND_REMOTE_RAW").is_ok() {
                        if let Err(e) = client.show_raw(
                            *id,
                            picture.width,
                            picture.height,
                            picture.fourcc,
                            picture.modifier,
                            &picture.planes,
                        ) {
                            log::error!("remote: window {id}: {e}");
                        }
                        frames += 1;
                        continue;
                    }
                    let size = (picture.width, picture.height);
                    if stream.converter.as_ref().is_none_or(|c| c.size() != size) {
                        let (device, frames) = stream.decoder.device();
                        match Converter::new(device, frames, size) {
                            Ok(converter) => stream.converter = Some(converter),
                            Err(e) => {
                                log::error!("remote: window {id}: {e}");
                                continue;
                            }
                        }
                    }
                    let Some(converter) = stream.converter.as_mut() else {
                        continue;
                    };
                    match converter.convert(&picture) {
                        Ok(converted) => {
                            if std::env::var("SPATIAND_REMOTE_SHM").is_ok() {
                                if let Err(e) = client.show_by_copy(*id, &converted) {
                                    log::error!("remote: window {id}: {e}");
                                }
                                frames += 1;
                                continue;
                            }
                            // The same picture the compositor is about to be given, read back
                            // to a file. This is how "the handover is wrong" is told apart
                            // from "the picture is wrong", which look identical on the glasses.
                            // `SPATIAND_REMOTE_DUMP_AT=n` takes the n-th picture instead of
                            // the first: the first proves little about a pool that is reused.
                            let dump_at: u64 = std::env::var("SPATIAND_REMOTE_DUMP_AT")
                                .ok()
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(0);
                            if let Ok(path) = std::env::var("SPATIAND_REMOTE_DUMP") {
                                if stream.shown >= dump_at && !std::path::Path::new(&path).exists() {
                                    match converted.to_bgra() {
                                        Ok(bytes) => {
                                            let _ = std::fs::write(&path, &bytes);
                                            log::info!(
                                                "remote: wrote the handed-over picture to {path} \
                                                 ({}x{}, {} bytes)",
                                                converted.width,
                                                converted.height,
                                                bytes.len()
                                            );
                                        }
                                        Err(e) => log::error!("remote: {e}"),
                                    }
                                }
                            }
                            if let Err(e) = client.show(*id, converted) {
                                log::error!("remote: window {id}: {e}");
                            }
                            stream.shown += 1;
                            frames += 1;
                        }
                        Err(e) => log::error!("remote: window {id}: {e}"),
                    }
                }
            }

            // --- the compositor's side of things ---
            if let Err(e) = client::pump(client, queue) {
                return Ended::NoCompositor(e);
            }
            for (id, width, height) in std::mem::take(&mut client.resized) {
                resize_pending.insert(id, (width.max(1) as u32, height.max(1) as u32));
            }
            for (id, (width, height)) in std::mem::take(&mut resize_pending) {
                let due = resize_sent
                    .get(&id)
                    .is_none_or(|at| at.elapsed() >= RESIZE_EVERY);
                if !due {
                    // Kept, not dropped: the size the drag ends on is the one that matters.
                    resize_pending.insert(id, (width, height));
                    continue;
                }
                resize_sent.insert(id, Instant::now());
                log::info!("remote: window {id} is now {width}x{height}; telling the host");
                say(&out, ClientMessage::Configure {
                    window: WindowId(id),
                    width,
                    height,
                });
            }
            for id in std::mem::take(&mut client.closing) {
                say(&out, ClientMessage::Close { window: WindowId(id) });
            }
            // Everything the wearer did, in the order they did it. Reliable and ordered,
            // because a key that arrives twice or out of turn is worse than one that is late.
            for (id, input) in std::mem::take(&mut client.input) {
                say(&out,
                    ClientMessage::Input {
                        window: WindowId(id),
                        input,
                    },
                );
            }

            if said.elapsed() >= Duration::from_secs(2) {
                let secs = said.elapsed().as_secs_f32();
                let mean = decode_ms.iter().sum::<f32>() / decode_ms.len().max(1) as f32;
                let dropped: u64 = streams.values().map(|s| s.dropped).sum();
                let skipped: u64 = streams.values().map(|s| s.skipped).sum();
                let gaps: u64 = streams.values().map(|s| s.gaps).sum();
                let waiting: Vec<String> = streams
                    .keys()
                    .map(|id| format!("{id}:{}", client.in_flight(*id)))
                    .collect();
                let pending: usize = streams.values().map(|s| s.queue.len()).sum();
                // How the link itself behaved, which the rest of this line cannot show: a
                // picture that stops arriving looks the same whether the network gave up or
                // the far end simply had nothing to send, and telling those apart by hand has
                // cost a whole evening. Throughput and loss say which.
                let now = connection.stats();
                let since = |after: u64, before: u64| after.saturating_sub(before);
                let down = since(now.udp_rx.bytes, carried.udp_rx.bytes) as f32 * 8e-6 / secs;
                let up = since(now.udp_tx.bytes, carried.udp_tx.bytes) as f32 * 8e-6 / secs;
                let sent = since(now.path.sent_packets, carried.path.sent_packets);
                let lost = since(now.path.lost_packets, carried.path.lost_packets);
                let squeezed = since(
                    now.path.congestion_events,
                    carried.path.congestion_events,
                );
                carried = now;
                log::info!(
                    "remote {}: {frames} shown in {secs:.1}s, {mean:.1} ms decoding, {dropped} \
                     overtaken, {gaps} lost whole, {skipped} skipped for a keyframe, rtt {:.1} ms, \
                     {down:.1} Mbit/s down {up:.2} up, {lost}/{sent} packets lost, {squeezed} \
                     congestion, buffers held [{}], {pending} waiting to decode",
                    config.host,
                    connection.rtt().as_secs_f32() * 1000.0,
                    waiting.join(" ")
                );
                frames = 0;
                decode_ms.clear();
                said = Instant::now();
            }
        }
        say(&out, ClientMessage::Detach);
        // A goodbye is only worth anything if it goes out before the link does. Dropping the
        // sender ends the writer, which finishes the stream once the queue is empty.
        drop(out);
        let _ = tokio::time::timeout(Duration::from_millis(200), writer).await;
        Ended::Stopped
    }
}

async fn connect(
    identity: &Identity,
    config: &Config,
) -> Result<quinn::Connection, String> {
    let endpoint = spatiand_stream::transport::client(identity, config.fingerprint)?;
    let address = resolve(&config.host).await?;
    let connecting = endpoint
        .connect(address, "spatiand")
        .map_err(|e| format!("cannot reach {address}: {e}"))?;
    // Bounded, because an unreachable address otherwise waits out QUIC's own handshake
    // timeout, and "offline" should not take that long to say.
    tokio::time::timeout(Duration::from_secs(5), connecting)
        .await
        .map_err(|_| format!("{} does not answer", config.host))?
        .map_err(|e| format!("{address} would not have this session: {e}"))
}

/// One message, one stream.
/// Queue something for the host. Never blocks; order is kept by the writer below.
fn say(out: &tokio::sync::mpsc::UnboundedSender<ClientMessage>, message: ClientMessage) {
    let _ = out.send(message);
}

/// Write everything the session says down **one** stream, in the order it was said.
///
/// It used to be a stream per message, which QUIC delivers reliably and, between streams, in
/// whatever order it likes. A key's press and its release are two messages: under load the
/// release could arrive first, and the key then stayed down for ever — repeating, because
/// XWayland repeats a key it has not been told about — while a single click became a drag
/// that never ended. One ordered stream is the whole fix, and it is also fewer streams.
async fn talk(
    connection: quinn::Connection,
    mut outbox: tokio::sync::mpsc::UnboundedReceiver<ClientMessage>,
) {
    let mut stream = match connection.open_uni().await {
        Ok(stream) => stream,
        Err(e) => {
            log::warn!("remote: no control stream ({e}); nothing can be said to this host");
            return;
        }
    };
    if stream
        .write_all(&spatiand_stream::CONTROL_MAGIC)
        .await
        .is_err()
    {
        return;
    }
    while let Some(message) = outbox.recv().await {
        let Ok(bytes) = spatiand_stream::to_bytes(&message) else {
            continue;
        };
        let length = (bytes.len() as u32).to_le_bytes();
        if stream.write_all(&length).await.is_err() || stream.write_all(&bytes).await.is_err() {
            return;
        }
    }
    let _ = stream.finish();
}
