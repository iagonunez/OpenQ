//! Replica side: connect to a main device and exchange messages with it.

use super::secure::{self, HandshakeError, Hello};
use super::{ClientMsg, KnownServer, PROTOCOL, RemoteConfig, Role, ServerMsg, normalize_code};
use parking_lot::Mutex;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use uuid::Uuid;

/// How to authenticate.
#[derive(Clone, Debug)]
pub enum Auth {
    /// With the token from an earlier pairing.
    Token,
    /// With a pairing code shown on the main device.
    Pair(String),
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
            .name("openq-remote-link".into())
            .spawn(move || {
                let (reason, needs_pairing) = match run(&sh, &addr, auth, &config, rx) {
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
    let hello = Hello { protocol: PROTOCOL, client_id: my_id, client_name: my_name, pairing };
    let secret_for = |s: &secure::ServerInfo| -> Result<Vec<u8>, String> {
        match &auth {
            Auth::Pair(code) => Ok(normalize_code(code).into_bytes()),
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
        ServerMsg::Welcome { role, token } => {
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
    *shared.state.lock() = LinkState::Connected { server: server.name.clone(), role };
    (shared.repaint)();

    // Writer: queued messages, plus a ping every second so the main device knows we're here.
    let sh = shared.clone();
    let writer = std::thread::Builder::new().name("openq-remote-out".into()).spawn(move || {
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
