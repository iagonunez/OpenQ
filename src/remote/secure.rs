//! Authenticated, encrypted link between two OpnQ instances.
//!
//! Both ends share a secret: the pairing code shown on the main device, or the device token
//! handed out when pairing. The handshake:
//!
//! 1. client → `Hello` (protocol, device id and name, pairing or token)          — plain frame
//! 2. server → `Challenge` (its id and name) or `Refused`                        — plain frame
//! 3. SPAKE2 messages both ways (client = A, server = B). Both ids go into the identities, so
//!    the key is bound to these two devices. A wrong secret just gives different keys, and an
//!    eavesdropper can't check guesses against what they saw.
//! 4. Keys come from HKDF-SHA256 over the SPAKE2 key, salted with a hash of the whole
//!    transcript: one key per direction plus two confirmation keys.
//! 5. client → HMAC(client confirmation key, transcript); the server checks it in constant
//!    time, then answers with its own HMAC (or a refusal). The client checks that too, so each
//!    side knows the other has the secret before anything else is sent.
//! 6. Everything after that is ChaCha20-Poly1305: one key per direction, and a 64-bit message
//!    counter as the nonce, so a replayed, reordered or altered frame fails to decrypt.
//!
//! A message is JSON. Big ones (a whole show with a large lighting design) are deflated and
//! sent in parts of at most 16 MB, each its own sealed frame; the first plaintext byte
//! of every frame says which (whole message, part, last part).

use super::PROTOCOL;
use anyhow::{Context, anyhow, bail};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;
use uuid::Uuid;

/// Largest frame accepted.
const MAX_FRAME: usize = PART + 4096;
/// Deflated messages go in parts of this size.
const PART: usize = 16 << 20;
/// Messages from this size on are deflated (small ones, e.g. live state, go as they are).
const COMPRESS_FROM: usize = 64 << 10;
/// Largest deflated message accepted (all parts together).
const MAX_PACKED: usize = 512 << 20;
/// Largest message accepted once inflated (a big rig's lighting design is a few hundred MB).
pub const MAX_MESSAGE: usize = 1 << 30;
/// Frame kind: the rest is a whole JSON message.
const KIND_PLAIN: u8 = 0;
/// Frame kind: part of a deflated message, more follow.
const KIND_PART: u8 = 1;
/// Frame kind: last (or only) part of a deflated message.
const KIND_LAST: u8 = 2;
/// Largest frame accepted before the peer is authenticated.
const MAX_HANDSHAKE_FRAME: usize = 4096;
/// Longest a whole handshake may take: a peer that trickles bytes can't hold a connection (and
/// one of the main device's few connection slots) open for longer.
pub const HANDSHAKE_TIME: std::time::Duration = std::time::Duration::from_secs(10);

fn write_frame(w: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(data.len()).map_err(|_| std::io::Error::other("frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(data)?;
    w.flush()
}

fn read_frame(r: &mut impl Read, max: usize) -> anyhow::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        bail!("frame of {len} bytes is too large");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Fill `buf` before `deadline` (the read timeout alone restarts with every byte that arrives).
/// Running out of time is a [`std::io::ErrorKind::TimedOut`] error.
fn read_exact_by(stream: &mut TcpStream, mut buf: &mut [u8], deadline: Instant) -> std::io::Result<()> {
    use std::io::ErrorKind;
    while !buf.is_empty() {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| std::io::Error::new(ErrorKind::TimedOut, "the handshake took too long"))?;
        stream.set_read_timeout(Some(left))?;
        match stream.read(buf) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(std::io::Error::new(ErrorKind::TimedOut, "the handshake took too long"));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A handshake frame, read before `deadline`.
fn read_frame_by(stream: &mut TcpStream, deadline: Instant) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    read_exact_by(stream, &mut len, deadline)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_HANDSHAKE_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("handshake frame of {len} bytes is too large")));
    }
    let mut buf = vec![0u8; len];
    read_exact_by(stream, &mut buf, deadline)?;
    Ok(buf)
}

