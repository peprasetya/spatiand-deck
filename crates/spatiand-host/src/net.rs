//! The host's half of the link, on its own thread.
//!
//! The compositor thread must never wait for a network, and a network must never wait for a
//! GPU. So they are separate, and everything between them is a message on a queue: the
//! compositor posts encoded pictures and window news, and takes whatever the session has said
//! since it last looked.
//!
//! ## What goes which way
//!
//! * **Control** — windows opening and closing, the catalogue, stream settings — goes on a
//!   reliable QUIC stream, length-prefixed. It is small and it must arrive.
//! * **Pictures** go as datagrams, cut to whatever the path will carry in one packet. A piece
//!   that goes missing loses its frame and nothing else; see [`spatiand_stream::video`].
//!
//! ## Only one session at a time
//!
//! A second one taking over is the useful behaviour — you have walked to another room and
//! picked up the other headset — and two at once is not: they would fight over window sizes,
//! which window has focus and what the bitrate should be. The newcomer wins and the old
//! connection is closed, because the alternative is being locked out by a session that is no
//! longer anywhere near you.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender};

use quinn::Connection;
use spatiand_stream::transport::{peer_fingerprint, Trust};
use spatiand_stream::video::{split, Packet, FLAG_KEYFRAME, FLAG_LAST};
use spatiand_stream::{ClientMessage, Fingerprint, HostMessage, Identity};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// What the compositor sends out.
pub enum ToSession {
    Control(HostMessage),
    /// One encoded picture, already whole.
    Video {
        window: u16,
        frame: u32,
        viewport: u32,
        keyframe: bool,
        captured_us: u64,
        bytes: Vec<u8>,
    },
    /// Ten milliseconds of one application's sound. See `audio`.
    Audio { app: String, pcm: Vec<u8> },
}

/// What arrives.
pub enum FromSession {
    /// Somebody is here. Carries who, so pairing can write it down.
    Joined {
        who: Fingerprint,
        address: SocketAddr,
    },
    Said(ClientMessage),
    Left,
}

/// The network, as the compositor sees it: post, and poll.
pub struct Net {
    out: UnboundedSender<ToSession>,
    inbox: Receiver<FromSession>,
}

impl Net {
    /// Start listening. The returned handle is the only thing the compositor touches.
    pub fn start(bind: SocketAddr, identity: Identity, trust: Trust) -> Result<Net, String> {
        let (out, outbox) = unbounded_channel();
        let (inbox_tx, inbox) = std::sync::mpsc::channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            // Two: one for the socket, one for everything else. A host is not a web server.
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("could not start the network runtime: {e}"))?;
        // Inside the runtime, because a QUIC endpoint takes hold of the runtime it is created
        // in and refuses to exist without one — "no async runtime found" is what it says, from
        // a line that does not look asynchronous at all.
        let (endpoint, address) = {
            let _guard = runtime.enter();
            let endpoint = spatiand_stream::transport::server(bind, &identity, trust)?;
            let address = endpoint
                .local_addr()
                .map_err(|e| format!("could not read the listening address: {e}"))?;
            (endpoint, address)
        };
        log::info!("listening for a session on {address}");

        std::thread::Builder::new()
            .name("spatiand-host-net".into())
            .spawn(move || {
                runtime.block_on(serve(endpoint, outbox, inbox_tx));
            })
            .map_err(|e| format!("could not start the network thread: {e}"))?;
        Ok(Net { out, inbox })
    }

    /// Post something. Never blocks, and never fails in a way the caller can do anything about:
    /// a session that has gone away is not an error the compositor should handle.
    pub fn send(&self, message: ToSession) {
        let _ = self.out.send(message);
    }

    /// A way to post from another thread: the sound readers each have one.
    pub fn sender(&self) -> UnboundedSender<ToSession> {
        self.out.clone()
    }

    /// Everything that has arrived since the last call.
    pub fn poll(&self) -> Vec<FromSession> {
        self.inbox.try_iter().collect()
    }
}

