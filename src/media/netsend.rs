//! Network cue messages, sent off the engine thread.
//!
//! Looking up a host name can take seconds: an mDNS name (`desk.local`) whose device is switched
//! off, a name with no DNS server answering. The engine thread runs every cue's timing and holds
//! the show lock, so it only queues the message here. A [`NetSender`] worker sends to IP
//! addresses and names it already knows at once; a name it doesn't know is looked up on a thread
//! of its own while that host's messages wait in order, so one slow name never holds up messages
//! to other hosts.
//!
//! TCP messages go to one thread per destination, which keeps its connection open (show devices
//! that take commands over TCP expect a session) and reconnects when it was closed.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use crate::model::NetTransport;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

/// How long a looked-up address is used before it is looked up again (a device that got a new
/// address from DHCP).
const RESOLVED_TTL: Duration = Duration::from_secs(60);
/// How long a failed lookup is remembered: messages meanwhile fail at once instead of each
/// waiting for a new lookup, and a device switched on is found soon after.
const FAILED_TTL: Duration = Duration::from_secs(5);
/// Messages queued for the worker at most (a cue firing in a loop can't grow it forever).
const QUEUE: usize = 1024;
/// Messages waiting for one host's lookup at most.
const WAITING: usize = 64;
/// Lookups running at once at most.
const LOOKUPS: usize = 8;
/// Errors kept for the engine log at most until drained.
const ERRORS: usize = 64;
/// TCP destinations with a connection thread at most.
const TCP_LINKS: usize = 32;
/// Messages waiting for one TCP destination at most.
const TCP_QUEUE: usize = 256;
/// How long connecting or writing to a TCP destination may take.
const TCP_TIMEOUT: Duration = Duration::from_secs(3);

/// Looks up a host name; replaced in tests.
pub type Resolve = Arc<dyn Fn(&str, u16) -> Result<SocketAddr, String> + Send + Sync>;

/// The system's lookup (blocking).
fn system_resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    use std::net::ToSocketAddrs;
    let all: Vec<SocketAddr> = (host, port).to_socket_addrs().map_err(|e| format!("{host}: {e}"))?.collect();
    // Show gear is nearly always IPv4; a name that also has an IPv6 address shouldn't make it the pick.
    all.iter().find(|a| a.is_ipv4()).or(all.first()).copied().ok_or_else(|| format!("cannot resolve {host}"))
}

type Key = (String, u16);

enum Msg {
    Send(Key, NetTransport, Vec<u8>),
    Resolved(Key, Result<SocketAddr, String>),
    Quit,
}

/// Sends Network cue messages from a worker thread (see the module docs).
pub struct NetSender {
    tx: SyncSender<Msg>,
    errors: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NetSender {
    /// Start the worker, using the system's name lookup.
    pub fn new() -> std::io::Result<Self> {
        Self::with_resolver(Arc::new(system_resolve))
    }

    /// Start the worker with a custom name lookup (tests).
    pub fn with_resolver(resolve: Resolve) -> std::io::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE);
        let errors = Arc::new(Mutex::new(Vec::new()));
        let e = errors.clone();
        let done_tx = tx.clone();
        let thread = std::thread::Builder::new()
            .name("opnq-net-send".into())
            .spawn(move || {
                Worker { socket, socket6: None, resolve, errors: e, done: done_tx, cache: HashMap::new(), waiting: HashMap::new(), tcp: HashMap::new() }
                    .run(rx)
            })?;
        Ok(Self { tx, errors, thread: Some(thread) })
    }

    /// Queue `bytes` for `host:port`. Never blocks; an error here means the message was not
    /// queued. Errors found later (lookup, send) come from [`NetSender::drain_errors`].
    pub fn send(&self, host: &str, port: u16, transport: NetTransport, bytes: Vec<u8>) -> Result<(), String> {
        match self.tx.try_send(Msg::Send((host.to_string(), port), transport, bytes)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("too many network messages waiting to be sent".into()),
            Err(TrySendError::Disconnected(_)) => Err("network sender stopped".into()),
        }
    }

    /// Errors since the last call, for the engine log.
    pub fn drain_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.lock())
    }
}

