//! Art-Net 4: ArtDmx output (optionally followed by ArtSync), ArtPoll node discovery, and the
//! ArtPollReply an OpenQ input sends so desks that only unicast to polled nodes find it.

use super::dmx::{DmxOutput, Universe, SLOTS};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Art-Net UDP port.
pub const PORT: u16 = 6454;
const ID: &[u8; 8] = b"Art-Net\0";
const OP_POLL: u16 = 0x2000;
const OP_POLL_REPLY: u16 = 0x2100;
const OP_DMX: u16 = 0x5000;
const OP_SYNC: u16 = 0x5200;
const PROTOCOL: u16 = 14;
/// Short name OpenQ answers ArtPoll with (its own replies are left out of discovery).
pub const SHORT_NAME: &str = "OpenQ";

/// An ArtDmx packet for a 15-bit port address.
pub fn dmx_packet(sequence: u8, universe: &Universe, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(ID);
    out.extend_from_slice(&OP_DMX.to_le_bytes());
    out.extend_from_slice(&PROTOCOL.to_be_bytes());
    out.push(sequence);
    out.push(0); // physical
    out.push((universe.id & 0xff) as u8); // SubUni
    out.push(((universe.id >> 8) & 0x7f) as u8); // Net
    out.extend_from_slice(&(SLOTS as u16).to_be_bytes());
    out.extend_from_slice(&universe.data);
}

/// An ArtSync packet: nodes in sync mode output every universe received since the last one at
/// once (no tearing across universes).
pub fn sync_packet() -> [u8; 14] {
    let mut p = [0u8; 14];
    p[..8].copy_from_slice(ID);
    p[8..10].copy_from_slice(&OP_SYNC.to_le_bytes());
    p[10..12].copy_from_slice(&PROTOCOL.to_be_bytes());
    p
}

/// True for an ArtPoll packet.
pub fn is_poll(p: &[u8]) -> bool {
    p.len() >= 12 && &p[..8] == ID && u16::from_le_bytes([p[8], p[9]]) == OP_POLL
}

/// The ArtPollReply packets describing a device at `ip` that receives the 15-bit port addresses
/// `universes` (an OpenQ input: "output ports" in Art-Net terms, DMX taken from the network).
/// A reply holds up to four ports sharing one net/sub-net, so several replies are made (bind
/// index 1, 2…); a device with no universes still answers once.
pub fn poll_replies(ip: Ipv4Addr, long_name: &str, universes: &[u16]) -> Vec<Vec<u8>> {
    let mut wires: Vec<u16> = universes.iter().map(|u| u & 0x7fff).collect();
    wires.sort_unstable();
    wires.dedup();
    // Groups of up to four sharing the upper 11 bits (net + sub-net).
    let mut groups: Vec<(u16, Vec<u8>)> = Vec::new();
    for w in wires {
        match groups.last_mut() {
            Some((hi, ports)) if *hi == w >> 4 && ports.len() < 4 => ports.push((w & 0x0f) as u8),
            _ => groups.push((w >> 4, vec![(w & 0x0f) as u8])),
        }
    }
    if groups.is_empty() {
        groups.push((0, Vec::new()));
    }
    let bind = groups.len() > 1;
    groups
        .iter()
        .enumerate()
        .map(|(i, (hi, ports))| {
            let mut p = vec![0u8; 239];
            p[..8].copy_from_slice(ID);
            p[8..10].copy_from_slice(&OP_POLL_REPLY.to_le_bytes());
            p[10..14].copy_from_slice(&ip.octets());
            p[14..16].copy_from_slice(&PORT.to_le_bytes());
            p[16..18].copy_from_slice(&[0, 1]); // firmware version
            p[18] = ((hi >> 4) & 0x7f) as u8; // net
            p[19] = (hi & 0x0f) as u8; // sub-net
            p[23] = 0b1110_0000; // status 1: indicators normal, port addresses set over the network
            let short = SHORT_NAME.as_bytes();
            p[26..26 + short.len()].copy_from_slice(short);
            let long = long_name.as_bytes();
            let n = long.len().min(63);
            p[44..44 + n].copy_from_slice(&long[..n]);
            let report = b"#0001 [0000] OpenQ input";
            p[108..108 + report.len()].copy_from_slice(report);
            p[173] = ports.len() as u8;
            for (k, port) in ports.iter().enumerate() {
                p[174 + k] = 0x80; // can output DMX taken from the network
                p[182 + k] = 0x80; // data being output
                p[190 + k] = *port; // SwOut
            }
            p[200] = 0; // style: node
            p[207..211].copy_from_slice(&ip.octets()); // bind IP: the root device
            p[211] = if bind { i as u8 + 1 } else { 0 }; // bind index
            p[212] = 0b0000_1000; // status 2: 15-bit port addresses
            p
        })
        .collect()
}

