//! Packet formats: parse and build Ethernet, ARP, IPv4 and ICMP.
//! All multi-byte fields are big-endian ("network order").

use crate::{Ipv4, Mac};
use alloc::vec::Vec;

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETH_HEADER: usize = 14;
/// Largest IP packet in one Ethernet frame.
pub const MTU: usize = 1500;

pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;
pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;
pub const ICMP_ECHO_REPLY: u8 = 0;
pub const ICMP_UNREACHABLE: u8 = 3;
pub const ICMP_ECHO_REQUEST: u8 = 8;

fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

/// The internet checksum (RFC 1071) of `data`: the value to store in the
/// checksum field (computed with that field set to zero). Verifying: the
/// checksum over data that includes a correct checksum field is 0.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

// --- Ethernet ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthHeader {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
}

pub fn parse_eth(frame: &[u8]) -> Option<(EthHeader, &[u8])> {
    if frame.len() < ETH_HEADER {
        return None;
    }
    let mac = |o: usize| [frame[o], frame[o + 1], frame[o + 2], frame[o + 3], frame[o + 4], frame[o + 5]];
    Some((EthHeader { dst: mac(0), src: mac(6), ethertype: be16(frame, 12) }, &frame[ETH_HEADER..]))
}

pub fn build_eth(dst: &Mac, src: &Mac, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(ETH_HEADER + payload.len());
    f.extend_from_slice(dst);
    f.extend_from_slice(src);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

// --- ARP (IPv4 over Ethernet) -----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arp {
    pub op: u16,
    pub sender_mac: Mac,
    pub sender_ip: Ipv4,
    pub target_mac: Mac,
    pub target_ip: Ipv4,
}

pub const ARP_LEN: usize = 28;

pub fn parse_arp(p: &[u8]) -> Option<Arp> {
    // hardware type 1 (Ethernet), protocol 0x0800 (IPv4), sizes 6 and 4
    if p.len() < ARP_LEN || p[..6] != [0, 1, 8, 0, 6, 4] {
        return None;
    }
    let mac = |o: usize| [p[o], p[o + 1], p[o + 2], p[o + 3], p[o + 4], p[o + 5]];
    let ip = |o: usize| Ipv4([p[o], p[o + 1], p[o + 2], p[o + 3]]);
    Some(Arp { op: be16(p, 6), sender_mac: mac(8), sender_ip: ip(14), target_mac: mac(18), target_ip: ip(24) })
}

pub fn build_arp(a: &Arp) -> [u8; ARP_LEN] {
    let mut p = [0u8; ARP_LEN];
    p[..6].copy_from_slice(&[0, 1, 8, 0, 6, 4]);
    p[6..8].copy_from_slice(&a.op.to_be_bytes());
    p[8..14].copy_from_slice(&a.sender_mac);
    p[14..18].copy_from_slice(&a.sender_ip.0);
    p[18..24].copy_from_slice(&a.target_mac);
    p[24..28].copy_from_slice(&a.target_ip.0);
    p
}

// --- IPv4 ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Header {
    pub src: Ipv4,
    pub dst: Ipv4,
    pub protocol: u8,
    pub ttl: u8,
    pub id: u16,
    /// True when this is one piece of a fragmented packet (not supported).
    pub fragment: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpError {
    Truncated,
    NotIpv4,
    BadHeaderLength,
    BadTotalLength,
    BadChecksum,
}

/// Parse an IPv4 packet. Returns the header and the payload (cut to the
/// packet's total length: Ethernet may have padded the frame).
pub fn parse_ipv4(p: &[u8]) -> Result<(Ipv4Header, &[u8]), IpError> {
    if p.len() < 20 {
        return Err(IpError::Truncated);
    }
    if p[0] >> 4 != 4 {
        return Err(IpError::NotIpv4);
    }
    let header_len = (p[0] & 0x0F) as usize * 4;
    if header_len < 20 || header_len > p.len() {
        return Err(IpError::BadHeaderLength);
    }
    let total = be16(p, 2) as usize;
    if total < header_len || total > p.len() {
        return Err(IpError::BadTotalLength);
    }
    if checksum(&p[..header_len]) != 0 {
        return Err(IpError::BadChecksum);
    }
    let flags_offset = be16(p, 6);
    let header = Ipv4Header {
        src: Ipv4([p[12], p[13], p[14], p[15]]),
        dst: Ipv4([p[16], p[17], p[18], p[19]]),
        protocol: p[9],
        ttl: p[8],
        id: be16(p, 4),
        fragment: flags_offset & 0x2000 != 0 || flags_offset & 0x1FFF != 0, // "more fragments" or an offset
    };
    Ok((header, &p[header_len..total]))
}

