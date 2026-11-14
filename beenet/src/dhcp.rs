//! DHCP client (RFC 2131): gets the address, subnet, gateway and DNS server
//! from the network.
//!
//!   DISCOVER (broadcast) -> OFFER -> REQUEST (broadcast) -> ACK: address in use
//!
//! Half-way through the lease the client renews it with the server directly;
//! when a lease runs out the address is dropped and the search starts again.
//! Without an answer it retries after 2, 4 and 8 seconds, reports failure,
//! and then tries again once a minute (a cable may be plugged in later).

use crate::stack::{Config, Stack};
use crate::{netmask, Ipv4, Mac};
use alloc::vec::Vec;

pub const CLIENT_PORT: u16 = 68;
pub const SERVER_PORT: u16 = 67;

const MAGIC: [u8; 4] = [99, 130, 83, 99];
const BOOTP_LEN: usize = 236;
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

/// Waiting time after the 1st, 2nd, ... transmission of DISCOVER.
const DISCOVER_WAIT_MS: [u64; 4] = [2_000, 4_000, 8_000, 8_000];
const REQUEST_WAIT_MS: u64 = 2_000;
const REQUEST_TRIES: u8 = 3;
const BACKOFF_MS: u64 = 60_000;
const RENEW_RETRY_MS: u64 = 30_000;
const DEFAULT_LEASE_SECS: u32 = 3_600;
const INFINITE: u32 = u32::MAX;

/// What the DHCP server gave us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    pub ip: Ipv4,
    pub prefix: u8,
    pub gateway: Option<Ipv4>,
    pub dns: Option<Ipv4>,
    pub server: Ipv4,
    pub lease_secs: u32,
    pub obtained_ms: u64,
}

impl Lease {
    /// When the lease ends (None: it never does).
    pub fn expires_ms(&self) -> Option<u64> {
        (self.lease_secs != INFINITE).then(|| self.obtained_ms + self.lease_secs as u64 * 1000)
    }

    fn renew_ms(&self) -> Option<u64> {
        (self.lease_secs != INFINITE).then(|| self.obtained_ms + self.lease_secs as u64 * 500)
    }

    fn config(&self) -> Config {
        Config { ip: self.ip, prefix: self.prefix, gateway: self.gateway }
    }
}

/// Changes of the automatic address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// An address was obtained and is in use.
    Bound(Lease),
    /// No DHCP server answered (the search continues once a minute).
    Failed,
    /// The lease ended or was withdrawn; the address was removed.
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Off,
    Discover { tries: u8, last_tx: u64 },
    Request { tries: u8, last_tx: u64, ip: Ipv4, server: Ipv4 },
    Bound { lease: Lease, last_renew_tx: Option<u64> },
    Backoff { until: u64 },
}

#[derive(Default)]
pub(crate) struct Dhcp {
    state: State,
    xid: u32,
}

struct Reply {
    kind: u8,
    your_ip: Ipv4,
    server: Option<Ipv4>,
    mask: Option<Ipv4>,
    router: Option<Ipv4>,
    dns: Option<Ipv4>,
    lease_secs: Option<u32>,
}

fn ip_at(d: &[u8], off: usize) -> Ipv4 {
    Ipv4([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

/// Parse a server message for this client and transaction. None for anything else.
fn parse(d: &[u8], xid: u32, mac: &Mac) -> Option<Reply> {
    if d.len() < BOOTP_LEN + 4 || d[0] != 2 || d[1] != 1 || d[2] != 6 {
        return None; // not a BOOTREPLY for Ethernet
    }
    if d[4..8] != xid.to_be_bytes() || d[28..34] != mac[..] || d[BOOTP_LEN..BOOTP_LEN + 4] != MAGIC {
        return None;
    }
    let mut r = Reply { kind: 0, your_ip: ip_at(d, 16), server: None, mask: None, router: None, dns: None, lease_secs: None };
    let mut i = BOOTP_LEN + 4;
    while i < d.len() {
        let code = d[i];
        if code == 255 {
            break;
        }
        if code == 0 {
            i += 1; // padding
            continue;
        }
        let len = *d.get(i + 1)? as usize;
        let value = d.get(i + 2..i + 2 + len)?; // a cut-off option makes the message invalid
        match (code, len) {
            (53, 1) => r.kind = value[0],
            (1, 4) => r.mask = Some(ip_at(value, 0)),
            (3, l) if l >= 4 => r.router = Some(ip_at(value, 0)),
            (6, l) if l >= 4 => r.dns = Some(ip_at(value, 0)),
            (51, 4) => r.lease_secs = Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]])),
            (54, 4) => r.server = Some(ip_at(value, 0)),
            _ => {}
        }
        i += 2 + len;
    }
    (r.kind != 0).then_some(r)
}

