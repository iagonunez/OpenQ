//! DMX input: Art-Net and sACN receivers, merged into the output or captured into the programmer.
//!
//! Each enabled [`InputConfig`] runs a receiver thread that listens for its routed wire
//! universes and keeps every source apart (sACN by CID, Art-Net by sender address). Per universe
//! the live sources with the highest sACN priority are merged highest-takes-precedence. A source
//! that goes quiet is dropped after [`SOURCE_TIMEOUT`] (an sACN source that terminates its stream
//! at once). The result, in logical universe numbers, sits in a slot the light thread reads with
//! `try_lock` every frame and merges into the output as the input's [`MergeMode`] says; the UI
//! reads it to [`capture`] another desk's look into the programmer. OpenQ's own packets
//! (`artnet::is_own_port`, `sacn::own_cid`) are ignored, so a broadcast output can't loop back.

use super::dmx::{DmxFrame, UniverseRoute, RETRY, SLOTS};
use super::model::{Attribute, FixtureId, Lighting};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A source that sends nothing for this long is dropped (sACN's network data loss timeout).
pub const SOURCE_TIMEOUT: Duration = Duration::from_millis(2500);
/// Most sources kept at once (one per sender and universe); more are ignored until some expire.
const MAX_SOURCES: usize = 256;

/// Protocol an input listens to.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum InputProtocol {
    /// Art-Net (UDP 6454, broadcast or unicast).
    #[default]
    ArtNet,
    /// sACN / E1.31 (UDP 5568, multicast per universe or unicast).
    Sacn,
}

impl InputProtocol {
    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            InputProtocol::ArtNet => "Art-Net",
            InputProtocol::Sacn => "sACN",
        }
    }

    /// UDP port the protocol uses.
    pub fn port(self) -> u16 {
        match self {
            InputProtocol::ArtNet => super::artnet::PORT,
            InputProtocol::Sacn => super::sacn::PORT,
        }
    }
}

/// How an input's universes combine with OpenQ's output.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MergeMode {
    /// Every slot takes the higher of OpenQ and the input.
    #[default]
    Htp,
    /// Universes the input receives are output as received (OpenQ's values there are replaced).
    Input,
    /// Not output: only shown and available to capture into the programmer.
    Capture,
}

impl MergeMode {
    /// Every mode, in menu order.
    pub const ALL: [MergeMode; 3] = [MergeMode::Htp, MergeMode::Input, MergeMode::Capture];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            MergeMode::Htp => crate::i18n::t("Merge (highest wins)"),
            MergeMode::Input => crate::i18n::t("Input replaces"),
            MergeMode::Capture => crate::i18n::t("Capture only"),
        }
    }
}

/// A configured DMX input.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct InputConfig {
    /// Name shown in the UI.
    pub name: String,
    /// Off = not listening.
    pub enabled: bool,
    /// Protocol.
    pub protocol: InputProtocol,
    /// IPv4 address of the network interface that joins sACN multicast groups (empty = the
    /// system's default).
    pub interface: String,
    /// Wire universe `wire` is received as logical universe `universe`.
    pub routes: Vec<UniverseRoute>,
    /// How it combines with the output.
    pub merge: MergeMode,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            name: "Art-Net in".into(),
            enabled: true,
            protocol: InputProtocol::ArtNet,
            interface: String::new(),
            routes: vec![UniverseRoute { universe: 1, wire: 0 }],
            merge: MergeMode::Htp,
        }
    }
}

/// Health of an input, for the UI.
#[derive(Clone, Default, Debug)]
pub struct InputStatus {
    /// Socket error (cleared when it opens).
    pub error: Option<String>,
    /// Socket open.
    pub open: bool,
    /// Packets taken (routed universes from other sources).
    pub packets: u64,
    /// Live sources: sender address, or sACN source name.
    pub sources: Vec<String>,
}

struct Shared {
    /// Merged universes by logical number (only those with a live source).
    frame: Mutex<DmxFrame>,
    status: Mutex<InputStatus>,
    quit: AtomicBool,
}