/// Build an IPv4 packet (20-byte header, "don't fragment" set).
/// `payload` must fit: at most `MTU - 20` bytes.
pub fn build_ipv4(src: Ipv4, dst: Ipv4, protocol: u8, ttl: u8, id: u16, payload: &[u8]) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut p = Vec::with_capacity(total);
    p.extend_from_slice(&[0x45, 0]);
    p.extend_from_slice(&(total as u16).to_be_bytes());
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&[0x40, 0, ttl, protocol, 0, 0]);
    p.extend_from_slice(&src.0);
    p.extend_from_slice(&dst.0);
    let sum = checksum(&p);
    p[10..12].copy_from_slice(&sum.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

// --- ICMP ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Icmp<'a> {
    pub kind: u8,
    pub code: u8,
    /// Bytes 4-7: identifier and sequence number for echo messages.
    pub rest: [u8; 4],
    pub data: &'a [u8],
}

impl Icmp<'_> {
    pub fn id(&self) -> u16 {
        u16::from_be_bytes([self.rest[0], self.rest[1]])
    }

    pub fn seq(&self) -> u16 {
        u16::from_be_bytes([self.rest[2], self.rest[3]])
    }
}

/// Parse an ICMP message, checking its checksum.
pub fn parse_icmp(p: &[u8]) -> Option<Icmp<'_>> {
    if p.len() < 8 || checksum(p) != 0 {
        return None;
    }
    Some(Icmp { kind: p[0], code: p[1], rest: [p[4], p[5], p[6], p[7]], data: &p[8..] })
}

pub fn build_icmp(kind: u8, code: u8, rest: [u8; 4], data: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(8 + data.len());
    p.extend_from_slice(&[kind, code, 0, 0]);
    p.extend_from_slice(&rest);
    p.extend_from_slice(data);
    let sum = checksum(&p);
    p[2..4].copy_from_slice(&sum.to_be_bytes());
    p
}

pub fn echo_rest(id: u16, seq: u16) -> [u8; 4] {
    let (i, s) = (id.to_be_bytes(), seq.to_be_bytes());
    [i[0], i[1], s[0], s[1]]
}

// --- UDP ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Udp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub data: &'a [u8],
}

/// The checksum of a UDP datagram or TCP segment including the IPv4
/// "pseudo header" (addresses, protocol, length).
fn pseudo_checksum(src: Ipv4, dst: Ipv4, protocol: u8, segment: &[u8]) -> u16 {
    let mut buf = Vec::with_capacity(12 + segment.len());
    buf.extend_from_slice(&src.0);
    buf.extend_from_slice(&dst.0);
    buf.extend_from_slice(&[0, protocol]);
    buf.extend_from_slice(&(segment.len() as u16).to_be_bytes());
    buf.extend_from_slice(segment);
    checksum(&buf)
}

fn udp_checksum(src: Ipv4, dst: Ipv4, datagram: &[u8]) -> u16 {
    pseudo_checksum(src, dst, PROTO_UDP, datagram)
}

// --- TCP ----------------------------------------------------------------------------

pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_PSH: u8 = 0x08;
pub const TCP_ACK: u8 = 0x10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tcp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    /// The "maximum segment size" option (only sent with SYN).
    pub mss: Option<u16>,
    pub data: &'a [u8],
}

