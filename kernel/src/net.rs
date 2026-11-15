//! Networking in the kernel: the network card, the beenet protocol stack
//! (ARP, IPv4, ICMP, UDP, DHCP, DNS, TCP) and what the `net`, `ping`, `arp`,
//! `nslookup`, `nc`, `wget`, `ntp` and `tftp` commands need.
//! See Docs/network-stack-plan.html (phases N1-N5).
//!
//! `poll()` moves frames between the card and the stack. It runs from the idle
//! loop (every 10 ms, so the OS answers pings at the prompt) and inside
//! waiting network calls.

use crate::e1000::E1000;
use crate::input::{self, Key};
use crate::ring::Global;
use crate::{interrupts, pci, rand, time};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beenet::{Config, Event, Ipv4, Lookup, Notice, Reason, Stack, TcpError, TcpHandle, TcpStatus, DNS_PORT};

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

/// How the IPv4 address is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Dhcp,
    Static(Config),
}

/// What is saved in /system/net.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    /// A DNS server set by hand (address, port); None: the one DHCP gives.
    pub dns: Option<(Ipv4, u16)>,
}

struct Net {
    dev: Box<dyn NetDevice>,
    stack: Stack,
    counters: Counters,
    settings: Settings,
    /// Messages for the user about the automatic address: (kind, text).
    notices: VecDeque<(&'static str, String)>,
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
            let stack = Stack::new(card.mac(), rand::next_u32());
            let settings = Settings { mode: Mode::Off, dns: None };
            *NET.get() =
                Some(Net { dev: Box::new(card), stack, counters: Counters::default(), settings, notices: VecDeque::new() });
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
        while let Some(notice) = self.stack.take_notice() {
            self.notice(notice);
        }
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

impl Net {
    /// "address 10.0.2.15/24 (DHCP), gateway ..., DNS ...", if DHCP gave us an address.
    fn lease_text(&self) -> Option<String> {
        let lease = self.stack.dhcp_lease()?;
        let mut text = format!("address {}/{} (DHCP)", lease.ip, lease.prefix);
        if let Some(g) = lease.gateway {
            text += &format!(", gateway {g}");
        }
        if let Some(dns) = self.stack.dns_server() {
            text += &format!(", DNS {}", format_dns_server(dns));
        }
        Some(text)
    }

    fn notice(&mut self, notice: Notice) {
        let entry = match notice {
            Notice::Bound(lease) => {
                if self.settings.dns.is_none() {
                    self.stack.set_dns(lease.dns.map(|d| (d, DNS_PORT)));
                }
                ("dhcp-bound", self.lease_text().unwrap_or_default())
            }
            Notice::Failed => ("dhcp-failed", String::from("no DHCP server answered; still trying (or: net static IP/PREFIX [GATEWAY])")),
            Notice::Lost => ("dhcp-lost", String::from("the DHCP lease ended; looking for a new address")),
        };
        if self.notices.len() == 8 {
            self.notices.pop_front();
        }
        self.notices.push_back(entry);
    }
}

/// The next message about the automatic address, for the shell to show:
/// (kind for the event log, text).
pub fn take_notice() -> Option<(&'static str, String)> {
    NET.get().as_mut()?.notices.pop_front()
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

// --- settings --------------------------------------------------------------------------

/// "IP/PREFIX [GATEWAY]", as typed after `net static`.
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

/// "ADDRESS" or "ADDRESS:PORT" of a DNS server.
pub fn parse_dns_server(text: &str) -> Option<(Ipv4, u16)> {
    match text.split_once(':') {
        Some((ip, port)) => Some((Ipv4::parse(ip)?, port.parse().ok().filter(|p| *p != 0)?)),
        None => Some((Ipv4::parse(text)?, DNS_PORT)),
    }
}

fn format_dns_server((ip, port): (Ipv4, u16)) -> String {
    if port == DNS_PORT {
        format!("{ip}")
    } else {
        format!("{ip}:{port}")
    }
}

/// The text of /system/net: a mode line ("dhcp", "none" or
/// "static IP/PREFIX [GATEWAY]") and optionally "dns ADDRESS[:PORT]".
pub fn parse_settings(text: &str) -> Result<Settings, &'static str> {
    let mut settings = Settings { mode: Mode::Dhcp, dns: None };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match line.split_once(' ').unwrap_or((line, "")) {
            ("dhcp", "") => settings.mode = Mode::Dhcp,
            ("none", "") => settings.mode = Mode::Off,
            ("static", config) => settings.mode = Mode::Static(parse_config(config)?),
            ("dns", server) => settings.dns = Some(parse_dns_server(server.trim()).ok_or("bad DNS server")?),
            _ => return Err("unknown line"),
        }
    }
    Ok(settings)
}

pub fn format_settings(s: &Settings) -> String {
    let mut out = match s.mode {
        Mode::Dhcp => String::from("dhcp"),
        Mode::Off => String::from("none"),
        Mode::Static(c) => format!("static {}", format_config(&c)),
    };
    if let Some(dns) = s.dns {
        out += &format!("\ndns {}", format_dns_server(dns));
    }
    out
}

pub fn settings() -> Option<Settings> {
    NET.get().as_ref().map(|n| n.settings)
}

/// Put settings into effect: a fixed address, automatic setup (DHCP starts
/// in the background) or no address.
pub fn apply(settings: Settings) -> Result<(), NetError> {
    let net = NET.get().as_mut().ok_or(NetError::NoCard)?;
    let now = time::uptime_ms();
    net.stack.reseed(rand::next_u32());
    match settings.mode {
        Mode::Static(config) => {
            net.stack.dhcp_stop();
            net.stack.set_config(Some(config));
        }
        Mode::Dhcp => {
            // Already on DHCP: leave the lease (or the running search) alone.
            if net.settings.mode != Mode::Dhcp || !net.stack.dhcp_running() {
                net.stack.set_config(None);
                net.stack.dhcp_start(now);
            }
        }
        Mode::Off => {
            net.stack.dhcp_stop();
            net.stack.set_config(None);
        }
    }
    let lease_dns = net.stack.dhcp_lease().and_then(|l| l.dns).map(|d| (d, DNS_PORT));
    net.stack.set_dns(settings.dns.or(if settings.mode == Mode::Dhcp { lease_dns } else { None }));
    net.settings = settings;
    net.poll();
    Ok(())
}

fn cancelled() -> bool {
    let mut stop = false;
    while let Some(key) = input::poll_key() {
        stop |= matches!(key, Key::Ctrl('c') | Key::Escape);
    }
    stop
}

pub enum DhcpWait {
    /// An address was obtained: the text to show.
    Bound(String),
    Failed,
    Cancelled,
}

/// Wait for the DHCP client to finish (after `apply` with `Mode::Dhcp`).
pub fn dhcp_wait(timeout_ms: u64) -> DhcpWait {
    let Some(net) = NET.get().as_mut() else { return DhcpWait::Failed };
    let end = time::uptime_ms() + timeout_ms;
    loop {
        net.poll();
        if let Some(text) = net.lease_text() {
            net.notices.retain(|(kind, _)| *kind != "dhcp-bound"); // shown here, not again at the prompt
            return DhcpWait::Bound(text);
        }
        while let Some((kind, text)) = net.notices.pop_front() {
            match kind {
                "dhcp-bound" => return DhcpWait::Bound(text),
                "dhcp-failed" => return DhcpWait::Failed,
                _ => {}
            }
        }
        if cancelled() {
            return DhcpWait::Cancelled;
        }
        if time::uptime_ms() >= end {
            return DhcpWait::Failed;
        }
        interrupts::wait_until(input::pending);
    }
}

// --- names --------------------------------------------------------------------------------

/// Turn a host name or an IPv4 address into an address (Ctrl+C cancels).
pub fn resolve(host: &str) -> Result<Ipv4, String> {
    if let Some(ip) = Ipv4::parse(host) {
        return Ok(ip);
    }
    let net = NET.get().as_mut().ok_or_else(|| String::from(NetError::NoCard.message()))?;
    while net.stack.take_event().is_some() {}
    let handle = match net.stack.dns_lookup(host, time::uptime_ms()) {
        Ok(Lookup::Ready(ip)) => return Ok(ip),
        Ok(Lookup::Pending(handle)) => handle,
        Err(e) => return Err(format!("{host}: {}", e.message())),
    };
    loop {
        net.poll();
        while let Some(event) = net.stack.take_event() {
            if let Event::Dns { handle: h, result } = event {
                if h == handle {
                    return result.map_err(|e| format!("{host}: {}", e.message()));
                }
            }
        }
        if cancelled() {
            return Err(String::from("cancelled"));
        }
        interrupts::wait_until(input::pending); // the lookup times out by itself after 5 s
    }
}

// --- TCP connections -------------------------------------------------------------------------

/// A TCP connection. Calls poll the network while they wait; Ctrl+C cancels.
/// Dropping it closes the connection (in the background if it is still busy).
pub struct TcpStream(TcpHandle);

pub enum Received {
    /// This many bytes were put in the buffer.
    Data(usize),
    /// Nothing has arrived yet.
    Nothing,
    /// The other side finished sending and everything was read.
    End,
    Failed(&'static str),
}

/// Connect to `dst:port`. Waits for the result.
pub fn tcp_connect(dst: Ipv4, port: u16) -> Result<TcpStream, &'static str> {
    let net = NET.get().as_mut().ok_or(NetError::NoCard.message())?;
    let handle = net.stack.tcp_connect(dst, port, time::uptime_ms()).map_err(|e| e.message())?;
    let stream = TcpStream(handle);
    loop {
        net.poll();
        match net.stack.tcp_status(handle) {
            Ok(TcpStatus::Connecting) => {}
            Ok(TcpStatus::Closed(reason)) => return Err(reason.message()),
            Ok(_) => return Ok(stream),
            Err(e) => return Err(e.message()),
        }
        if cancelled() {
            return Err("cancelled"); // dropping `stream` gives up the attempt
        }
        interrupts::wait_until(input::pending);
    }
}

impl TcpStream {
    /// Send all of `data` (waits while the send buffer is full).
    pub fn send(&self, mut data: &[u8]) -> Result<(), &'static str> {
        let net = NET.get().as_mut().ok_or(NetError::NoCard.message())?;
        while !data.is_empty() {
            let n = net.stack.tcp_send(self.0, data, time::uptime_ms()).map_err(|e| e.message())?;
            data = &data[n..];
            net.poll();
            if n == 0 {
                if cancelled() {
                    return Err("cancelled");
                }
                interrupts::wait_until(input::pending);
            }
        }
        Ok(())
    }