/// Reads an input from other threads (the light thread merges it, the UI shows and captures it).
#[derive(Clone)]
pub struct InputHandle {
    /// Input name.
    pub name: String,
    /// How it combines with the output.
    pub merge: MergeMode,
    shared: Arc<Shared>,
}

impl InputHandle {
    /// Current status.
    pub fn status(&self) -> InputStatus {
        self.shared.status.lock().clone()
    }

    /// Copy of the received universes.
    pub fn frame(&self) -> DmxFrame {
        self.shared.frame.lock().clone()
    }

    /// Copy the received universes into `out` unless the receiver is writing them right now
    /// (then `out` keeps the previous copy). Doesn't allocate once `out` has room.
    pub fn read_into(&self, out: &mut DmxFrame) {
        if let Some(f) = self.shared.frame.try_lock() {
            out.universes.clone_from(&f.universes);
        }
    }
}

/// Merge `input` into `out` as `mode` says.
pub fn merge(out: &mut DmxFrame, input: &DmxFrame, mode: MergeMode) {
    for u in &input.universes {
        match mode {
            MergeMode::Htp => {
                let o = out.get_mut(u.id);
                for (a, b) in o.data.iter_mut().zip(&u.data) {
                    *a = (*a).max(*b);
                }
            }
            MergeMode::Input => out.get_mut(u.id).data = u.data,
            MergeMode::Capture => {}
        }
    }
}

/// A running input: a thread that receives one protocol.
pub struct InputReceiver {
    config: InputConfig,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl InputReceiver {
    /// Start listening as `config` says (on the protocol's port).
    pub fn start(config: InputConfig) -> Self {
        let port = config.protocol.port();
        Self::start_on(config, port)
    }

    /// Start listening on another UDP port (tests).
    pub fn start_on(config: InputConfig, port: u16) -> Self {
        let shared = Arc::new(Shared { frame: Mutex::new(DmxFrame::default()), status: Mutex::new(InputStatus::default()), quit: AtomicBool::new(false) });
        let (s, c) = (shared.clone(), config.clone());
        let name = format!("openq-dmx-in-{}", config.protocol.label());
        let thread = std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                let r = crate::recovery::catch(|| {
                    run(&s, &c, port);
                    Ok(())
                });
                if let Err(e) = r {
                    log::error!("DMX input {}: {e:#}", c.name);
                    s.status.lock().error = Some(format!("{e:#}"));
                }
            })
            .ok();
        Self { config, shared, thread }
    }

    /// The config it was started with.
    pub fn config(&self) -> &InputConfig {
        &self.config
    }

    /// A handle that reads it from another thread.
    pub fn handle(&self) -> InputHandle {
        InputHandle { name: self.config.name.clone(), merge: self.config.merge, shared: self.shared.clone() }
    }
}

impl Drop for InputReceiver {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // The socket wakes up every READ_TIMEOUT; don't wait much longer than that.
            let deadline = Instant::now() + READ_TIMEOUT * 3;
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// Bind the protocol's port shared with other programs (SO_REUSEADDR), so a desk and OpenQ can
/// listen on one machine; sACN joins the multicast group of each routed universe.
fn open_socket(config: &InputConfig, port: u16) -> anyhow::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let iface: Ipv4Addr = match config.interface.trim() {
        "" => Ipv4Addr::UNSPECIFIED,
        s => s.parse().map_err(|_| anyhow::anyhow!("{s} isn't an IPv4 address"))?,
    };
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_broadcast(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    if config.protocol == InputProtocol::Sacn {
        let mut wires: Vec<u16> = config.routes.iter().map(|r| r.wire).collect();
        wires.sort_unstable();
        wires.dedup();
        for w in wires {
            // No multicast route (an offline machine) still allows unicast: just report it.
            if let Err(e) = s.join_multicast_v4(&super::sacn::multicast(w), &iface) {
                log::warn!("DMX input {}: can't join the group of universe {w}: {e}", config.name);
            }
        }
    }
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(READ_TIMEOUT))?;
    Ok(s)
}

