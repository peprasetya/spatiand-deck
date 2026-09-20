//! The link: one encrypted connection carrying everything.
//!
//! ## Why QUIC
//!
//! Three kinds of traffic go between a host and a session, and they want opposite things.
//! Control and keys must arrive, in order. Pictures must arrive *now* or not at all — a
//! retransmitted frame is a frame the wearer has already waited too long for. Head poses are
//! the same, only more so.
//!
//! TCP can only offer the first: one lost packet holds up everything behind it, so a dropped
//! packet becomes a visible freeze rather than a dropped frame. Raw UDP offers only the second,
//! and leaves encryption, congestion control and path discovery to be invented here.
//!
//! QUIC is UDP with all three: **reliable ordered streams** for control, **unreliable
//! datagrams** for pictures and poses, no head-of-line blocking between them, and encryption
//! that cannot be switched off. It also survives the wearer's machine changing address, which
//! a headset carried between a wireless network and a tunnel does routinely.
//!
//! The cost is about thirty bytes a packet and some encryption, against a video stream measured
//! in megabits. The one thing to watch is that datagrams are paced by the congestion
//! controller, which can add delay under load; if that ever proves worse than the alternative,
//! pictures can move to a plain UDP socket beside this connection without changing anything
//! else here.
//!
//! ## Who is allowed to connect
//!
//! Both ends hold a long-lived self-signed certificate, and **each trusts exactly the
//! fingerprints it has been told to trust**. There is no certificate authority, no name to
//! verify and nothing to expire: this is two machines that have met, not a browser visiting a
//! stranger.
//!
//! Trust is established once, by pairing (see [`pairing_code`]), and written down. Until a host has been
//! paired it trusts nobody, which is the only safe default — an address range is not an
//! identity, and the one people reach for (`100.64.0.0/10`) is shared carrier space that a
//! phone tether can hand out.

use std::net::SocketAddr;
use std::sync::Arc;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Endpoint, ServerConfig, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};

/// The name both ends use in their certificates.
///
/// It is never checked against anything — a fingerprint is the identity here — but a
/// certificate has to have a name, and one that says what this is beats one that claims to be
/// a domain somebody else owns.
const CERT_NAME: &str = "spatiand";

/// A certificate's SHA-256 digest: what one machine remembers about another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(cert: &CertificateDer<'_>) -> Fingerprint {
        let mut hasher = Sha256::new();
        hasher.update(cert.as_ref());
        Fingerprint(hasher.finalize().into())
    }

    /// Short enough to read out loud, long enough to mean something.
    ///
    /// Eight hex characters of a SHA-256 is what gets shown when a person is asked to compare
    /// two machines by eye; the whole digest is what is actually compared in code.
    pub fn short(&self) -> String {
        self.0[..4].iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Fingerprint {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.len() != 64 {
            return Err(format!("a fingerprint is 64 hex characters, not {}", text.len()));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(|_| "a fingerprint is hexadecimal".to_string())?;
        }
        Ok(Fingerprint(out))
    }
}

/// This machine's own certificate and key, kept between runs.
///
/// Kept, because a fingerprint the other end has written down must still be ours tomorrow. A
/// host that generated a fresh certificate each time would have to be paired again after every
/// restart.
#[derive(Clone)]
pub struct Identity {
    pub certificate: CertificateDer<'static>,
    key: Arc<PrivateKeyDer<'static>>,
}

impl Identity {
    /// Make a new one.
    pub fn new() -> Result<Identity, String> {
        let cert = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_string()])
            .map_err(|e| format!("could not make a certificate: {e}"))?;
        Ok(Identity {
            certificate: CertificateDer::from(cert.cert),
            key: Arc::new(PrivateKeyDer::try_from(cert.signing_key.serialize_der()).map_err(
                |e| format!("could not keep the new key: {e}"),
            )?),
        })
    }

    /// Load the one in `dir`, making it the first time.
    pub fn load_or_create(dir: &std::path::Path) -> Result<Identity, String> {
        let cert_path = dir.join("identity.cert");
        let key_path = dir.join("identity.key");
        if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            let key = PrivateKeyDer::try_from(key)
                .map_err(|e| format!("{} is not a key: {e}", key_path.display()))?;
            return Ok(Identity {
                certificate: CertificateDer::from(cert),
                key: Arc::new(key),
            });
        }
        let identity = Identity::new()?;
        std::fs::create_dir_all(dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
        std::fs::write(&cert_path, identity.certificate.as_ref())
            .map_err(|e| format!("could not write {}: {e}", cert_path.display()))?;
        std::fs::write(&key_path, identity.key.secret_der())
            .map_err(|e| format!("could not write {}: {e}", key_path.display()))?;
        // The key is readable by this user and nobody else. It is not a password — it cannot be
        // used away from a machine the other end has paired with — but it is still the thing
        // that says "I am this host".
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
        log::info!("new identity {}", identity.fingerprint());
        Ok(identity)
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.certificate)
    }

    fn key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

