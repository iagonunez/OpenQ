//! Replica side: connect to a main device and exchange messages with it.

use super::secure::{self, HandshakeError, Hello};
use super::coupling::{Grant, Peer, Roster};
use super::{ClientMsg, KnownServer, PROTOCOL, RemoteConfig, Role, ServerMsg, normalize_code};
use parking_lot::Mutex;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use uuid::Uuid;

/// Reason a link closes with when the other device is a standby member of a coupling group (it
/// isn't playing the show, so there is nothing to mirror there).
pub const STANDBY: &str = "This device is on standby, not the main device";

/// How to authenticate.
#[derive(Clone, Debug)]
pub enum Auth {
    /// With the token from an earlier pairing.
    Token,
    /// With a pairing code shown on the main device.
    Pair(String),
    /// With the key of the coupling group both computers belong to.
    Group,
}

/// Where a link is.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkState {
    /// Handshake in progress.
    Connecting,
    /// Authenticated and syncing.
    Connected {
        /// Main device name.
        server: String,
        /// What we may do there.
        role: Role,
    },
    /// Ended; `needs_pairing` when the main device rejected our credentials.
    Closed {
        /// Why it ended, for the user.
        reason: String,
        /// Credentials were rejected: a pairing code is needed.
        needs_pairing: bool,
    },
}

struct Shared {
    state: Mutex<LinkState>,
    inbox: Mutex<Vec<ServerMsg>>,
    stop: AtomicBool,
    /// For closing the connection from `Drop`.
    socket: Mutex<Option<TcpStream>>,
    repaint: Box<dyn Fn() + Send + Sync>,
}

/// A connection to a main device. Dropping it disconnects.
pub struct Link {
    shared: Arc<Shared>,
    tx: std::sync::mpsc::Sender<ClientMsg>,
    /// What the user asked to connect to.
    pub address: String,
}

fn resolve(address: &str) -> anyhow::Result<SocketAddr> {
    let with_port = if address.parse::<SocketAddr>().is_ok() || address.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        address.to_string()
    } else {
        format!("{address}:{}", super::DEFAULT_PORT)
    };
    with_port.to_socket_addrs()?.next().ok_or_else(|| anyhow::anyhow!("cannot resolve {address}"))
}

impl Link {
    /// Start connecting in the background; `repaint` is called whenever something arrives.
    pub fn connect(address: &str, auth: Auth, config: Arc<Mutex<RemoteConfig>>, repaint: impl Fn() + Send + Sync + 'static) -> Self {
        Self::connect_within(address, auth, config, None, repaint)
    }

    /// [`Link::connect`] that gives up on a main device that says nothing for `silence` (it
    /// sends something at least once a second); `None` waits 10 s.
    pub fn connect_within(
        address: &str,
        auth: Auth,
        config: Arc<Mutex<RemoteConfig>>,
        silence: Option<Duration>,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(LinkState::Connecting),
            inbox: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            socket: Mutex::new(None),
            repaint: Box::new(repaint),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let (sh, addr) = (shared.clone(), address.trim().to_string());
        std::thread::Builder::new()
            .name("opnq-remote-link".into())
            .spawn(move || {
                let (reason, needs_pairing) = match run(&sh, &addr, auth, &config, silence, rx) {
                    Ok(()) => ("Disconnected".to_string(), false),
                    Err(LinkError::Auth(why)) => (why, true),
                    Err(LinkError::Other(e)) => (format!("{e:#}"), false),
                };
                *sh.state.lock() = LinkState::Closed { reason, needs_pairing };
                (sh.repaint)();
            })
            .expect("spawn link thread");
        Self { shared, tx, address: address.trim().to_string() }
    }

    /// Current state.
    pub fn state(&self) -> LinkState {
        self.shared.state.lock().clone()
    }

    /// Take the messages received since the last call.
    pub fn drain(&self) -> Vec<ServerMsg> {
        std::mem::take(&mut *self.shared.inbox.lock())
    }