/// One sender's latest data for one wire universe.
struct Source {
    /// sACN CID, or the sender's address for Art-Net.
    key: [u8; 16],
    name: String,
    wire: u16,
    priority: u8,
    data: [u8; SLOTS],
    seen: Instant,
}

fn addr_key(a: SocketAddr) -> [u8; 16] {
    let mut k = [0u8; 16];
    if let SocketAddr::V4(v4) = a {
        k[..4].copy_from_slice(&v4.ip().octets());
        k[4..6].copy_from_slice(&v4.port().to_be_bytes());
    }
    k
}

fn run(shared: &Shared, config: &InputConfig, port: u16) {
    let mut socket: Option<UdpSocket> = None;
    let mut next_open = Instant::now();
    let mut sources: Vec<Source> = Vec::new();
    // Desks poll every few seconds; a few answers a second is plenty.
    let mut poll_budget = crate::remote::discovery::Budget::new(10);
    let mut buf = vec![0u8; 1500];
    let mut merged = DmxFrame::default();
    let mut last_status = Instant::now();
    let mut packets = 0u64;
    // Wires with new data or sources gone since the last merge: only their universes are merged
    // again (merging every route and source per packet was quadratic in a big rig).
    let mut dirty: Vec<u16> = Vec::new();
    let mut universes: Vec<u16> = Vec::new();
    while !shared.quit.load(Ordering::Relaxed) {
        let Some(sock) = socket.as_ref() else {
            if Instant::now() < next_open {
                std::thread::sleep(READ_TIMEOUT);
                continue;
            }
            match open_socket(config, port) {
                Ok(s) => {
                    socket = Some(s);
                    let mut st = shared.status.lock();
                    st.open = true;
                    st.error = None;
                }
                Err(e) => {
                    next_open = Instant::now() + RETRY;
                    let mut st = shared.status.lock();
                    if st.error.is_none() {
                        log::warn!("DMX input {}: {e:#}", config.name);
                    }
                    st.open = false;
                    st.error = Some(format!("{e:#}"));
                }
            }
            continue;
        };
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if config.protocol == InputProtocol::ArtNet && super::artnet::is_poll(&buf[..n]) {
                    // Replies are bigger than polls: capped, so forged polls can't use OpenQ to
                    // flood someone else.
                    if poll_budget.take() {
                        answer_poll(sock, config, from);
                    }
                } else if let Some(wire) = take(config, &buf[..n], from, &mut sources) {
                    packets += 1;
                    if !dirty.contains(&wire) {
                        dirty.push(wire);
                    }
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => {}
            // Windows reports an ICMP "port unreachable" from an earlier send as a receive error.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(e) => {
                log::warn!("DMX input {} lost: {e}", config.name);
                socket = None;
                next_open = Instant::now() + RETRY;
                let mut st = shared.status.lock();
                st.open = false;
                st.error = Some(e.to_string());
                continue;
            }
        }
        let now = Instant::now();
        sources.retain(|s| {
            let live = now.duration_since(s.seen) < SOURCE_TIMEOUT;
            if !live && !dirty.contains(&s.wire) {
                dirty.push(s.wire);
            }
            live
        });
        if !dirty.is_empty() {
            universes.clear();
            universes.extend(config.routes.iter().filter(|r| dirty.contains(&r.wire)).map(|r| r.universe));
            universes.sort_unstable();
            universes.dedup();
            merge_universes(config, &sources, &universes, &mut merged);
            let mut frame = shared.frame.lock();
            for &u in &universes {
                match merged.get(u) {
                    Some(m) => frame.get_mut(u).data = m.data,
                    None => frame.remove(u),
                }
            }
            dirty.clear();
        }
        if last_status.elapsed() >= Duration::from_millis(250) {
            last_status = now;
            let mut names: Vec<String> = sources.iter().map(|s| s.name.clone()).collect();
            names.sort();
            names.dedup();
            let mut st = shared.status.lock();
            st.packets = packets;
            st.sources = names;
        }
    }
}

