//! DNS resolver: turn a name into an IPv4 address (A records over UDP).
//!
//! One question per lookup, sent to the configured server from a random port
//! with a random ID; asked again after 1 and 3 seconds, given up after 5.
//! Answers must come from that server, to that port, with that ID and the
//! same question, or they are ignored. Results are cached for their
//! time-to-live (between 5 seconds and one hour).
//!
//! Not done: DNSSEC, TCP for oversized answers, IPv6 (AAAA) records.

use crate::stack::{Error, Event, Stack};
use crate::Ipv4;
use alloc::string::String;
use alloc::vec::Vec;

pub const PORT: u16 = 53;
const MAX_QUERIES: usize = 4;
const CACHE_SIZE: usize = 16;
/// Elapsed time at which the question is sent again.
const RESEND_AT_MS: [u64; 2] = [1_000, 3_000];
const TIMEOUT_MS: u64 = 5_000;
const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsError {
    /// No DNS server is configured.
    NoServer,
    BadName,
    /// The server says the name does not exist.
    NotFound,
    /// The server could not answer (its error code).
    ServerFailure(u8),
    /// The name exists but has no IPv4 address.
    NoAddress,
    Timeout,
    /// Too many lookups at once.
    Busy,
    /// The question could not be sent.
    Network(Error),
}

impl DnsError {
    pub fn message(&self) -> &'static str {
        match self {
            DnsError::NoServer => "no DNS server configured (net dns ADDRESS)",
            DnsError::BadName => "not a valid host name",
            DnsError::NotFound => "no such name",
            DnsError::ServerFailure(_) => "the DNS server could not answer",
            DnsError::NoAddress => "the name has no IPv4 address",
            DnsError::Timeout => "no answer from the DNS server",
            DnsError::Busy => "too many name lookups at once",
            DnsError::Network(e) => e.message(),
        }
    }
}

/// The result of starting a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// Known already (from the cache).
    Ready(Ipv4),
    /// Asked; an `Event::Dns` with this handle follows.
    Pending(u32),
}

struct Query {
    handle: u32,
    txid: u16,
    port: u16,
    name: String,
    started: u64,
    sends: u8,
}

#[derive(Default)]
pub(crate) struct Dns {
    server: Option<(Ipv4, u16)>,
    queries: Vec<Query>,
    /// (name, address, expiry time in ms)
    cache: Vec<(String, Ipv4, u64)>,
    next_handle: u32,
}

/// A host name we are willing to look up: labels of letters, digits, '-' and
/// '_' (1-63 characters each), at most 253 characters, no empty labels.
pub fn is_valid_name(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

fn normalize(name: &str) -> String {
    name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase()
}

fn build_query(txid: u16, name: &str) -> Vec<u8> {
    let mut q = Vec::with_capacity(18 + name.len());
    q.extend_from_slice(&txid.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // a standard query, recursion desired
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // one question
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&TYPE_A.to_be_bytes());
    q.extend_from_slice(&CLASS_IN.to_be_bytes());
    q
}

fn be16(d: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(off)?, *d.get(off + 1)?]))
}

/// Skip over a (possibly compressed) name; returns the offset after it.
fn skip_name(d: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *d.get(pos)?;
        match len & 0xC0 {
            0xC0 => return (pos + 2 <= d.len()).then_some(pos + 2), // a pointer ends the name
            0 if len == 0 => return Some(pos + 1),
            0 => pos += 1 + len as usize,
            _ => return None,
        }
    }
}

/// Does the name at `pos` (uncompressed, as in a question) equal `name`?
fn question_matches(d: &[u8], mut pos: usize, name: &str) -> Option<usize> {
    for label in name.split('.') {
        let len = *d.get(pos)? as usize;
        let got = d.get(pos + 1..pos + 1 + len)?;
        if len != label.len() || !got.eq_ignore_ascii_case(label.as_bytes()) {
            return None;
        }
        pos += 1 + len;
    }
    (*d.get(pos)? == 0).then_some(pos + 1)
}

