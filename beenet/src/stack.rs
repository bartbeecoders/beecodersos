//! The stack: one network interface with an IPv4 address, an ARP cache, and
//! ICMP echo. A state machine with no I/O of its own:
//!
//!   receive(frame, now)   a frame arrived from the card
//!   poll(now)             timers: ARP retries
//!   transmit()            the next frame to give to the card
//!   take_event()          things the user of the stack wants to know
//!
//! All tables are bounded, so a flood of packets can't use up memory.

use crate::wire::{self, Arp, ARP_REPLY, ARP_REQUEST, ETHERTYPE_ARP, ETHERTYPE_IPV4};
use crate::{netmask, Ipv4, Mac, BROADCAST_MAC};
use alloc::collections::VecDeque;
use alloc::vec::Vec;

const ARP_CACHE_SIZE: usize = 32;
/// An ARP entry is used for this long after it was last confirmed.
const ARP_LIFETIME_MS: u64 = 60_000;
const ARP_RETRY_MS: u64 = 1_000;
const ARP_TRIES: u8 = 3;
/// Packets waiting for an ARP answer.
const MAX_PENDING: usize = 16;
const MAX_OUT: usize = 64;
const MAX_EVENTS: usize = 32;
const TTL: u8 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub ip: Ipv4,
    pub prefix: u8,
    pub gateway: Option<Ipv4>,
}

impl Config {
    fn same_subnet(&self, other: Ipv4) -> bool {
        let mask = netmask(self.prefix).to_u32();
        self.ip.to_u32() & mask == other.to_u32() & mask
    }

