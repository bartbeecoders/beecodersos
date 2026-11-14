//! Networking in the kernel: the network card, the beenet protocol stack
//! (ARP, IPv4, ICMP) and what the `net`, `ping` and `arp` commands need.
//! See Docs/network-stack-plan.html (phases N1-N2).
//!
//! `poll()` moves frames between the card and the stack. It runs from the idle
//! loop (every 10 ms, so the OS answers pings at the prompt) and inside
//! waiting network calls.

use crate::e1000::E1000;
use crate::input::{self, Key};
use crate::ring::Global;
use crate::{interrupts, pci, rand, time};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beenet::{Config, Event, Ipv4, Reason, Stack};

/// Largest Ethernet frame without the checksum, and the smallest the wire allows.
pub const MAX_FRAME: usize = 1514;
pub const MIN_FRAME: usize = 60;
/// EtherType "IEEE 802 local experimental 1": used by `net test`.
pub const ETHERTYPE_TEST: u16 = 0x88B5;
const BROADCAST: [u8; 6] = [0xFF; 6];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// The transmit ring is full; try again after the card caught up.
    Busy,
    BadFrame,
    NoCard,
}

impl NetError {
    pub fn message(self) -> &'static str {
        match self {
            NetError::Busy => "the network card is busy (transmit queue full)",
            NetError::BadFrame => "frame too short or too long",
            NetError::NoCard => "no network card",
        }
    }
}

/// A network card: sends and receives whole Ethernet frames (no checksum).
pub trait NetDevice {
    fn name(&self) -> &'static str;
    fn mac(&self) -> [u8; 6];
    fn link_up(&self) -> bool;
    fn send(&mut self, frame: &[u8]) -> Result<(), NetError>;
    /// The next received frame, if any. Never blocks.
    fn receive(&mut self) -> Option<Vec<u8>>;
}

#[derive(Default)]
struct Counters {
    tx_frames: u64,
    tx_bytes: u64,
    rx_frames: u64,
    rx_bytes: u64,
    /// Source, EtherType and length of the last received frame.
    last_rx: Option<([u8; 6], u16, usize)>,
}

struct Net {
    dev: Box<dyn NetDevice>,
    stack: Stack,
    counters: Counters,
}

static NET: Global<Option<Net>> = Global::new(None);

