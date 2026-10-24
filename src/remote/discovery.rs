//! Finding OpnQ instances on the local network: a replica broadcasts a query on UDP and
//! every main device with remote access on answers with its id, name and port. Nothing secret
//! is exchanged; pairing and authentication happen over TCP afterwards.
//!
//! The query goes to 255.255.255.255 (which most systems send out of one interface only), to
//! each interface's own broadcast address (a computer on a show network and on the venue's) and
//! to loopback. Main devices also announce themselves over mDNS (DNS-SD service `_opnq._tcp`,
//! [`Announcer`]) and a search browses for them: broadcasts never cross a router, but networks
//! split into VLANs or subnets often pass mDNS on through a reflector (an "mDNS gateway" in the
//! switches or routers). Without one, type the address.

use super::{DISCOVERY_PORT, PROTOCOL};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

const QUERY: &[u8] = b"OPNQ-DISCOVER/1";
/// mDNS (DNS-SD) service type main devices announce.
pub const SERVICE: &str = "_opnq._tcp.local.";
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
    opnq: u32,
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
    std::thread::Builder::new().name("opnq-discovery".into()).spawn(move || {
        let mut buf = [0u8; 64];
        let mut budget = Budget::new(ANSWERS_PER_SECOND);
        while !stop.load(Ordering::Relaxed) {
            if let Ok((n, from)) = sock.recv_from(&mut buf)
                && &buf[..n] == QUERY
                && budget.take()
            {
                let a = Announce { opnq: PROTOCOL, id, name: name(), port };
                if let Ok(bytes) = serde_json::to_vec(&a) {
                    let _ = sock.send_to(&bytes, from);
                }
            }
        }
    })
}

/// A main device announced over mDNS while this lives ([`SERVICE`]; instance name = its id, TXT
/// `id`, `name`, `opnq` = protocol). Dropping it says goodbye and stops the mDNS thread.
pub struct Announcer {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
}

impl Announcer {
    /// Announce instance `id` called `name`, taking connections on `port`, on every interface
    /// (addresses follow the interfaces as they come and go).
    pub fn start(id: Uuid, name: &str, port: u16) -> Result<Self, mdns_sd::Error> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let instance = id.simple().to_string();
        // A TXT entry holds 255 bytes, key included.
        let name = clean_name(name);
        let props = [("id", instance.as_str()), ("name", name.as_str()), ("opnq", &PROTOCOL.to_string())];
        let host = format!("opnq-{}.local.", &instance[..12]);
        let info = mdns_sd::ServiceInfo::new(SERVICE, &instance, &host, "", port, &props[..])?.enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        if let Err(e) = daemon.register(info) {
            let _ = daemon.shutdown();
            return Err(e);
        }
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        if let Ok(done) = self.daemon.unregister(&self.fullname) {
            // The goodbye goes out at once; don't hold up turning sharing off for long.
            let _ = done.recv_timeout(Duration::from_millis(200));
        }
        let _ = self.daemon.shutdown();
    }
}

/// An instance name as shown in lists: no control characters, at most 64 characters.
fn clean_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Ask the network who is there, collecting answers for `wait`: broadcast answers and mDNS
/// announcements.
pub fn search(wait: Duration) -> std::io::Result<Vec<Found>> {
    // mDNS browses on its own thread meanwhile (unavailable: broadcast only).
    let mdns = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => match d.browse(SERVICE) {
            Ok(rx) => Some((d, rx)),
            Err(e) => {
                log::debug!("mDNS browse: {e}");
                let _ = d.shutdown();
                None
            }
        },
        Err(e) => {
            log::debug!("mDNS unavailable: {e}");
            None
        }
    };
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
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            // E.g. an ICMP "port unreachable" from one destination: wait on for the others.
            Err(_) => continue,
        };
        let Ok(a) = serde_json::from_slice::<Announce>(&buf[..n]) else { continue };
        add(&mut found, Found { id: a.id, name: clean_name(&a.name), address: SocketAddr::new(from.ip(), a.port), protocol: a.opnq });
    }
    if let Some((daemon, rx)) = mdns {
        while let Ok(event) = rx.try_recv() {
            if let mdns_sd::ServiceEvent::ServiceResolved(s) = event
                && let Some(f) = from_mdns(&s)
            {
                add(&mut found, f);
            }
        }
        let _ = daemon.shutdown();
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Add an answer, once per instance; a LAN address wins over loopback for one seen both ways.
fn add(found: &mut Vec<Found>, f: Found) {
    match found.iter_mut().find(|x| x.id == f.id) {
        Some(x) if x.address.ip().is_loopback() && !f.address.ip().is_loopback() => x.address = f.address,
        Some(_) => {}
        None => found.push(f),
    }
}

/// An instance from its mDNS announcement: IPv4 preferred, IPv6 only if global (a link-local
/// address needs its interface, which a typed `host:port` can't carry).
fn from_mdns(s: &mdns_sd::ResolvedService) -> Option<Found> {
    let txt = &s.txt_properties;
    let id = Uuid::parse_str(txt.get_property_val_str("id")?).ok()?;
    let protocol = txt.get_property_val_str("opnq")?.parse().ok()?;
    let name = clean_name(txt.get_property_val_str("name").unwrap_or_default());
    let ips: Vec<std::net::IpAddr> = s.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
    let rank = |ip: &std::net::IpAddr| match ip {
        std::net::IpAddr::V4(v) if !v.is_loopback() && !v.is_link_local() => 0,
        std::net::IpAddr::V4(_) => 1,
        std::net::IpAddr::V6(v) if !v.is_loopback() && !v.is_unicast_link_local() => 2,
        std::net::IpAddr::V6(_) => 3,
    };
    let ip = ips.into_iter().filter(|ip| rank(ip) != 3).min_by_key(rank)?;
    Some(Found { id, name, address: SocketAddr::new(ip, s.port), protocol })
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

    /// A main device's mDNS announcement is found by a search (on this computer: multicast
    /// loops back).
    #[test]
    fn mdns_announcement_is_found() {
        let id = Uuid::new_v4();
        let announcer = match Announcer::start(id, "Main\u{7}device", 53299) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("no mDNS here: {e}");
                return;
            }
        };
        let mut seen = None;
        for _ in 0..3 {
            seen = search(Duration::from_millis(1500)).unwrap().into_iter().find(|f| f.id == id);
            if seen.is_some() {
                break;
            }
        }
        let f = seen.expect("announced instance found");
        assert_eq!((f.name.as_str(), f.address.port(), f.protocol), ("Maindevice", 53299, PROTOCOL));
        drop(announcer);
    }

    #[test]
    fn broadcast_addresses() {
        let b = directed_broadcast(Ipv4Addr::new(10, 1, 2, 3), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(b, Ipv4Addr::new(10, 1, 255, 255));
        let d = destinations();
        assert_eq!(d.first(), Some(&Ipv4Addr::BROADCAST));
        assert_eq!(d.last(), Some(&Ipv4Addr::LOCALHOST));
    }
}