async fn serve(
    endpoint: quinn::Endpoint,
    mut outbox: UnboundedReceiver<ToSession>,
    inbox: Sender<FromSession>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let connection = match incoming.await {
            Ok(connection) => connection,
            Err(e) => {
                log::warn!("a session did not get as far as connecting: {e}");
                continue;
            }
        };
        let Some(who) = peer_fingerprint(&connection) else {
            log::warn!("a session connected without a certificate; closing it");
            connection.close(1u32.into(), b"no identity");
            continue;
        };
        let address = connection.remote_address();
        log::info!("session {} joined from {address}", who.short());
        let _ = inbox.send(FromSession::Joined { who, address });

        // One at a time: this returns when the session goes, and the next is accepted then.
        session(&connection, &mut outbox, &inbox).await;
        log::info!("session {} left", who.short());
        let _ = inbox.send(FromSession::Left);
    }
}

async fn session(
    connection: &Connection,
    outbox: &mut UnboundedReceiver<ToSession>,
    inbox: &Sender<FromSession>,
) {
    let mut control = match connection.open_uni().await {
        Ok(stream) => stream,
        Err(e) => {
            log::warn!("could not open the control stream: {e}");
            return;
        }
    };

    // What the session says, read on its own task so that a slow reader cannot hold up
    // pictures.
    let reading = tokio::spawn({
        let connection = connection.clone();
        let inbox = inbox.clone();
        async move {
            loop {
                let stream = match connection.accept_uni().await {
                    Ok(stream) => stream,
                    Err(_) => return,
                };
                tokio::spawn(heard(stream, inbox.clone()));
            }
        }
    });

    let mut frames_sent: u64 = 0;
    let mut dropped: u64 = 0;
    // One stream per application's sound, each written by a task of its own so that a sound
    // stream held up by flow control can never hold up pictures.
    let mut sounds: HashMap<String, tokio::sync::mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut sound_dropped: u64 = 0;
    loop {
        tokio::select! {
            outgoing = outbox.recv() => {
                let Some(outgoing) = outgoing else { break };
                match outgoing {
                    ToSession::Control(message) => {
                        let bytes = match spatiand_stream::to_bytes(&message) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                log::error!("could not encode a message for the session: {e}");
                                continue;
                            }
                        };
                        let length = (bytes.len() as u32).to_le_bytes();
                        if control.write_all(&length).await.is_err()
                            || control.write_all(&bytes).await.is_err()
                        {
                            break;
                        }
                    }
                    ToSession::Audio { app, pcm } => {
                        let alive = sounds.get(&app).is_some_and(|s| !s.is_closed());
                        if !alive {
                            sounds.insert(app.clone(), sound_stream(connection, app.clone()));
                        }
                        // A third of a second may wait; past that the listener is hearing the
                        // past, and the oldest is what should go — but a bounded queue can only
                        // refuse the newest, which at this size is the same few milliseconds.
                        if let Some(Err(tokio::sync::mpsc::error::TrySendError::Full(_))) =
                            sounds.get(&app).map(|s| s.try_send(pcm))
                        {
                            sound_dropped += 1;
                            if sound_dropped.is_power_of_two() {
                                log::info!("{sound_dropped} pieces of sound dropped: the link is behind");
                            }
                        }
                    }
                    ToSession::Video { window, frame, viewport, keyframe, captured_us, bytes } => {
                        // One frame, cut to the path's own packet size. Asking the connection
                        // each time rather than assuming an MTU: it changes when the route
                        // does, and a datagram over the limit is refused outright.
                        let room = connection
                            .max_datagram_size()
                            .unwrap_or(1200)
                            .min(1400);
                        let mut sent_all = true;
                        for (parts, part, chunk) in split(&bytes, room) {
                            let mut packet = Vec::with_capacity(room);
                            Packet {
                                window,
                                flags: if keyframe { FLAG_KEYFRAME } else { 0 }
                                    | if part + 1 == parts { FLAG_LAST } else { 0 },
                                frame,
                                viewport,
                                parts,
                                part,
                                captured_us,
                                payload: chunk,
                            }
                            .write(&mut packet);
                            if connection.send_datagram(packet.into()).is_err() {
                                sent_all = false;
                                break;
                            }
                        }
                        if sent_all {
                            frames_sent += 1;
                        } else {
                            // Not fatal and not worth a word each time: a datagram refused
                            // means the congestion controller is full, and the next frame is
                            // more useful than this one anyway.
                            dropped += 1;
                            if dropped.is_power_of_two() {
                                log::info!(
                                    "{dropped} frames dropped before sending ({frames_sent} sent)"
                                );
                            }
                        }
                    }
                }
            }
            _ = connection.closed() => break,
        }
    }
    reading.abort();
}

