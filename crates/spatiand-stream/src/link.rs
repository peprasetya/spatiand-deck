//! The session's end of a connection: saying things, hearing things, and pairing.
//!
//! Framing lives here rather than in either program, because both ends have to agree on it
//! byte for byte and a second copy is how they stop agreeing.
//!
//! * The session says one [`ClientMessage`] per unidirectional stream. A malformed message can
//!   never desynchronise the next one, and QUIC streams cost nothing to open.
//! * The host says everything on one unidirectional stream it opens first, each message
//!   prefixed with its length as a little-endian `u32`, because ordering between them matters:
//!   a window's `Stream` must not overtake its `Opened`.

use std::net::SocketAddr;
use std::time::Duration;

use crate::transport::{client_first_contact, pairing_code, peer_fingerprint, Fingerprint, Identity};
use crate::{ClientMessage, HostMessage};

/// Where a host listens unless told otherwise.
///
/// Nothing standard lives here, and it is deliberately not one of the ports a game streamer
/// already uses: a host and a Sunshine can run on the same machine.
pub const DEFAULT_PORT: u16 = 47600;

/// The largest control message believed. The catalogue is the big one, with its icons.
const MAX_CONTROL: usize = 8 * 1024 * 1024;

/// Say one thing to the host.
pub async fn say(connection: &quinn::Connection, message: &ClientMessage) -> Result<(), String> {
    let bytes = crate::to_bytes(message).map_err(|e| format!("could not encode: {e}"))?;
    let mut stream = connection
        .open_uni()
        .await
        .map_err(|e| format!("the link is gone: {e}"))?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| format!("the link is gone: {e}"))?;
    let _ = stream.finish();
    Ok(())
}

/// Read one message from the host's control stream, and hand the stream back for the next.
///
/// `None` is the stream ending, for whatever reason: the host closing, the link dropping, or
/// something unreadable, which is treated as the end rather than skipped because a length
/// prefix that cannot be trusted leaves nothing after it that can be.
pub async fn hear(mut stream: quinn::RecvStream) -> Option<(HostMessage, quinn::RecvStream)> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await.ok()?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_CONTROL {
        log::warn!("a control message claiming {length} bytes is not one");
        return None;
    }
    let mut bytes = vec![0u8; length];
    stream.read_exact(&mut bytes).await.ok()?;
    match crate::from_bytes::<HostMessage>(&bytes) {
        Ok(message) => Some((message, stream)),
        Err(e) => {
            log::warn!("unreadable control message: {e}");
            None
        }
    }
}

/// Write one control message the way [`hear`] reads it. The host's half.
pub async fn tell(stream: &mut quinn::SendStream, message: &HostMessage) -> Result<(), String> {
    let bytes = crate::to_bytes(message).map_err(|e| format!("could not encode: {e}"))?;
    stream
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(&bytes).await.map_err(|e| e.to_string())
}

/// Turn what somebody typed into somewhere to connect: a name or address, with the default
/// port added when none was given.
pub async fn resolve(address: &str) -> Result<SocketAddr, String> {
    let with_port = if has_port(address) {
        address.to_string()
    } else if address.contains(':') && !address.starts_with('[') {
        // A bare IPv6 address: it has colons of its own, so the port needs brackets round it.
        format!("[{address}]:{DEFAULT_PORT}")
    } else {
        format!("{address}:{DEFAULT_PORT}")
    };
    let mut found = tokio::net::lookup_host(with_port.as_str())
        .await
        .map_err(|e| format!("cannot find {address}: {e}"))?;
    found.next().ok_or_else(|| format!("cannot find {address}"))
}

fn has_port(address: &str) -> bool {
    if let Some(rest) = address.strip_prefix('[') {
        // [v6]:port
        return rest.split_once("]:").is_some();
    }
    // name:port or v4:port — exactly one colon. More than one is a bare IPv6 address.
    address.matches(':').count() == 1
}

/// How a pairing is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pairing {
    /// Connected, and this is what the host is. Show the code.
    Compare { fingerprint: Fingerprint, code: String },
}

/// The outcome of a pairing the host agreed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub fingerprint: Fingerprint,
    /// What the host calls itself.
    pub name: String,
}