/// Answer an ArtPoll with this input's universes, so desks that only unicast to nodes they
/// found send to OpenQ too. Replies go to the poller's Art-Net port, and to the port it polled
/// from when that is another one (controllers that poll from a private port listen there).
fn answer_poll(sock: &UdpSocket, config: &InputConfig, from: SocketAddr) {
    let std::net::IpAddr::V4(peer) = from.ip() else { return };
    let Some(ip) = super::artnet::local_ip_toward(peer) else { return };
    let wires: Vec<u16> = config.routes.iter().map(|r| r.wire).collect();
    let name = if config.name.trim().is_empty() { "OpenQ".to_string() } else { format!("OpenQ · {}", config.name.trim()) };
    let mut to = vec![SocketAddr::from((peer, super::artnet::PORT))];
    if from.port() != super::artnet::PORT {
        to.push(from);
    }
    for p in super::artnet::poll_replies(ip, &name, &wires) {
        for t in &to {
            if let Err(e) = sock.send_to(&p, t) {
                log::debug!("DMX input {}: ArtPollReply to {t}: {e}", config.name);
            }
        }
    }
}

/// Take a packet into `sources`. False if it isn't for this input (other protocol or universe,
/// OpenQ's own, sACN preview data).
/// Take a packet into its source's data; the wire it came on, if it changed anything.
fn take(config: &InputConfig, p: &[u8], from: SocketAddr, sources: &mut Vec<Source>) -> Option<u16> {
    let (key, name, wire, priority, data, terminated) = match config.protocol {
        InputProtocol::ArtNet => {
            if super::artnet::is_own_port(from.port()) && is_local(from) {
                return None;
            }
            let (wire, data) = super::artnet::parse_dmx(p)?;
            (addr_key(from), from.ip().to_string(), wire, 100, data, false)
        }
        InputProtocol::Sacn => {
            let d = super::sacn::parse(p)?;
            if d.cid == super::sacn::own_cid() || d.options & super::sacn::PREVIEW != 0 {
                return None;
            }
            // Source name, as the sender calls itself (falls back to its address).
            let raw = &p[44..108];
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            let mut name = String::from_utf8_lossy(&raw[..end]).trim().to_string();
            if name.is_empty() {
                name = from.ip().to_string();
            }
            (d.cid, name, d.universe, d.priority.min(200), d.data, d.options & super::sacn::TERMINATED != 0)
        }
    };
    if !config.routes.iter().any(|r| r.wire == wire) {
        return None;
    }
    let at = sources.iter().position(|s| s.key == key && s.wire == wire);
    if terminated {
        if let Some(i) = at {
            sources.swap_remove(i);
        }
        return Some(wire);
    }
    let i = match at {
        Some(i) => i,
        None if sources.len() < MAX_SOURCES => {
            sources.push(Source { key, name, wire, priority, data: [0; SLOTS], seen: Instant::now() });
            sources.len() - 1
        }
        None => return None,
    };
    let s = &mut sources[i];
    let n = data.len().min(SLOTS);
    s.data[..n].copy_from_slice(&data[..n]);
    s.data[n..].fill(0);
    s.priority = priority;
    s.seen = Instant::now();
    Some(wire)
}

/// True for loopback and this machine's addresses as far as cheaply known (the unspecified
/// address counts too). A remote desk that happens to send from an OpenQ output's port number is
/// still taken.
fn is_local(a: SocketAddr) -> bool {
    a.ip().is_loopback() || a.ip().is_unspecified() || local_ips().contains(&a.ip())
}

