//! Networking in the kernel (network plan phase N1): the network card, frame
//! counters and the `net` command. The protocol stack (ARP, IP, ...) arrives
//! in later phases; for now received frames are counted and dropped.
//! See Docs/network-stack-plan.html.

use crate::e1000::E1000;
use crate::pci;
use crate::ring::Global;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

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
struct Stats {
    tx_frames: u64,
    tx_bytes: u64,
    rx_frames: u64,
    rx_bytes: u64,
    /// Source, EtherType and length of the last received frame.
    last_rx: Option<([u8; 6], u16, usize)>,
}

struct Net {
    dev: Box<dyn NetDevice>,
    stats: Stats,
}

static NET: Global<Option<Net>> = Global::new(None);

pub fn format_mac(mac: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

/// Build an Ethernet II frame.
pub fn ethernet_frame(dst: &[u8; 6], src: &[u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(14 + payload.len());
    f.extend_from_slice(dst);
    f.extend_from_slice(src);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// Find and start a network card. Returns the boot message.
pub fn init() -> Result<String, String> {
    let devices = pci::scan();
    match E1000::find(&devices) {
        Some(Ok(card)) => {
            let line =
                format!("{} {}, link {}", card.name(), format_mac(&card.mac()), if card.link_up() { "up" } else { "down" });
            *NET.get() = Some(Net { dev: Box::new(card), stats: Stats::default() });
            Ok(line)
        }
        Some(Err(e)) => Err(String::from(e)),
        None => Err(format!("no supported network card ({} PCI devices; see lspci)", devices.len())),
    }
}

/// Take received frames from the card. Called from the idle loop and from
/// network commands; bounded so a flood can't hold the shell.
pub fn poll() {
    let Some(net) = NET.get() else { return };
    for _ in 0..64 {
        let Some(frame) = net.dev.receive() else { break };
        net.stats.rx_frames += 1;
        net.stats.rx_bytes += frame.len() as u64;
        let src = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
        net.stats.last_rx = Some((src, u16::from_be_bytes([frame[12], frame[13]]), frame.len()));
        // N2: hand the frame to the protocol stack here.
    }
}

pub fn send(frame: &[u8]) -> Result<(), NetError> {
    let net = NET.get().as_mut().ok_or(NetError::NoCard)?;
    net.dev.send(frame)?;
    net.stats.tx_frames += 1;
    net.stats.tx_bytes += frame.len().max(MIN_FRAME) as u64;
    Ok(())
}

/// `net test`: send one broadcast frame with a harmless experimental
/// EtherType, to check the transmit path. Returns its length on the wire.
pub fn send_test_frame() -> Result<usize, NetError> {
    let mac = NET.get().as_ref().ok_or(NetError::NoCard)?.dev.mac();
    let payload = format!("BeeCodersOs {} b{} net test", crate::OS_VERSION, crate::OS_BUILD);
    let frame = ethernet_frame(&BROADCAST, &mac, ETHERTYPE_TEST, payload.as_bytes());
    send(&frame)?;
    Ok(frame.len().max(MIN_FRAME))
}

/// For the `net` command.
pub fn describe() -> Option<String> {
    poll();
    let net = NET.get().as_ref()?;
    let s = &net.stats;
    let mut out = format!(
        "card:    {}\nmac:     {}\nlink:    {}\nframes:  {} sent, {} received\nbytes:   {} sent, {} received",
        net.dev.name(),
        format_mac(&net.dev.mac()),
        if net.dev.link_up() { "up" } else { "down" },
        s.tx_frames,
        s.rx_frames,
        s.tx_bytes,
        s.rx_bytes
    );
    if let Some((src, ethertype, len)) = s.last_rx {
        out += &format!("\nlast received: {len} bytes from {}, type 0x{ethertype:04x}", format_mac(&src));
    }
    out += "\naddress: none yet (IP arrives in network phase N2/N3)";
    Some(out)
}