impl Drop for NetSender {
    fn drop(&mut self) {
        // The worker never waits on a lookup, so it takes the message and ends quickly.
        let _ = self.tx.send(Msg::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Worker {
    socket: UdpSocket,
    /// For IPv6 destinations, bound when the first one is sent to.
    socket6: Option<UdpSocket>,
    resolve: Resolve,
    errors: Arc<Mutex<Vec<String>>>,
    /// For lookup threads' answers (a blocking send only waits on the lookup's own thread).
    done: SyncSender<Msg>,
    /// Lookup answers and when they were made.
    cache: HashMap<Key, (Result<SocketAddr, String>, Instant)>,
    /// Messages for hosts being looked up, in order.
    waiting: HashMap<Key, VecDeque<Vec<u8>>>,
    /// TCP destinations' connection threads.
    tcp: HashMap<Key, SyncSender<Vec<u8>>>,
}

impl Worker {
    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv() {
                Ok(Msg::Quit) | Err(_) => return,
                Ok(m) => self.handle(m),
            }
        }
    }

    fn error(&self, e: String) {
        push_error(&self.errors, e);
    }

    fn send_to(&mut self, (host, port): &Key, addr: SocketAddr, bytes: &[u8]) {
        let sent = if addr.is_ipv6() {
            match &self.socket6 {
                Some(s) => s.send_to(bytes, addr),
                None => UdpSocket::bind("[::]:0").and_then(|s| {
                    let r = s.send_to(bytes, addr);
                    self.socket6 = Some(s);
                    r
                }),
            }
        } else {
            self.socket.send_to(bytes, addr)
        };
        if let Err(e) = sent {
            self.error(format!("Network cue to {host}:{port}: {e}"));
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Send(key, NetTransport::Udp, bytes) => self.send(key, bytes),
            Msg::Send(key, NetTransport::Tcp, bytes) => self.send_tcp(key, bytes),
            Msg::Resolved(key, r) => {
                let queued = self.waiting.remove(&key).unwrap_or_default();
                match &r {
                    Ok(addr) => queued.iter().for_each(|b| self.send_to(&key, *addr, b)),
                    Err(e) if !queued.is_empty() => self.error(format!("Network cue to {}:{}: {e}", key.0, key.1)),
                    Err(_) => {}
                }
                self.cache.insert(key, (r, Instant::now()));
            }
            Msg::Quit => {} // handled by `run`
        }
    }

    fn send_tcp(&mut self, key: Key, bytes: Vec<u8>) {
        let bytes = match self.tcp.get(&key) {
            None => bytes,
            Some(tx) => match tx.try_send(bytes) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    return self.error(format!("Network cue to {}:{}: too many messages waiting for the connection", key.0, key.1));
                }
                // The thread ended (it never does by itself, but a panic would): start a new one.
                Err(TrySendError::Disconnected(b)) => {
                    self.tcp.remove(&key);
                    b
                }
            },
        };
        if self.tcp.len() >= TCP_LINKS {
            return self.error(format!("Network cue to {}:{}: too many TCP destinations", key.0, key.1));
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(TCP_QUEUE);
        let _ = tx.try_send(bytes);
        let link = TcpLink { key: key.clone(), resolve: self.resolve.clone(), errors: self.errors.clone(), stream: None, failed: None };
        match std::thread::Builder::new().name(crate::recovery::thread_name("opnq-tcp-", &key.0)).spawn(move || link.run(rx)) {
            Ok(_) => {
                self.tcp.insert(key, tx);
            }
            Err(e) => self.error(format!("Network cue: cannot start a TCP connection: {e}")),
        }
    }