/// How long a connection may go quiet before it is assumed gone.
///
/// Short, because the interesting case is a headset that has walked out of range: the host
/// should stop encoding for nobody promptly. Applications are untouched either way — losing a
/// viewer is not losing anything else.
const IDLE_TIMEOUT_MS: u32 = 5_000;
/// How often a quiet connection proves it is still there.
const KEEPALIVE_MS: u64 = 1_000;

fn transport_config() -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    config.max_idle_timeout(Some(
        std::time::Duration::from_millis(IDLE_TIMEOUT_MS as u64)
            .try_into()
            .expect("a valid timeout"),
    ));
    config.keep_alive_interval(Some(std::time::Duration::from_millis(KEEPALIVE_MS)));
    // Room for a keyframe or two in flight. A keyframe of a busy picture is a few hundred
    // kilobytes, and quinn drops datagrams silently — oldest first when sending, newest when
    // receiving — once these fill, which loses a piece of exactly the frame everything after it
    // depends on.
    config.datagram_send_buffer_size(4 << 20);
    config.datagram_receive_buffer_size(Some(8 << 20));
    Arc::new(config)
}

/// Who a host will let in.
#[derive(Debug, Clone)]
pub enum Trust {
    /// These machines and no others.
    Paired(Vec<Fingerprint>),
    /// Anyone, for as long as the host is pairing.
    ///
    /// Safe only because it is deliberate, brief and started from a shell on the host: the
    /// first session to arrive is written down and the window closes. It is never the state a
    /// host sits in.
    Anyone,
    /// Decided per connection, by a [`Gate`] the host can change while it runs.
    ///
    /// What a host running as a service needs: it cannot restart to start pairing, because
    /// restarting would take every open application with it.
    Gate(Arc<Gate>),
}

/// Who may connect *right now*: the paired list, plus anybody while a pairing is open.
///
/// Shared between the host's main loop, which changes it, and the TLS handshake, which reads
/// it on every connection.
#[derive(Debug, Default)]
pub struct Gate {
    paired: std::sync::RwLock<Vec<Fingerprint>>,
    open: std::sync::atomic::AtomicBool,
}

