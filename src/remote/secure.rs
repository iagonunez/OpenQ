//! Authenticated, encrypted link between two OpenQ instances.
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
use uuid::Uuid;

/// Largest frame accepted (a big workspace is a few MB of JSON).
const MAX_FRAME: usize = 64 << 20;
/// Largest frame accepted before the peer is authenticated.
const MAX_HANDSHAKE_FRAME: usize = 4096;

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
        let plain = serde_json::to_vec(msg)?;
        let sealed = self.cipher.encrypt(&nonce(self.counter), plain.as_slice()).map_err(|_| anyhow!("encryption failed"))?;
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
}

impl Receiver {
    /// Receive and decrypt one message; fails on any tampering, replay or reordering.
    pub fn recv<T: DeserializeOwned>(&mut self) -> anyhow::Result<T> {
        let sealed = read_frame(&mut self.stream, MAX_FRAME)?;
        let plain = self
            .cipher
            .decrypt(&nonce(self.counter), sealed.as_slice())
            .map_err(|_| anyhow!("message failed authentication"))?;
        self.counter += 1;
        Ok(serde_json::from_slice(&plain)?)
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
        c2s: k(b"openq remote v1 client-to-server"),
        s2c: k(b"openq remote v1 server-to-client"),
        confirm_client: k(b"openq remote v1 confirm client"),
        confirm_server: k(b"openq remote v1 confirm server"),
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
    (Identity::new(format!("openq-client:{client}").as_bytes()), Identity::new(format!("openq-server:{server}").as_bytes()))
}

fn channel(stream: TcpStream, send_key: [u8; 32], recv_key: [u8; 32]) -> anyhow::Result<(Sender, Receiver)> {
    let reader = stream.try_clone()?;
    Ok((
        Sender { stream, cipher: ChaCha20Poly1305::new(Key::from_slice(&send_key)), counter: 0 },
        Receiver { stream: std::io::BufReader::new(reader), cipher: ChaCha20Poly1305::new(Key::from_slice(&recv_key)), counter: 0 },
    ))
}

/// Client side. `secret_for` gets the server's id and name and returns the shared secret
/// (pairing code or the token stored for that server).
pub fn client_handshake(
    mut stream: TcpStream,
    hello: &Hello,
    secret_for: impl FnOnce(&ServerInfo) -> Result<Vec<u8>, String>,
) -> Result<(Sender, Receiver, ServerInfo), HandshakeError> {
    let hello_bytes = serde_json::to_vec(hello).map_err(anyhow::Error::from)?;
    write_frame(&mut stream, &hello_bytes)?;
    let reply_bytes = read_frame(&mut stream, MAX_HANDSHAKE_FRAME)?;
    let server = match serde_json::from_slice(&reply_bytes).context("bad handshake reply")? {
        HelloReply::Challenge { protocol, server_id, server_name } => {
            if protocol != PROTOCOL {
                return Err(HandshakeError::Refused(format!(
                    "The other OpenQ uses a different remote protocol (v{protocol}, this one is v{PROTOCOL}). Update both."
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
    let msg_b = read_frame(&mut stream, MAX_HANDSHAKE_FRAME)?;
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
    let verdict: Verdict = serde_json::from_slice(&read_frame(&mut stream, MAX_HANDSHAKE_FRAME)?).context("bad handshake verdict")?;
    match verdict {
        Verdict::Accepted { confirm } if mac_ok(&keys.confirm_server, &transcript, &confirm) => {}
        Verdict::Accepted { .. } => {
            return Err(HandshakeError::Auth("The other device could not prove it knows the secret".into()));
        }
        Verdict::Refused(why) => return Err(HandshakeError::Auth(why)),
    }
    let (tx, rx) = channel(stream, keys.c2s, keys.s2c)?;
    Ok((tx, rx, server))
}

/// Server side. `secret_for` looks up the secret for the hello (or refuses it).
pub fn server_handshake(
    mut stream: TcpStream,
    server: &ServerInfo,
    secret_for: impl FnOnce(&Hello) -> Result<Vec<u8>, String>,
) -> Result<(Sender, Receiver, Hello), HandshakeError> {
    let hello_bytes = read_frame(&mut stream, MAX_HANDSHAKE_FRAME)?;
    let hello: Hello = serde_json::from_slice(&hello_bytes).context("bad hello")?;
    let refuse = |stream: &mut TcpStream, why: String| -> HandshakeError {
        let _ = write_frame(stream, &serde_json::to_vec(&HelloReply::Refused(why.clone())).unwrap_or_default());
        HandshakeError::Refused(why)
    };
    if hello.protocol != PROTOCOL {
        return Err(refuse(&mut stream, format!("This OpenQ uses remote protocol v{PROTOCOL}; the connecting one uses v{}. Update both.", hello.protocol)));
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
    let msg_a = read_frame(&mut stream, MAX_HANDSHAKE_FRAME)?;
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
    let confirm = read_frame(&mut stream, MAX_HANDSHAKE_FRAME)?;
    if !mac_ok(&keys.confirm_client, &transcript, &confirm) {
        let why = if hello.pairing { "Wrong pairing code" } else { "Authentication failed: pair this device again" };
        let _ = write_frame(&mut stream, &serde_json::to_vec(&Verdict::Refused(why.into())).unwrap_or_default());
        return Err(HandshakeError::Auth(why.into()));
    }
    let verdict = Verdict::Accepted { confirm: mac(&keys.confirm_server, &transcript) };
    write_frame(&mut stream, &serde_json::to_vec(&verdict).map_err(anyhow::Error::from)?)?;
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
            match server_handshake(s, &info, |_| Ok(server_secret.to_vec())) {
                Ok((mut tx, mut rx, hello)) => {
                    let got: String = rx.recv().unwrap();
                    tx.send(&format!("{got} from {}", hello.client_name)).unwrap();
                    Ok(got)
                }
                Err(e) => Err(e.to_string()),
            }
        });
        let hello = Hello { protocol: PROTOCOL, client_id: Uuid::new_v4(), client_name: "tablet".into(), pairing: true };
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