    /// Take received data, without waiting.
    pub fn receive(&self, buf: &mut [u8]) -> Received {
        let Some(net) = NET.get().as_mut() else { return Received::Failed(NetError::NoCard.message()) };
        net.poll();
        let result = net.stack.tcp_recv(self.0, buf, time::uptime_ms());
        net.poll(); // send a window update right away, if reading made room
        match result {
            Ok(0) => Received::Nothing,
            Ok(n) => Received::Data(n),
            Err(TcpError::Closed(beenet::CloseReason::Normal)) => Received::End,
            Err(e) => Received::Failed(e.message()),
        }
    }

    /// Finish sending (the other side sees the end of our data). Data can still arrive.
    pub fn finish(&self) {
        if let Some(net) = NET.get().as_mut() {
            let _ = net.stack.tcp_close(self.0, time::uptime_ms());
            net.poll();
        }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        if let Some(net) = NET.get().as_mut() {
            let now = time::uptime_ms();
            // Close politely if we can; `tcp_release` resets whatever is still open.
            let _ = net.stack.tcp_close(self.0, now);
            net.poll();
            net.stack.tcp_release(self.0, now);
            net.poll();
        }
    }
}

/// Wait up to `ms` for a key or network activity (one timer tick at a time).
pub fn idle(ms: u64) {
    let end = time::uptime_ms() + ms;
    while time::uptime_ms() < end && !input::pending() {
        poll();
        interrupts::wait_until(input::pending);
    }
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
        if cancelled() {
            return Ping::Cancelled;
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
        if cancelled() {
            return false;
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
            let how = match (net.settings.mode, net.stack.dhcp_lease().and_then(|l| l.expires_ms())) {
                (Mode::Dhcp, Some(end)) => {
                    format!("DHCP, lease ends in {}", time::format_duration(end.saturating_sub(time::uptime_ms())))
                }
                (Mode::Dhcp, None) => String::from("DHCP"),
                _ => String::from("static"),
            };
            out += &format!("address: {}/{} ({how})\n", cfg.ip, cfg.prefix);
            out += &match cfg.gateway {
                Some(g) => format!("gateway: {g}\n"),
                None => String::from("gateway: none (local network only)\n"),
            };
        }
        None if net.settings.mode == Mode::Dhcp => out += "address: none yet (DHCP is looking for one; or: net static IP/PREFIX [GATEWAY])\n",
        None => out += "address: none (net dhcp, or net static IP/PREFIX [GATEWAY])\n",
    }
    out += &match net.stack.dns_server() {
        Some(dns) => format!("dns:     {}{}\n", format_dns_server(dns), if net.settings.dns.is_some() { " (set by hand)" } else { "" }),
        None => String::from("dns:     none (net dns ADDRESS)\n"),
    };
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

// --- phase N5: HTTP, SNTP, TFTP ------------------------------------------------------------

/// "HOST" or "HOST:PORT" (for services that are not on their usual port).
pub fn split_host_port(text: &str, default: u16) -> Option<(&str, u16)> {
    match text.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host, port.parse().ok().filter(|p| *p != 0)?)),
        Some(_) => None,
        None if text.is_empty() => None,
        None => Some((text, default)),
    }
}