/// Build a client message.
fn build(kind: u8, xid: u32, mac: &Mac, client_ip: Ipv4, requested: Option<Ipv4>, server: Option<Ipv4>) -> Vec<u8> {
    let mut m = alloc::vec![0u8; BOOTP_LEN];
    m[0] = 1; // BOOTREQUEST
    m[1] = 1; // Ethernet
    m[2] = 6;
    m[4..8].copy_from_slice(&xid.to_be_bytes());
    if client_ip == Ipv4::UNSPECIFIED {
        m[10] = 0x80; // "broadcast" flag: we can't be reached by IP address yet
    }
    m[12..16].copy_from_slice(&client_ip.0);
    m[28..34].copy_from_slice(mac);
    m.extend_from_slice(&MAGIC);
    m.extend_from_slice(&[53, 1, kind]);
    m.extend_from_slice(&[61, 7, 1]); // client identifier: Ethernet + MAC
    m.extend_from_slice(mac);
    if let Some(ip) = requested {
        m.extend_from_slice(&[50, 4]);
        m.extend_from_slice(&ip.0);
    }
    if let Some(ip) = server {
        m.extend_from_slice(&[54, 4]);
        m.extend_from_slice(&ip.0);
    }
    let hostname = b"beecodersos";
    m.extend_from_slice(&[12, hostname.len() as u8]);
    m.extend_from_slice(hostname);
    m.extend_from_slice(&[55, 4, 1, 3, 6, 51]); // please send: subnet mask, router, DNS, lease time
    m.push(255);
    m.resize(m.len().max(300), 0); // some servers drop messages shorter than 300 bytes
    m
}

/// The prefix length of a subnet mask; 24 if the mask is missing or not a run of ones.
fn prefix_of(mask: Option<Ipv4>) -> u8 {
    match mask.map(|m| (m.to_u32().leading_ones() as u8, m)) {
        Some((p, m)) if (1..=30).contains(&p) && netmask(p) == m => p,
        _ => 24,
    }
}

impl Stack {
    /// Start (or restart) looking for an address. Any current address stays
    /// until the caller removes it or a new one is obtained.
    pub fn dhcp_start(&mut self, now_ms: u64) {
        self.dhcp.xid = self.random();
        self.dhcp.state = State::Discover { tries: 1, last_tx: now_ms };
        self.dhcp_send_discover();
    }

    /// Stop the client. The address it obtained (if any) stays configured.
    pub fn dhcp_stop(&mut self) {
        self.dhcp.state = State::Off;
    }

    pub fn dhcp_running(&self) -> bool {
        self.dhcp.state != State::Off
    }

    /// True while an address is being requested (no answer yet).
    pub fn dhcp_searching(&self) -> bool {
        matches!(self.dhcp.state, State::Discover { .. } | State::Request { .. })
    }

    pub fn dhcp_lease(&self) -> Option<Lease> {
        match self.dhcp.state {
            State::Bound { lease, .. } => Some(lease),
            _ => None,
        }
    }

    fn dhcp_send_discover(&mut self) {
        let msg = build(DISCOVER, self.dhcp.xid, &self.mac, Ipv4::UNSPECIFIED, None, None);
        self.udp_broadcast_unconfigured(CLIENT_PORT, SERVER_PORT, &msg);
    }

    fn dhcp_send_request(&mut self, ip: Ipv4, server: Ipv4) {
        let msg = build(REQUEST, self.dhcp.xid, &self.mac, Ipv4::UNSPECIFIED, Some(ip), Some(server));
        self.udp_broadcast_unconfigured(CLIENT_PORT, SERVER_PORT, &msg);
    }

    fn dhcp_lose_address(&mut self, now_ms: u64) {
        self.set_config(None);
        self.notices.push_back(Notice::Lost);
        self.dhcp_start(now_ms);
    }