impl Tcp<'_> {
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// Parse a TCP segment; the checksum must be correct.
pub fn parse_tcp(p: &[u8], src: Ipv4, dst: Ipv4) -> Option<Tcp<'_>> {
    if p.len() < 20 || pseudo_checksum(src, dst, PROTO_TCP, p) != 0 {
        return None;
    }
    let header_len = (p[12] >> 4) as usize * 4;
    if header_len < 20 || header_len > p.len() {
        return None;
    }
    // Options: we only care about MSS (kind 2, length 4).
    let (mut mss, mut i) = (None, 20);
    while i < header_len {
        match p[i] {
            0 => break,  // end of options
            1 => i += 1, // padding
            kind => {
                let len = *p.get(i + 1)? as usize;
                if len < 2 || i + len > header_len {
                    return None;
                }
                if kind == 2 && len == 4 {
                    mss = Some(be16(p, i + 2));
                }
                i += len;
            }
        }
    }
    let be32 = |o: usize| u32::from_be_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
    Some(Tcp {
        src_port: be16(p, 0),
        dst_port: be16(p, 2),
        seq: be32(4),
        ack: be32(8),
        flags: p[13] & 0x3F,
        window: be16(p, 14),
        mss,
        data: &p[header_len..],
    })
}

#[allow(clippy::too_many_arguments)]
pub fn build_tcp(src: Ipv4, dst: Ipv4, t: &Tcp) -> Vec<u8> {
    let header_len = if t.mss.is_some() { 24 } else { 20 };
    let mut p = Vec::with_capacity(header_len + t.data.len());
    p.extend_from_slice(&t.src_port.to_be_bytes());
    p.extend_from_slice(&t.dst_port.to_be_bytes());
    p.extend_from_slice(&t.seq.to_be_bytes());
    p.extend_from_slice(&t.ack.to_be_bytes());
    p.extend_from_slice(&[(header_len as u8 / 4) << 4, t.flags]);
    p.extend_from_slice(&t.window.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0]); // checksum, urgent pointer
    if let Some(mss) = t.mss {
        p.extend_from_slice(&[2, 4]);
        p.extend_from_slice(&mss.to_be_bytes());
    }
    p.extend_from_slice(t.data);
    let sum = pseudo_checksum(src, dst, PROTO_TCP, &p);
    p[16..18].copy_from_slice(&sum.to_be_bytes());
    p
}

/// Parse a UDP datagram. The checksum is verified when the sender used one
/// (a checksum field of 0 means "none").
pub fn parse_udp(p: &[u8], src: Ipv4, dst: Ipv4) -> Option<Udp<'_>> {
    if p.len() < 8 {
        return None;
    }
    let len = be16(p, 4) as usize;
    if len < 8 || len > p.len() {
        return None;
    }
    let p = &p[..len];
    if be16(p, 6) != 0 && udp_checksum(src, dst, p) != 0 {
        return None;
    }
    Some(Udp { src_port: be16(p, 0), dst_port: be16(p, 2), data: &p[8..] })
}