/// Parse an answer to our question. None: not a valid answer to this
/// question (keep waiting). Some: the final result, with the time-to-live.
fn parse_response(d: &[u8], txid: u16, name: &str) -> Option<Result<(Ipv4, u32), DnsError>> {
    if be16(d, 0)? != txid {
        return None;
    }
    let flags = be16(d, 2)?;
    if flags & 0x8000 == 0 || flags & 0x7800 != 0 || be16(d, 4)? != 1 {
        return None; // not a response to a standard query with one question
    }
    let after_name = question_matches(d, 12, name)?;
    if be16(d, after_name)? != TYPE_A || be16(d, after_name + 2)? != CLASS_IN {
        return None;
    }
    match (flags & 0x000F) as u8 {
        0 => {}
        3 => return Some(Err(DnsError::NotFound)),
        code => return Some(Err(DnsError::ServerFailure(code))),
    }
    let answers = be16(d, 6)?.min(64);
    let mut pos = after_name + 4;
    for _ in 0..answers {
        // A cut-off record list ends the search: use what we have (nothing).
        let Some(p) = skip_name(d, pos) else { break };
        let (Some(rtype), Some(class), Some(len)) = (be16(d, p), be16(d, p + 2), be16(d, p + 8)) else { break };
        let Some(data) = d.get(p + 10..p + 10 + len as usize) else { break };
        if rtype == TYPE_A && class == CLASS_IN && len == 4 {
            let ttl = u32::from_be_bytes([d[p + 4], d[p + 5], d[p + 6], d[p + 7]]);
            return Some(Ok((Ipv4([data[0], data[1], data[2], data[3]]), ttl)));
        }
        pos = p + 10 + len as usize; // e.g. a CNAME: the address follows
    }
    Some(Err(DnsError::NoAddress))
}

impl Stack {
    /// Set the DNS server (address and port, normally 53), or None.
    pub fn set_dns(&mut self, server: Option<(Ipv4, u16)>) {
        if self.dns.server != server {
            self.dns.cache.clear();
        }
        self.dns.server = server;
    }

    pub fn dns_server(&self) -> Option<(Ipv4, u16)> {
        self.dns.server
    }

    /// Look up a host name. `Ready` if cached; otherwise `Pending(handle)` and
    /// an `Event::Dns` with that handle later.
    pub fn dns_lookup(&mut self, name: &str, now_ms: u64) -> Result<Lookup, DnsError> {
        if !is_valid_name(name) {
            return Err(DnsError::BadName);
        }
        let name = normalize(name);
        self.dns.cache.retain(|(_, _, expires)| *expires > now_ms);
        if let Some((_, ip, _)) = self.dns.cache.iter().find(|(n, _, _)| *n == name) {
            return Ok(Lookup::Ready(*ip));
        }
        let (server, server_port) = self.dns.server.ok_or(DnsError::NoServer)?;
        if self.dns.queries.len() >= MAX_QUERIES {
            return Err(DnsError::Busy);
        }
        let port = loop {
            let p = 49152 + (self.random() % 16384) as u16;
            if !self.dns.queries.iter().any(|q| q.port == p) {
                break p;
            }
        };
        let txid = self.random() as u16;
        self.udp_send(port, server, server_port, &build_query(txid, &name), now_ms).map_err(DnsError::Network)?;
        self.dns.next_handle = self.dns.next_handle.wrapping_add(1);
        let handle = self.dns.next_handle;
        self.dns.queries.push(Query { handle, txid, port, name, started: now_ms, sends: 1 });
        Ok(Lookup::Pending(handle))
    }

    pub(crate) fn dns_poll(&mut self, now_ms: u64) {
        let Some((server, server_port)) = self.dns.server else { return };
        let mut i = 0;
        while i < self.dns.queries.len() {
            let q = &self.dns.queries[i];
            let elapsed = now_ms.saturating_sub(q.started);
            if elapsed >= TIMEOUT_MS {
                let handle = self.dns.queries.remove(i).handle;
                self.event(Event::Dns { handle, result: Err(DnsError::Timeout) });
                continue;
            }
            if (q.sends as usize) <= RESEND_AT_MS.len() && elapsed >= RESEND_AT_MS[q.sends as usize - 1] {
                let (port, query) = (q.port, build_query(q.txid, &q.name));
                self.dns.queries[i].sends += 1;
                let _ = self.udp_send(port, server, server_port, &query, now_ms);
            }
            i += 1;
        }
    }