impl Gate {
    pub fn new(paired: Vec<Fingerprint>) -> Gate {
        Gate {
            paired: std::sync::RwLock::new(paired),
            open: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn set_paired(&self, paired: Vec<Fingerprint>) {
        if let Ok(mut list) = self.paired.write() {
            *list = paired;
        }
    }

    /// Let a stranger through the handshake, or stop doing so.
    ///
    /// Getting through is not being trusted: the host still decides what to do with a session
    /// it has not paired with, and serves it nothing until the pairing is confirmed.
    pub fn set_open(&self, open: bool) {
        self.open.store(open, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_open(&self) -> bool {
        self.open.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn admits(&self, who: &Fingerprint) -> bool {
        self.is_open() || self.paired.read().is_ok_and(|list| list.contains(who))
    }
}

/// What a connection's other end turned out to be.
pub fn peer_fingerprint(connection: &quinn::Connection) -> Option<Fingerprint> {
    let identity = connection.peer_identity()?;
    let chain = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    chain.first().map(Fingerprint::of)
}

/// Listen for a session.
pub fn server(bind: SocketAddr, identity: &Identity, trust: Trust) -> Result<Endpoint, String> {
    let verifier: Arc<dyn ClientCertVerifier> = match trust {
        Trust::Paired(trusted) => Arc::new(Pinned::fixed(trusted)),
        Trust::Anyone => Arc::new(Pinned::fixed(Vec::new())),
        Trust::Gate(gate) => Arc::new(Pinned {
            rule: Rule::Gate(gate),
        }),
    };
    let crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|e| format!("TLS 1.3 is not available: {e}"))?
    .with_client_cert_verifier(verifier)
    .with_single_cert(vec![identity.certificate.clone()], identity.key())
    .map_err(|e| format!("could not use this host's certificate: {e}"))?;

    let mut config = ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(crypto).map_err(|e| format!("QUIC refused the TLS setup: {e}"))?,
    ));
    config.transport_config(transport_config());
    Endpoint::server(config, bind).map_err(|e| format!("could not listen on {bind}: {e}"))
}

/// Connect to a host this session has **not** met, to find out who it is.
///
/// The one place a session accepts a certificate it was not told about, and it is safe only
/// because of what happens next: nothing is written down until the wearer has compared the
/// [`pairing_code`] shown here with the one the host shows, and said they match. A machine
/// in the middle would have to present its own certificate, which gives a different code.
///
/// Read the fingerprint off the connection with [`peer_fingerprint`].
pub fn client_first_contact(identity: &Identity) -> Result<Endpoint, String> {
    client_trusting(identity, Vec::new())
}

/// Connect to a host, trusting exactly the fingerprint it was paired with.
pub fn client(identity: &Identity, expect: Fingerprint) -> Result<Endpoint, String> {
    client_trusting(identity, vec![expect])
}

fn client_trusting(identity: &Identity, trusted: Vec<Fingerprint>) -> Result<Endpoint, String> {
    let crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|e| format!("TLS 1.3 is not available: {e}"))?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(Pinned::fixed(trusted)))
    .with_client_auth_cert(vec![identity.certificate.clone()], identity.key())
    .map_err(|e| format!("could not use this session's certificate: {e}"))?;

    let mut config = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).map_err(|e| format!("QUIC refused the TLS setup: {e}"))?,
    ));
    config.transport_config(transport_config());
    // Any port; the host is the one that listens.
    let mut endpoint = Endpoint::client("[::]:0".parse().expect("a valid address"))
        .or_else(|_| Endpoint::client("0.0.0.0:0".parse().expect("a valid address")))
        .map_err(|e| format!("could not open a socket: {e}"))?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

/// The six digits both ends show while pairing, so a person can see they are talking to each
/// other and not to something in between.
///
/// Numeric comparison, the way Bluetooth pairs a keyboard: each side computes the code from
/// **both** fingerprints, so a machine in the middle — which must present its own certificate
/// to each side — makes the two screens disagree. Nothing has to be typed on the headset, and
/// nothing secret crosses the wire. The order of the arguments does not matter.
///
/// Six digits is one chance in a million for an attacker who gets one try, which is what a
/// pairing in progress gives them: a mismatch is refused and the window closes.
pub fn pairing_code(a: &Fingerprint, b: &Fingerprint) -> String {
    let (first, second) = if a.0 <= b.0 { (a, b) } else { (b, a) };
    let mut hasher = Sha256::new();
    hasher.update(b"spatiand pairing v1");
    hasher.update(first.0);
    hasher.update(second.0);
    let digest: [u8; 32] = hasher.finalize().into();
    let number = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{:03} {:03}", number / 1000, number % 1000)
}

/// Accepts exactly the certificates it was told about, in both directions.
///
/// This is `dangerous` in rustls' terms because it ignores the web's whole trust model: no
/// authority, no name checking, no expiry. That model answers "is this really the bank?" and
/// the question here is "is this the machine I paired with?", which a fingerprint answers
/// exactly and a certificate authority cannot answer at all.
#[derive(Debug)]
struct Pinned {
    rule: Rule,
}

#[derive(Debug)]
enum Rule {
    /// Empty means "whoever turns up", which only a host in its pairing window ever uses.
    Fixed(Vec<Fingerprint>),
    Gate(Arc<Gate>),
}

impl Pinned {
    fn fixed(trusted: Vec<Fingerprint>) -> Pinned {
        Pinned {
            rule: Rule::Fixed(trusted),
        }
    }