    pub(crate) fn dhcp_poll(&mut self, now_ms: u64) {
        match self.dhcp.state {
            State::Off => {}
            State::Discover { tries, last_tx } => {
                if now_ms.saturating_sub(last_tx) < DISCOVER_WAIT_MS[(tries as usize - 1).min(3)] {
                    return;
                }
                if tries as usize >= DISCOVER_WAIT_MS.len() {
                    self.notices.push_back(Notice::Failed);
                    self.dhcp.state = State::Backoff { until: now_ms + BACKOFF_MS };
                } else {
                    self.dhcp.state = State::Discover { tries: tries + 1, last_tx: now_ms };
                    self.dhcp_send_discover();
                }
            }
            State::Request { tries, last_tx, ip, server } => {
                if now_ms.saturating_sub(last_tx) < REQUEST_WAIT_MS {
                    return;
                }
                if tries >= REQUEST_TRIES {
                    self.dhcp_start(now_ms); // the server went quiet: start over
                } else {
                    self.dhcp.state = State::Request { tries: tries + 1, last_tx: now_ms, ip, server };
                    self.dhcp_send_request(ip, server);
                }
            }
            State::Backoff { until } => {
                if now_ms >= until {
                    self.dhcp_start(now_ms);
                }
            }
            State::Bound { lease, last_renew_tx } => {
                if lease.expires_ms().is_some_and(|t| now_ms >= t) {
                    return self.dhcp_lose_address(now_ms);
                }
                let due = lease.renew_ms().is_some_and(|t| now_ms >= t);
                if due && last_renew_tx.is_none_or(|t| now_ms.saturating_sub(t) >= RENEW_RETRY_MS) {
                    // Renew with the server directly, from our address.
                    let msg = build(REQUEST, self.dhcp.xid, &self.mac, lease.ip, None, None);
                    let _ = self.udp_send(CLIENT_PORT, lease.server, SERVER_PORT, &msg, now_ms);
                    self.dhcp.state = State::Bound { lease, last_renew_tx: Some(now_ms) };
                }
            }
        }
    }

    fn dhcp_lease_from(&self, r: &Reply, server: Ipv4, now_ms: u64) -> Option<Lease> {
        let ip = r.your_ip;
        let prefix = prefix_of(r.mask);
        let mask = netmask(prefix).to_u32();
        let host = ip.to_u32() & !mask;
        if host == 0 || host == !mask {
            return None; // not a usable host address
        }
        // A gateway outside our subnet could never be reached: ignore it.
        let gateway = r.router.filter(|g| *g != ip && g.to_u32() & mask == ip.to_u32() & mask);
        let dns = r.dns.filter(|d| *d != Ipv4::UNSPECIFIED && *d != Ipv4::BROADCAST);
        let lease_secs = r.lease_secs.unwrap_or(DEFAULT_LEASE_SECS).max(60);
        Some(Lease { ip, prefix, gateway, dns, server, lease_secs, obtained_ms: now_ms })
    }