/// This machine's address on the route toward `peer` (no packet is sent).
pub fn local_ip_toward(peer: Ipv4Addr) -> Option<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((peer, PORT)).ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        _ => None,
    }
}

/// Parse an ArtDmx packet: (port address, data).
pub fn parse_dmx(p: &[u8]) -> Option<(u16, &[u8])> {
    if p.len() < 18 || &p[..8] != ID || u16::from_le_bytes([p[8], p[9]]) != OP_DMX {
        return None;
    }
    let universe = p[14] as u16 | ((p[15] as u16 & 0x7f) << 8);
    let len = u16::from_be_bytes([p[16], p[17]]) as usize;
    p.get(18..18 + len).map(|d| (universe, d))
}

/// Local ports OpenQ's Art-Net outputs send from: inputs ignore packets from them (a broadcast
/// output would otherwise come back in and latch through an HTP merge).
static OWN_PORTS: parking_lot::Mutex<Vec<u16>> = parking_lot::Mutex::new(Vec::new());

/// True if `port` is the source port of one of this process's Art-Net outputs.
pub fn is_own_port(port: u16) -> bool {
    OWN_PORTS.lock().contains(&port)
}

/// Sends ArtDmx to one node (unicast) or a broadcast address.
pub struct ArtNetOutput {
    socket: UdpSocket,
    port: u16,
    target: SocketAddr,
    sequence: u8,
    buf: Vec<u8>,
    /// Send ArtSync after each batch of universes.
    sync: bool,
}

impl ArtNetOutput {
    /// `target`: IP, `IP:port`, or empty for 255.255.255.255. With `sync`, every batch of
    /// universes is followed by an ArtSync to the same destination.
    pub fn open(target: &str, sync: bool) -> anyhow::Result<Self> {
        let target = parse_target(target, Ipv4Addr::BROADCAST, PORT)?;
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_broadcast(true)?;
        let port = socket.local_addr()?.port();
        OWN_PORTS.lock().push(port);
        Ok(Self { socket, port, target, sequence: 0, buf: Vec::with_capacity(18 + SLOTS), sync })
    }
}

impl Drop for ArtNetOutput {
    fn drop(&mut self) {
        let mut own = OWN_PORTS.lock();
        if let Some(i) = own.iter().position(|p| *p == self.port) {
            own.swap_remove(i);
        }
    }
}

impl DmxOutput for ArtNetOutput {
    fn send(&mut self, universe: &Universe) -> anyhow::Result<()> {
        // 0 means "sequencing off"; run 1..=255.
        self.sequence = self.sequence % 255 + 1;
        dmx_packet(self.sequence, universe, &mut self.buf);
        self.socket.send_to(&self.buf, self.target)?;
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        if self.sync {
            self.socket.send_to(&sync_packet(), self.target)?;
        }
        Ok(())
    }
}

/// `host`, `host:port` or empty (default address).
pub(crate) fn parse_target(s: &str, default: Ipv4Addr, port: u16) -> anyhow::Result<SocketAddr> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(SocketAddr::from((default, port)));
    }
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Ok(a);
    }
    if let Ok(ip) = s.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    use std::net::ToSocketAddrs;
    (s, port).to_socket_addrs()?.next().ok_or_else(|| anyhow::anyhow!("unknown address {s}"))
}