    fn check(&self, presented: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let fingerprint = Fingerprint::of(presented);
        let admitted = match &self.rule {
            Rule::Fixed(trusted) => trusted.is_empty() || trusted.contains(&fingerprint),
            Rule::Gate(gate) => gate.admits(&fingerprint),
        };
        if admitted {
            Ok(())
        } else {
            Err(rustls::Error::General(format!(
                "not a machine this one has been paired with ({})",
                fingerprint.short()
            )))
        }
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity).map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // TLS 1.3 only, so this cannot be reached; refusing is the honest answer.
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity).map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
    }

    #[test]
    fn a_fingerprint_is_the_same_both_ways_round() {
        let identity = Identity::new().unwrap();
        let text = identity.fingerprint().to_string();
        let back: Fingerprint = text.parse().unwrap();
        assert_eq!(back, identity.fingerprint());
        assert_eq!(identity.fingerprint().short().len(), 8);
    }

    #[test]
    fn nonsense_is_not_a_fingerprint() {
        assert!("".parse::<Fingerprint>().is_err());
        assert!("zz".repeat(32).parse::<Fingerprint>().is_err());
        assert!("ab".repeat(31).parse::<Fingerprint>().is_err());
    }

    #[test]
    fn an_identity_kept_on_disk_stays_the_same() {
        let dir = std::env::temp_dir().join(format!("spatiand-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = Identity::load_or_create(&dir).unwrap();
        let again = Identity::load_or_create(&dir).unwrap();
        assert_eq!(
            first.fingerprint(),
            again.fingerprint(),
            "a host that forgets its identity has to be paired again every restart"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole point, end to end: two machines that have each other's fingerprints can talk,
    /// and everything else is refused.
    ///
    #[test]
    fn paired_machines_can_talk_and_strangers_cannot() {
        let host = Identity::new().unwrap();
        let session = Identity::new().unwrap();
        let stranger = Identity::new().unwrap();

        runtime().block_on(async {
            let endpoint = server(
                "127.0.0.1:0".parse().unwrap(),
                &host,
                Trust::Paired(vec![session.fingerprint()]),
            )
            .expect("listens");
            let address = endpoint.local_addr().unwrap();

            let accepting = tokio::spawn(async move {
                while let Some(incoming) = endpoint.accept().await {
                    if let Ok(connection) = incoming.await {
                        if let Ok(mut stream) = connection.open_uni().await {
                            let _ = stream.write_all(b"hello").await;
                            let _ = stream.finish();
                            // Hold the connection open long enough for the other end to read.
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    }
                }
            });

            // The paired session gets in and is talked to.
            let good = client(&session, host.fingerprint()).expect("client");
            let connection = good
                .connect(address, CERT_NAME)
                .expect("starts")
                .await
                .expect("the host accepts a paired session");
            let stream = connection.accept_uni().await.expect("a stream");
            let mut stream = stream;
            let said = stream.read_to_end(64).await.expect("reads");
            assert_eq!(said, b"hello");

            // A machine the host has never heard of gets nothing.
            //
            // Note *where* it fails. In TLS 1.3 the client sends its certificate in its last
            // flight and considers the handshake done as soon as the server's is verified —
            // before the server has looked at the client at all. So `connect` can succeed and
            // the refusal arrives a moment later as the connection being closed. Nothing may
            // treat a connected socket as an authorised one; the first exchange is the proof.
            let bad = client(&stranger, host.fingerprint()).expect("client");
            let refused = match bad.connect(address, CERT_NAME).expect("starts").await {
                Err(_) => true,
                Ok(connection) => connection.accept_uni().await.is_err(),
            };
            assert!(
                refused,
                "an unpaired machine must not be able to use the connection"
            );

            // And a session that is lied to about which host it is reaching refuses to speak.
            let suspicious = client(&session, stranger.fingerprint()).expect("client");
            let refused = suspicious.connect(address, CERT_NAME).expect("starts").await;
            assert!(
                refused.is_err(),
                "a session must not talk to a host whose fingerprint it does not recognise"
            );

            accepting.abort();
        });
    }
}

#[cfg(test)]
mod pairing_tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
    }

    /// A host that is pairing takes the next machine along, and says which it was.
    #[test]
    fn a_pairing_host_accepts_a_stranger_and_learns_its_name() {
        let host = Identity::new().unwrap();
        let stranger = Identity::new().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let endpoint =
                server("127.0.0.1:0".parse().unwrap(), &host, Trust::Anyone).expect("listens");
            let address = endpoint.local_addr().unwrap();
            let learned = tokio::spawn(async move {
                let incoming = endpoint.accept().await.expect("something connects");
                let connection = incoming.await.expect("a connection");
                let who = peer_fingerprint(&connection);
                // Hold it open long enough for the other side to finish its handshake.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                who
            });

            let joining = client(&stranger, host.fingerprint()).expect("client");
            let connection = joining
                .connect(address, CERT_NAME)
                .expect("starts")
                .await
                .expect("a pairing host lets anybody in");
            connection.close(0u32.into(), b"done");

            assert_eq!(
                learned.await.unwrap(),
                Some(stranger.fingerprint()),
                "pairing has to learn who it paired with, or it cannot write it down"
            );
        });
    }

    #[test]
    fn both_ends_show_the_same_code_and_a_stranger_changes_it() {
        let host = Identity::new().unwrap().fingerprint();
        let session = Identity::new().unwrap().fingerprint();
        let middle = Identity::new().unwrap().fingerprint();
        let code = pairing_code(&host, &session);
        assert_eq!(code, pairing_code(&session, &host), "order must not matter");
        assert_eq!(code.len(), 7, "three digits, a space, three digits: {code}");
        assert!(code.chars().filter(|c| c.is_ascii_digit()).count() == 6);
        assert_ne!(
            pairing_code(&middle, &session),
            code,
            "a machine in the middle has to make the screens disagree"
        );
    }

    #[test]
    fn a_first_contact_connects_to_a_host_it_has_never_met_and_learns_who_it_is() {
        let host = Identity::new().unwrap();
        let session = Identity::new().unwrap();
        runtime().block_on(async {
            let endpoint =
                server("127.0.0.1:0".parse().unwrap(), &host, Trust::Anyone).expect("listens");
            let address = endpoint.local_addr().unwrap();
            let held = tokio::spawn(async move {
                let connection = endpoint.accept().await.unwrap().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                drop(connection);
            });
            let first = client_first_contact(&session).expect("client");
            let connection = first
                .connect(address, CERT_NAME)
                .unwrap()
                .await
                .expect("a first contact accepts whatever the host is");
            assert_eq!(peer_fingerprint(&connection), Some(host.fingerprint()));
            connection.close(0u32.into(), b"done");
            let _ = held.await;
        });
    }

    #[test]
    fn a_gate_lets_in_the_paired_always_and_strangers_only_while_open() {
        let host = Identity::new().unwrap();
        let friend = Identity::new().unwrap();
        let stranger = Identity::new().unwrap();
        let gate = Arc::new(Gate::new(vec![friend.fingerprint()]));
        runtime().block_on(async {
            let endpoint = server(
                "127.0.0.1:0".parse().unwrap(),
                &host,
                Trust::Gate(gate.clone()),
            )
            .unwrap();
            let address = endpoint.local_addr().unwrap();
            let accepting = tokio::spawn(async move {
                while let Some(incoming) = endpoint.accept().await {
                    tokio::spawn(async move {
                        if let Ok(connection) = incoming.await {
                            if let Ok(mut stream) = connection.open_uni().await {
                                let _ = stream.write_all(b"hello").await;
                                let _ = stream.finish();
                                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            }
                        }
                    });
                }
            });

            // The same proof the test above uses: being *talked to*, not merely connecting.
            async fn usable(me: &Identity, host: &Identity, address: std::net::SocketAddr) -> bool {
                let endpoint = client(me, host.fingerprint()).unwrap();
                let Ok(connecting) = endpoint.connect(address, CERT_NAME) else {
                    return false;
                };
                let Ok(connection) = connecting.await else {
                    return false;
                };
                let Ok(mut stream) = connection.accept_uni().await else {
                    return false;
                };
                stream.read_to_end(64).await.is_ok_and(|said| said == b"hello")
            }

            assert!(usable(&friend, &host, address).await, "a paired session gets in");
            assert!(
                !usable(&stranger, &host, address).await,
                "a stranger does not while the gate is shut"
            );
            gate.set_open(true);
            assert!(usable(&stranger, &host, address).await, "and does while it is open");
            gate.set_open(false);
            gate.set_paired(vec![friend.fingerprint(), stranger.fingerprint()]);
            assert!(
                usable(&stranger, &host, address).await,
                "and does for good once written down, with the gate shut again"
            );
            accepting.abort();
        });
    }
}