/// Read one thing the session opened a stream for.
///
/// Almost everything is a single message — one per stream, which costs nothing in QUIC and
/// means a malformed message can never desynchronise the next one. The exception is sound
/// coming *up* from the headset, which is endless and so is recognised by its magic before
/// anything tries to read it to the end. See `spatiand_stream::audio` and `crate::microphone`.
async fn heard(mut stream: quinn::RecvStream, inbox: Sender<FromSession>) {
    // Enough to tell the two apart, and read in a way that tolerates a message shorter than
    // the magic: `Detach` is a couple of bytes.
    let mut first = Vec::new();
    while first.len() < 8 {
        let mut buffer = [0u8; 8];
        match stream.read(&mut buffer).await {
            Ok(Some(n)) => first.extend_from_slice(&buffer[..n]),
            Ok(None) | Err(_) => break,
        }
    }
    // Everything the session says comes down one ordered stream; see
    // `spatiand_stream::CONTROL_MAGIC` for why it has to be one.
    if first.len() >= 8 && first[..4] == spatiand_stream::CONTROL_MAGIC {
        listen_control(stream, first[4..].to_vec(), inbox).await;
        return;
    }
    let magic = first.len() >= 8
        && spatiand_stream::audio::AudioHeader::length(&first[..8].try_into().unwrap()).is_some();
    if !magic {
        match stream.read_to_end(64 * 1024).await {
            Ok(rest) => {
                first.extend_from_slice(&rest);
                match spatiand_stream::from_bytes::<ClientMessage>(&first) {
                    Ok(message) => {
                        let _ = inbox.send(FromSession::Said(message));
                    }
                    Err(e) => log::warn!("a session sent something unreadable: {e}"),
                }
            }
            Err(e) => log::warn!("could not read what a session said: {e}"),
        }
        return;
    }
    listen(stream, &first).await;
}

/// Read the session's control stream: length-prefixed messages, in order, until it ends.
///
/// In order is the point. A key's press and its release only mean anything in the sequence
/// they were done in, and this is the only thing that guarantees it — see
/// `spatiand_stream::CONTROL_MAGIC`.
async fn listen_control(mut stream: quinn::RecvStream, rest: Vec<u8>, inbox: Sender<FromSession>) {
    /// No single message is anywhere near this. A length beyond it is a desynchronised
    /// stream, and reading it would be an allocation the other end chose.
    const LARGEST: usize = 1 << 20;
    let mut held = rest;
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        // Everything whole that is already in hand.
        loop {
            if held.len() < 4 {
                break;
            }
            let length = u32::from_le_bytes(held[..4].try_into().unwrap()) as usize;
            if length > LARGEST {
                log::warn!("a session's control stream said a message was {length} bytes; closing it");
                return;
            }
            if held.len() < 4 + length {
                break;
            }
            match spatiand_stream::from_bytes::<ClientMessage>(&held[4..4 + length]) {
                Ok(message) => {
                    if inbox.send(FromSession::Said(message)).is_err() {
                        return;
                    }
                }
                Err(e) => log::warn!("a session sent something unreadable: {e}"),
            }
            held.drain(..4 + length);
        }
        match stream.read(&mut buffer).await {
            Ok(Some(n)) => held.extend_from_slice(&buffer[..n]),
            Ok(None) | Err(_) => return,
        }
    }
}