fn unreachable_text(reason: Reason) -> &'static str {
    match reason {
        Reason::NoArpReply => "host unreachable (no machine with that address answered)",
        Reason::Icmp(3) => "nothing is listening on that port",
        Reason::Icmp(_) => "host unreachable (a router said so)",
    }
}

/// While a transfer is moving, check the card continuously instead of
/// sleeping until the next timer tick (the card is polled: a sleep costs up
/// to 10 ms per packet exchange).
fn transfer_wait(last_activity_ms: u64) {
    if time::uptime_ms() - last_activity_ms < 50 {
        core::hint::spin_loop();
    } else {
        interrupts::wait_until(input::pending);
    }
}

/// An open UDP port; closed when dropped.
struct UdpPort(u16);

impl UdpPort {
    fn open(net: &mut Net) -> Result<UdpPort, String> {
        net.stack.udp_open(None).map(UdpPort).map_err(|e| String::from(e.message()))
    }
}

impl Drop for UdpPort {
    fn drop(&mut self) {
        if let Some(net) = NET.get().as_mut() {
            net.stack.udp_close(self.0);
        }
    }
}

/// Ask a time server for the time (SNTP). Returns Unix seconds and
/// milliseconds, corrected for half the round trip.
pub fn sntp_query(server: Ipv4, port: u16) -> Result<(u64, u32, u8), String> {
    let net = NET.get().as_mut().ok_or_else(|| String::from(NetError::NoCard.message()))?;
    let socket = UdpPort::open(net)?;
    let _ = time::micros(); // calibrate before timing anything
    while net.stack.take_event().is_some() {}
    let mut last_error = "no answer from the time server (timed out)";
    for _attempt in 0..3 {
        let nonce = (rand::next_u32() as u64) << 32 | rand::next_u32() as u64;
        let (sent_ms, sent) = (time::uptime_ms(), time::micros());
        net.stack
            .udp_send(socket.0, server, port, &beenet::sntp::request(nonce), sent_ms)
            .map_err(|e| String::from(e.message()))?;
        while time::uptime_ms() - sent_ms < 2000 || net.stack.resolving() {
            net.poll();
            while let Some((from, from_port, data)) = net.stack.udp_recv(socket.0) {
                if from != server || from_port != port {
                    continue;
                }
                match beenet::sntp::parse_reply(&data, nonce) {
                    Ok(t) => {
                        let half_trip_ms = (time::micros() - sent) / 2000;
                        let total = t.unix_secs * 1000 + t.millis as u64 + half_trip_ms;
                        return Ok((total / 1000, (total % 1000) as u32, t.stratum));
                    }
                    // The server answered our request, but has no time to give.
                    Err(e @ beenet::sntp::SntpError::NotSynchronised) => return Err(String::from(e.message())),
                    Err(e) => last_error = e.message(), // a stray or old reply: keep waiting
                }
            }
            while let Some(event) = net.stack.take_event() {
                if let Event::Unreachable { dst, reason } = event {
                    if dst == server {
                        return Err(String::from(match reason {
                            Reason::Icmp(3) => "that machine has no time service (port closed)",
                            r => unreachable_text(r),
                        }));
                    }
                }
            }
            if cancelled() {
                return Err(String::from("cancelled"));
            }
            core::hint::spin_loop(); // no sleeping: the round trip is being timed
        }
    }
    Err(String::from(last_error))
}

