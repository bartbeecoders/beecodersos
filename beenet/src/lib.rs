//! beenet: the BeeCodersOs network stack.
//!
//! It never touches hardware: frames come in through [`Stack::receive`], frames
//! to send come out of [`Stack::transmit`], and time is passed in as
//! milliseconds. That makes it testable on the host (`cargo test`), including
//! two stacks wired back to back.
//!
//! Ethernet II, ARP with a cache, IPv4 (one interface, local subnet or default
//! gateway, no fragments), ICMP echo and "unreachable" (network phase N2);
//! UDP sockets, a DHCP client and a DNS resolver (phase N3); TCP (phase N4);
//! the client side of HTTP, SNTP and TFTP as socket-free modules (phase N5).
//! See Docs/network-stack-plan.html.
//!
//! Every received byte is untrusted: parsers check every length before use.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod dhcp;
mod dns;
pub mod base64;
pub mod hive;
pub mod html;
pub mod http;
pub mod sha256;
pub mod sntp;
mod stack;
mod tcp;
pub mod telnet;
pub mod tftp;
pub mod wire;

pub use dhcp::{Lease, Notice};
pub use dns::{is_valid_name, DnsError, Lookup, PORT as DNS_PORT};
pub use stack::{ArpEntry, Config, Datagram, Error, Event, Reason, Stack, Stats, UDP_MAX_DATA};
pub use tcp::{CloseReason, TcpError, TcpHandle, TcpStatus, RECV_BUFFER, SEND_BUFFER};

use core::fmt;

/// An Ethernet hardware address.
pub type Mac = [u8; 6];
pub const BROADCAST_MAC: Mac = [0xFF; 6];

/// An IPv4 address.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    pub const UNSPECIFIED: Ipv4 = Ipv4([0; 4]);
    pub const BROADCAST: Ipv4 = Ipv4([255; 4]);

    pub fn to_u32(self) -> u32 {
        u32::from_be_bytes(self.0)
    }

    pub fn from_u32(v: u32) -> Self {
        Ipv4(v.to_be_bytes())
    }

    /// Parse dotted decimal ("192.168.1.10"). Strict: four numbers 0-255, no extras.
    pub fn parse(s: &str) -> Option<Ipv4> {
        let mut out = [0u8; 4];
        let mut parts = s.split('.');
        for b in out.iter_mut() {
            let p = parts.next()?;
            if p.is_empty() || p.len() > 3 || !p.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            *b = p.parse().ok()?;
        }
        parts.next().is_none().then_some(Ipv4(out))
    }
}

impl fmt::Display for Ipv4 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

/// Parse "ADDRESS/PREFIX" (e.g. "10.0.2.15/24"). The prefix is 1-30.
pub fn parse_cidr(s: &str) -> Option<(Ipv4, u8)> {
    let (addr, prefix) = s.split_once('/')?;
    let prefix: u8 = prefix.parse().ok()?;
    (1..=30).contains(&prefix).then_some(())?;
    Some((Ipv4::parse(addr)?, prefix))
}

/// The netmask for a prefix length, e.g. 24 -> 255.255.255.0.
pub fn netmask(prefix: u8) -> Ipv4 {
    Ipv4::from_u32(if prefix == 0 { 0 } else { u32::MAX << (32 - prefix.min(32)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(Ipv4::parse("192.168.1.10"), Some(Ipv4([192, 168, 1, 10])));
        for bad in ["", "1.2.3", "1.2.3.4.5", "1.2.3.256", "1..3.4", "a.b.c.d", "1.2.3.4 ", "0001.2.3.4", "1.2.3.-4"] {
            assert_eq!(Ipv4::parse(bad), None, "{bad}");
        }
        assert_eq!(format!("{}", Ipv4([10, 0, 2, 15])), "10.0.2.15");
        assert_eq!(parse_cidr("10.0.2.15/24"), Some((Ipv4([10, 0, 2, 15]), 24)));
        assert_eq!(parse_cidr("10.0.2.15"), None);
        assert_eq!(parse_cidr("10.0.2.15/0"), None);
        assert_eq!(parse_cidr("10.0.2.15/31"), None);
        assert_eq!(netmask(24), Ipv4([255, 255, 255, 0]));
        assert_eq!(netmask(20), Ipv4([255, 255, 240, 0]));
        assert_eq!(netmask(30), Ipv4([255, 255, 255, 252]));
    }
}