/// True if a handshake failed because the peer ran out of time ([`HANDSHAKE_TIME`]).
pub fn is_timeout(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(n)
}

/// Sending half of an established link.
pub struct Sender {
    stream: TcpStream,
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Sender {
    /// Encrypt and send one message.
    pub fn send<T: Serialize>(&mut self, msg: &T) -> anyhow::Result<()> {
        let mut plain = vec![KIND_PLAIN];
        serde_json::to_writer(&mut plain, msg)?;
        if plain.len() < COMPRESS_FROM {
            return self.seal(&plain);
        }
        let mut z = flate2::write::DeflateEncoder::new(Vec::with_capacity(plain.len() / 8), flate2::Compression::fast());
        z.write_all(&plain[1..])?;
        drop(plain);
        let packed = z.finish()?;
        let count = packed.len().div_ceil(PART);
        let mut frame = Vec::with_capacity(PART.min(packed.len()) + 1);
        for (i, part) in packed.chunks(PART).enumerate() {
            frame.clear();
            frame.push(if i + 1 == count { KIND_LAST } else { KIND_PART });
            frame.extend_from_slice(part);
            self.seal(&frame)?;
        }
        Ok(())
    }

    fn seal(&mut self, plain: &[u8]) -> anyhow::Result<()> {
        let sealed = self.cipher.encrypt(&nonce(self.counter), plain).map_err(|_| anyhow!("encryption failed"))?;
        self.counter += 1;
        write_frame(&mut self.stream, &sealed)?;
        Ok(())
    }

    /// Close both directions (unblocks the receiving half).
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Receiving half of an established link.
pub struct Receiver {
    stream: std::io::BufReader<TcpStream>,
    cipher: ChaCha20Poly1305,
    counter: u64,
    /// Largest message accepted (JSON bytes, inflated), at most [`MAX_MESSAGE`].
    limit: usize,
}

impl Receiver {
    /// Receive and decrypt one message; fails on any tampering, replay or reordering.
    pub fn recv<T: DeserializeOwned>(&mut self) -> anyhow::Result<T> {
        let limit = self.limit;
        let mut packed: Vec<u8> = Vec::new();
        loop {
            // A frame carries at most `limit` bytes of JSON (or of deflated JSON) plus its header and tag.
            let sealed = read_frame(&mut self.stream, MAX_FRAME.min(limit + 64))?;
            let plain = self
                .cipher
                .decrypt(&nonce(self.counter), sealed.as_slice())
                .map_err(|_| anyhow!("message failed authentication"))?;
            self.counter += 1;
            let Some((&kind, body)) = plain.split_first() else { bail!("empty message") };
            match kind {
                KIND_PLAIN if packed.is_empty() => return Ok(serde_json::from_slice(body)?),
                KIND_PART | KIND_LAST => {
                    if packed.len() + body.len() > MAX_PACKED.min(limit) {
                        bail!("message too large");
                    }
                    packed.extend_from_slice(body);
                    if kind == KIND_LAST {
                        // Inflate with a cap, so a hostile peer can't exhaust memory.
                        let mut json = Vec::new();
                        flate2::read::DeflateDecoder::new(packed.as_slice()).take(limit as u64 + 1).read_to_end(&mut json)?;
                        if json.len() > limit {
                            bail!("message too large");
                        }
                        drop(packed);
                        return Ok(serde_json::from_slice(&json)?);
                    }
                }
                _ => bail!("unexpected frame kind {kind}"),
            }
        }
    }

    /// Largest message `recv` accepts from now on (capped at [`MAX_MESSAGE`]): the main device
    /// keeps it small for devices that can't edit, so they can't make it hold big messages.
    pub fn set_limit(&mut self, bytes: usize) {
        self.limit = bytes.min(MAX_MESSAGE);
    }

    /// Read timeout for `recv`.
    pub fn set_timeout(&self, t: Option<std::time::Duration>) {
        let _ = self.stream.get_ref().set_read_timeout(t);
    }
}

/// First message of a handshake.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Hello {
    /// Protocol version of the client.
    pub protocol: u32,
    /// Client instance id.
    pub client_id: Uuid,
    /// Client display name.
    pub client_name: String,
    /// Authenticate with a pairing code rather than a device token.
    pub pairing: bool,
    /// Only asking whether the other side is there and what it is doing (coupling): the server
    /// answers [`crate::remote::ServerMsg::Welcome`] and hangs up, leaving no trace in its
    /// connection list or log.
    #[serde(default)]
    pub probe: bool,
    /// A member of a coupling group: with `pairing` it is joining the group (the code is the
    /// secret); without, it proves it knows the group's key.
    #[serde(default)]
    pub coupling: bool,
    /// The group it says it belongs to (not when joining).
    #[serde(default)]
    pub group: Option<Uuid>,
    /// The port its server listens on (a joining computer tells where to reach it).
    #[serde(default)]
    pub port: u16,
}

#[derive(Serialize, Deserialize, Debug)]
enum HelloReply {
    Challenge { protocol: u32, server_id: Uuid, server_name: String },
    Refused(String),
}

#[derive(Serialize, Deserialize, Debug)]
enum Verdict {
    Accepted { confirm: Vec<u8> },
    Refused(String),
}

/// Who the main device is.
pub struct ServerInfo {
    /// Instance id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
}

/// Why a handshake failed.
#[derive(Debug)]
pub enum HandshakeError {
    /// The peer didn't know the secret (counts towards lockouts).
    Auth(String),
    /// Refused before authenticating (unknown device, pairing closed...).
    Refused(String),
    /// Network or protocol error.
    Io(anyhow::Error),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Auth(m) | HandshakeError::Refused(m) => f.write_str(m),
            HandshakeError::Io(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for HandshakeError {
    fn from(e: anyhow::Error) -> Self {
        HandshakeError::Io(e)
    }
}

impl From<std::io::Error> for HandshakeError {
    fn from(e: std::io::Error) -> Self {
        HandshakeError::Io(e.into())
    }
}

struct Keys {
    c2s: [u8; 32],
    s2c: [u8; 32],
    confirm_client: [u8; 32],
    confirm_server: [u8; 32],
}

fn derive(spake_key: &[u8], transcript: &[u8; 32]) -> Keys {
    let hk = Hkdf::<Sha256>::new(Some(transcript), spake_key);
    let k = |info: &[u8]| {
        let mut out = [0u8; 32];
        hk.expand(info, &mut out).expect("32 bytes is a valid HKDF length");
        out
    };
    Keys {
        c2s: k(b"opnq remote v1 client-to-server"),
        s2c: k(b"opnq remote v1 server-to-client"),
        confirm_client: k(b"opnq remote v1 confirm client"),
        confirm_server: k(b"opnq remote v1 confirm server"),
    }
}

fn mac(key: &[u8; 32], transcript: &[u8; 32]) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(transcript);
    m.finalize().into_bytes().to_vec()
}

fn mac_ok(key: &[u8; 32], transcript: &[u8; 32], tag: &[u8]) -> bool {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(transcript);
    m.verify_slice(tag).is_ok()
}

fn identities(client: Uuid, server: Uuid) -> (Identity, Identity) {
    (Identity::new(format!("opnq-client:{client}").as_bytes()), Identity::new(format!("opnq-server:{server}").as_bytes()))
}

fn channel(stream: TcpStream, send_key: [u8; 32], recv_key: [u8; 32]) -> anyhow::Result<(Sender, Receiver)> {
    let reader = stream.try_clone()?;
    Ok((
        Sender { stream, cipher: ChaCha20Poly1305::new(Key::from_slice(&send_key)), counter: 0 },
        Receiver {
            stream: std::io::BufReader::new(reader),
            cipher: ChaCha20Poly1305::new(Key::from_slice(&recv_key)),
            counter: 0,
            limit: MAX_MESSAGE,
        },
    ))
}

/// Client side. `secret_for` gets the server's id and name and returns the shared secret
/// (pairing code or the token stored for that server).
pub fn client_handshake(
    mut stream: TcpStream,
    hello: &Hello,
    secret_for: impl FnOnce(&ServerInfo) -> Result<Vec<u8>, String>,
) -> Result<(Sender, Receiver, ServerInfo), HandshakeError> {
    let deadline = Instant::now() + HANDSHAKE_TIME;
    let timeout = stream.read_timeout()?;
    let hello_bytes = serde_json::to_vec(hello).map_err(anyhow::Error::from)?;
    write_frame(&mut stream, &hello_bytes)?;
    let reply_bytes = read_frame_by(&mut stream, deadline)?;
    let server = match serde_json::from_slice(&reply_bytes).context("bad handshake reply")? {
        HelloReply::Challenge { protocol, server_id, server_name } => {
            if protocol != PROTOCOL {
                return Err(HandshakeError::Refused(format!(
                    "The other OpnQ uses a different remote protocol (v{protocol}, this one is v{PROTOCOL}). Update both."
                )));
            }
            ServerInfo { id: server_id, name: server_name }
        }
        HelloReply::Refused(why) => return Err(HandshakeError::Refused(why)),
    };
    let secret = secret_for(&server).map_err(HandshakeError::Refused)?;
    let (id_a, id_b) = identities(hello.client_id, server.id);
    let (spake, msg_a) = Spake2::<Ed25519Group>::start_a(&Password::new(&secret), &id_a, &id_b);
    write_frame(&mut stream, &msg_a)?;
    let msg_b = read_frame_by(&mut stream, deadline)?;
    let key = spake.finish(&msg_b).map_err(|_| HandshakeError::Auth("bad key exchange message".into()))?;
    let transcript: [u8; 32] = Sha256::new()
        .chain_update(&hello_bytes)
        .chain_update(&reply_bytes)
        .chain_update(&msg_a)
        .chain_update(&msg_b)
        .finalize()
        .into();
    let keys = derive(&key, &transcript);
    write_frame(&mut stream, &mac(&keys.confirm_client, &transcript))?;
    let verdict: Verdict = serde_json::from_slice(&read_frame_by(&mut stream, deadline)?).context("bad handshake verdict")?;
    match verdict {
        Verdict::Accepted { confirm } if mac_ok(&keys.confirm_server, &transcript, &confirm) => {}
        Verdict::Accepted { .. } => {
            return Err(HandshakeError::Auth("The other device could not prove it knows the secret".into()));
        }
        Verdict::Refused(why) => return Err(HandshakeError::Auth(why)),
    }
    stream.set_read_timeout(timeout)?;
    let (tx, rx) = channel(stream, keys.c2s, keys.s2c)?;
    Ok((tx, rx, server))
}

/// Server side. `secret_for` looks up the secret for the hello (or refuses it). `may_check` runs
/// once the client's proof has arrived, right before it is checked (the only point where a guess
/// is tested): refusing there checks nothing, so the server can count guesses exactly (pairing
/// attempts) without peers that never send a proof holding attempts.
pub fn server_handshake(
    mut stream: TcpStream,
    server: &ServerInfo,
    secret_for: impl FnOnce(&Hello) -> Result<Vec<u8>, String>,
    may_check: impl FnOnce(&Hello) -> Result<(), String>,
) -> Result<(Sender, Receiver, Hello), HandshakeError> {
    let deadline = Instant::now() + HANDSHAKE_TIME;
    let timeout = stream.read_timeout()?;
    let hello_bytes = read_frame_by(&mut stream, deadline)?;
    let hello: Hello = serde_json::from_slice(&hello_bytes).context("bad hello")?;
    let refuse = |stream: &mut TcpStream, why: String| -> HandshakeError {
        let _ = write_frame(stream, &serde_json::to_vec(&HelloReply::Refused(why.clone())).unwrap_or_default());
        HandshakeError::Refused(why)
    };
    if hello.protocol != PROTOCOL {
        return Err(refuse(&mut stream, format!("This OpnQ uses remote protocol v{PROTOCOL}; the connecting one uses v{}. Update both.", hello.protocol)));
    }
    let secret = match secret_for(&hello) {
        Ok(s) => s,
        Err(why) => return Err(refuse(&mut stream, why)),
    };
    let reply_bytes = serde_json::to_vec(&HelloReply::Challenge { protocol: PROTOCOL, server_id: server.id, server_name: server.name.clone() })
        .map_err(anyhow::Error::from)?;
    write_frame(&mut stream, &reply_bytes)?;
    let (id_a, id_b) = identities(hello.client_id, server.id);
    let (spake, msg_b) = Spake2::<Ed25519Group>::start_b(&Password::new(&secret), &id_a, &id_b);
    let msg_a = read_frame_by(&mut stream, deadline)?;
    write_frame(&mut stream, &msg_b)?;
    let key = spake.finish(&msg_a).map_err(|_| HandshakeError::Auth("bad key exchange message".into()))?;
    let transcript: [u8; 32] = Sha256::new()
        .chain_update(&hello_bytes)
        .chain_update(&reply_bytes)
        .chain_update(&msg_a)
        .chain_update(&msg_b)
        .finalize()
        .into();
    let keys = derive(&key, &transcript);
    let confirm = read_frame_by(&mut stream, deadline)?;
    if let Err(why) = may_check(&hello) {
        let _ = write_frame(&mut stream, &serde_json::to_vec(&Verdict::Refused(why.clone())).unwrap_or_default());
        return Err(HandshakeError::Refused(why));
    }
    if !mac_ok(&keys.confirm_client, &transcript, &confirm) {
        let why = if hello.pairing { "Wrong pairing code" } else { "Authentication failed: pair this device again" };
        let _ = write_frame(&mut stream, &serde_json::to_vec(&Verdict::Refused(why.into())).unwrap_or_default());
        return Err(HandshakeError::Auth(why.into()));
    }
    let verdict = Verdict::Accepted { confirm: mac(&keys.confirm_server, &transcript) };
    write_frame(&mut stream, &serde_json::to_vec(&verdict).map_err(anyhow::Error::from)?)?;
    stream.set_read_timeout(timeout)?;
    let (tx, rx) = channel(stream, keys.s2c, keys.c2s)?;
    Ok((tx, rx, hello))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn run(server_secret: &'static [u8], client_secret: &'static [u8]) -> (Result<String, String>, Result<String, String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let info = ServerInfo { id: Uuid::new_v4(), name: "main".into() };
            match server_handshake(s, &info, |_| Ok(server_secret.to_vec()), |_| Ok(())) {
                Ok((mut tx, mut rx, hello)) => {
                    let got: String = rx.recv().unwrap();
                    tx.send(&format!("{got} from {}", hello.client_name)).unwrap();
                    Ok(got)
                }
                Err(e) => Err(e.to_string()),
            }
        });
        let hello = Hello { protocol: PROTOCOL, client_id: Uuid::new_v4(), client_name: "tablet".into(), pairing: true, probe: false, coupling: false, group: None, port: 0 };
        let client = match client_handshake(TcpStream::connect(addr).unwrap(), &hello, |s| {
            assert_eq!(s.name, "main");
            Ok(client_secret.to_vec())
        }) {
            Ok((mut tx, mut rx, _)) => {
                tx.send(&"hello".to_string()).unwrap();
                rx.recv::<String>().map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        };
        (server.join().unwrap(), client)
    }