    /// Queue a message for the main device.
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.tx.send(msg);
    }

    /// A clonable handle for sending (e.g. from a cache's fetch hook).
    pub fn sender(&self) -> std::sync::mpsc::Sender<ClientMsg> {
        self.tx.clone()
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.shared.socket.lock().take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

enum LinkError {
    /// Pairing or token rejected: the user needs a (new) pairing code.
    Auth(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for LinkError {
    fn from(e: anyhow::Error) -> Self {
        LinkError::Other(e)
    }
}

impl From<std::io::Error> for LinkError {
    fn from(e: std::io::Error) -> Self {
        LinkError::Other(e.into())
    }
}

fn run(
    shared: &Arc<Shared>,
    address: &str,
    auth: Auth,
    config: &Arc<Mutex<RemoteConfig>>,
    silence: Option<Duration>,
    outbox: std::sync::mpsc::Receiver<ClientMsg>,
) -> Result<(), LinkError> {
    let addr = resolve(address)?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| anyhow::anyhow!("cannot reach {addr}: {e}"))?;
    stream.set_nodelay(true)?;
    *shared.socket.lock() = stream.try_clone().ok();
    if shared.stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let (my_id, my_name) = {
        let c = config.lock();
        (c.instance_id, c.instance_name.clone())
    };
    let pairing = matches!(auth, Auth::Pair(_));
    let group = matches!(auth, Auth::Group).then(|| config.lock().coupling.group.clone()).flatten();
    if matches!(auth, Auth::Group) && group.is_none() {
        return Err(anyhow::anyhow!("This computer is not in a coupling group").into());
    }
    let hello = Hello {
        protocol: PROTOCOL,
        client_id: my_id,
        client_name: my_name,
        pairing,
        probe: false,
        coupling: group.is_some(),
        group: group.as_ref().map(|g| g.id),
        port: 0,
    };
    let secret_for = |s: &secure::ServerInfo| -> Result<Vec<u8>, String> {
        match &auth {
            Auth::Pair(code) => Ok(normalize_code(code).into_bytes()),
            Auth::Group => group.as_ref().map(|g| g.secret()).ok_or_else(|| "This computer is not in a coupling group".to_string()),
            Auth::Token => config
                .lock()
                .servers
                .iter()
                .find(|k| k.id == s.id)
                .map(|k| k.token.clone().into_bytes())
                .ok_or_else(|| format!("Not paired with {} yet: enter the pairing code shown there", s.name)),
        }
    };
    let (mut tx, mut rx, server) = match secure::client_handshake(stream, &hello, secret_for) {
        Ok(ok) => ok,
        Err(HandshakeError::Auth(why)) | Err(HandshakeError::Refused(why)) => return Err(LinkError::Auth(why)),
        Err(HandshakeError::Io(e)) => return Err(LinkError::Other(e)),
    };

    let role = match rx.recv::<ServerMsg>()? {
        ServerMsg::Welcome { presence: Some(p), .. } if !p.acting => return Err(anyhow::anyhow!(STANDBY).into()),
        // Coupling members are not paired devices: nothing to remember about them.
        ServerMsg::Welcome { role, .. } if group.is_some() => role,
        ServerMsg::Welcome { role, token, .. } => {
            let mut c = config.lock();
            match token {
                Some(token) => {
                    c.servers.retain(|k| k.id != server.id);
                    c.servers.push(KnownServer { id: server.id, name: server.name.clone(), address: address.to_string(), token });
                }
                None => {
                    if let Some(k) = c.servers.iter_mut().find(|k| k.id == server.id) {
                        k.address = address.to_string();
                        k.name = server.name.clone();
                    }
                }
            }
            if let Err(e) = c.save() {
                log::warn!("cannot save remote config: {e:#}");
            }
            role
        }
        other => return Err(anyhow::anyhow!("unexpected first message {other:?}").into()),
    };
    if let Some(silence) = silence
        && let Some(s) = shared.socket.lock().as_ref()
    {
        // (The same socket the receiver reads: the main device sends something every second.)
        let _ = s.set_read_timeout(Some(silence));
    }
    *shared.state.lock() = LinkState::Connected { server: server.name.clone(), role };
    (shared.repaint)();

    // Writer: queued messages, plus a ping every second so the main device knows we're here.
    let sh = shared.clone();
    let writer = std::thread::Builder::new().name("opnq-remote-out".into()).spawn(move || {
        while !sh.stop.load(Ordering::Relaxed) {
            let msg = match outbox.recv_timeout(Duration::from_secs(1)) {
                Ok(m) => m,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ClientMsg::Ping,
                Err(_) => break,
            };
            if tx.send(&msg).is_err() {
                break;
            }
        }
        tx.shutdown();
    })?;

    let result = loop {
        if shared.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match rx.recv::<ServerMsg>() {
            Ok(ServerMsg::Role(r)) => {
                if let LinkState::Connected { role, .. } = &mut *shared.state.lock() {
                    *role = r;
                }
                shared.inbox.lock().push(ServerMsg::Role(r));
            }
            Ok(ServerMsg::Refused(why)) if why.contains("removed") => break Err(LinkError::Auth(why)),
            Ok(m) => shared.inbox.lock().push(m),
            Err(e) => {
                break if shared.stop.load(Ordering::Relaxed) { Ok(()) } else { Err(anyhow::anyhow!("Connection lost: {e:#}").into()) };
            }
        }
        (shared.repaint)();
    };
    shared.stop.store(true, Ordering::Relaxed);
    let _ = writer.join();
    result
}

/// Id of a known (paired) server reachable at `address`, if any.
pub fn known_at(config: &RemoteConfig, address: &str) -> Option<Uuid> {
    config.servers.iter().find(|k| k.address == address).map(|k| k.id)
}

/// Ask a coupling member what it is doing: connect, log in with the group key, read its
/// [`super::coupling::Presence`] and the priority list it holds, and hang up. Takes about `limit`
/// at most.
pub fn probe(config: &Arc<Mutex<RemoteConfig>>, id: Uuid, address: &str, limit: Duration) -> (Peer, Option<Roster>) {
    use super::secure::{HandshakeError, Hello};
    let unreachable = |e: &dyn std::fmt::Display| (Peer::Unreachable(e.to_string()), None);
    let Some(group) = config.lock().coupling.group.clone() else { return unreachable(&"not in a group") };
    let addr = match resolve(address) {
        Ok(a) => a,
        Err(e) => return unreachable(&format!("{e:#}")),
    };
    let stream = match TcpStream::connect_timeout(&addr, limit) {
        Ok(s) => s,
        Err(e) => return unreachable(&e),
    };
    if stream.set_nodelay(true).and_then(|_| stream.set_read_timeout(Some(limit))).and_then(|_| stream.set_write_timeout(Some(limit))).is_err() {
        return unreachable(&"socket");
    }
    let (my_id, my_name) = {
        let c = config.lock();
        (c.instance_id, c.instance_name.clone())
    };
    let hello = Hello {
        protocol: PROTOCOL,
        client_id: my_id,
        client_name: my_name,
        pairing: false,
        probe: true,
        coupling: true,
        group: Some(group.id),
        port: 0,
    };
    let secret_for = |s: &super::secure::ServerInfo| -> Result<Vec<u8>, String> {
        if s.id != id {
            return Err(format!("{} answered instead", s.name));
        }
        Ok(group.secret())
    };
    let (_tx, mut rx, _server) = match secure::client_handshake(stream, &hello, secret_for) {
        Ok(ok) => ok,
        Err(HandshakeError::Auth(why)) | Err(HandshakeError::Refused(why)) => return (Peer::NotPaired(why), None),
        Err(HandshakeError::Io(e)) => return unreachable(&format!("{e:#}")),
    };
    match rx.recv::<ServerMsg>() {
        Ok(ServerMsg::Welcome { presence: Some(p), .. }) => {
            let peer = if p.acting {
                Peer::Main { term: p.term }
            } else {
                Peer::Standby { orphan_secs: if p.orphan_secs.is_finite() { p.orphan_secs.max(0.0) } else { 0.0 }, term: p.term }
            };
            (peer, p.roster)
        }
        Ok(ServerMsg::Welcome { .. }) => unreachable(&"coupling is off there"),
        Ok(other) => unreachable(&format!("unexpected {other:?}")),
        Err(e) => unreachable(&format!("{e:#}")),
    }
}

/// Join the coupling group of the computer at `address`, with the pairing code it shows. The
/// group's key and list come back; the caller keeps them ([`super::coupling::Coupling::join`]).
pub fn join(config: &Arc<Mutex<RemoteConfig>>, address: &str, code: &str) -> anyhow::Result<Grant> {
    use super::secure::{HandshakeError, Hello};
    let addr = resolve(address)?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| anyhow::anyhow!("cannot reach {addr}: {e}"))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let (my_id, my_name, port) = {
        let c = config.lock();
        (c.instance_id, c.instance_name.clone(), c.port)
    };
    let hello = Hello { protocol: PROTOCOL, client_id: my_id, client_name: my_name, pairing: true, probe: false, coupling: true, group: None, port };
    let (_tx, mut rx, _server) = match secure::client_handshake(stream, &hello, |_| Ok(normalize_code(code).into_bytes())) {
        Ok(ok) => ok,
        Err(HandshakeError::Auth(why)) | Err(HandshakeError::Refused(why)) => anyhow::bail!(why),
        Err(HandshakeError::Io(e)) => return Err(e),
    };
    match rx.recv::<ServerMsg>()? {
        ServerMsg::Welcome { .. } => {}
        ServerMsg::Refused(why) => anyhow::bail!(why),
        other => anyhow::bail!("unexpected {other:?}"),
    }
    match rx.recv::<ServerMsg>()? {
        ServerMsg::Grant(g) => Ok(*g),
        ServerMsg::Refused(why) => anyhow::bail!(why),
        other => anyhow::bail!("unexpected {other:?}"),
    }
}
