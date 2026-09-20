//! A session, with no headset attached: connect to a host, ask for an application, and write
//! down what arrives.
//!
//! This is the other end of the link reduced to the part that can be checked without a
//! compositor, a GPU or a person wearing anything. It proves the things that are hard to see
//! from the host's side: that the pairing holds, that control messages arrive in order, that
//! frames reassemble from their pieces, and that what comes out is a video file that plays.
//!
//! ```text
//! probe-session <host:port> <fingerprint> [--launch chrome] [--out /tmp/seen.hevc] [--seconds 20]
//! ```
//!
//! The fingerprint is the host's, from `spatiand-host --fingerprint`. This probe's own
//! fingerprint is printed when it starts, which is what a host pairs with.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use spatiand_stream::video::{Arrival, Packet, Reassembler};
use spatiand_stream::{ClientMessage, Fingerprint, HostMessage, Identity};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(address), Some(fingerprint)) = (args.first(), args.get(1)) else {
        eprintln!("probe-session <host:port> <fingerprint> [--launch <app>] [--out <file>] [--seconds n]");
        std::process::exit(2);
    };
    let host: Fingerprint = match fingerprint.parse() {
        Ok(fingerprint) => fingerprint,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let seconds: u64 = value("--seconds").and_then(|v| v.parse().ok()).unwrap_or(20);
    let out = value("--out");

    // Kept between runs, in the same place a session would keep it, so a host only has to pair
    // with this probe once.
    let dir = std::env::temp_dir().join("spatiand-probe-session");
    let identity = Identity::load_or_create(&dir).expect("an identity");
    println!("this probe is {}", identity.fingerprint());

    let endpoint = spatiand_stream::transport::client(&identity, host).expect("a client");
    let address: std::net::SocketAddr = match tokio::net::lookup_host(address.as_str())
        .await
        .ok()
        .and_then(|mut a| a.next())
    {
        Some(address) => address,
        None => {
            eprintln!("cannot resolve {address}");
            std::process::exit(2);
        }
    };
    let connection = endpoint
        .connect(address, "spatiand")
        .expect("a connection attempt")
        .await
        .expect("the host to accept this session");
    println!("connected to {address}");

    say(
        &connection,
        ClientMessage::Hello {
            version: spatiand_stream::VERSION,
            codecs: vec![spatiand_stream::Codec::H265],
            max_size: (3840, 2160),
            refresh_mhz: 72_000,
            session: "probe".into(),
        },
    )
    .await;
    if let Some(app) = value("--launch") {
        say(&connection, ClientMessage::Launch { app }).await;
    }

    let mut file = out.as_ref().map(|path| {
        std::fs::File::create(path).unwrap_or_else(|e| panic!("cannot write {path}: {e}"))
    });
    let mut windows: HashMap<u16, Reassembler> = HashMap::new();
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut lost = 0u64;
    let started = Instant::now();
    // `--resize 1024x768` asks for that size three seconds in, to prove a window really is
    // resized on the host rather than the picture being stretched.
    let resize: Option<(u32, u32)> = value("--resize").and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    let mut asked_resize = false;
    let mut said = Instant::now();
    // Sound, counted per application as it arrives on its own streams.
    let sound: std::sync::Arc<std::sync::Mutex<HashMap<String, u64>>> = Default::default();

    let control = connection.accept_uni().await.expect("a control stream");
    let mut control = Box::pin(read_control(control));

    while started.elapsed() < Duration::from_secs(seconds) {
        tokio::select! {
            message = &mut control => {
                match message {
                    Some((message, rest)) => {
                        println!("{message:?}");
                        control = Box::pin(read_control(rest));
                    }
                    None => break,
                }
            }
            incoming = connection.accept_uni() => {
                let Ok(mut stream) = incoming else { break };
                let sound = sound.clone();
                tokio::spawn(async move {
                    let mut first = [0u8; 8];
                    if stream.read_exact(&mut first).await.is_err() {
                        return;
                    }
                    let Some(length) = spatiand_stream::audio::AudioHeader::length(&first) else {
                        eprintln!("a stream that is not sound");
                        return;
                    };
                    let mut body = vec![0u8; length];
                    if stream.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let Some(header) = spatiand_stream::audio::AudioHeader::decode(&body) else {
                        return;
                    };
                    println!("sound from {} at {} Hz x {}", header.app, header.rate, header.channels);
                    let mut buffer = vec![0u8; 8192];
                    while let Ok(Some(n)) = stream.read(&mut buffer).await {
                        *sound.lock().unwrap().entry(header.app.clone()).or_default() += n as u64;
                    }
                });
            }
            datagram = connection.read_datagram() => {
                let Ok(datagram) = datagram else { break };
                let Some(packet) = Packet::read(&datagram) else {
                    eprintln!("a packet made no sense");
                    continue;
                };
                let window = packet.window;
                match windows.entry(window).or_default().accept(&packet) {
                    Arrival::Frame(frame) => {
                        frames += 1;
                        bytes += frame.bytes.len() as u64;
                        if let Some(file) = file.as_mut() {
                            file.write_all(&frame.bytes).expect("writes");
                        }
                    }
                    Arrival::Lost(frame) => {
                        lost += 1;
                        say(&connection, ClientMessage::WantKeyframe {
                            window: spatiand_stream::WindowId(window as u32),
                        })
                        .await;
                        eprintln!("window {window}: frame {frame} lost, asked for a keyframe");
                    }
                    Arrival::Partial | Arrival::Stale => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }

        if let Some((width, height)) = resize {
            if !asked_resize && started.elapsed() > Duration::from_secs(3) {
                asked_resize = true;
                println!("asking for {width}x{height}");
                say(&connection, ClientMessage::Configure {
                    window: spatiand_stream::WindowId(1),
                    width,
                    height,
                })
                .await;
            }
        }

        if said.elapsed() >= Duration::from_secs(2) {
            let secs = said.elapsed().as_secs_f64();
            // The round trip is what a head will feel, and it is the one number this probe can
            // measure honestly: the two machines' clocks are not the same, so comparing a
            // host's capture time against a local one would measure their disagreement rather
            // than the network.
            let stats = connection.stats();
            println!(
                "{frames} frames, {:.1} Mbit/s, {lost} lost, rtt {:.1} ms, {} datagrams lost \
                 in flight, cwnd {} KiB",
                bytes as f64 * 8.0 / secs / 1_000_000.0,
                connection.rtt().as_secs_f64() * 1000.0,
                stats.path.lost_packets,
                stats.path.cwnd / 1024,
            );
            for (app, heard) in sound.lock().unwrap().drain() {
                println!(
                    "  sound from {app}: {:.0} ms of stereo",
                    heard as f64 / spatiand_stream::audio::FRAME_BYTES as f64
                        / spatiand_stream::audio::RATE as f64
                        * 1000.0
                );
            }
            bytes = 0;
            said = Instant::now();
        }
    }

    say(&connection, ClientMessage::Detach).await;
    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
    println!("{frames} frames in {seconds}s, {lost} lost");
    if let Some(path) = out {
        println!("wrote {path}");
    }
}

/// One message, one stream. Cheap in QUIC, and impossible to desynchronise.
async fn say(connection: &quinn::Connection, message: ClientMessage) {
    let bytes = spatiand_stream::to_bytes(&message).expect("encodes");
    match connection.open_uni().await {
        Ok(mut stream) => {
            let _ = stream.write_all(&bytes).await;
            let _ = stream.finish();
        }
        Err(e) => eprintln!("could not say anything: {e}"),
    }
}

/// Read one length-prefixed control message, and hand back the stream for the next.
async fn read_control(mut stream: quinn::RecvStream) -> Option<(HostMessage, quinn::RecvStream)> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await.ok()?;
    let length = u32::from_le_bytes(length) as usize;
    if length > 8 * 1024 * 1024 {
        eprintln!("a control message claiming {length} bytes is not one");
        return None;
    }
    let mut bytes = vec![0u8; length];
    stream.read_exact(&mut bytes).await.ok()?;
    match spatiand_stream::from_bytes::<HostMessage>(&bytes) {
        Ok(message) => Some((message, stream)),
        Err(e) => {
            eprintln!("unreadable control message: {e}");
            None
        }
    }
}