    fn send(&mut self, key: Key, bytes: Vec<u8>) {
        if let Ok(ip) = key.0.trim().parse::<IpAddr>() {
            return self.send_to(&key, SocketAddr::new(ip, key.1), &bytes);
        }
        if let Some(q) = self.waiting.get_mut(&key) {
            if q.len() < WAITING {
                q.push_back(bytes);
            } else {
                self.error(format!("Network cue to {}:{}: too many messages waiting for the address", key.0, key.1));
            }
            return;
        }
        if let Some((r, when)) = self.cache.get(&key) {
            let ttl = if r.is_ok() { RESOLVED_TTL } else { FAILED_TTL };
            if when.elapsed() < ttl {
                match r.clone() {
                    Ok(addr) => self.send_to(&key, addr, &bytes),
                    Err(e) => self.error(format!("Network cue to {}:{}: {e}", key.0, key.1)),
                }
                return;
            }
        }
        if self.waiting.len() >= LOOKUPS {
            return self.error(format!("Network cue to {}:{}: too many host names being looked up", key.0, key.1));
        }
        self.waiting.insert(key.clone(), VecDeque::from([bytes]));
        let (resolve, done) = (self.resolve.clone(), self.done.clone());
        let lookup_key = key.clone();
        let spawned = std::thread::Builder::new().name(crate::recovery::thread_name("opnq-dns-", &key.0)).spawn(move || {
            let key = lookup_key;
            let r = crate::recovery::catch(|| resolve(&key.0, key.1).map_err(anyhow::Error::msg)).map_err(|e| format!("{e:#}"));
            let _ = done.send(Msg::Resolved(key, r));
        });
        if let Err(e) = spawned {
            // No answer will ever come: don't leave the host waiting for one.
            self.waiting.remove(&key);
            self.error(format!("Network cue: cannot look up a host name: {e}"));
        }
    }
}

fn push_error(errors: &Mutex<Vec<String>>, e: String) {
    let mut errors = errors.lock();
    if errors.len() < ERRORS {
        errors.push(e);
    }
}

/// One TCP destination's connection, on its own thread: looking up the name, connecting and
/// writing may all wait, and only this destination's messages wait with them.
struct TcpLink {
    key: Key,
    resolve: Resolve,
    errors: Arc<Mutex<Vec<String>>>,
    stream: Option<TcpStream>,
    /// When connecting last failed: messages meanwhile fail at once (see [`FAILED_TTL`]).
    failed: Option<(Instant, String)>,
}

impl TcpLink {
    fn run(mut self, rx: Receiver<Vec<u8>>) {
        while let Ok(bytes) = rx.recv() {
            if let Err(e) = crate::recovery::catch(|| Ok(self.write(&bytes))).map_err(|e| format!("{e:#}")).and_then(|r| r) {
                push_error(&self.errors, format!("Network cue to {}:{}: {e}", self.key.0, self.key.1));
            }
        }
    }

    fn connect(&mut self) -> Result<&mut TcpStream, String> {
        if self.stream.is_none() {
            if let Some((when, e)) = &self.failed
                && when.elapsed() < FAILED_TTL
            {
                return Err(e.clone());
            }
            let r = (self.resolve)(&self.key.0, self.key.1).and_then(|addr| {
                let s = TcpStream::connect_timeout(&addr, TCP_TIMEOUT).map_err(|e| e.to_string())?;
                s.set_write_timeout(Some(TCP_TIMEOUT)).map_err(|e| e.to_string())?;
                let _ = s.set_nodelay(true);
                Ok(s)
            });
            match r {
                Ok(s) => {
                    self.stream = Some(s);
                    self.failed = None;
                }
                Err(e) => {
                    self.failed = Some((Instant::now(), e.clone()));
                    return Err(e);
                }
            }
        }
        Ok(self.stream.as_mut().expect("just set"))
    }

    /// Whether the peer closed the connection; whatever it sent us is read and dropped.
    fn closed(s: &mut TcpStream) -> bool {
        if s.set_nonblocking(true).is_err() {
            return true;
        }
        let mut buf = [0u8; 4096];
        let closed = loop {
            match s.read(&mut buf) {
                Ok(0) => break true,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break false,
                Err(_) => break true,
            }
        };
        closed || s.set_nonblocking(false).is_err()
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.stream.as_mut().is_some_and(Self::closed) {
            self.stream = None;
        }
        // A connection that broke since the last message: once more on a new one.
        if self.connect()?.write_all(bytes).is_ok() {
            return Ok(());
        }
        self.stream = None;
        let r = self.connect()?.write_all(bytes);
        if r.is_err() {
            self.stream = None;
        }
        r.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listener() -> (UdpSocket, u16) {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let port = s.local_addr().unwrap().port();
        (s, port)
    }

    fn recv(s: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 64];
        let n = s.recv(&mut buf).expect("a message");
        buf[..n].to_vec()
    }

    /// Messages to an IPv6 address go out (the IPv4 socket can't send them).
    #[test]
    fn sends_to_ipv6() {
        let Ok(rx) = UdpSocket::bind("[::1]:0") else { return }; // no IPv6 here
        rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let port = rx.local_addr().unwrap().port();
        let net = NetSender::new().unwrap();
        net.send("::1", port, NetTransport::Udp, b"v6".to_vec()).unwrap();
        assert_eq!(recv(&rx), b"v6");
        assert!(net.drain_errors().is_empty());
    }