/// This machine's IPv4 addresses, found once by asking the OS which source it routes through
/// (no packet is sent). Enough to spot OpenQ's own broadcasts on the main interface.
fn local_ips() -> &'static [std::net::IpAddr] {
    static IPS: std::sync::OnceLock<Vec<std::net::IpAddr>> = std::sync::OnceLock::new();
    IPS.get_or_init(|| {
        let mut v = Vec::new();
        for probe in ["10.255.255.255:9", "192.168.255.255:9", "172.31.255.255:9", "2.255.255.255:9"] {
            if let Ok(s) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                && s.set_broadcast(true).is_ok()
                && s.connect(probe).is_ok()
                && let Ok(a) = s.local_addr()
                && !v.contains(&a.ip())
            {
                v.push(a.ip());
            }
        }
        v
    })
}

/// Merge the live sources of each route: highest priority wins, equal priorities HTP.
#[cfg(test)]
fn merge_sources(config: &InputConfig, sources: &[Source], out: &mut DmxFrame) {
    out.universes.clear();
    let all: Vec<u16> = config.routes.iter().map(|r| r.universe).collect();
    merge_universes(config, sources, &all, out);
}

/// Merge again universes `universes` (each once) of `out` from the live sources of the routes
/// feeding them (highest priority wins, equal priorities HTP); a universe no live source feeds
/// leaves `out`.
fn merge_universes(config: &InputConfig, sources: &[Source], universes: &[u16], out: &mut DmxFrame) {
    for &id in universes {
        let mut fed = false;
        for r in config.routes.iter().filter(|r| r.universe == id) {
            let live = sources.iter().filter(|s| s.wire == r.wire);
            let Some(top) = live.clone().map(|s| s.priority).max() else { continue };
            let u = out.get_mut(id);
            if !fed {
                u.data = [0; SLOTS];
                fed = true;
            }
            for s in live.filter(|s| s.priority == top) {
                for (a, b) in u.data.iter_mut().zip(&s.data) {
                    *a = (*a).max(*b);
                }
            }
        }
        if !fed {
            out.remove(id);
        }
    }
}

