//! Network cue messages, sent off the engine thread.
//!
//! Looking up a host name can take seconds: an mDNS name (`desk.local`) whose device is switched
//! off, a name with no DNS server answering. The engine thread runs every cue's timing and holds
//! the show lock, so it only queues the message here. A [`NetSender`] worker sends to IP
//! addresses and names it already knows at once; a name it doesn't know is looked up on a thread
//! of its own while that host's messages wait in order, so one slow name never holds up messages
//! to other hosts.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr, UdpSocket};
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

/// Looks up a host name; replaced in tests.
pub type Resolve = Arc<dyn Fn(&str, u16) -> Result<SocketAddr, String> + Send + Sync>;

/// The system's lookup (blocking).
fn system_resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    use std::net::ToSocketAddrs;
    (host, port).to_socket_addrs().map_err(|e| format!("{host}: {e}"))?.next().ok_or_else(|| format!("cannot resolve {host}"))
}

type Key = (String, u16);

enum Msg {
    Send(Key, Vec<u8>),
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
            .name("openq-net-send".into())
            .spawn(move || Worker { socket, resolve, errors: e, done: done_tx, cache: HashMap::new(), waiting: HashMap::new() }.run(rx))?;
        Ok(Self { tx, errors, thread: Some(thread) })
    }

    /// Queue `bytes` for `host:port`. Never blocks; an error here means the message was not
    /// queued. Errors found later (lookup, send) come from [`NetSender::drain_errors`].
    pub fn send(&self, host: &str, port: u16, bytes: Vec<u8>) -> Result<(), String> {
        match self.tx.try_send(Msg::Send((host.to_string(), port), bytes)) {
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
    resolve: Resolve,
    errors: Arc<Mutex<Vec<String>>>,
    /// For lookup threads' answers (a blocking send only waits on the lookup's own thread).
    done: SyncSender<Msg>,
    /// Lookup answers and when they were made.
    cache: HashMap<Key, (Result<SocketAddr, String>, Instant)>,
    /// Messages for hosts being looked up, in order.
    waiting: HashMap<Key, VecDeque<Vec<u8>>>,
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
        let mut errors = self.errors.lock();
        if errors.len() < ERRORS {
            errors.push(e);
        }
    }

    fn send_to(&self, (host, port): &Key, addr: SocketAddr, bytes: &[u8]) {
        if let Err(e) = self.socket.send_to(bytes, addr) {
            self.error(format!("Network cue to {host}:{port}: {e}"));
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Send(key, bytes) => self.send(key, bytes),
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
        let spawned = std::thread::Builder::new().name(crate::recovery::thread_name("openq-dns-", &key.0)).spawn(move || {
            let r = crate::recovery::catch(|| resolve(&key.0, key.1).map_err(anyhow::Error::msg)).map_err(|e| format!("{e:#}"));
            let _ = done.send(Msg::Resolved(key, r));
        });
        if let Err(e) = spawned {
            self.error(format!("Network cue: cannot look up a host name: {e}"));
        }
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
        net.send("slow.local", port, b"slow 1".to_vec()).unwrap();
        net.send("slow.local", port, b"slow 2".to_vec()).unwrap();
        net.send("127.0.0.1", port, b"ip".to_vec()).unwrap();
        assert!(t.elapsed() < Duration::from_millis(50), "queueing never waits for a lookup");
        assert_eq!(recv(&rx), b"ip", "an IP address goes out while the name is looked up");
        assert_eq!(recv(&rx), b"slow 1");
        assert_eq!(recv(&rx), b"slow 2", "a host's messages keep their order");
        // Known now: no second lookup.
        let t = Instant::now();
        net.send("slow.local", port, b"slow 3".to_vec()).unwrap();
        assert_eq!(recv(&rx), b"slow 3");
        assert!(t.elapsed() < Duration::from_millis(200), "cached");
        assert!(net.drain_errors().is_empty());
    }

    #[test]
    fn a_failed_lookup_is_reported_once_per_message() {
        let resolve: Resolve = Arc::new(|host, _| Err(format!("cannot resolve {host}")));
        let net = NetSender::with_resolver(resolve).unwrap();
        net.send("gone.local", 9, b"a".to_vec()).unwrap();
        let t = Instant::now();
        let mut errors = Vec::new();
        while errors.is_empty() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            errors = net.drain_errors();
        }
        assert_eq!(errors, ["Network cue to gone.local:9: cannot resolve gone.local"]);
        // Remembered: the next message fails without a new lookup.
        net.send("gone.local", 9, b"b".to_vec()).unwrap();
        let t = Instant::now();
        while errors.len() < 2 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            errors.extend(net.drain_errors());
        }
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn a_hung_lookup_does_not_hang_drop() {
        let resolve: Resolve = Arc::new(|_, _| {
            std::thread::sleep(Duration::from_secs(30));
            Err("timed out".into())
        });
        let net = NetSender::with_resolver(resolve).unwrap();
        net.send("hung.local", 9, b"x".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let t = Instant::now();
        drop(net);
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