    /// A UDP datagram arrived on the DHCP client port.
    pub(crate) fn dhcp_receive(&mut self, data: &[u8], now_ms: u64) {
        let Some(r) = parse(data, self.dhcp.xid, &self.mac) else { return };
        match self.dhcp.state {
            State::Discover { .. } if r.kind == OFFER => {
                let Some(server) = r.server else { return };
                if r.your_ip == Ipv4::UNSPECIFIED || r.your_ip == Ipv4::BROADCAST {
                    return;
                }
                self.dhcp.state = State::Request { tries: 1, last_tx: now_ms, ip: r.your_ip, server };
                self.dhcp_send_request(r.your_ip, server);
            }
            State::Request { ip, server, .. } if r.kind == ACK && r.your_ip == ip => {
                let Some(lease) = self.dhcp_lease_from(&r, server, now_ms) else {
                    return self.dhcp_start(now_ms);
                };
                self.dhcp.state = State::Bound { lease, last_renew_tx: None };
                if self.config != Some(lease.config()) {
                    self.set_config(Some(lease.config()));
                }
                self.notices.push_back(Notice::Bound(lease));
            }
            State::Request { .. } if r.kind == NAK => self.dhcp_start(now_ms),
            State::Bound { lease, .. } if r.kind == ACK && r.your_ip == lease.ip => {
                // Renewed: keep the address, restart the clock.
                if let Some(new) = self.dhcp_lease_from(&r, r.server.unwrap_or(lease.server), now_ms) {
                    self.dhcp.state = State::Bound { lease: new, last_renew_tx: None };
                }
            }
            State::Bound { last_renew_tx: Some(_), .. } if r.kind == NAK => self.dhcp_lose_address(now_ms),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::*;
    use crate::BROADCAST_MAC;

    const MAC: Mac = [0x52, 0x54, 0x00, 0xbe, 0xe0, 0x01];
    const SERVER_MAC: Mac = [0x02, 0, 0, 0, 0, 0x01];
    const SERVER: Ipv4 = Ipv4([192, 168, 122, 1]);
    const OFFERED: Ipv4 = Ipv4([192, 168, 122, 57]);

    fn drain(s: &mut Stack) -> Vec<Vec<u8>> {
        core::iter::from_fn(|| s.transmit()).collect()
    }

    /// What a client message says: (message type, xid, requested IP, server id, src IP, dst IP, dst MAC).
    fn client_message(frame: &[u8]) -> (u8, u32, Option<Ipv4>, Option<Ipv4>, Ipv4, Ipv4, Mac) {
        let (eth, payload) = parse_eth(frame).unwrap();
        let (ip, data) = parse_ipv4(payload).unwrap();
        let udp = parse_udp(data, ip.src, ip.dst).unwrap();
        assert_eq!((udp.src_port, udp.dst_port), (68, 67));
        let d = udp.data;
        assert!(d.len() >= 300);
        assert_eq!((d[0], d[1], d[2]), (1, 1, 6));
        assert_eq!(&d[28..34], &MAC);
        assert_eq!(&d[236..240], &MAGIC);
        let (mut kind, mut requested, mut server) = (0, None, None);
        let mut i = 240;
        while d[i] != 255 {
            let (code, len) = (d[i], d[i + 1] as usize);
            match code {
                53 => kind = d[i + 2],
                50 => requested = Some(ip_at(d, i + 2)),
                54 => server = Some(ip_at(d, i + 2)),
                _ => {}
            }
            i += 2 + len;
        }
        (kind, u32::from_be_bytes([d[4], d[5], d[6], d[7]]), requested, server, ip.src, ip.dst, eth.dst)
    }

    /// A server message, built by hand (not with the client's builder).
    fn server_message(kind: u8, xid: u32, your_ip: Ipv4, options: &[(u8, &[u8])]) -> Vec<u8> {
        let mut m = vec![0u8; 236];
        m[0] = 2;
        m[1] = 1;
        m[2] = 6;
        m[4..8].copy_from_slice(&xid.to_be_bytes());
        m[16..20].copy_from_slice(&your_ip.0);
        m[28..34].copy_from_slice(&MAC);
        m.extend_from_slice(&MAGIC);
        m.extend_from_slice(&[53, 1, kind]);
        for (code, value) in options {
            m.push(*code);
            m.push(value.len() as u8);
            m.extend_from_slice(value);
        }
        m.push(255);
        m
    }

    fn frame(message: &[u8], dst_ip: Ipv4) -> Vec<u8> {
        let udp = build_udp(SERVER, dst_ip, 67, 68, message);
        build_eth(&MAC, &SERVER_MAC, ETHERTYPE_IPV4, &build_ipv4(SERVER, dst_ip, PROTO_UDP, 64, 1, &udp))
    }

    const STANDARD: &[(u8, &[u8])] = &[
        (54, &[192, 168, 122, 1]),
        (51, &[0, 0, 14, 16]), // 3600 s
        (1, &[255, 255, 255, 0]),
        (3, &[192, 168, 122, 1]),
        (6, &[192, 168, 122, 1, 8, 8, 8, 8]), // two DNS servers: the first is used
    ];

    /// Run the handshake; returns the stack and the transaction ID.
    fn bound(options: &[(u8, &[u8])]) -> (Stack, u32) {
        let mut s = Stack::new(MAC, 0x1234_5678);
        s.dhcp_start(0);
        let (_, xid, ..) = client_message(&drain(&mut s)[0]);
        s.receive(&frame(&server_message(OFFER, xid, OFFERED, options), OFFERED), 10);
        drain(&mut s);
        s.receive(&frame(&server_message(ACK, xid, OFFERED, options), OFFERED), 20);
        (s, xid)
    }

    #[test]
    fn full_handshake() {
        let mut s = Stack::new(MAC, 0x1234_5678);
        s.dhcp_start(0);
        assert!(s.dhcp_searching());
        let out = drain(&mut s);
        assert_eq!(out.len(), 1);
        let (kind, xid, requested, server, src, dst, dst_mac) = client_message(&out[0]);
        assert_eq!((kind, requested, server), (DISCOVER, None, None));
        assert_eq!((src, dst, dst_mac), (Ipv4::UNSPECIFIED, Ipv4::BROADCAST, BROADCAST_MAC));

        // The offer is sent to the offered address, which is not ours yet: still accepted.
        s.receive(&frame(&server_message(OFFER, xid, OFFERED, STANDARD), OFFERED), 10);
        let out = drain(&mut s);
        let (kind, xid2, requested, server, src, ..) = client_message(&out[0]);
        assert_eq!((kind, xid2, requested, server, src), (REQUEST, xid, Some(OFFERED), Some(SERVER), Ipv4::UNSPECIFIED));
        assert_eq!(s.config(), None, "the address is not used before the server confirms it");

        s.receive(&frame(&server_message(ACK, xid, OFFERED, STANDARD), OFFERED), 20);
        let lease = Lease { ip: OFFERED, prefix: 24, gateway: Some(SERVER), dns: Some(SERVER), server: SERVER, lease_secs: 3600, obtained_ms: 20 };
        assert_eq!(s.take_notice(), Some(Notice::Bound(lease)));
        assert_eq!(s.dhcp_lease(), Some(lease));
        assert_eq!(s.config(), Some(Config { ip: OFFERED, prefix: 24, gateway: Some(SERVER) }));
        assert!(!s.dhcp_searching());
        // The new address is announced with a gratuitous ARP.
        let out = drain(&mut s);
        assert_eq!(parse_arp(parse_eth(&out[0]).unwrap().1).unwrap().sender_ip, OFFERED);
    }

    #[test]
    fn retries_then_reports_failure_and_keeps_trying() {
        let mut s = Stack::new(MAC, 7);
        s.dhcp_start(0);
        let mut sent_at = vec![0u64];
        drain(&mut s);
        for now in (100..=30_000).step_by(100) {
            s.poll(now);
            if !drain(&mut s).is_empty() {
                sent_at.push(now);
            }
            if s.take_notice() == Some(Notice::Failed) {
                sent_at.push(now + 1); // marker
                break;
            }
        }
        assert_eq!(sent_at, [0, 2_000, 6_000, 14_000, 22_001]);
        assert!(!s.dhcp_searching() && s.dhcp_running());
        // A minute later it starts again by itself.
        s.poll(22_000 + BACKOFF_MS);
        assert_eq!(client_message(&drain(&mut s)[0]).0, DISCOVER);
    }

    #[test]
    fn ignores_messages_that_are_not_ours() {
        let mut s = Stack::new(MAC, 9);
        s.dhcp_start(0);
        let (_, xid, ..) = client_message(&drain(&mut s)[0]);
        let good = server_message(OFFER, xid, OFFERED, STANDARD);

        s.receive(&frame(&server_message(OFFER, xid ^ 1, OFFERED, STANDARD), OFFERED), 1); // another transaction
        let mut other_mac = good.clone();
        other_mac[33] ^= 1;
        s.receive(&frame(&other_mac, OFFERED), 1);
        let mut request = good.clone();
        request[0] = 1; // a client message, not a reply
        s.receive(&frame(&request, OFFERED), 1);
        s.receive(&frame(&server_message(OFFER, xid, OFFERED, &[]), OFFERED), 1); // no server identifier
        s.receive(&frame(&server_message(OFFER, xid, Ipv4::UNSPECIFIED, STANDARD), OFFERED), 1);
        s.receive(&frame(&server_message(ACK, xid, OFFERED, STANDARD), OFFERED), 1); // ACK without a request
        // Cut off anywhere before the server identifier is complete (header 236,
        // magic 4, message type 3, server id 6). A cut between two later options
        // leaves a valid, shorter message, so those lengths are not tested here.
        for len in 0..236 + 4 + 3 + 6 {
            s.receive(&frame(&good[..len], OFFERED), 1);
        }
        let mut cut_option = good.clone();
        let n = cut_option.len();
        cut_option[n - 1] = 6; // an option header instead of "end", with no value
        s.receive(&frame(&cut_option, OFFERED), 1);
        assert!(drain(&mut s).is_empty(), "no REQUEST may be sent for any of these");
        assert_eq!(s.config(), None);

        s.receive(&frame(&good, OFFERED), 2);
        assert_eq!(client_message(&drain(&mut s)[0]).0, REQUEST);
    }

    #[test]
    fn nak_starts_over() {
        let mut s = Stack::new(MAC, 11);
        s.dhcp_start(0);
        let (_, xid, ..) = client_message(&drain(&mut s)[0]);
        s.receive(&frame(&server_message(OFFER, xid, OFFERED, STANDARD), OFFERED), 10);
        drain(&mut s);
        s.receive(&frame(&server_message(NAK, xid, Ipv4::UNSPECIFIED, &[(54, &[192, 168, 122, 1])]), Ipv4::BROADCAST), 20);
        let (kind, new_xid, ..) = client_message(&drain(&mut s)[0]);
        assert_eq!(kind, DISCOVER);
        assert_ne!(new_xid, xid);
        assert_eq!(s.config(), None);
    }

    #[test]
    fn renews_half_way_and_loses_an_expired_lease() {
        let (mut s, xid) = bound(STANDARD);
        s.take_notice();
        drain(&mut s);
        s.poll(1_799_000);
        assert!(drain(&mut s).is_empty(), "too early to renew");
        s.poll(1_800_020);
        // The renewal goes to the server directly: first its MAC address is looked up.
        let out = drain(&mut s);
        assert_eq!(parse_arp(parse_eth(&out[0]).unwrap().1).unwrap().target_ip, SERVER);
        let arp = build_arp(&Arp { op: ARP_REPLY, sender_mac: SERVER_MAC, sender_ip: SERVER, target_mac: MAC, target_ip: OFFERED });
        s.receive(&build_eth(&MAC, &SERVER_MAC, ETHERTYPE_ARP, &arp), 1_800_030);
        let out = drain(&mut s);
        let (kind, xid2, requested, server, src, dst, dst_mac) = client_message(&out[0]);
        assert_eq!((kind, xid2, requested, server), (REQUEST, xid, None, None));
        assert_eq!((src, dst, dst_mac), (OFFERED, SERVER, SERVER_MAC));

        s.receive(&frame(&server_message(ACK, xid, OFFERED, STANDARD), OFFERED), 1_800_100);
        assert_eq!(s.dhcp_lease().unwrap().obtained_ms, 1_800_100);
        assert_eq!(s.take_notice(), None, "a renewal is silent");

        // No answer to later renewals (the server is gone: not even ARP is answered):
        // it keeps trying, keeps the address, and drops it when the lease ends.
        let renew = 1_800_100 + 1_800_000;
        for t in [0, 10_000, 30_000, 60_000] {
            s.poll(renew + t);
        }
        let out = drain(&mut s);
        assert!(out.len() >= 2);
        assert!(out.iter().all(|f| parse_arp(parse_eth(f).unwrap().1).unwrap().target_ip == SERVER));
        assert_eq!(s.config().map(|c| c.ip), Some(OFFERED));
        s.poll(1_800_100 + 3_600_000);
        assert_eq!(s.take_notice(), Some(Notice::Lost));
        assert_eq!(s.config(), None);
        // The search starts again (the last frame; an ARP retry may come before it).
        assert_eq!(client_message(drain(&mut s).last().unwrap()).0, DISCOVER);
    }

    #[test]
    fn odd_answers_are_handled() {
        // A /20 network; the router is outside it and is ignored; no DNS, no lease time.
        let (s, _) = bound(&[(54, &[192, 168, 122, 1]), (1, &[255, 255, 240, 0]), (3, &[10, 0, 0, 1])]);
        let lease = s.dhcp_lease().unwrap();
        assert_eq!((lease.prefix, lease.gateway, lease.dns, lease.lease_secs), (20, None, None, DEFAULT_LEASE_SECS));
        // A mask that is not a run of ones falls back to /24.
        let (s, _) = bound(&[(54, &[192, 168, 122, 1]), (1, &[255, 0, 255, 0])]);
        assert_eq!(s.dhcp_lease().unwrap().prefix, 24);
        // An infinite lease is never renewed.
        let (mut s, _) = bound(&[(54, &[192, 168, 122, 1]), (51, &[255, 255, 255, 255])]);
        drain(&mut s);
        s.poll(u64::MAX / 2);
        assert!(drain(&mut s).is_empty());
        assert!(s.dhcp_lease().is_some());
        // The broadcast address of the subnet is not a host address: start over.
        let mut s = Stack::new(MAC, 3);
        s.dhcp_start(0);
        let (_, xid, ..) = client_message(&drain(&mut s)[0]);
        let bad = Ipv4([192, 168, 122, 255]);
        s.receive(&frame(&server_message(OFFER, xid, bad, STANDARD), bad), 1);
        drain(&mut s);
        s.receive(&frame(&server_message(ACK, xid, bad, STANDARD), bad), 2);
        assert_eq!(s.config(), None);
        // dhcp_stop keeps the address but stops the client.
        let (mut s, _) = bound(STANDARD);
        s.dhcp_stop();
        assert!(!s.dhcp_running() && s.config().is_some() && s.dhcp_lease().is_none());
    }
}