/// Programmer values that reproduce the input's look: every attribute of `units` read back from
/// `frame` (the inverse of the output's DMX encoding, pan/tilt inversion included). With no units
/// given, every patched unit whose channels aren't all at their default. Fixtures without a
/// dimmer channel get their virtual dimmer at full (their colour already holds the intensity).
pub fn capture(l: &Lighting, frame: &DmxFrame, units: &[FixtureId]) -> Vec<(FixtureId, Attribute, f32)> {
    let all = units.is_empty();
    let units = if all { l.units() } else { units.to_vec() };
    let mut out = Vec::new();
    for id in units {
        let Some((f, _)) = l.unit(id) else { continue };
        let Some(chans) = l.unit_channels(id) else { continue };
        let mut values: Vec<(Attribute, f32)> = Vec::new();
        let mut changed = false;
        for c in chans {
            // Each channel from where its DMX break is patched.
            let at = f.address_of(c.dmx_break);
            let (Some(u), Some(start)) = (frame.get(at.universe), (at.address as usize).checked_sub(1)) else { continue };
            let Some(mut v) = c.read(start, &u.data) else { continue };
            changed |= (v - c.default_value()).abs() > 0.5 / 255.0;
            if (c.attribute == Attribute::Pan && f.invert_pan) || (c.attribute == Attribute::Tilt && f.invert_tilt) {
                v = 1.0 - v;
            }
            if !values.iter().any(|(a, _)| *a == c.attribute) {
                values.push((c.attribute, v));
            }
        }
        if all && !changed {
            continue;
        }
        if l.unit_attributes(id).contains(&Attribute::Dimmer) && !values.iter().any(|(a, _)| *a == Attribute::Dimmer) {
            values.push((Attribute::Dimmer, 1.0));
        }
        out.extend(values.into_iter().map(|(a, v)| (id, a, v)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::dmx::Universe;
    use crate::light::model::Fixture;

    fn wait(mut f: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(2) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn free_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[test]
    fn sacn_priority_and_htp() {
        let port = free_port();
        let cfg = InputConfig { protocol: InputProtocol::Sacn, routes: vec![UniverseRoute { universe: 3, wire: 7 }], ..Default::default() };
        let rx = InputReceiver::start_on(cfg, port);
        let h = rx.handle();
        assert!(wait(|| h.status().open));
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut buf = Box::new([0u8; super::super::sacn::PACKET_LEN]);
        let send = |cid: u8, prio: u8, slot0: u8, slot1: u8, buf: &mut [u8; super::super::sacn::PACKET_LEN]| {
            let mut u = Universe::new(7);
            u.data[0] = slot0;
            u.data[1] = slot1;
            super::super::sacn::data_packet(&[cid; 16], "Desk", prio, 1, &u, buf);
            tx.send_to(&buf[..], ("127.0.0.1", port)).unwrap();
        };
        // Two sources at the same priority merge HTP.
        send(1, 100, 200, 10, &mut buf);
        send(2, 100, 50, 90, &mut buf);
        assert!(wait(|| h.frame().get(3).is_some_and(|u| u.data[..2] == [200, 90])), "{:?}", h.frame().get(3).map(|u| u.data[..2].to_vec()));
        // A higher priority source takes over.
        send(3, 150, 5, 6, &mut buf);
        assert!(wait(|| h.frame().get(3).is_some_and(|u| u.data[..2] == [5, 6])));
        // Our own packets and other universes are ignored.
        let mut u = Universe::new(7);
        u.data[0] = 255;
        super::super::sacn::data_packet(&super::super::sacn::own_cid(), "OpenQ", 200, 1, &u, &mut buf);
        tx.send_to(&buf[..], ("127.0.0.1", port)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(h.frame().get(3).unwrap().data[0], 5);
        // Terminating the stream drops the source at once.
        let mut u = Universe::new(7);
        u.data[0] = 5;
        super::super::sacn::data_packet(&[3; 16], "Desk", 150, 2, &u, &mut buf);
        buf[112] = super::super::sacn::TERMINATED;
        tx.send_to(&buf[..], ("127.0.0.1", port)).unwrap();
        assert!(wait(|| h.frame().get(3).is_some_and(|u| u.data[..2] == [200, 90])));
        assert!(wait(|| h.status().sources == ["Desk"]));
    }

    /// An ArtPoll is answered with the input's universes.
    #[test]
    fn artnet_input_answers_poll() {
        let port = free_port();
        let cfg = InputConfig { name: "Desk".into(), routes: vec![UniverseRoute { universe: 1, wire: 0 }, UniverseRoute { universe: 2, wire: 1 }], ..Default::default() };
        let rx = InputReceiver::start_on(cfg, port);
        assert!(wait(|| rx.handle().status().open));
        let poller = UdpSocket::bind("127.0.0.1:0").unwrap();
        poller.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut poll = super::super::artnet::sync_packet().to_vec();
        poll[8..10].copy_from_slice(&0x2000u16.to_le_bytes());
        poller.send_to(&poll, ("127.0.0.1", port)).unwrap();
        let mut buf = [0u8; 600];
        let n = poller.recv(&mut buf).unwrap();
        let node = super::super::artnet::parse_poll_reply(&buf[..n]).unwrap();
        assert_eq!(node.outputs, [0, 1]);
        assert_eq!(node.long_name, "OpenQ · Desk");
        assert_eq!(rx.handle().status().packets, 0, "a poll isn't DMX");
    }

    #[test]
    fn artnet_routes_and_ignores_own_output() {
        let port = free_port();
        let cfg = InputConfig { routes: vec![UniverseRoute { universe: 1, wire: 0 }], ..Default::default() };
        let rx = InputReceiver::start_on(cfg, port);
        let h = rx.handle();
        assert!(wait(|| h.status().open));
        let mut own = super::super::artnet::ArtNetOutput::open(&format!("127.0.0.1:{port}"), false).unwrap();
        let mut u = Universe::new(0);
        u.data[0] = 99;
        use super::super::dmx::DmxOutput;
        own.send(&u).unwrap();
        // Another sender.
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut p = Vec::new();
        u.data[0] = 42;
        super::super::artnet::dmx_packet(1, &u, &mut p);
        tx.send_to(&p, ("127.0.0.1", port)).unwrap();
        let mut other = Universe::new(5);
        other.data[0] = 1;
        super::super::artnet::dmx_packet(1, &other, &mut p);
        tx.send_to(&p, ("127.0.0.1", port)).unwrap();
        assert!(wait(|| h.frame().get(1).is_some_and(|u| u.data[0] == 42)));
        std::thread::sleep(Duration::from_millis(100));
        let f = h.frame();
        assert_eq!(f.universes.len(), 1, "universe 5 isn't routed");
        assert_eq!(f.get(1).unwrap().data[0], 42, "own output ignored");
        assert!(wait(|| h.status().packets == 1));
    }

    /// Mutated and truncated packets never panic and never grow the sources past the limit.
    #[test]
    fn garbage_packets() {
        let mut rng = fastrand::Rng::with_seed(7);
        let routes: Vec<UniverseRoute> = (0..4).map(|w| UniverseRoute { universe: w + 1, wire: w }).collect();
        let mut art = Vec::new();
        super::super::artnet::dmx_packet(1, &Universe::new(1), &mut art);
        let mut acn = [0u8; super::super::sacn::PACKET_LEN];
        super::super::sacn::data_packet(&[9; 16], "x", 100, 1, &Universe::new(1), &mut acn);
        for protocol in [InputProtocol::ArtNet, InputProtocol::Sacn] {
            let cfg = InputConfig { protocol, routes: routes.clone(), ..Default::default() };
            let mut sources = Vec::new();
            let mut out = DmxFrame::default();
            for i in 0..20_000u32 {
                let mut p = if rng.bool() { art.clone() } else { acn.to_vec() };
                for _ in 0..rng.usize(0..8) {
                    let k = rng.usize(0..p.len());
                    p[k] = rng.u8(..);
                }
                p.truncate(rng.usize(0..=p.len()));
                let from = SocketAddr::from(([10, 0, (i % 7) as u8, rng.u8(..)], 6454));
                take(&cfg, &p, from, &mut sources);
                assert!(sources.len() <= MAX_SOURCES);
            }
            merge_sources(&cfg, &sources, &mut out);
        }
    }

    /// Merging only the universes a packet touched gives the same frame as merging everything,
    /// and a universe whose sources are gone leaves the frame.
    #[test]
    fn merging_touched_universes_matches_a_full_merge() {
        let mut rng = fastrand::Rng::with_seed(11);
        // Wires 0..6 into universes 1..4 (two wires share universes 1 and 2).
        let routes: Vec<UniverseRoute> = (0..6).map(|w| UniverseRoute { universe: w % 4 + 1, wire: w }).collect();
        let cfg = InputConfig { protocol: InputProtocol::Sacn, routes, ..Default::default() };
        let mut sources = Vec::new();
        let mut merged = DmxFrame::default();
        for i in 0..2_000u32 {
            let mut u = Universe::new(0);
            u.data[rng.usize(0..SLOTS)] = rng.u8(..);
            let wire = rng.u16(0..7);
            let cid = [rng.u8(0..4); 16];
            let mut p = [0u8; super::super::sacn::PACKET_LEN];
            super::super::sacn::data_packet(&cid, "x", rng.u8(90..110), i as u8, &Universe { id: wire, ..u }, &mut p);
            let from = SocketAddr::from(([10, 0, 0, 1], 5568));
            let touched = take(&cfg, &p, from, &mut sources);
            if i % 50 == 0 && !sources.is_empty() {
                // A source goes quiet.
                let gone = sources.swap_remove(rng.usize(0..sources.len()));
                merge_universes(&cfg, &sources, &cfg.routes.iter().filter(|r| r.wire == gone.wire).map(|r| r.universe).collect::<Vec<_>>(), &mut merged);
            }
            if let Some(w) = touched {
                let us: Vec<u16> = cfg.routes.iter().filter(|r| r.wire == w).map(|r| r.universe).collect();
                merge_universes(&cfg, &sources, &us, &mut merged);
            } else {
                assert_eq!(wire, 6, "only the unrouted wire is ignored");
            }
            let mut full = DmxFrame::default();
            merge_sources(&cfg, &sources, &mut full);
            assert_eq!(merged, full, "packet {i}");
        }
        sources.clear();
        merge_universes(&cfg, &sources, &[1, 2, 3, 4], &mut merged);
        assert!(merged.universes.is_empty());
    }

    #[test]
    fn merge_modes() {
        let mut out = DmxFrame::default();
        out.get_mut(1).data[..2].copy_from_slice(&[100, 10]);
        let mut input = DmxFrame::default();
        input.get_mut(1).data[..2].copy_from_slice(&[50, 60]);
        input.get_mut(2).data[0] = 7;
        let mut htp = out.clone();
        merge(&mut htp, &input, MergeMode::Htp);
        assert_eq!(htp.get(1).unwrap().data[..2], [100, 60]);
        assert_eq!(htp.get(2).unwrap().data[0], 7);
        let mut over = out.clone();
        merge(&mut over, &input, MergeMode::Input);
        assert_eq!(over.get(1).unwrap().data[..2], [50, 60]);
        let mut cap = out.clone();
        merge(&mut cap, &input, MergeMode::Capture);
        assert_eq!(cap, out);
    }

    #[test]
    fn capture_reads_back_the_output() {
        use crate::light::library;
        use crate::light::playback::{LightShow, Playback};
        let mut l = Lighting::default();
        for t in library::generics() {
            l.add_type(&t);
        }
        let mover = l.fixture_types.iter().find(|t| t.modes.iter().any(|m| m.channel(Attribute::Pan).is_some_and(|c| c.fine.is_some()))).unwrap().clone();
        let rgb = l.fixture_types.iter().find(|t| t.id == "generic/rgb").unwrap().clone();
        let mode = mover.modes.iter().find(|m| m.channel(Attribute::Pan).is_some_and(|c| c.fine.is_some())).unwrap().name.clone();
        l.fixtures.push(Fixture { id: 1, fixture_type: mover.id.clone(), mode, address: 1, invert_pan: true, ..Default::default() });
        l.fixtures.push(Fixture { id: 2, fixture_type: rgb.id.clone(), mode: rgb.modes[0].name.clone(), address: 100, ..Default::default() });
        l.fixtures.push(Fixture { id: 3, fixture_type: rgb.id.clone(), mode: rgb.modes[0].name.clone(), address: 110, ..Default::default() });
        let show = Arc::new(LightShow::compile(&l));
        let mut p = Playback::new(show, Instant::now());
        let set = vec![(1, Attribute::Dimmer, 0.8), (1, Attribute::Pan, 0.3), (2, Attribute::Red, 1.0), (2, Attribute::Blue, 0.4), (2, Attribute::Dimmer, 1.0)];
        p.set_programmer(&set);
        let frame = p.tick(Instant::now()).clone();
        let got = capture(&l, &frame, &[]);
        let find = |f, a| got.iter().find(|x| x.0 == f && x.1 == a).map(|x| x.2);
        assert!((find(1, Attribute::Pan).unwrap() - 0.3).abs() < 1e-4, "16-bit and inverted");
        assert!((find(1, Attribute::Dimmer).unwrap() - 0.8).abs() < 1.0 / 255.0);
        assert!((find(2, Attribute::Blue).unwrap() - 0.4).abs() < 1.0 / 255.0);
        assert_eq!(find(2, Attribute::Dimmer), Some(1.0), "virtual dimmer");
        assert!(!got.iter().any(|x| x.0 == 3), "untouched fixture skipped");
        // Asking for a unit takes it even at its defaults.
        assert!(capture(&l, &frame, &[3]).iter().any(|x| x.0 == 3));
    }
}