    /// A UDP datagram arrived. True if its port belongs to a lookup (whether
    /// or not the datagram was a valid answer).
    pub(crate) fn dns_receive(&mut self, src: Ipv4, src_port: u16, dst_port: u16, data: &[u8], now_ms: u64) -> bool {
        let Some(i) = self.dns.queries.iter().position(|q| q.port == dst_port) else { return false };
        if self.dns.server != Some((src, src_port)) {
            return true; // not from the server we asked
        }
        let q = &self.dns.queries[i];
        let Some(result) = parse_response(data, q.txid, &q.name) else { return true };
        let q = self.dns.queries.remove(i);
        let result = result.map(|(ip, ttl)| {
            if self.dns.cache.len() >= CACHE_SIZE {
                self.dns.cache.remove(0);
            }
            self.dns.cache.push((q.name, ip, now_ms + ttl.clamp(5, 3600) as u64 * 1000));
            ip
        });
        self.event(Event::Dns { handle: q.handle, result });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::Config;
    use crate::Mac;

    const MAC_A: Mac = [0x52, 0x54, 0x00, 0xbe, 0xe0, 0x01];
    const MAC_S: Mac = [0x02, 0, 0, 0, 0, 0x53];
    const IP_A: Ipv4 = Ipv4([10, 0, 0, 2]);
    const SERVER: Ipv4 = Ipv4([10, 0, 0, 53]);
    const ADDR: Ipv4 = Ipv4([93, 184, 216, 34]);

    /// A client and a "server" stack that listens on port 53.
    fn pair() -> (Stack, Stack) {
        let mut a = Stack::new(MAC_A, 0xBEE);
        let mut s = Stack::new(MAC_S, 0x5E7);
        a.set_config(Some(Config { ip: IP_A, prefix: 24, gateway: None }));
        s.set_config(Some(Config { ip: SERVER, prefix: 24, gateway: None }));
        a.set_dns(Some((SERVER, PORT)));
        s.udp_open(Some(PORT)).unwrap();
        exchange(&mut a, &mut s, 0);
        (a, s)
    }

    fn exchange(a: &mut Stack, b: &mut Stack, now: u64) {
        loop {
            let fa: Vec<_> = core::iter::from_fn(|| a.transmit()).collect();
            let fb: Vec<_> = core::iter::from_fn(|| b.transmit()).collect();
            if fa.is_empty() && fb.is_empty() {
                return;
            }
            fa.iter().for_each(|f| b.receive(f, now));
            fb.iter().for_each(|f| a.receive(f, now));
        }
    }

    /// The server's answer to `query`: the question echoed, then `records`
    /// (type, data), each named with a compression pointer to the question.
    fn answer(query: &[u8], rcode: u8, records: &[(u16, &[u8])]) -> Vec<u8> {
        let mut r = query.to_vec();
        r[2] = 0x81;
        r[3] = 0x80 | rcode;
        r[6..8].copy_from_slice(&(records.len() as u16).to_be_bytes());
        for (rtype, data) in records {
            r.extend_from_slice(&[0xC0, 0x0C]);
            r.extend_from_slice(&rtype.to_be_bytes());
            r.extend_from_slice(&[0, 1, 0, 0, 0, 60]); // class IN, TTL 60 s
            r.extend_from_slice(&(data.len() as u16).to_be_bytes());
            r.extend_from_slice(data);
        }
        r
    }

    /// Start a lookup and return (handle, the query as the server received it, client port).
    fn ask(a: &mut Stack, s: &mut Stack, name: &str, now: u64) -> (u32, Vec<u8>, u16) {
        let Ok(Lookup::Pending(handle)) = a.dns_lookup(name, now) else { panic!("expected a pending lookup") };
        exchange(a, s, now);
        let (from, port, query) = s.udp_recv(PORT).expect("the server got the question");
        assert_eq!(from, IP_A);
        (handle, query, port)
    }

    fn reply(a: &mut Stack, s: &mut Stack, port: u16, data: &[u8], now: u64) {
        s.udp_send(PORT, IP_A, port, data, now).unwrap();
        exchange(a, s, now);
    }

    #[test]
    fn names() {
        for good in ["example.com", "a.b-c.d_e.org", "localhost", "EXAMPLE.com.", "xn--bcher-kva.example", "1.2.3.4.in-addr.arpa"] {
            assert!(is_valid_name(good), "{good}");
        }
        let long_label = "a".repeat(64);
        let long_name = ["abcdefgh"; 32].join(".");
        for bad in ["", ".", "a..b", "-a.com", "a-.com", "exa mple.com", "ex/ample.com", "bücher.de", long_label.as_str(), long_name.as_str()] {
            assert!(!is_valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn query_bytes() {
        // Byte-exact: ID, flags (recursion desired), one question, labels, type A, class IN.
        assert_eq!(build_query(0x1234, "bee.test"),
            [0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 3, b'b', b'e', b'e', 4, b't', b'e', b's', b't', 0, 0, 1, 0, 1]);
    }

    #[test]
    fn lookup_answer_and_cache() {
        let (mut a, mut s) = pair();
        let (handle, query, port) = ask(&mut a, &mut s, "Example.COM.", 100);
        assert!(port >= 49152);
        assert_eq!(&query[12..], &build_query(0, "example.com")[12..]); // lower-cased, no trailing dot
        reply(&mut a, &mut s, port, &answer(&query, 0, &[(TYPE_A, &ADDR.0)]), 150);
        assert_eq!(a.take_event(), Some(Event::Dns { handle, result: Ok(ADDR) }));
        // Cached for the record's lifetime (60 s): no packet is sent.
        assert_eq!(a.dns_lookup("example.com", 30_000), Ok(Lookup::Ready(ADDR)));
        assert!(a.transmit().is_none());
        assert!(matches!(a.dns_lookup("example.com", 61_000), Ok(Lookup::Pending(_))));
    }

    #[test]
    fn cname_then_address() {
        let (mut a, mut s) = pair();
        let (handle, query, port) = ask(&mut a, &mut s, "www.bee.test", 0);
        // www.bee.test is an alias (CNAME, type 5) for another name, whose address follows.
        let alias: &[u8] = &[4, b'h', b'o', b's', b't', 0xC0, 0x10];
        reply(&mut a, &mut s, port, &answer(&query, 0, &[(5, alias), (TYPE_A, &ADDR.0)]), 1);
        assert_eq!(a.take_event(), Some(Event::Dns { handle, result: Ok(ADDR) }));
    }

    #[test]
    fn errors_from_the_server() {
        let (mut a, mut s) = pair();
        let (handle, query, port) = ask(&mut a, &mut s, "nope.test", 0);
        reply(&mut a, &mut s, port, &answer(&query, 3, &[]), 1);
        assert_eq!(a.take_event(), Some(Event::Dns { handle, result: Err(DnsError::NotFound) }));
        let (handle, query, port) = ask(&mut a, &mut s, "broken.test", 0);
        reply(&mut a, &mut s, port, &answer(&query, 2, &[]), 1);
        assert_eq!(a.take_event(), Some(Event::Dns { handle, result: Err(DnsError::ServerFailure(2)) }));
        // Only an IPv6 (AAAA, type 28) record: no IPv4 address.
        let (handle, query, port) = ask(&mut a, &mut s, "v6only.test", 0);
        reply(&mut a, &mut s, port, &answer(&query, 0, &[(28, &[0; 16])]), 1);
        assert_eq!(a.take_event(), Some(Event::Dns { handle, result: Err(DnsError::NoAddress) }));
        // Errors are not cached.
        assert!(matches!(a.dns_lookup("nope.test", 2), Ok(Lookup::Pending(_))));
    }

    #[test]
    fn forged_or_broken_answers_are_ignored() {
        let (mut a, mut s) = pair();
        let (handle, query, port) = ask(&mut a, &mut s, "bee.test", 0);
        let good = answer(&query, 0, &[(TYPE_A, &ADDR.0)]);
        let evil = Ipv4([6, 6, 6, 6]);

        let mut wrong_id = answer(&query, 0, &[(TYPE_A, &evil.0)]);
        wrong_id[1] ^= 1;
        reply(&mut a, &mut s, port, &wrong_id, 1);
        let mut other_question = answer(&query, 0, &[(TYPE_A, &evil.0)]);
        other_question[13] = b'x'; // "xee.test"
        reply(&mut a, &mut s, port, &other_question, 1);
        let mut not_a_response = answer(&query, 0, &[(TYPE_A, &evil.0)]);
        not_a_response[2] = 0x01;
        reply(&mut a, &mut s, port, &not_a_response, 1);
        // From the right server but the wrong port, and from another machine.
        s.udp_open(Some(5353)).unwrap();
        s.udp_send(5353, IP_A, port, &answer(&query, 0, &[(TYPE_A, &evil.0)]), 1).unwrap();
        exchange(&mut a, &mut s, 1);
        let mut other = Stack::new([2, 0, 0, 0, 0, 0x66], 1);
        other.set_config(Some(Config { ip: Ipv4([10, 0, 0, 66]), prefix: 24, gateway: None }));
        other.udp_send(PORT, IP_A, port, &answer(&query, 0, &[(TYPE_A, &evil.0)]), 1).unwrap();
        exchange(&mut a, &mut other, 1);
        // Cut off at every length, and a compression pointer loop.
        for len in 0..good.len() {
            reply(&mut a, &mut s, port, &good[..len], 1);
            match a.take_event() {
                None | Some(Event::Dns { result: Err(DnsError::NoAddress), .. }) => {}
                other => panic!("cut at {len}: {other:?}"),
            }
            if a.dns.queries.is_empty() {
                // A cut inside the answer section ends that lookup: ask again.
                let (h, q, p) = ask(&mut a, &mut s, "bee.test", 1);
                assert_eq!((q.len(), p >= 49152, h > handle), (query.len(), true, true));
                return forged_tail(a, s, h, q, p);
            }
        }
        unreachable!("the truncated answers above end the lookup at some length");
    }

    fn forged_tail(mut a: Stack, mut s: Stack, handle: u32, query: Vec<u8>, port: u16) {
        let mut looped = answer(&query, 0, &[(TYPE_A, &ADDR.0)]);
        let n = query.len();
        looped[n] = 0xC0;
        looped[n + 1] = n as u8; // the record's name points at itself
        reply(&mut a, &mut s, port, &looped, 2);
        // A pointer is never followed (only skipped), so this is simply an answer.
        assert_eq!(a.take_event(), Some(Event::Dns { handle, result: Ok(ADDR) }));
        assert_eq!(a.dns_lookup("bee.test", 3), Ok(Lookup::Ready(ADDR)));
    }

    #[test]
    fn retries_then_times_out() {
        let (mut a, mut s) = pair();
        let Ok(Lookup::Pending(handle)) = a.dns_lookup("slow.test", 0) else { panic!() };
        let mut asked_at = Vec::new();
        for now in (0..=6_000).step_by(100) {
            a.poll(now);
            exchange(&mut a, &mut s, now);
            while s.udp_recv(PORT).is_some() {
                asked_at.push(now);
            }
            if let Some(e) = a.take_event() {
                assert_eq!(e, Event::Dns { handle, result: Err(DnsError::Timeout) });
                asked_at.push(now + 1); // marker
                break;
            }
        }
        assert_eq!(asked_at, [0, 1_000, 3_000, 5_001]);
        assert!(a.dns.queries.is_empty());
    }

    #[test]
    fn limits_and_missing_settings() {
        let (mut a, _s) = pair();
        assert_eq!(a.dns_lookup("bad name", 0), Err(DnsError::BadName));
        for i in 0..MAX_QUERIES {
            assert!(matches!(a.dns_lookup(&format!("n{i}.test"), 0), Ok(Lookup::Pending(_))));
        }
        assert_eq!(a.dns_lookup("one-too-many.test", 0), Err(DnsError::Busy));
        let mut b = Stack::new(MAC_A, 1);
        b.set_config(Some(Config { ip: IP_A, prefix: 24, gateway: None }));
        assert_eq!(b.dns_lookup("bee.test", 0), Err(DnsError::NoServer));
        let mut c = Stack::new(MAC_A, 1);
        c.set_dns(Some((SERVER, PORT)));
        assert_eq!(c.dns_lookup("bee.test", 0), Err(DnsError::Network(Error::NoAddress)));
        // Changing the server forgets cached answers from the old one.
        let (mut a, mut s) = pair();
        let (_, query, port) = ask(&mut a, &mut s, "bee.test", 0);
        reply(&mut a, &mut s, port, &answer(&query, 0, &[(TYPE_A, &ADDR.0)]), 1);
        a.set_dns(Some((Ipv4([10, 0, 0, 54]), PORT)));
        assert!(matches!(a.dns_lookup("bee.test", 2), Ok(Lookup::Pending(_))));
    }
}