    fn subnet_broadcast(&self) -> Ipv4 {
        Ipv4::from_u32(self.ip.to_u32() | !netmask(self.prefix).to_u32())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No IPv4 address is configured.
    NoAddress,
    /// The destination is outside the local network and there is no gateway.
    NoRoute,
    /// Too many packets are already waiting for an address lookup.
    QueueFull,
    TooLarge,
}

impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Error::NoAddress => "no IP address configured",
            Error::NoRoute => "no route: the address is outside the local network and no gateway is set",
            Error::QueueFull => "too many packets waiting for address lookups",
            Error::TooLarge => "packet too large",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Nobody on the local network answered the ARP request for the next hop.
    NoArpReply,
    /// A router or host sent ICMP "destination unreachable" with this code.
    Icmp(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    EchoReply { from: Ipv4, id: u16, seq: u16, ttl: u8, len: usize },
    Unreachable { dst: Ipv4, reason: Reason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArpEntry {
    pub ip: Ipv4,
    pub mac: Mac,
    pub updated_ms: u64,
}

/// A next hop being looked up, and the packets waiting for it.
struct Lookup {
    ip: Ipv4,
    last_request_ms: u64,
    tries: u8,
}

struct Pending {
    next_hop: Ipv4,
    dst: Ipv4,
    packet: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub rx_arp: u64,
    pub rx_ipv4: u64,
    pub echo_requests_answered: u64,
    /// Frames ignored: malformed, not for us, or a protocol we don't speak.
    pub dropped: u64,
    pub dropped_fragments: u64,
    /// Another machine used our IP address in an ARP packet.
    pub address_conflicts: u64,
}

pub struct Stack {
    mac: Mac,
    config: Option<Config>,
    arp: Vec<ArpEntry>,
    lookups: Vec<Lookup>,
    pending: Vec<Pending>,
    out: VecDeque<Vec<u8>>,
    events: VecDeque<Event>,
    next_id: u16,
    pub stats: Stats,
}

impl Stack {
    /// `ip_id_seed`: starting value for the IPv4 identification field.
    pub fn new(mac: Mac, ip_id_seed: u16) -> Self {
        Stack {
            mac,
            config: None,
            arp: Vec::new(),
            lookups: Vec::new(),
            pending: Vec::new(),
            out: VecDeque::new(),
            events: VecDeque::new(),
            next_id: ip_id_seed,
            stats: Stats::default(),
        }
    }

    pub fn mac(&self) -> Mac {
        self.mac
    }

    pub fn config(&self) -> Option<Config> {
        self.config
    }

    /// Set (or with None, remove) the IPv4 configuration. Announces a new
    /// address with a gratuitous ARP so neighbours update their caches.
    pub fn set_config(&mut self, config: Option<Config>) {
        self.config = config;
        self.arp.clear();
        self.lookups.clear();
        self.pending.clear();
        if let Some(c) = config {
            self.send_arp(ARP_REQUEST, [0; 6], c.ip, &BROADCAST_MAC);
        }
    }

    pub fn arp_table(&self) -> &[ArpEntry] {
        &self.arp
    }

    /// True while a next-hop address lookup is in progress.
    pub fn resolving(&self) -> bool {
        !self.lookups.is_empty()
    }

    /// The next frame to hand to the network card.
    pub fn transmit(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    /// Put back a frame the card could not take yet.
    pub fn untransmit(&mut self, frame: Vec<u8>) {
        self.out.push_front(frame);
    }

    pub fn take_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Queue an ICMP echo request ("ping").
    pub fn ping(&mut self, dst: Ipv4, id: u16, seq: u16, payload: &[u8], now_ms: u64) -> Result<(), Error> {
        let msg = wire::build_icmp(wire::ICMP_ECHO_REQUEST, 0, wire::echo_rest(id, seq), payload);
        self.send_ipv4(dst, wire::PROTO_ICMP, &msg, now_ms)
    }

    // --- sending ---------------------------------------------------------------

    fn emit(&mut self, frame: Vec<u8>) {
        if self.out.len() < MAX_OUT {
            self.out.push_back(frame);
        } else {
            self.stats.dropped += 1;
        }
    }

    fn event(&mut self, e: Event) {
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(e);
    }

    fn send_arp(&mut self, op: u16, target_mac: Mac, target_ip: Ipv4, eth_dst: &Mac) {
        let Some(c) = self.config else { return };
        let arp = wire::build_arp(&Arp { op, sender_mac: self.mac, sender_ip: c.ip, target_mac, target_ip });
        let frame = wire::build_eth(eth_dst, &self.mac, ETHERTYPE_ARP, &arp);
        self.emit(frame);
    }

    fn lookup_mac(&self, ip: Ipv4, now_ms: u64) -> Option<Mac> {
        self.arp.iter().find(|e| e.ip == ip && now_ms.saturating_sub(e.updated_ms) < ARP_LIFETIME_MS).map(|e| e.mac)
    }

    /// Send an IPv4 packet: choose the next hop, find its hardware address
    /// (queueing the packet while ARP runs) and frame it.
    pub fn send_ipv4(&mut self, dst: Ipv4, protocol: u8, payload: &[u8], now_ms: u64) -> Result<(), Error> {
        let c = self.config.ok_or(Error::NoAddress)?;
        if payload.len() > wire::MTU - 20 {
            return Err(Error::TooLarge);
        }
        self.next_id = self.next_id.wrapping_add(1);
        let packet = wire::build_ipv4(c.ip, dst, protocol, TTL, self.next_id, payload);
        if dst == Ipv4::BROADCAST || dst == c.subnet_broadcast() {
            let frame = wire::build_eth(&BROADCAST_MAC, &self.mac, ETHERTYPE_IPV4, &packet);
            self.emit(frame);
            return Ok(());
        }
        let next_hop = if c.same_subnet(dst) { dst } else { c.gateway.ok_or(Error::NoRoute)? };
        if let Some(mac) = self.lookup_mac(next_hop, now_ms) {
            let frame = wire::build_eth(&mac, &self.mac, ETHERTYPE_IPV4, &packet);
            self.emit(frame);
            return Ok(());
        }
        if self.pending.len() >= MAX_PENDING {
            return Err(Error::QueueFull);
        }
        self.pending.push(Pending { next_hop, dst, packet });
        if !self.lookups.iter().any(|l| l.ip == next_hop) {
            self.lookups.push(Lookup { ip: next_hop, last_request_ms: now_ms, tries: 1 });
            self.send_arp(ARP_REQUEST, [0; 6], next_hop, &BROADCAST_MAC);
        }
        Ok(())
    }

    /// Timers: repeat unanswered ARP requests, give up after a few tries.
    pub fn poll(&mut self, now_ms: u64) {
        let mut i = 0;
        while i < self.lookups.len() {
            let l = &mut self.lookups[i];
            if now_ms.saturating_sub(l.last_request_ms) < ARP_RETRY_MS {
                i += 1;
                continue;
            }
            let ip = l.ip;
            if l.tries >= ARP_TRIES {
                self.lookups.remove(i);
                let mut failed = Vec::new();
                self.pending.retain(|p| {
                    if p.next_hop == ip {
                        failed.push(p.dst);
                    }
                    p.next_hop != ip
                });
                for dst in failed {
                    self.event(Event::Unreachable { dst, reason: Reason::NoArpReply });
                }
            } else {
                l.tries += 1;
                l.last_request_ms = now_ms;
                self.send_arp(ARP_REQUEST, [0; 6], ip, &BROADCAST_MAC);
                i += 1;
            }
        }
    }

    // --- receiving -------------------------------------------------------------

    /// Process a received frame.
    pub fn receive(&mut self, frame: &[u8], now_ms: u64) {
        let Some((eth, payload)) = wire::parse_eth(frame) else {
            self.stats.dropped += 1;
            return;
        };
        if eth.dst != self.mac && eth.dst != BROADCAST_MAC {
            self.stats.dropped += 1;
            return;
        }
        match eth.ethertype {
            ETHERTYPE_ARP => self.receive_arp(payload, now_ms),
            ETHERTYPE_IPV4 => self.receive_ipv4(payload, now_ms),
            _ => self.stats.dropped += 1,
        }
    }

    fn learn(&mut self, ip: Ipv4, mac: Mac, now_ms: u64) {
        if let Some(e) = self.arp.iter_mut().find(|e| e.ip == ip) {
            e.mac = mac;
            e.updated_ms = now_ms;
        } else {
            if self.arp.len() == ARP_CACHE_SIZE {
                // Replace the entry that was confirmed longest ago.
                let oldest = (0..self.arp.len()).min_by_key(|&i| self.arp[i].updated_ms).unwrap_or(0);
                self.arp.swap_remove(oldest);
            }
            self.arp.push(ArpEntry { ip, mac, updated_ms: now_ms });
        }
        // Send what was waiting for this address.
        self.lookups.retain(|l| l.ip != ip);
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].next_hop == ip {
                let p = self.pending.remove(i);
                let frame = wire::build_eth(&mac, &self.mac, ETHERTYPE_IPV4, &p.packet);
                self.emit(frame);
            } else {
                i += 1;
            }
        }
    }

    fn receive_arp(&mut self, payload: &[u8], now_ms: u64) {
        let (Some(arp), Some(c)) = (wire::parse_arp(payload), self.config) else {
            self.stats.dropped += 1;
            return;
        };
        self.stats.rx_arp += 1;
        if arp.sender_mac == self.mac {
            return; // our own broadcast, echoed back
        }
        if arp.sender_ip == c.ip {
            self.stats.address_conflicts += 1;
            return;
        }
        // Senders must be real hosts on our network; ignore probes (0.0.0.0),
        // broadcast addresses and anything from outside the subnet.
        let plausible = arp.sender_ip != Ipv4::UNSPECIFIED
            && arp.sender_ip != c.subnet_broadcast()
            && c.same_subnet(arp.sender_ip)
            && arp.sender_mac != BROADCAST_MAC
            && arp.sender_mac != [0; 6];
        if !plausible {
            return;
        }
        let asked_for_us = arp.target_ip == c.ip;
        // Learn from packets addressed to us, and refresh entries we already
        // have or are waiting for; don't fill the cache from every broadcast.
        let known = self.arp.iter().any(|e| e.ip == arp.sender_ip) || self.lookups.iter().any(|l| l.ip == arp.sender_ip);
        if asked_for_us || known {
            self.learn(arp.sender_ip, arp.sender_mac, now_ms);
        }
        if arp.op == ARP_REQUEST && asked_for_us {
            self.send_arp(ARP_REPLY, arp.sender_mac, arp.sender_ip, &arp.sender_mac);
        }
    }

    fn receive_ipv4(&mut self, payload: &[u8], now_ms: u64) {
        let (Ok((ip, data)), Some(c)) = (wire::parse_ipv4(payload), self.config) else {
            self.stats.dropped += 1;
            return;
        };
        let for_us = ip.dst == c.ip;
        if !for_us && ip.dst != Ipv4::BROADCAST && ip.dst != c.subnet_broadcast() {
            self.stats.dropped += 1;
            return;
        }
        if ip.fragment {
            self.stats.dropped_fragments += 1;
            return;
        }
        self.stats.rx_ipv4 += 1;
        if ip.protocol != wire::PROTO_ICMP {
            self.stats.dropped += 1; // UDP and TCP arrive in later phases
            return;
        }
        let Some(icmp) = wire::parse_icmp(data) else {
            self.stats.dropped += 1;
            return;
        };
        match icmp.kind {
            // Answer pings sent to our own address (not to broadcast addresses).
            wire::ICMP_ECHO_REQUEST if for_us && icmp.code == 0 => {
                let reply = wire::build_icmp(wire::ICMP_ECHO_REPLY, 0, icmp.rest, icmp.data);
                if self.send_ipv4(ip.src, wire::PROTO_ICMP, &reply, now_ms).is_ok() {
                    self.stats.echo_requests_answered += 1;
                }
            }
            wire::ICMP_ECHO_REPLY if for_us => {
                self.event(Event::EchoReply { from: ip.src, id: icmp.id(), seq: icmp.seq(), ttl: ip.ttl, len: icmp.data.len() });
            }
            wire::ICMP_UNREACHABLE if for_us => {
                // The message quotes the start of our packet: its IPv4 header
                // holds the destination that could not be reached.
                if let Some(dst) = icmp.data.get(16..20) {
                    let dst = Ipv4([dst[0], dst[1], dst[2], dst[3]]);
                    self.event(Event::Unreachable { dst, reason: Reason::Icmp(icmp.code) });
                }
            }
            _ => self.stats.dropped += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::*;

    const MAC_A: Mac = [0x52, 0x54, 0x00, 0xbe, 0xe0, 0x01];
    const MAC_B: Mac = [0x02, 0, 0, 0, 0, 0x0b];
    const IP_A: Ipv4 = Ipv4([10, 0, 0, 2]);
    const IP_B: Ipv4 = Ipv4([10, 0, 0, 3]);
    const GW: Ipv4 = Ipv4([10, 0, 0, 1]);

    fn stack(mac: Mac, ip: Ipv4) -> Stack {
        let mut s = Stack::new(mac, 100);
        s.set_config(Some(Config { ip, prefix: 24, gateway: Some(GW) }));
        s
    }

    fn drain(s: &mut Stack) -> Vec<Vec<u8>> {
        core::iter::from_fn(|| s.transmit()).collect()
    }

    /// Deliver everything each stack wants to send to the other, until quiet.
    fn exchange(a: &mut Stack, b: &mut Stack, now: u64) -> usize {
        let mut frames = 0;
        loop {
            let (fa, fb) = (drain(a), drain(b));
            if fa.is_empty() && fb.is_empty() {
                return frames;
            }
            frames += fa.len() + fb.len();
            fa.iter().for_each(|f| b.receive(f, now));
            fb.iter().for_each(|f| a.receive(f, now));
        }
    }

    #[test]
    fn announces_its_address() {
        let mut a = stack(MAC_A, IP_A);
        let frames = drain(&mut a);
        assert_eq!(frames.len(), 1);
        let (eth, payload) = parse_eth(&frames[0]).unwrap();
        assert_eq!((eth.dst, eth.src, eth.ethertype), (BROADCAST_MAC, MAC_A, ETHERTYPE_ARP));
        let arp = parse_arp(payload).unwrap();
        assert_eq!((arp.op, arp.sender_ip, arp.target_ip), (ARP_REQUEST, IP_A, IP_A));
    }

    #[test]
    fn two_stacks_ping_each_other() {
        let (mut a, mut b) = (stack(MAC_A, IP_A), stack(MAC_B, IP_B));
        exchange(&mut a, &mut b, 0);
        a.ping(IP_B, 0x1234, 1, b"hello bee", 10).unwrap();
        assert!(a.resolving());
        // ARP request, ARP reply, echo request, echo reply.
        assert_eq!(exchange(&mut a, &mut b, 10), 4);
        assert_eq!(a.take_event(), Some(Event::EchoReply { from: IP_B, id: 0x1234, seq: 1, ttl: 64, len: 9 }));
        assert_eq!(a.take_event(), None);
        assert!(!a.resolving());
        assert_eq!(b.stats.echo_requests_answered, 1);
        // Both sides learned the other's address: the next ping needs no ARP.
        assert_eq!(a.arp_table().iter().map(|e| (e.ip, e.mac)).collect::<Vec<_>>(), [(IP_B, MAC_B)]);
        assert_eq!(b.arp_table().iter().map(|e| (e.ip, e.mac)).collect::<Vec<_>>(), [(IP_A, MAC_A)]);
        a.ping(IP_B, 0x1234, 2, b"", 20).unwrap();
        assert_eq!(exchange(&mut a, &mut b, 20), 2);
        assert_eq!(a.take_event(), Some(Event::EchoReply { from: IP_B, id: 0x1234, seq: 2, ttl: 64, len: 0 }));
    }

    #[test]
    fn answers_arp_with_exact_bytes() {
        let mut a = stack(MAC_A, IP_A);
        drain(&mut a);
        let request = build_eth(&BROADCAST_MAC, &MAC_B, ETHERTYPE_ARP,
            &build_arp(&Arp { op: ARP_REQUEST, sender_mac: MAC_B, sender_ip: IP_B, target_mac: [0; 6], target_ip: IP_A }));
        a.receive(&request, 0);
        let reply = drain(&mut a);
        let expected = build_eth(&MAC_B, &MAC_A, ETHERTYPE_ARP,
            &build_arp(&Arp { op: ARP_REPLY, sender_mac: MAC_A, sender_ip: IP_A, target_mac: MAC_B, target_ip: IP_B }));
        assert_eq!(reply, [expected]);
        // A request for someone else gets no answer and is not learned.
        let other = build_eth(&BROADCAST_MAC, &[2, 0, 0, 0, 0, 0x0c], ETHERTYPE_ARP,
            &build_arp(&Arp { op: ARP_REQUEST, sender_mac: [2, 0, 0, 0, 0, 0x0c], sender_ip: Ipv4([10, 0, 0, 9]), target_mac: [0; 6], target_ip: IP_B }));
        a.receive(&other, 0);
        assert!(drain(&mut a).is_empty());
        assert_eq!(a.arp_table().len(), 1);
    }

    #[test]
    fn off_subnet_goes_to_the_gateway() {
        let mut a = stack(MAC_A, IP_A);
        drain(&mut a);
        let far = Ipv4([93, 184, 216, 34]);
        a.ping(far, 1, 1, b"x", 0).unwrap();
        let frames = drain(&mut a);
        // ARP is for the gateway, not for the far address.
        assert_eq!(parse_arp(parse_eth(&frames[0]).unwrap().1).unwrap().target_ip, GW);
        let gw_mac = [2, 0, 0, 0, 0, 0x01];
        let reply = build_eth(&MAC_A, &gw_mac, ETHERTYPE_ARP,
            &build_arp(&Arp { op: ARP_REPLY, sender_mac: gw_mac, sender_ip: GW, target_mac: MAC_A, target_ip: IP_A }));
        a.receive(&reply, 5);
        let frames = drain(&mut a);
        let (eth, payload) = parse_eth(&frames[0]).unwrap();
        assert_eq!(eth.dst, gw_mac);
        assert_eq!(parse_ipv4(payload).unwrap().0.dst, far); // the IP destination is unchanged

        // Without a gateway there is no route.
        let mut b = Stack::new(MAC_B, 0);
        b.set_config(Some(Config { ip: IP_B, prefix: 24, gateway: None }));
        assert_eq!(b.ping(far, 1, 1, b"", 0), Err(Error::NoRoute));
        assert_eq!(Stack::new(MAC_B, 0).ping(IP_A, 1, 1, b"", 0), Err(Error::NoAddress));
    }

    #[test]
    fn unanswered_arp_gives_up() {
        let mut a = stack(MAC_A, IP_A);
        drain(&mut a);
        a.ping(IP_B, 1, 1, b"", 0).unwrap();
        a.ping(IP_B, 1, 2, b"", 0).unwrap();
        assert_eq!(drain(&mut a).len(), 1); // one ARP request for both packets
        a.poll(500);
        assert!(drain(&mut a).is_empty()); // too early to retry
        a.poll(1000);
        a.poll(2000);
        assert_eq!(drain(&mut a).len(), 2); // two retries
        assert_eq!(a.take_event(), None);
        a.poll(3000);
        assert_eq!(a.take_event(), Some(Event::Unreachable { dst: IP_B, reason: Reason::NoArpReply }));
        assert_eq!(a.take_event(), Some(Event::Unreachable { dst: IP_B, reason: Reason::NoArpReply }));
        assert!(!a.resolving());
        assert!(drain(&mut a).is_empty());
    }

    fn echo_request(dst_mac: &Mac, dst_ip: Ipv4) -> Vec<u8> {
        let icmp = build_icmp(ICMP_ECHO_REQUEST, 0, echo_rest(7, 1), b"data");
        build_eth(dst_mac, &MAC_B, ETHERTYPE_IPV4, &build_ipv4(IP_B, dst_ip, PROTO_ICMP, 64, 1, &icmp))
    }

    #[test]
    fn drops_what_is_not_for_us_or_broken() {
        let mut a = stack(MAC_A, IP_A);
        exchange(&mut a, &mut stack(MAC_B, IP_B), 0);
        let good = echo_request(&MAC_A, IP_A);

        a.receive(&echo_request(&[2, 0, 0, 0, 0, 0x99], IP_A), 0); // another machine's MAC
        a.receive(&echo_request(&MAC_A, Ipv4([10, 0, 0, 77])), 0); // another IP
        a.receive(&echo_request(&BROADCAST_MAC, Ipv4([10, 0, 0, 255])), 0); // broadcast ping: not answered
        let mut bad_ip = good.clone();
        bad_ip[22] ^= 0xFF; // TTL changed: header checksum wrong
        a.receive(&bad_ip, 0);
        let mut bad_icmp = good.clone();
        let last = bad_icmp.len() - 1;
        bad_icmp[last] ^= 1;
        a.receive(&bad_icmp, 0);
        for len in 0..good.len() {
            a.receive(&good[..len], 0); // truncated at every length
        }
        assert!(drain(&mut a).is_empty(), "nothing may be answered");
        assert!(a.stats.dropped >= 5);

        // A fragment is counted and dropped.
        let mut frag = good.clone();
        frag[20] = 0x20;
        frag[24] = 0;
        frag[25] = 0;
        let sum = checksum(&frag[14..34]);
        frag[24..26].copy_from_slice(&sum.to_be_bytes());
        a.receive(&frag, 0);
        assert_eq!(a.stats.dropped_fragments, 1);

        // The intact request is answered (the sender's MAC is not known yet: ARP first).
        a.receive(&good, 0);
        let out = drain(&mut a);
        assert_eq!(parse_arp(parse_eth(&out[0]).unwrap().1).unwrap().target_ip, IP_B);
    }

    #[test]
    fn icmp_unreachable_names_the_destination() {
        let mut a = stack(MAC_A, IP_A);
        drain(&mut a);
        let far = Ipv4([203, 0, 113, 9]);
        // What a router sends: type 3, code 1 (host unreachable), quoting our packet's header.
        let quoted = build_ipv4(IP_A, far, PROTO_ICMP, 63, 9, &[8, 0, 0, 0, 0, 0, 0, 0]);
        let icmp = build_icmp(ICMP_UNREACHABLE, 1, [0; 4], &quoted);
        let frame = build_eth(&MAC_A, &MAC_B, ETHERTYPE_IPV4, &build_ipv4(GW, IP_A, PROTO_ICMP, 64, 1, &icmp));
        a.receive(&frame, 0);
        assert_eq!(a.take_event(), Some(Event::Unreachable { dst: far, reason: Reason::Icmp(1) }));
    }

    #[test]
    fn tables_are_bounded() {
        let mut a = stack(MAC_A, IP_A);
        drain(&mut a);
        // More neighbours than the cache holds: the oldest are replaced.
        for i in 0..100u8 {
            let mac = [2, 0, 0, 0, 1, i];
            let ip = Ipv4([10, 0, 0, 100 + (i % 150)]);
            let req = build_eth(&BROADCAST_MAC, &mac, ETHERTYPE_ARP,
                &build_arp(&Arp { op: ARP_REQUEST, sender_mac: mac, sender_ip: ip, target_mac: [0; 6], target_ip: IP_A }));
            a.receive(&req, i as u64);
            drain(&mut a);
        }
        assert_eq!(a.arp_table().len(), ARP_CACHE_SIZE);
        assert!(a.arp_table().iter().all(|e| e.updated_ms >= 100 - ARP_CACHE_SIZE as u64));
        // More packets than the waiting queue holds.
        let results: Vec<_> = (0..40u8).map(|i| a.ping(Ipv4([10, 0, 0, 20 + i]), 1, 1, b"", 200)).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), MAX_PENDING);
        assert_eq!(results[MAX_PENDING], Err(Error::QueueFull));
        // An entry that is not refreshed expires.
        let mut b = stack(MAC_B, IP_B);
        let mut a = stack(MAC_A, IP_A);
        exchange(&mut a, &mut b, 0);
        a.ping(IP_B, 1, 1, b"", 0).unwrap();
        exchange(&mut a, &mut b, 0);
        a.ping(IP_B, 1, 2, b"", ARP_LIFETIME_MS + 1).unwrap();
        assert!(a.resolving(), "expired entry must be looked up again");
    }

    #[test]
    fn ignores_spoofed_or_conflicting_arp() {
        let mut a = stack(MAC_A, IP_A);
        drain(&mut a);
        let arp = |sender_mac, sender_ip| build_eth(&MAC_A, &MAC_B, ETHERTYPE_ARP,
            &build_arp(&Arp { op: ARP_REPLY, sender_mac, sender_ip, target_mac: MAC_A, target_ip: IP_A }));
        a.receive(&arp(MAC_B, IP_A), 0); // someone claims our address
        assert_eq!(a.stats.address_conflicts, 1);
        a.receive(&arp(MAC_B, Ipv4([192, 168, 9, 9])), 0); // outside our subnet
        a.receive(&arp(MAC_B, Ipv4::UNSPECIFIED), 0);
        a.receive(&arp(BROADCAST_MAC, IP_B), 0);
        a.receive(&arp(MAC_B, Ipv4([10, 0, 0, 255])), 0); // the broadcast address
        assert!(a.arp_table().is_empty());
        assert!(drain(&mut a).is_empty());
    }
}