pub fn build_udp(src: Ipv4, dst: Ipv4, src_port: u16, dst_port: u16, data: &[u8]) -> Vec<u8> {
    let len = 8 + data.len();
    let mut p = Vec::with_capacity(len);
    p.extend_from_slice(&src_port.to_be_bytes());
    p.extend_from_slice(&dst_port.to_be_bytes());
    p.extend_from_slice(&(len as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(data);
    // A computed checksum of 0 is sent as 0xFFFF (0 means "no checksum").
    let sum = match udp_checksum(src, dst, &p) {
        0 => 0xFFFF,
        s => s,
    };
    p[6..8].copy_from_slice(&sum.to_be_bytes());
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_round_trip_and_rejects() {
        let (a, b) = (Ipv4([10, 0, 0, 2]), Ipv4([10, 0, 0, 3]));
        let syn = Tcp { src_port: 50000, dst_port: 80, seq: 0x0102_0304, ack: 0, flags: TCP_SYN, window: 16384, mss: Some(1460), data: b"" };
        let bytes = build_tcp(a, b, &syn);
        // Byte-exact header: ports, sequence, data offset 6 words, SYN, window, MSS option.
        assert_eq!(&bytes[..16], &[0xC3, 0x50, 0, 80, 1, 2, 3, 4, 0, 0, 0, 0, 0x60, 0x02, 0x40, 0x00]);
        assert_eq!(&bytes[20..], &[2, 4, 0x05, 0xB4]);
        assert_eq!(parse_tcp(&bytes, a, b), Some(syn));
        let data = Tcp { src_port: 80, dst_port: 50000, seq: 9, ack: 0x0102_0305, flags: TCP_ACK | TCP_PSH, window: 100, mss: None, data: b"hello" };
        let bytes = build_tcp(b, a, &data);
        assert_eq!(parse_tcp(&bytes, b, a), Some(data));
        assert_eq!(parse_tcp(&bytes, a, b).map(|t| t.data), Some(&b"hello"[..])); // swapping both addresses keeps the sum
        assert_eq!(parse_tcp(&bytes, b, Ipv4([10, 0, 0, 9])), None);
        let mut bad = bytes.clone();
        bad[22] ^= 1;
        assert_eq!(parse_tcp(&bad, b, a), None);
        for len in 0..bytes.len() {
            assert_eq!(parse_tcp(&bytes[..len], b, a), None);
        }
        // A header length pointing past the segment, and a broken option, are rejected
        // (the checksum is recomputed so only that field is wrong).
        let fix = |mut p: Vec<u8>| {
            p[16] = 0;
            p[17] = 0;
            let s = pseudo_checksum(b, a, PROTO_TCP, &p);
            p[16..18].copy_from_slice(&s.to_be_bytes());
            p
        };
        let mut long = bytes.clone();
        long[12] = 0xF0;
        assert_eq!(parse_tcp(&fix(long), b, a), None);
        let mut short = bytes.clone();
        short[12] = 0x40;
        assert_eq!(parse_tcp(&fix(short), b, a), None);
        let mut opt = build_tcp(a, b, &syn);
        opt[21] = 9; // option length runs past the header
        let s = { opt[16] = 0; opt[17] = 0; pseudo_checksum(a, b, PROTO_TCP, &opt) };
        opt[16..18].copy_from_slice(&s.to_be_bytes());
        assert_eq!(parse_tcp(&opt, a, b), None);
    }

    #[test]
    fn udp_round_trip_and_rejects() {
        let (a, b) = (Ipv4([10, 0, 0, 2]), Ipv4([10, 0, 0, 3]));
        let d = build_udp(a, b, 5000, 53, b"query");
        assert_eq!(parse_udp(&d, a, b), Some(Udp { src_port: 5000, dst_port: 53, data: b"query" }));
        assert_eq!(parse_udp(&d, a, Ipv4([10, 0, 0, 4])), None); // wrong address: checksum fails
        let mut bad = d.clone();
        bad[9] ^= 1;
        assert_eq!(parse_udp(&bad, a, b), None);
        let mut nosum = d.clone();
        nosum[6] = 0;
        nosum[7] = 0;
        assert!(parse_udp(&nosum, a, b).is_some()); // "no checksum" is accepted
        let mut short = d.clone();
        short[5] = 7; // length field smaller than the header
        assert_eq!(parse_udp(&short, a, b), None);
        let mut long = d.clone();
        long[5] = 200; // length field larger than the packet
        assert_eq!(parse_udp(&long, a, b), None);
        for len in 0..d.len() {
            let _ = parse_udp(&d[..len], a, b);
        }
        // Padding after the datagram is ignored.
        let mut padded = d.clone();
        padded.extend_from_slice(&[0; 9]);
        assert_eq!(parse_udp(&padded, a, b).unwrap().data, b"query");
    }

    #[test]
    fn checksum_known_values() {
        // RFC 1071 example: the sum of these words is 0xddf2, so the checksum is 0x220d.
        assert_eq!(checksum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]), 0x220d);
        assert_eq!(checksum(&[]), 0xFFFF);
        assert_eq!(checksum(&[0xFF]), 0x00FF); // odd length: padded with a zero byte
        // A well-known example IPv4 header; its checksum field is b861.
        let header = [0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0xb8, 0x61, 0xc0, 0xa8, 0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7];
        assert_eq!(checksum(&header), 0);
        let mut zeroed = header;
        zeroed[10] = 0;
        zeroed[11] = 0;
        assert_eq!(checksum(&zeroed), 0xb861);
    }

    #[test]
    fn ethernet_round_trip() {
        let f = build_eth(&[1; 6], &[2; 6], ETHERTYPE_ARP, b"xy");
        assert_eq!(parse_eth(&f), Some((EthHeader { dst: [1; 6], src: [2; 6], ethertype: 0x0806 }, &b"xy"[..])));
        assert_eq!(parse_eth(&f[..13]), None);
    }

    #[test]
    fn arp_round_trip_and_rejects() {
        let a = Arp { op: ARP_REQUEST, sender_mac: [2, 0, 0, 0, 0, 1], sender_ip: Ipv4([10, 0, 0, 1]), target_mac: [0; 6], target_ip: Ipv4([10, 0, 0, 2]) };
        let bytes = build_arp(&a);
        // Byte-exact: an ARP request as seen on the wire.
        assert_eq!(bytes, [0, 1, 8, 0, 6, 4, 0, 1, 2, 0, 0, 0, 0, 1, 10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 10, 0, 0, 2]);
        assert_eq!(parse_arp(&bytes), Some(a));
        assert_eq!(parse_arp(&bytes[..27]), None);
        let mut ipv6 = bytes;
        ipv6[2] = 0x86; // not IPv4
        assert_eq!(parse_arp(&ipv6), None);
    }

    #[test]
    fn ipv4_round_trip_and_rejects() {
        let p = build_ipv4(Ipv4([10, 0, 0, 2]), Ipv4([10, 0, 0, 1]), PROTO_ICMP, 64, 7, b"payload");
        let (h, payload) = parse_ipv4(&p).unwrap();
        assert_eq!((h.src, h.dst, h.protocol, h.ttl, h.id, h.fragment), (Ipv4([10, 0, 0, 2]), Ipv4([10, 0, 0, 1]), 1, 64, 7, false));
        assert_eq!(payload, b"payload");
        // Ethernet padding after the packet is cut off.
        let mut padded = p.clone();
        padded.extend_from_slice(&[0; 20]);
        assert_eq!(parse_ipv4(&padded).unwrap().1, b"payload");

        assert_eq!(parse_ipv4(&p[..19]), Err(IpError::Truncated));
        assert_eq!(parse_ipv4(&p[..24]), Err(IpError::BadTotalLength));
        let mut bad = p.clone();
        bad[8] = 63; // TTL changed without fixing the checksum
        assert_eq!(parse_ipv4(&bad), Err(IpError::BadChecksum));
        let mut v6 = p.clone();
        v6[0] = 0x65;
        assert_eq!(parse_ipv4(&v6), Err(IpError::NotIpv4));
        let mut ihl = p.clone();
        ihl[0] = 0x44;
        assert_eq!(parse_ipv4(&ihl), Err(IpError::BadHeaderLength));
        let mut ihl = p.clone();
        ihl[0] = 0x4F; // header longer than the packet
        assert_eq!(parse_ipv4(&ihl), Err(IpError::BadHeaderLength));

        // A fragment is recognised (and later dropped by the stack).
        let mut frag = p.clone();
        frag[6] = 0x20;
        frag[10] = 0;
        frag[11] = 0;
        let sum = checksum(&frag[..20]);
        frag[10..12].copy_from_slice(&sum.to_be_bytes());
        assert!(parse_ipv4(&frag).unwrap().0.fragment);
    }

    #[test]
    fn icmp_round_trip() {
        let m = build_icmp(ICMP_ECHO_REQUEST, 0, echo_rest(0x1234, 5), b"abcdefg"); // odd length
        let i = parse_icmp(&m).unwrap();
        assert_eq!((i.kind, i.code, i.id(), i.seq(), i.data), (8, 0, 0x1234, 5, &b"abcdefg"[..]));
        let mut bad = m.clone();
        bad[9] ^= 1;
        assert_eq!(parse_icmp(&bad), None);
        assert_eq!(parse_icmp(&m[..7]), None);
    }

    #[test]
    fn parsers_survive_any_truncation_and_garbage() {
        let ip = build_ipv4(Ipv4([1, 2, 3, 4]), Ipv4([5, 6, 7, 8]), 1, 64, 1, &build_icmp(8, 0, [0; 4], &[7; 40]));
        let frame = build_eth(&[1; 6], &[2; 6], ETHERTYPE_IPV4, &ip);
        for len in 0..=frame.len() {
            let f = &frame[..len];
            if let Some((_, payload)) = parse_eth(f) {
                let _ = parse_arp(payload);
                if let Ok((_, inner)) = parse_ipv4(payload) {
                    let _ = parse_icmp(inner);
                }
            }
        }
        // Pseudo-random garbage must never panic either.
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..2000 {
            let mut buf = [0u8; 64];
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            buf[0] = 0x45 + (x as u8 & 3); // often a plausible IPv4 first byte
            let _ = parse_arp(&buf);
            if let Ok((_, inner)) = parse_ipv4(&buf) {
                let _ = parse_icmp(inner);
            }
        }
    }
}