/// Takes pieces of a file as they arrive; an error stops the transfer.
pub type Sink<'a> = &'a mut dyn FnMut(&[u8]) -> Result<(), String>;

fn tftp_run(server: Ipv4, mut transfer: beenet::tftp::Transfer, sink: Sink) -> Result<u64, String> {
    use beenet::tftp::Status;
    let net = NET.get().as_mut().ok_or_else(|| String::from(NetError::NoCard.message()))?;
    let socket = UdpPort::open(net)?;
    while net.stack.take_event().is_some() {}
    let mut last_activity = time::uptime_ms();
    let mut failure: Option<String> = None;
    loop {
        let now = time::uptime_ms();
        net.poll();
        while let Some((from, from_port, data)) = net.stack.udp_recv(socket.0) {
            if from == server {
                transfer.receive(from_port, &data, now);
                last_activity = now;
            }
        }
        transfer.poll(now);
        let received = transfer.take_received();
        if !received.is_empty() && failure.is_none() {
            if let Err(e) = sink(&received) {
                failure = Some(e);
                transfer.cancel();
            }
        }
        while let Some(event) = net.stack.take_event() {
            if let Event::Unreachable { dst, reason } = event {
                if dst == server && failure.is_none() {
                    failure = Some(String::from(match reason {
                        Reason::Icmp(3) => "that machine has no TFTP service (port closed)",
                        r => unreachable_text(r),
                    }));
                    transfer.cancel();
                }
            }
        }
        if failure.is_none() && cancelled() {
            failure = Some(String::from("cancelled"));
            transfer.cancel();
        }
        while let Some((port, packet)) = transfer.transmit() {
            if let Err(e) = net.stack.udp_send(socket.0, server, port, &packet, now) {
                failure.get_or_insert(String::from(e.message()));
                transfer.cancel();
            }
        }
        net.poll();
        match (transfer.status(), failure.take()) {
            (Status::Running, None) => {}
            (Status::Done, None) => return Ok(transfer.bytes()),
            (_, Some(e)) => return Err(e),
            (Status::Failed(e), None) => return Err(String::from(e.message())),
        }
        transfer_wait(last_activity);
    }
}

