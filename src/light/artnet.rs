//! Art-Net 4: ArtDmx output and ArtPoll node discovery.

use super::dmx::{DmxOutput, Universe, SLOTS};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Art-Net UDP port.
pub const PORT: u16 = 6454;
const ID: &[u8; 8] = b"Art-Net\0";
const OP_POLL: u16 = 0x2000;
const OP_POLL_REPLY: u16 = 0x2100;
const OP_DMX: u16 = 0x5000;
const PROTOCOL: u16 = 14;

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

/// Parse an ArtDmx packet: (port address, data).
pub fn parse_dmx(p: &[u8]) -> Option<(u16, &[u8])> {
    if p.len() < 18 || &p[..8] != ID || u16::from_le_bytes([p[8], p[9]]) != OP_DMX {
        return None;
    }
    let universe = p[14] as u16 | ((p[15] as u16 & 0x7f) << 8);
    let len = u16::from_be_bytes([p[16], p[17]]) as usize;
    p.get(18..18 + len).map(|d| (universe, d))
}

/// Sends ArtDmx to one node (unicast) or a broadcast address.
pub struct ArtNetOutput {
    socket: UdpSocket,
    target: SocketAddr,
    sequence: u8,
    buf: Vec<u8>,
}

impl ArtNetOutput {
    /// `target`: IP, `IP:port`, or empty for 255.255.255.255.
    pub fn open(target: &str) -> anyhow::Result<Self> {
        let target = parse_target(target, Ipv4Addr::BROADCAST, PORT)?;
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_broadcast(true)?;
        Ok(Self { socket, target, sequence: 0, buf: Vec::with_capacity(18 + SLOTS) })
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
        if let Some(node) = parse_poll_reply(&buf[..n])
            && !nodes.iter().any(|x| x.ip == node.ip)
        {
            nodes.push(node);
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
        let mut out = ArtNetOutput::open(&rx.local_addr().unwrap().to_string()).unwrap();
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