/// Pair with a host that is waiting to be paired with.
///
/// Connects without knowing what the host is, reports the host's fingerprint and the code
/// both ends will show, then waits for the host to be told yes or no by the person at it.
/// **It does not decide anything.** Whether the wearer agrees the codes match is asked
/// separately, and nothing should be written down unless both the host said yes here *and* the
/// wearer said yes there: a host saying yes proves only that *something* said yes.
pub async fn pair(
    identity: &Identity,
    address: &str,
    mut progress: impl FnMut(Pairing),
) -> Result<Paired, String> {
    let target = resolve(address).await?;
    let endpoint = client_first_contact(identity)?;
    let connection = endpoint
        .connect(target, "spatiand")
        .map_err(|e| format!("cannot reach {address}: {e}"))?;
    let connection = tokio::time::timeout(Duration::from_secs(10), connection)
        .await
        .map_err(|_| format!("{address} did not answer; is spatiand-host running on it?"))?
        .map_err(|e| format!("{address} would not connect: {e}"))?;

    let fingerprint = peer_fingerprint(&connection)
        .ok_or_else(|| "the host presented no certificate".to_string())?;
    progress(Pairing::Compare {
        fingerprint,
        code: pairing_code(&identity.fingerprint(), &fingerprint),
    });

    say(
        &connection,
        &ClientMessage::Hello {
            version: crate::VERSION,
            codecs: Vec::new(),
            max_size: (0, 0),
            refresh_mhz: 0,
            session: "spatiand pairing".into(),
        },
    )
    .await
    .map_err(|_| not_pairing(address))?;

    // The host says nothing until somebody at it answers, so this waits on a person.
    let wait = async {
        let control = connection.accept_uni().await.map_err(|_| not_pairing(address))?;
        let mut control = control;
        loop {
            let Some((message, rest)) = hear(control).await else {
                return Err(format!("{address} closed the connection without saying yes"));
            };
            control = rest;
            match message {
                HostMessage::Welcome { host, .. } => {
                    return Ok(Paired {
                        fingerprint,
                        name: host,
                    })
                }
                HostMessage::Refused { reason, .. } => return Err(reason),
                _ => {}
            }
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(180), wait)
        .await
        .unwrap_or_else(|_| Err("nobody answered on the host in three minutes".into()));
    connection.close(0u32.into(), b"pairing done");
    result
}

fn not_pairing(address: &str) -> String {
    format!(
        "{address} is not expecting a new headset. On it, run  spatiand-host --pair  and try \
         again within two minutes."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{server, Gate, Trust};
    use std::sync::Arc;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
    }

    #[test]
    fn a_typed_address_gets_the_default_port_only_when_it_has_none() {
        assert!(!has_port("workshop"));
        assert!(has_port("workshop:47600"));
        assert!(has_port("192.168.1.20:1"));
        assert!(!has_port("fe80::1"), "a bare v6 address is all colons and no port");
        assert!(has_port("[fe80::1]:47600"));
        assert!(!has_port("[fe80::1]"));
        runtime().block_on(async {
            assert_eq!(resolve("127.0.0.1").await.unwrap().port(), DEFAULT_PORT);
            assert_eq!(resolve("127.0.0.1:9").await.unwrap().port(), 9);
            assert_eq!(resolve("::1").await.unwrap().port(), DEFAULT_PORT);
        });
    }

    /// A host that lets anyone through its gate, and answers the first session the way the
    /// real host does once the person at it has said `answer`.
    async fn host_that_answers(answer: Option<&'static str>) -> (SocketAddr, Fingerprint, Arc<Gate>) {
        let identity = Identity::new().unwrap();
        let fingerprint = identity.fingerprint();
        let gate = Arc::new(Gate::new(Vec::new()));
        gate.set_open(true);
        let endpoint =
            server("127.0.0.1:0".parse().unwrap(), &identity, Trust::Gate(gate.clone())).unwrap();
        let address = endpoint.local_addr().unwrap();
        tokio::spawn(async move {
            let Some(incoming) = endpoint.accept().await else { return };
            let Ok(connection) = incoming.await else { return };
            let Ok(mut control) = connection.open_uni().await else { return };
            // The person at the host takes a moment.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let message = match answer {
                Some(name) => HostMessage::Welcome {
                    version: crate::VERSION,
                    host: name.into(),
                    reattached: false,
                },
                None => HostMessage::Refused {
                    version: crate::VERSION,
                    reason: "the code was not confirmed on the host".into(),
                },
            };
            let _ = tell(&mut control, &message).await;
            let _ = connection.closed().await;
        });
        (address, fingerprint, gate)
    }

    #[test]
    fn pairing_shows_the_code_first_and_learns_the_hosts_name_when_it_says_yes() {
        let session = Identity::new().unwrap();
        runtime().block_on(async {
            let (address, host, _gate) = host_that_answers(Some("workshop")).await;
            let mut seen = Vec::new();
            let paired = pair(&session, &address.to_string(), |p| seen.push(p))
                .await
                .expect("pairs");
            assert_eq!(paired.fingerprint, host);
            assert_eq!(paired.name, "workshop");
            assert_eq!(
                seen,
                vec![Pairing::Compare {
                    fingerprint: host,
                    code: pairing_code(&session.fingerprint(), &host),
                }],
                "the code has to be on screen before the host is asked"
            );
        });
    }

    #[test]
    fn a_host_that_says_no_is_a_failed_pairing_with_its_reason() {
        let session = Identity::new().unwrap();
        runtime().block_on(async {
            let (address, _, _gate) = host_that_answers(None).await;
            let error = pair(&session, &address.to_string(), |_| {}).await.unwrap_err();
            assert!(error.contains("not confirmed"), "{error}");
        });
    }

    #[test]
    fn a_host_that_is_not_pairing_says_how_to_make_it() {
        let session = Identity::new().unwrap();
        runtime().block_on(async {
            let (address, _, gate) = host_that_answers(Some("x")).await;
            gate.set_open(false);
            let error = pair(&session, &address.to_string(), |_| {}).await.unwrap_err();
            assert!(error.contains("spatiand-host --pair"), "{error}");
        });
    }
}