/// Fetch a file from a TFTP server, at most `limit` bytes. Returns its size.
pub fn tftp_get(server: Ipv4, port: u16, name: &str, limit: u64, sink: Sink) -> Result<u64, String> {
    let transfer = beenet::tftp::Transfer::get(name, limit, port, time::uptime_ms()).map_err(|e| String::from(e.message()))?;
    tftp_run(server, transfer, sink)
}

/// Store `data` on a TFTP server as `name`.
pub fn tftp_put(server: Ipv4, port: u16, name: &str, data: Vec<u8>) -> Result<u64, String> {
    let transfer = beenet::tftp::Transfer::put(name, data, port, time::uptime_ms()).map_err(|e| String::from(e.message()))?;
    tftp_run(server, transfer, &mut |_| Ok(()))
}

pub struct HttpResult {
    /// The address the content came from, if the server sent us elsewhere.
    pub redirected_to: Option<String>,
    pub bytes: u64,
}

const MAX_REDIRECTS: usize = 5;
/// Give up when a server sends nothing for this long.
const HTTP_STALL_MS: u64 = 15_000;

/// HTTP GET: the body goes to `sink` piece by piece; at most `limit` bytes.
/// Follows redirects. Anything but a 2xx answer is an error.
pub fn http_get(address: &str, limit: u64, sink: Sink) -> Result<HttpResult, String> {
    use beenet::http::{request, Response, Url};
    let mut url = Url::parse(address).map_err(|e| String::from(e.message()))?;
    let agent = format!("BeeCodersOs/{}", crate::OS_VERSION);
    let mut redirected_to = None;
    for _hop in 0..=MAX_REDIRECTS {
        let ip = resolve(&url.host)?;
        let stream = tcp_connect(ip, url.port).map_err(|e| format!("{} port {}: {e}", url.host, url.port))?;
        stream.send(&request("GET", &url, &agent, None)).map_err(String::from)?;
        let (mut response, mut body, mut buf) = (Response::new(), Vec::new(), [0u8; 2048]);
        let mut bytes = 0u64;
        let mut last_activity = time::uptime_ms();
        while !response.complete() && response.redirect().is_none() {
            match stream.receive(&mut buf) {
                Received::Data(n) => {
                    last_activity = time::uptime_ms();
                    response.feed(&buf[..n], &mut body).map_err(|e| String::from(e.message()))?;
                }
                Received::End => {
                    response.end_of_stream().map_err(|e| String::from(e.message()))?;
                }
                Received::Failed(e) => return Err(String::from(e)),
                Received::Nothing => {
                    if cancelled() {
                        return Err(String::from("cancelled"));
                    }
                    if time::uptime_ms() - last_activity > HTTP_STALL_MS {
                        return Err(String::from("the server stopped answering"));
                    }
                    transfer_wait(last_activity);
                }
            }
            if !response.headers_done() || response.redirect().is_some() {
                continue;
            }
            if !(200..300).contains(&response.status()) {
                return Err(format!("the server answered: {} {}", response.status(), response.reason()));
            }
            if response.content_length().is_some_and(|n| n > limit) || bytes + body.len() as u64 > limit {
                return Err(String::from("too large"));
            }
            if !body.is_empty() {
                bytes += body.len() as u64;
                sink(&body)?;
                body.clear();
            }
        }
        let Some(location) = response.redirect() else {
            return Ok(HttpResult { redirected_to, bytes });
        };
        url = url.join(location).map_err(|e| match e {
            beenet::http::HttpError::Https => {
                String::from("the server sends us to an https:// address, and HTTPS (encrypted HTTP) is not supported yet")
            }
            e => format!("bad redirect: {}", e.message()),
        })?;
        redirected_to = Some(format!("{url}"));
    }
    Err(String::from("too many redirects"))
}
