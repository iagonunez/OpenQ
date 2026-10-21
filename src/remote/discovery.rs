//! Finding OpenQ instances on the local network: a replica broadcasts a query on UDP and
//! every main device with remote access on answers with its id, name and port. Nothing secret
//! is exchanged; pairing and authentication happen over TCP afterwards.
//!
//! The query goes to 255.255.255.255 (which most systems send out of one interface only), to
//! each interface's own broadcast address (a computer on a show network and on the venue's) and
//! to loopback. Other subnets and VLANs aren't reached: type the address there.

use super::{DISCOVERY_PORT, PROTOCOL};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

const QUERY: &[u8] = b"OPENQ-DISCOVER/1";
/// Answers sent per second at most: the answer is bigger than the query, so without a cap
/// queries with a forged sender address would turn this into a traffic amplifier.
const ANSWERS_PER_SECOND: u32 = 20;

/// At most `per_second` events in each one-second window.
pub(crate) struct Budget {
    per_second: u32,
    pub(crate) window: Instant,
    used: u32,
}

impl Budget {
    /// A budget of `per_second`.
    pub(crate) fn new(per_second: u32) -> Self {
        Self { per_second, window: Instant::now(), used: 0 }
    }

    /// Use one; false when this second's are used up.
    pub(crate) fn take(&mut self) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.used = 0;
        }
        self.used += 1;
        self.used <= self.per_second
    }

    /// Use one, first waiting for the next window if this one's are used up (back-pressure: the
    /// sender waits instead of losing what it sent).
    pub(crate) fn take_or_wait(&mut self) {
        if !self.take() {
            std::thread::sleep(Duration::from_secs(1).saturating_sub(self.window.elapsed()));
            self.take();
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Announce {
    openq: u32,
    id: Uuid,
    name: String,
    port: u16,
}

/// An instance that answered.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// Its instance id.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// Where to connect.
    pub address: SocketAddr,
    /// Its protocol version.
    pub protocol: u32,
}

/// How often the discovery responder checks whether it should stop (short: turning sharing off
/// waits for it).
pub const STOP_CHECK: Duration = Duration::from_millis(50);

/// Answer discovery queries until `stop` is set. Returns at once if the port is taken. The
/// thread checks `stop` every [`STOP_CHECK`]; join it to be sure the port is free again.
pub fn respond(
    id: Uuid,
    name: impl Fn() -> String + Send + 'static,
    port: u16,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT))?;
    sock.set_read_timeout(Some(STOP_CHECK))?;
    std::thread::Builder::new().name("openq-discovery".into()).spawn(move || {
        let mut buf = [0u8; 64];
        let mut budget = Budget::new(ANSWERS_PER_SECOND);
        while !stop.load(Ordering::Relaxed) {
            if let Ok((n, from)) = sock.recv_from(&mut buf)
                && &buf[..n] == QUERY
                && budget.take()
            {
                let a = Announce { openq: PROTOCOL, id, name: name(), port };
                if let Ok(bytes) = serde_json::to_vec(&a) {
                    let _ = sock.send_to(&bytes, from);
                }
            }
        }
    })
}

/// Ask the network who is there, collecting answers for `wait`.
pub fn search(wait: Duration) -> std::io::Result<Vec<Found>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_broadcast(true)?;
    for dest in destinations() {
        let _ = sock.send_to(QUERY, (dest, DISCOVERY_PORT));
    }
    let deadline = Instant::now() + wait;
    let mut found: Vec<Found> = Vec::new();
    let mut buf = [0u8; 1024];
    while let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        sock.set_read_timeout(Some(left))?;
        let Ok((n, from)) = sock.recv_from(&mut buf) else { break };
        let Ok(a) = serde_json::from_slice::<Announce>(&buf[..n]) else { continue };
        let address = SocketAddr::new(from.ip(), a.port);
        match found.iter_mut().find(|f| f.id == a.id) {
            // Prefer a LAN address over loopback for an instance seen both ways.
            Some(f) if f.address.ip().is_loopback() && !from.ip().is_loopback() => f.address = address,
            Some(_) => {}
            None => found.push(Found { id: a.id, name: a.name, address, protocol: a.openq }),
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Where discovery queries go: the limited broadcast, each IPv4 interface's broadcast address
/// and loopback.
fn destinations() -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(a) = &iface.addr
            && !a.ip.is_loopback()
            && !iface.is_p2p()
        {
            let b = a.broadcast.unwrap_or_else(|| directed_broadcast(a.ip, a.netmask));
            if !out.contains(&b) {
                out.push(b);
            }
        }
    }
    out.push(Ipv4Addr::LOCALHOST);
    out
}

/// The broadcast address of `ip`'s subnet.
fn directed_broadcast(ip: Ipv4Addr, netmask: Ipv4Addr) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(ip) | !u32::from(netmask))
}

/// This machine's LAN addresses as far as the OS routing table tells (no packets are sent).
pub fn local_addresses() -> Vec<std::net::IpAddr> {
    let mut out = Vec::new();
    for probe in ["192.168.0.1:9", "10.0.0.1:9", "172.16.0.1:9", "8.8.8.8:9"] {
        if let Ok(s) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            && s.connect(probe).is_ok()
            && let Ok(a) = s.local_addr()
            && !a.ip().is_unspecified()
            && !out.contains(&a.ip())
        {
            out.push(a.ip());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_addresses() {
        let b = directed_broadcast(Ipv4Addr::new(10, 1, 2, 3), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(b, Ipv4Addr::new(10, 1, 255, 255));
        let d = destinations();
        assert_eq!(d.first(), Some(&Ipv4Addr::BROADCAST));
        assert_eq!(d.last(), Some(&Ipv4Addr::LOCALHOST));
    }
}