    #[test]
    fn a_slow_lookup_holds_up_neither_the_caller_nor_other_hosts() {
        let (rx, port) = listener();
        let resolve: Resolve = Arc::new(|host, port| {
            if host == "slow.local" {
                std::thread::sleep(Duration::from_millis(400));
            }
            Ok(SocketAddr::from(([127, 0, 0, 1], port)))
        });
        let net = NetSender::with_resolver(resolve).unwrap();
        let t = Instant::now();
        net.send("slow.local", port, NetTransport::Udp, b"slow 1".to_vec()).unwrap();
        net.send("slow.local", port, NetTransport::Udp, b"slow 2".to_vec()).unwrap();
        net.send("127.0.0.1", port, NetTransport::Udp, b"ip".to_vec()).unwrap();
        assert!(t.elapsed() < Duration::from_millis(50), "queueing never waits for a lookup");
        assert_eq!(recv(&rx), b"ip", "an IP address goes out while the name is looked up");
        assert_eq!(recv(&rx), b"slow 1");
        assert_eq!(recv(&rx), b"slow 2", "a host's messages keep their order");
        // Known now: no second lookup.
        let t = Instant::now();
        net.send("slow.local", port, NetTransport::Udp, b"slow 3".to_vec()).unwrap();
        assert_eq!(recv(&rx), b"slow 3");
        assert!(t.elapsed() < Duration::from_millis(200), "cached");
        assert!(net.drain_errors().is_empty());
    }

    #[test]
    fn a_failed_lookup_is_reported_once_per_message() {
        let resolve: Resolve = Arc::new(|host, _| Err(format!("cannot resolve {host}")));
        let net = NetSender::with_resolver(resolve).unwrap();
        net.send("gone.local", 9, NetTransport::Udp, b"a".to_vec()).unwrap();
        let t = Instant::now();
        let mut errors = Vec::new();
        while errors.is_empty() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            errors = net.drain_errors();
        }
        assert_eq!(errors, ["Network cue to gone.local:9: cannot resolve gone.local"]);
        // Remembered: the next message fails without a new lookup.
        net.send("gone.local", 9, NetTransport::Udp, b"b".to_vec()).unwrap();
        let t = Instant::now();
        while errors.len() < 2 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            errors.extend(net.drain_errors());
        }
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn tcp_keeps_its_connection_and_reconnects_after_the_peer_closes() {
        use std::net::TcpListener;
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let net = NetSender::new().unwrap();
        let read = |s: &mut TcpStream, n: usize| {
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = vec![0u8; n];
            s.read_exact(&mut buf).unwrap();
            buf
        };
        net.send("127.0.0.1", port, NetTransport::Tcp, b"one\r\n".to_vec()).unwrap();
        net.send("127.0.0.1", port, NetTransport::Tcp, b"two\r\n".to_vec()).unwrap();
        let (mut c, _) = server.accept().unwrap();
        assert_eq!(read(&mut c, 10), b"one\r\ntwo\r\n", "both on one connection");
        c.write_all(b"reply ignored").unwrap();
        drop(c);
        std::thread::sleep(Duration::from_millis(50));
        net.send("127.0.0.1", port, NetTransport::Tcp, b"three".to_vec()).unwrap();
        let (mut c, _) = server.accept().unwrap();
        assert_eq!(read(&mut c, 5), b"three", "a closed connection is opened again");
        assert!(net.drain_errors().is_empty());
    }

    #[test]
    fn a_refused_tcp_connection_is_reported() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let net = NetSender::new().unwrap();
        net.send("127.0.0.1", port, NetTransport::Tcp, b"x".to_vec()).unwrap();
        let t = Instant::now();
        let mut errors = Vec::new();
        while errors.is_empty() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            errors = net.drain_errors();
        }
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn a_hung_lookup_does_not_hang_drop() {
        let resolve: Resolve = Arc::new(|_, _| {
            std::thread::sleep(Duration::from_secs(30));
            Err("timed out".into())
        });
        let net = NetSender::with_resolver(resolve).unwrap();
        net.send("hung.local", 9, NetTransport::Udp, b"x".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let t = Instant::now();
        drop(net);
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