/// An Art-Net node that answered an ArtPoll.
#[derive(Clone, PartialEq, Debug)]
pub struct Node {
    /// Node IP.
    pub ip: Ipv4Addr,
    /// Short name.
    pub short_name: String,
    /// Long name.
    pub long_name: String,
    /// Port addresses of its outputs (15-bit).
    pub outputs: Vec<u16>,
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// Parse an ArtPollReply.
pub fn parse_poll_reply(p: &[u8]) -> Option<Node> {
    if p.len() < 194 || &p[..8] != ID || u16::from_le_bytes([p[8], p[9]]) != OP_POLL_REPLY {
        return None;
    }
    let ip = Ipv4Addr::new(p[10], p[11], p[12], p[13]);
    let (net, sub) = (p[18] as u16 & 0x7f, p[19] as u16 & 0x0f);
    let ports = (u16::from_be_bytes([p[172], p[173]]) as usize).min(4);
    let outputs = (0..ports)
        .filter(|&i| p[174 + i] & 0x80 != 0) // port can output DMX
        .map(|i| (net << 8) | (sub << 4) | (p[190 + i] as u16 & 0x0f))
        .collect();
    Some(Node { ip, short_name: cstr(&p[26..44]), long_name: cstr(&p[44..108]), outputs })
}

/// Broadcast an ArtPoll and collect replies for `wait`. Blocks: call it off the UI thread.
pub fn discover(wait: Duration) -> anyhow::Result<Vec<Node>> {
    // Replies go to port 6454; if another program holds it, fall back to any port (some nodes
    // answer the sender's port).
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PORT)).or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))?;
    socket.set_broadcast(true)?;
    let mut poll = Vec::with_capacity(14);
    poll.extend_from_slice(ID);
    poll.extend_from_slice(&OP_POLL.to_le_bytes());
    poll.extend_from_slice(&PROTOCOL.to_be_bytes());
    poll.extend_from_slice(&[0, 0]); // flags, diag priority
    socket.send_to(&poll, (Ipv4Addr::BROADCAST, PORT))?;
    let end = Instant::now() + wait;
    let mut nodes: Vec<Node> = Vec::new();
    let mut buf = [0u8; 1024];
    while let Some(left) = end.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        socket.set_read_timeout(Some(left))?;
        let Ok((n, _)) = socket.recv_from(&mut buf) else { break };
        let Some(node) = parse_poll_reply(&buf[..n]) else { continue };
        // OpenQ's own inputs answer too.
        if node.short_name == SHORT_NAME && local_ip_toward(node.ip) == Some(node.ip) {
            continue;
        }
        // A node with more than four ports answers once per bind index.
        match nodes.iter_mut().find(|x| x.ip == node.ip) {
            Some(x) => {
                for o in node.outputs {
                    if !x.outputs.contains(&o) {
                        x.outputs.push(o);
                    }
                }
                x.outputs.sort_unstable();
            }
            None => nodes.push(node),
        }
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmx_loopback() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut out = ArtNetOutput::open(&rx.local_addr().unwrap().to_string(), true).unwrap();
        let mut u = Universe::new(0x123);
        u.data[0] = 255;
        u.data[511] = 9;
        out.send(&u).unwrap();
        let mut buf = [0u8; 600];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(n, 18 + 512);
        assert_eq!(buf[12], 1, "sequence starts at 1");
        let (uni, data) = parse_dmx(&buf[..n]).unwrap();
        assert_eq!(uni, 0x123);
        assert_eq!((data[0], data[511]), (255, 9));
        out.flush().unwrap();
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], &sync_packet(), "ArtSync follows the batch");
    }

    /// OpenQ's replies parse back as a node outputting the input's universes, split by sub-net
    /// and four ports per reply.
    #[test]
    fn own_poll_replies() {
        let ip = Ipv4Addr::new(10, 0, 0, 9);
        let r = poll_replies(ip, "OpenQ show", &[0, 1, 2, 3, 4, 0x123, 1]);
        assert_eq!(r.len(), 3, "0..=3, 4, 0x123");
        let nodes: Vec<Node> = r.iter().map(|p| parse_poll_reply(p).unwrap()).collect();
        assert_eq!(nodes[0].outputs, [0, 1, 2, 3]);
        assert_eq!(nodes[1].outputs, [4]);
        assert_eq!(nodes[2].outputs, [0x123]);
        assert!(nodes.iter().all(|n| n.ip == ip && n.short_name == SHORT_NAME && n.long_name == "OpenQ show"));
        assert_eq!(r.iter().map(|p| p[211]).collect::<Vec<_>>(), [1, 2, 3]);
        let one = poll_replies(ip, "x", &[]);
        assert_eq!((one.len(), one[0][173], one[0][211]), (1, 0, 0));
        assert!(is_poll(&{
            let mut p = sync_packet().to_vec();
            p[8..10].copy_from_slice(&OP_POLL.to_le_bytes());
            p
        }));
        assert!(!is_poll(&sync_packet()));
    }

    #[test]
    fn poll_reply() {
        let mut p = vec![0u8; 239];
        p[..8].copy_from_slice(ID);
        p[8..10].copy_from_slice(&OP_POLL_REPLY.to_le_bytes());
        p[10..14].copy_from_slice(&[10, 0, 0, 7]);
        p[18] = 1; // net
        p[19] = 2; // sub
        p[26..30].copy_from_slice(b"Node");
        p[173] = 2; // two ports
        p[174] = 0x80;
        p[175] = 0x80;
        p[190] = 3;
        p[191] = 4;
        let n = parse_poll_reply(&p).unwrap();
        assert_eq!(n.ip, Ipv4Addr::new(10, 0, 0, 7));
        assert_eq!(n.short_name, "Node");
        assert_eq!(n.outputs, [0x123, 0x124]);
    }
}