pub fn format_mac(mac: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

/// Build an Ethernet II frame.
pub fn ethernet_frame(dst: &[u8; 6], src: &[u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    beenet::wire::build_eth(dst, src, ethertype, payload)
}

/// Find and start a network card. Returns the boot message.
pub fn init() -> Result<String, String> {
    let devices = pci::scan();
    match E1000::find(&devices) {
        Some(Ok(card)) => {
            let line =
                format!("{} {}, link {}", card.name(), format_mac(&card.mac()), if card.link_up() { "up" } else { "down" });
            let stack = Stack::new(card.mac(), rand::next_u32() as u16);
            *NET.get() = Some(Net { dev: Box::new(card), stack, counters: Counters::default() });
            Ok(line)
        }
        Some(Err(e)) => Err(String::from(e)),
        None => Err(format!("no supported network card ({} PCI devices; see lspci)", devices.len())),
    }
}

pub fn present() -> bool {
    NET.get().is_some()
}

impl Net {
    fn send_frame(&mut self, frame: &[u8]) -> Result<(), NetError> {
        self.dev.send(frame)?;
        self.counters.tx_frames += 1;
        self.counters.tx_bytes += frame.len().max(MIN_FRAME) as u64;
        Ok(())
    }

    fn poll(&mut self) {
        let now = time::uptime_ms();
        // Bounded, so a flood of frames can't hold the shell.
        for _ in 0..64 {
            let Some(frame) = self.dev.receive() else { break };
            self.counters.rx_frames += 1;
            self.counters.rx_bytes += frame.len() as u64;
            let src = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
            self.counters.last_rx = Some((src, u16::from_be_bytes([frame[12], frame[13]]), frame.len()));
            self.stack.receive(&frame, now);
        }
        self.stack.poll(now);
        while let Some(frame) = self.stack.transmit() {
            match self.send_frame(&frame) {
                Ok(()) => {}
                Err(NetError::Busy) => {
                    self.stack.untransmit(frame); // the card is full: try again next poll
                    break;
                }
                Err(_) => {}
            }
        }
    }
}

/// Move frames between the card and the protocol stack.
pub fn poll() {
    if let Some(net) = NET.get() {
        net.poll();
    }
}

/// `net test`: send one broadcast frame with a harmless experimental
/// EtherType, to check the transmit path. Returns its length on the wire.
pub fn send_test_frame() -> Result<usize, NetError> {
    let net = NET.get().as_mut().ok_or(NetError::NoCard)?;
    let payload = format!("BeeCodersOs {} b{} net test", crate::OS_VERSION, crate::OS_BUILD);
    let frame = ethernet_frame(&BROADCAST, &net.dev.mac(), ETHERTYPE_TEST, payload.as_bytes());
    net.send_frame(&frame)?;
    Ok(frame.len().max(MIN_FRAME))
}

// --- IPv4 configuration ---------------------------------------------------------------

/// Text form used in /system/net and by `net static`: "IP/PREFIX [GATEWAY]".
pub fn parse_config(text: &str) -> Result<Config, &'static str> {
    let mut words = text.split_whitespace();
    let (ip, prefix) = words.next().and_then(beenet::parse_cidr).ok_or("address must look like 192.168.1.50/24")?;
    let gateway = match words.next() {
        Some(g) => Some(Ipv4::parse(g).ok_or("gateway must look like 192.168.1.1")?),
        None => None,
    };
    if words.next().is_some() {
        return Err("too many values");
    }
    let mask = beenet::netmask(prefix).to_u32();
    let host = ip.to_u32() & !mask;
    if host == 0 || host == !mask {
        return Err("that is the network or broadcast address of the subnet, not a host address");
    }
    if let Some(g) = gateway {
        if g.to_u32() & mask != ip.to_u32() & mask || g == ip {
            return Err("the gateway must be another address in the same subnet");
        }
    }
    Ok(Config { ip, prefix, gateway })
}

pub fn format_config(c: &Config) -> String {
    match c.gateway {
        Some(g) => format!("{}/{} {}", c.ip, c.prefix, g),
        None => format!("{}/{}", c.ip, c.prefix),
    }
}

/// Set or remove the IPv4 address. Announces a new address on the network.
pub fn configure(config: Option<Config>) -> Result<(), NetError> {
    let net = NET.get().as_mut().ok_or(NetError::NoCard)?;
    net.stack.set_config(config);
    net.poll();
    Ok(())
}

// --- ping -------------------------------------------------------------------------------

pub enum Ping {
    Reply { ttl: u8, micros: u64, len: usize },
    Timeout,
    Unreachable(Reason),
    Cancelled,
    Failed(&'static str),
}

/// Send one echo request and wait for its reply (Ctrl+C cancels).
/// Polls without sleeping, so the round-trip time is accurate.
pub fn ping(dst: Ipv4, id: u16, seq: u16, timeout_ms: u64) -> Ping {
    let Some(net) = NET.get().as_mut() else { return Ping::Failed(NetError::NoCard.message()) };
    let payload: [u8; 32] = core::array::from_fn(|i| b'a' + (i % 26) as u8);
    let _ = time::micros(); // calibrate the clock before timing anything
    while net.stack.take_event().is_some() {} // forget old results
    let start_ms = time::uptime_ms();
    let start = time::micros();
    if let Err(e) = net.stack.ping(dst, id, seq, &payload, start_ms) {
        return Ping::Failed(e.message());
    }
    loop {
        net.poll();
        while let Some(event) = net.stack.take_event() {
            match event {
                Event::EchoReply { from, id: i, seq: s, ttl, len } if from == dst && i == id && s == seq => {
                    return Ping::Reply { ttl, micros: time::micros() - start, len };
                }
                Event::Unreachable { dst: d, reason } if d == dst => return Ping::Unreachable(reason),
                _ => {}
            }
        }
        while let Some(key) = input::poll_key() {
            if matches!(key, Key::Ctrl('c') | Key::Escape) {
                return Ping::Cancelled;
            }
        }
        // While the next hop's address is still being looked up, keep waiting:
        // the stack reports "unreachable" itself when nobody answers.
        if time::uptime_ms() - start_ms >= timeout_ms && !net.stack.resolving() {
            return Ping::Timeout;
        }
        core::hint::spin_loop();
    }
}

/// Wait `ms` while keeping the network going. False if Ctrl+C was pressed.
pub fn sleep(ms: u64) -> bool {
    let end = time::uptime_ms() + ms;
    while time::uptime_ms() < end {
        poll();
        while let Some(key) = input::poll_key() {
            if matches!(key, Key::Ctrl('c') | Key::Escape) {
                return false;
            }
        }
        interrupts::wait_until(input::pending); // the 100 Hz timer wakes us
    }
    true
}

// --- text for the shell -------------------------------------------------------------------

/// For the `arp` command: (IP, MAC, seconds since confirmed).
pub fn arp_table() -> Vec<(Ipv4, [u8; 6], u64)> {
    poll();
    let now = time::uptime_ms();
    let Some(net) = NET.get().as_ref() else { return Vec::new() };
    let mut rows: Vec<_> =
        net.stack.arp_table().iter().map(|e| (e.ip, e.mac, now.saturating_sub(e.updated_ms) / 1000)).collect();
    rows.sort();
    rows
}

/// For the `net` command.
pub fn describe() -> Option<String> {
    poll();
    let net = NET.get().as_ref()?;
    let c = &net.counters;
    let mut out = format!(
        "card:    {}\nmac:     {}\nlink:    {}\n",
        net.dev.name(),
        format_mac(&net.dev.mac()),
        if net.dev.link_up() { "up" } else { "down" }
    );
    match net.stack.config() {
        Some(cfg) => {
            out += &format!("address: {}/{}\n", cfg.ip, cfg.prefix);
            out += &match cfg.gateway {
                Some(g) => format!("gateway: {g}\n"),
                None => String::from("gateway: none (local network only)\n"),
            };
        }
        None => out += "address: none (set one: net static IP/PREFIX [GATEWAY])\n",
    }
    out += &format!(
        "frames:  {} sent, {} received\nbytes:   {} sent, {} received",
        c.tx_frames, c.rx_frames, c.tx_bytes, c.rx_bytes
    );
    if let Some((src, ethertype, len)) = c.last_rx {
        out += &format!("\nlast received: {len} bytes from {}, type 0x{ethertype:04x}", format_mac(&src));
    }
    let s = &net.stack.stats;
    if s.dropped + s.dropped_fragments + s.address_conflicts > 0 {
        out += &format!("\nignored: {} frames, {} fragments", s.dropped, s.dropped_fragments);
        if s.address_conflicts > 0 {
            out += &format!("\nWARNING: another machine is using our IP address ({} times seen)", s.address_conflicts);
        }
    }
    Some(out)
}
