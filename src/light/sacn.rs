//! sACN (ANSI E1.31) output: data packets to the universe's multicast group or a unicast IP.

use super::dmx::{DmxOutput, Universe, SLOTS};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

/// sACN UDP port.
pub const PORT: u16 = 5568;
/// Length of a full data packet (512 slots).
pub const PACKET_LEN: usize = 126 + SLOTS;
const ACN_ID: &[u8; 12] = b"ASC-E1.17\0\0\0";

/// Multicast group of a universe (239.255.hi.lo).
pub fn multicast(universe: u16) -> Ipv4Addr {
    Ipv4Addr::new(239, 255, (universe >> 8) as u8, universe as u8)
}

fn flags_len(len: usize) -> [u8; 2] {
    (0x7000 | len as u16).to_be_bytes()
}

/// Build a data packet.
pub fn data_packet(cid: &[u8; 16], name: &str, priority: u8, sequence: u8, universe: &Universe, out: &mut [u8; PACKET_LEN]) {
    out.fill(0);
    out[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
    out[4..16].copy_from_slice(ACN_ID);
    out[16..18].copy_from_slice(&flags_len(PACKET_LEN - 16));
    out[18..22].copy_from_slice(&4u32.to_be_bytes());
    out[22..38].copy_from_slice(cid);
    // Framing layer.
    out[38..40].copy_from_slice(&flags_len(PACKET_LEN - 38));
    out[40..44].copy_from_slice(&2u32.to_be_bytes());
    let n = name.len().min(63);
    out[44..44 + n].copy_from_slice(&name.as_bytes()[..n]);
    out[108] = priority.min(200);
    out[111] = sequence;
    out[113..115].copy_from_slice(&universe.id.to_be_bytes());
    // DMP layer.
    out[115..117].copy_from_slice(&flags_len(PACKET_LEN - 115));
    out[117] = 0x02;
    out[118] = 0xa1;
    out[121..123].copy_from_slice(&1u16.to_be_bytes());
    out[123..125].copy_from_slice(&(SLOTS as u16 + 1).to_be_bytes());
    out[125] = 0; // start code
    out[126..].copy_from_slice(&universe.data);
}

/// Parse a data packet: (universe, priority, data).
pub fn parse_data(p: &[u8]) -> Option<(u16, u8, &[u8])> {
    parse(p).map(|d| (d.universe, d.priority, d.data))
}

/// Options bit: preview data (for visualisers, not for output).
pub const PREVIEW: u8 = 0x80;
/// Options bit: the source stops sending this universe.
pub const TERMINATED: u8 = 0x40;

/// A received data packet.
#[derive(Debug, PartialEq)]
pub struct DataPacket<'a> {
    /// Source id.
    pub cid: [u8; 16],
    /// Universe.
    pub universe: u16,
    /// Source priority.
    pub priority: u8,
    /// Options bits ([`PREVIEW`], [`TERMINATED`]).
    pub options: u8,
    /// Slot values (null start code only).
    pub data: &'a [u8],
}

/// Parse a data packet with a null start code (other start codes and other vectors give None).
pub fn parse(p: &[u8]) -> Option<DataPacket<'_>> {
    if p.len() < 126 || &p[4..16] != ACN_ID || p[117] != 0x02 || p[125] != 0 {
        return None;
    }
    if u32::from_be_bytes([p[18], p[19], p[20], p[21]]) != 4 || u32::from_be_bytes([p[40], p[41], p[42], p[43]]) != 2 {
        return None;
    }
    let count = (u16::from_be_bytes([p[123], p[124]]) as usize).clamp(1, SLOTS + 1);
    Some(DataPacket {
        cid: p[22..38].try_into().ok()?,
        universe: u16::from_be_bytes([p[113], p[114]]),
        priority: p[108],
        options: p[112],
        data: p.get(126..125 + count)?,
    })
}

/// The source id every sACN output of this process sends (one OpnQ = one source), so inputs
/// can ignore OpnQ's own packets.
pub fn own_cid() -> [u8; 16] {
    static CID: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    *CID.get_or_init(|| uuid::Uuid::new_v4().into_bytes())
}

/// Sends sACN data packets.
pub struct SacnOutput {
    socket: UdpSocket,
    unicast: Option<SocketAddr>,
    cid: [u8; 16],
    priority: u8,
    sequence: u8,
    buf: Box<[u8; PACKET_LEN]>,
}

impl SacnOutput {
    /// `target` empty = multicast per universe.
    pub fn open(target: &str, priority: u8) -> anyhow::Result<Self> {
        let unicast = (!target.trim().is_empty())
            .then(|| super::artnet::parse_target(target, Ipv4Addr::LOCALHOST, PORT))
            .transpose()?;
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_multicast_ttl_v4(8)?;
        let cid = own_cid();
        Ok(Self { socket, unicast, cid, priority, sequence: 0, buf: Box::new([0; PACKET_LEN]) })
    }
}

impl DmxOutput for SacnOutput {
    fn send(&mut self, universe: &Universe) -> anyhow::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        data_packet(&self.cid, "OpnQ", self.priority, self.sequence, universe, &mut self.buf);
        let to = self.unicast.unwrap_or_else(|| SocketAddr::from((multicast(universe.id), PORT)));
        self.socket.send_to(&self.buf[..], to)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unicast_loopback() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut out = SacnOutput::open(&rx.local_addr().unwrap().to_string(), 120).unwrap();
        let mut u = Universe::new(7);
        u.data[0] = 1;
        u.data[511] = 2;
        out.send(&u).unwrap();
        let mut buf = [0u8; 700];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(n, PACKET_LEN);
        assert_eq!(&buf[16..18], &[0x72, 0x6e], "root flags+length = 0x7000 | 622");
        let (uni, prio, data) = parse_data(&buf[..n]).unwrap();
        assert_eq!((uni, prio, data.len(), data[0], data[511]), (7, 120, 512, 1, 2));
        assert_eq!(multicast(0x0102), Ipv4Addr::new(239, 255, 1, 2));
    }
}