/// Play the wearer's microphone into the graph until the session stops sending it.
async fn listen(mut stream: quinn::RecvStream, first: &[u8]) {
    let microphone = crate::microphone::shared();
    use spatiand_stream::audio::{AudioHeader, Coding};
    let length = match AudioHeader::length(&first[..8].try_into().unwrap()) {
        Some(length) => length,
        None => return,
    };
    let mut body = first[8..].to_vec();
    while body.len() < length {
        let mut buffer = vec![0u8; length - body.len()];
        match stream.read(&mut buffer).await {
            Ok(Some(n)) => body.extend_from_slice(&buffer[..n]),
            Ok(None) | Err(_) => return,
        }
    }
    let rest = body.split_off(length);
    let Some(header) = AudioHeader::decode(&body) else {
        log::warn!("a session sent a sound stream with an unreadable header");
        return;
    };
    log::info!(
        "microphone: {} at {} Hz x {}",
        header.app,
        header.rate,
        header.channels
    );
    // Opus unless the session says otherwise, in which case what arrives is samples and goes
    // straight in. An end that cannot encode falls back and says so in its own log.
    let mut voice = match header.coding {
        Coding::Pcm => None,
        Coding::Opus => match crate::voice::Decoder::new(header.rate, header.channels) {
            Ok(decoder) => Some((decoder, spatiand_stream::audio::Packets::default())),
            Err(e) => {
                log::warn!("microphone: cannot decode what the session is sending ({e})");
                return;
            }
        },
    };
    let mut put = |bytes: &[u8]| -> bool {
        let Some((decoder, packets)) = voice.as_mut() else {
            if let Ok(mut microphone) = microphone.lock() {
                microphone.feed(bytes, header.rate, header.channels);
            }
            return true;
        };
        packets.feed(bytes);
        loop {
            match packets.next() {
                Ok(None) => return true,
                Err(e) => {
                    log::warn!("microphone: {e}");
                    return false;
                }
                Ok(Some(packet)) => match decoder.decode(&packet) {
                    // One bad packet is a click, not a reason to hang up.
                    Err(e) => log::warn!("microphone: a packet would not decode ({e})"),
                    Ok(pcm) => {
                        if let Ok(mut microphone) = microphone.lock() {
                            microphone.feed(&pcm, header.rate, header.channels);
                        }
                    }
                },
            }
        }
    };
    if !rest.is_empty() {
        put(&rest);
    }
    let mut buffer = vec![0u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(Some(n)) => {
                if !put(&buffer[..n]) {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    drop(put);
    if let Ok(mut microphone) = microphone.lock() {
        microphone.quiet();
    }
    log::info!("microphone: the session stopped sending; the device here stays and goes quiet");
}

/// Open a stream for one application's sound and keep writing to it, from a task of its own.
fn sound_stream(connection: &Connection, app: String) -> tokio::sync::mpsc::Sender<Vec<u8>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let connection = connection.clone();
    tokio::spawn(async move {
        let mut stream = match connection.open_uni().await {
            Ok(stream) => stream,
            Err(e) => {
                log::warn!("could not open a sound stream for {app}: {e}");
                return;
            }
        };
        let header = spatiand_stream::audio::AudioHeader {
            app: app.clone(),
            rate: spatiand_stream::audio::RATE,
            channels: spatiand_stream::audio::CHANNELS,
            // An application's sound stays raw: it is the downlink, which has room, and a
            // codec there would cost delay on every window that makes a noise.
            coding: spatiand_stream::audio::Coding::Pcm,
        };
        if stream.write_all(&header.encode()).await.is_err() {
            return;
        }
        log::info!("sound: sending {app}");
        while let Some(pcm) = rx.recv().await {
            if stream.write_all(&pcm).await.is_err() {
                return;
            }
        }
        let _ = stream.finish();
    });
    tx
}