    /// Messages bigger than one frame go deflated in several parts and come back whole; small
    /// ones still go plain.
    #[test]
    fn big_messages_go_in_parts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Hex digits deflate to about half: ~40 MB of them need more than one part.
        let mut rng = fastrand::Rng::with_seed(3);
        let big: String = (0..40 << 20).map(|_| char::from_digit(rng.u32(0..16), 16).unwrap()).collect();
        let expect = big.clone();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let info = ServerInfo { id: Uuid::new_v4(), name: "main".into() };
            let (mut tx, mut rx, _) = server_handshake(s, &info, |_| Ok(b"K".to_vec()), |_| Ok(())).unwrap();
            tx.send(&big).unwrap();
            tx.send(&"small".to_string()).unwrap();
            let back: Vec<u32> = rx.recv().unwrap();
            back.len()
        });
        let hello = Hello { protocol: PROTOCOL, client_id: Uuid::new_v4(), client_name: "tablet".into(), pairing: true, probe: false, coupling: false, group: None, port: 0 };
        let (mut tx, mut rx, _) = client_handshake(TcpStream::connect(addr).unwrap(), &hello, |_| Ok(b"K".to_vec())).unwrap();
        assert!(rx.recv::<String>().unwrap() == expect);
        assert_eq!(rx.recv::<String>().unwrap(), "small");
        // Compressible: one part.
        tx.send(&vec![7u32; 1 << 20]).unwrap();
        assert_eq!(server.join().unwrap(), 1 << 20);
    }

    /// A peer that trickles bytes runs into the handshake's overall deadline, although each byte
    /// arrives well within a read timeout.
    #[test]
    fn slow_handshakes_time_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let slow = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            // A 256-byte frame, one byte at a time.
            for b in [0, 0, 1, 0].into_iter().chain(std::iter::repeat(0)).take(24) {
                if s.write_all(&[b]).is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let t = Instant::now();
        let e = read_frame_by(&mut s, Instant::now() + std::time::Duration::from_millis(300)).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        assert!(is_timeout(&anyhow::Error::from(e)));
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
        drop(s);
        slow.join().unwrap();
    }

    /// A receiver refuses messages over its limit, plain or deflated.
    #[test]
    fn messages_over_the_limit_are_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let info = ServerInfo { id: Uuid::new_v4(), name: "main".into() };
            let (_tx, mut rx, _) = server_handshake(s, &info, |_| Ok(b"K".to_vec()), |_| Ok(())).unwrap();
            rx.set_limit(1000);
            let small: String = rx.recv().unwrap();
            let big = rx.recv::<String>().is_err();
            (small, big)
        });
        let hello = Hello { protocol: PROTOCOL, client_id: Uuid::new_v4(), client_name: "tablet".into(), pairing: true, probe: false, coupling: false, group: None, port: 0 };
        let (mut tx, _rx, _) = client_handshake(TcpStream::connect(addr).unwrap(), &hello, |_| Ok(b"K".to_vec())).unwrap();
        tx.send(&"x".repeat(10)).unwrap();
        let _ = tx.send(&"y".repeat(1 << 20));
        let (small, big) = server.join().unwrap();
        assert_eq!(small, "x".repeat(10));
        assert!(big, "a 1 MB message got through a 1000-byte limit");
    }

    #[test]
    fn matching_secrets_connect() {
        let (s, c) = run(b"7KQM3XRT", b"7KQM3XRT");
        assert_eq!(s.unwrap(), "hello");
        assert_eq!(c.unwrap(), "hello from tablet");
    }

    #[test]
    fn wrong_secret_is_refused_on_both_sides() {
        let (s, c) = run(b"7KQM3XRT", b"7KQM3XRX");
        assert_eq!(s.unwrap_err(), "Wrong pairing code");
        assert_eq!(c.unwrap_err(), "Wrong pairing code");
    }

    #[test]
    fn tampered_frames_fail() {
        let key = [7u8; 32];
        let c = ChaCha20Poly1305::new(Key::from_slice(&key));
        let mut sealed = c.encrypt(&nonce(0), b"go".as_slice()).unwrap();
        // Replaying under a different counter, or flipping a bit, must fail.
        assert!(c.decrypt(&nonce(1), sealed.as_slice()).is_err());
        sealed[0] ^= 1;
        assert!(c.decrypt(&nonce(0), sealed.as_slice()).is_err());
    }
}
