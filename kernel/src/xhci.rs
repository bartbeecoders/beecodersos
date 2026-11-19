//! xHCI: the USB 3 host controller (it also drives USB 2 and 1 devices).
//!
//! The controller is told what to do through rings of 16-byte records
//! ("TRBs") in memory: a command ring, one transfer ring per device
//! endpoint, and an event ring on which it reports what happened. This
//! driver sets those up, finds the devices plugged into the machine's own
//! ports, and reads the reports of keyboards and mice (usb.rs).
//!
//! It runs in its own thread ("usb") and looks at the event ring every timer
//! tick: no interrupts are used. Only that thread touches the controller.
//! Memory for the rings comes from the heap: physical addresses are virtual
//! addresses here, and all of it lies below 4 GiB.
//!
//! Not supported: hubs, storage, and devices that need more than the
//! keyboard or mouse boot protocol. Boot flag `usb=off` leaves the
//! controller alone (the firmware then keeps imitating a PS/2 keyboard).

use crate::pci::{self, Bar};
use crate::usb::{self, DeviceInfo, HidInterface, Kind};
use crate::{console, keyboard, thread, time};
use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::format;
use alloc::string::String;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use beenet::uvc;

const PAGE: usize = 4096;
const BELOW_4G: u64 = 1 << 32;

// --- memory the controller reads and writes --------------------------------------

/// One zeroed page, aligned, below 4 GiB.
pub struct Page(*mut u8);

impl Page {
    pub fn new() -> Option<Page> {
        let p = unsafe { alloc_zeroed(Layout::from_size_align(PAGE, PAGE).ok()?) };
        if p.is_null() {
            return None;
        }
        let page = Page(p);
        (page.addr() + PAGE as u64 <= BELOW_4G).then_some(page)
    }

    pub fn addr(&self) -> u64 {
        self.0 as u64
    }

    fn read32(&self, at: usize) -> u32 {
        assert!(at + 4 <= PAGE);
        unsafe { (self.0.add(at) as *const u32).read_volatile() }
    }

    fn write32(&self, at: usize, value: u32) {
        assert!(at + 4 <= PAGE);
        unsafe { (self.0.add(at) as *mut u32).write_volatile(value) }
    }

    fn write64(&self, at: usize, value: u64) {
        self.write32(at, value as u32);
        self.write32(at + 4, (value >> 32) as u32);
    }

    fn bytes(&self, len: usize) -> Vec<u8> {
        self.bytes_at(0, len)
    }

    fn bytes_at(&self, from: usize, len: usize) -> Vec<u8> {
        let end = (from + len).min(PAGE);
        let from = from.min(end);
        // What the controller wrote is in memory before its event said so.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        unsafe { core::slice::from_raw_parts(self.0.add(from), end - from) }.to_vec()
    }

    fn write_bytes(&self, data: &[u8]) {
        for (i, b) in data.iter().take(PAGE).enumerate() {
            unsafe { self.0.add(i).write_volatile(*b) };
        }
    }

    fn clear(&self) {
        unsafe { self.0.write_bytes(0, PAGE) };
    }
}

/// A zeroed block of `EVENT_TRBS` events, aligned to its size, below 4 GiB:
/// the event ring. A camera alone reports up to 8000 transfers a second.
pub struct EventBlock(*mut u8);

pub const EVENT_TRBS: usize = 4096;
const EVENT_BYTES: usize = EVENT_TRBS * 16;

impl EventBlock {
    fn new() -> Option<EventBlock> {
        let p = unsafe { alloc_zeroed(Layout::from_size_align(EVENT_BYTES, EVENT_BYTES).ok()?) };
        if p.is_null() {
            return None;
        }
        let block = EventBlock(p);
        (block.addr() + EVENT_BYTES as u64 <= BELOW_4G).then_some(block)
    }

    fn addr(&self) -> u64 {
        self.0 as u64
    }

    fn read32(&self, at: usize) -> u32 {
        assert!(at + 4 <= EVENT_BYTES);
        unsafe { (self.0.add(at) as *const u32).read_volatile() }
    }
}

impl Drop for EventBlock {
    fn drop(&mut self) {
        if let Ok(layout) = Layout::from_size_align(EVENT_BYTES, EVENT_BYTES) {
            unsafe { dealloc(self.0, layout) };
        }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        if let Ok(layout) = Layout::from_size_align(PAGE, PAGE) {
            unsafe { dealloc(self.0, layout) };
        }
    }
}

// --- TRBs and rings ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Trb {
    pub param: u64,
    pub status: u32,
    pub control: u32,
}

const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ISOCH: u32 = 5;
/// Isochronous TRB: start at the next chance (no frame number given).
const START_ASAP: u32 = 1 << 31;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EVALUATE_CONTEXT: u32 = 13;
const TRB_STOP_ENDPOINT: u32 = 15;
const EVENT_TRANSFER: u32 = 32;
const EVENT_COMMAND: u32 = 33;
const EVENT_PORT: u32 = 34;

const CYCLE: u32 = 1 << 0;
const TOGGLE_CYCLE: u32 = 1 << 1;
const INTERRUPT_ON_COMPLETION: u32 = 1 << 5;
const IMMEDIATE_DATA: u32 = 1 << 6;
const DIRECTION_IN: u32 = 1 << 16;

const CODE_SUCCESS: u8 = 1;
const CODE_SHORT_PACKET: u8 = 13;
const CODE_RING_UNDERRUN: u8 = 14;
const CODE_RING_OVERRUN: u8 = 15;

impl Trb {
    pub fn new(kind: u32, param: u64, status: u32, flags: u32) -> Trb {
        Trb { param, status, control: kind << 10 | flags }
    }

    pub fn kind(&self) -> u32 {
        self.control >> 10 & 0x3F
    }

    /// Completion code of an event.
    pub fn code(&self) -> u8 {
        (self.status >> 24) as u8
    }

    pub fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }
}

/// The three TRBs of a control transfer (two when there is no data).
pub fn control_trbs(request_type: u8, request: u8, value: u16, index: u16, buffer: u64, length: u16) -> Vec<Trb> {
    let device_to_host = request_type & 0x80 != 0;
    let setup = request_type as u64 | (request as u64) << 8 | (value as u64) << 16 | (index as u64) << 32 | (length as u64) << 48;
    // Bits 16-17 of the setup TRB: 0 = no data, 2 = data out, 3 = data in.
    let transfer = match (length, device_to_host) {
        (0, _) => 0,
        (_, true) => 3,
        (_, false) => 2,
    };
    let mut trbs = alloc::vec![Trb::new(TRB_SETUP, setup, 8, IMMEDIATE_DATA | transfer << 16)];
    if length > 0 {
        trbs.push(Trb::new(TRB_DATA, buffer, length as u32, if device_to_host { DIRECTION_IN } else { 0 }));
    }
    // The status stage goes the other way than the data (or in, without data).
    let status_in = length == 0 || !device_to_host;
    trbs.push(Trb::new(TRB_STATUS, 0, 0, INTERRUPT_ON_COMPLETION | if status_in { DIRECTION_IN } else { 0 }));
    trbs
}

pub const RING_TRBS: usize = PAGE / 16;

/// A ring we write and the controller reads (commands, transfers). The last
/// place holds a link back to the first.
pub struct Ring {
    page: Page,
    next: usize,
    cycle: bool,
}

impl Ring {
    pub fn new() -> Option<Ring> {
        Some(Ring { page: Page::new()?, next: 0, cycle: true })
    }

    pub fn addr(&self) -> u64 {
        self.page.addr()
    }

    fn write(&self, at: usize, trb: Trb) {
        self.page.write64(at * 16, trb.param);
        self.page.write32(at * 16 + 8, trb.status);
        // The cycle bit hands the TRB to the controller: written last.
        self.page.write32(at * 16 + 12, trb.control & !CYCLE | self.cycle as u32);
    }

    /// Add a TRB; returns its address.
    pub fn push(&mut self, trb: Trb) -> u64 {
        let at = self.next;
        self.write(at, trb);
        self.next += 1;
        if self.next == RING_TRBS - 1 {
            self.write(self.next, Trb::new(TRB_LINK, self.page.addr(), 0, TOGGLE_CYCLE));
            self.cycle = !self.cycle;
            self.next = 0;
        }
        self.page.addr() + at as u64 * 16
    }

    /// (param, status, control) as they are in memory: for the tests.
    pub fn peek(&self, at: usize) -> Trb {
        let at = at % RING_TRBS * 16;
        Trb {
            param: self.page.read32(at) as u64 | (self.page.read32(at + 4) as u64) << 32,
            status: self.page.read32(at + 8),
            control: self.page.read32(at + 12),
        }
    }
}

/// A transfer ring of several pages, for a camera: each page holds 255
/// TRBs and a link to the next; the last page links back to the first.
pub struct IsoRing {
    pages: Vec<Page>,
    page: usize,
    next: usize,
    cycle: bool,
}

impl IsoRing {
    pub fn new(pages: usize) -> Option<IsoRing> {
        let mut list = Vec::new();
        for _ in 0..pages.max(1) {
            list.push(Page::new()?);
        }
        Some(IsoRing { pages: list, page: 0, next: 0, cycle: true })
    }

    pub fn addr(&self) -> u64 {
        self.pages[0].addr()
    }

    /// How many TRBs it holds at most.
    pub fn capacity(&self) -> usize {
        self.pages.len() * (RING_TRBS - 1)
    }

    /// A TRB as it is in memory: for the tests.
    pub fn peek(&self, page: usize, at: usize) -> Trb {
        let p = &self.pages[page % self.pages.len()];
        let at = at % RING_TRBS * 16;
        Trb { param: p.read32(at) as u64 | (p.read32(at + 4) as u64) << 32, status: p.read32(at + 8), control: p.read32(at + 12) }
    }

    fn write(&self, page: usize, at: usize, trb: Trb) {
        let p = &self.pages[page];
        p.write64(at * 16, trb.param);
        p.write32(at * 16 + 8, trb.status);
        p.write32(at * 16 + 12, trb.control & !CYCLE | self.cycle as u32);
    }

    /// Add a TRB; returns its address.
    pub fn push(&mut self, trb: Trb) -> u64 {
        let (page, at) = (self.page, self.next);
        self.write(page, at, trb);
        self.next += 1;
        if self.next == RING_TRBS - 1 {
            let last = self.page + 1 == self.pages.len();
            let to = if last { 0 } else { self.page + 1 };
            let link = Trb::new(TRB_LINK, self.pages[to].addr(), 0, if last { TOGGLE_CYCLE } else { 0 });
            self.write(page, self.next, link);
            if last {
                self.cycle = !self.cycle;
            }
            self.page = to;
            self.next = 0;
        }
        self.pages[page].addr() + at as u64 * 16
    }
}

/// How often an endpoint is asked for a report: the device's number (for
/// low and full speed in milliseconds, otherwise a power of two of 125 µs)
/// as the controller wants it (a power of two of 125 µs, 3 to 10 for slow devices).
pub fn endpoint_interval(speed: u8, interval: u8) -> u32 {
    match speed {
        SPEED_LOW | SPEED_FULL => {
            let frames = (interval.max(1) as u32) * 8;
            (31 - frames.leading_zeros()).clamp(3, 10)
        }
        _ => (interval.clamp(1, 16) - 1) as u32,
    }
}

const SPEED_FULL: u8 = 1;
const SPEED_LOW: u8 = 2;
const SPEED_HIGH: u8 = 3;

/// Largest packet on the control endpoint before the device has said so.
pub fn default_max_packet(speed: u8) -> u16 {
    match speed {
        SPEED_LOW | SPEED_FULL => 8,
        SPEED_HIGH => 64,
        _ => 512,
    }
}

fn speed_name(speed: u8) -> &'static str {
    match speed {
        SPEED_FULL => "full speed",
        SPEED_LOW => "low speed",
        SPEED_HIGH => "high speed",
        4 => "super speed",
        _ => "unknown speed",
    }
}

// --- registers ---------------------------------------------------------------------

const USBCMD: u64 = 0x00;
const USBSTS: u64 = 0x04;
const PAGESIZE: u64 = 0x08;
const CRCR: u64 = 0x18;
const DCBAAP: u64 = 0x30;
const CONFIG: u64 = 0x38;
const PORTSC: u64 = 0x400;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const STS_HALTED: u32 = 1 << 0;
const STS_NOT_READY: u32 = 1 << 11;

const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_POWER: u32 = 1 << 9;
const PORT_CONNECT_CHANGE: u32 = 1 << 17;
const PORT_RESET_CHANGE: u32 = 1 << 21;
/// Bits that clear when written as 1: all the "something changed" flags.
const PORT_CHANGES: u32 = 0x7F << 17;
/// Bits to write back as they are. Writing 1 to "enabled" would switch the port off.
const PORT_KEEP: u32 = PORT_POWER | 3 << 14 | 7 << 25;

/// What to write to a port register to set `bits` without disturbing the rest.
pub fn port_write(current: u32, bits: u32) -> u32 {
    current & PORT_KEEP | bits
}

/// Intel chipsets (7, 8 and 9 series) wire their ports to the old USB 2
/// controller until told otherwise: these registers move them to xHCI.
const INTEL_SWITCHABLE: &[u16] = &[0x1e31, 0x8c31, 0x9c31, 0x8cb1, 0x9cb1];
const INTEL_USB2_ROUTE: u8 = 0xD0;
const INTEL_USB2_ROUTE_MASK: u8 = 0xD4;
const INTEL_USB3_ENABLE: u8 = 0xD8;
const INTEL_USB3_ENABLE_MASK: u8 = 0xDC;

struct Endpoint {
    /// The controller's number for it: endpoint number * 2 + 1 (towards us).
    dci: u8,
    kind: Kind,
    ring: Ring,
    buffer: Page,
    length: u16,
    last_keys: [u8; 8],
    /// The held key and when it repeats next.
    repeat: Option<(u8, u64)>,
    failed: bool,
}

struct Device {
    slot: u8,
    port: u8,
    speed: u8,
    context: Page,
    input: Page,
    control: Ring,
    buffer: Page,
    endpoints: Vec<Endpoint>,
    /// Reports received: shows in `lsusb` that a device really talks to us.
    reports: u32,
}

/// A camera sending pictures: an isochronous endpoint kept full of transfers.
struct Video {
    slot: u8,
    dci: u8,
    interface: u8,
    ring: IsoRing,
    pages: Vec<Page>,
    /// Bytes per transfer (one (micro)frame), and how many fit in a page.
    td_len: usize,
    per_page: usize,
    /// Packets per transfer, minus one (for the TRB).
    burst: u32,
    /// Transfers handed to the controller, oldest first: (TRB address, buffer).
    queued: VecDeque<(u64, usize)>,
    frames: uvc::Frames,
    kind: uvc::Kind,
    width: u16,
    height: u16,
    seq: u32,
    /// New transfers were queued: ring the doorbell.
    ring_bell: bool,
    stats: usb::CameraStats,
}

/// Transfers kept queued for a camera: at 8000 a second, an eighth of a second.
const VIDEO_TDS: usize = 1000;

impl Video {
    fn buffer(&self, n: usize) -> (u64, &Page, usize) {
        let page = &self.pages[n / self.per_page];
        let offset = n % self.per_page * self.td_len;
        (page.addr() + offset as u64, page, offset)
    }

    fn queue(&mut self, n: usize) {
        let (addr, _, _) = self.buffer(n);
        let flags = INTERRUPT_ON_COMPLETION | START_ASAP | self.burst << 16;
        let trb = self.ring.push(Trb::new(TRB_ISOCH, addr, self.td_len as u32, flags));
        self.queued.push_back((trb, n));
        self.ring_bell = true;
    }

    /// A transfer finished (and the ones before it, if they gave no event).
    fn completed(&mut self, trb: u64, code: u8, left: u32) {
        let Some(pos) = self.queued.iter().position(|q| q.0 == trb) else {
            self.stats.lost += 1;
            return;
        };
        for i in 0..=pos {
            let Some((_, n)) = self.queued.pop_front() else { break };
            self.stats.transfers += 1;
            if i < pos {
                self.stats.lost += 1;
            } else if code == CODE_SUCCESS || code == CODE_SHORT_PACKET {
                let got = self.td_len.saturating_sub(left as usize);
                self.stats.bytes += got as u64;
                let (_, page, offset) = self.buffer(n);
                let packet = page.bytes_at(offset, got);
                self.frames.push(&packet);
            } else {
                self.stats.errors += 1;
                self.stats.last_error = code;
            }
            self.queue(n);
        }
        self.stats.pictures = self.frames.finished;
        self.stats.damaged = self.frames.dropped;
        if let Some(data) = self.frames.take() {
            if self.kind == uvc::Kind::Mjpeg && !beenet::jpeg::is_jpeg(&data) {
                self.stats.not_jpeg += 1;
                return;
            }
            self.seq += 1;
            *usb::CAMERA_PICTURE.get() =
                Some(usb::Picture { seq: self.seq, kind: self.kind, width: self.width, height: self.height, data });
        }
    }
}

struct Xhci {
    op: u64,
    runtime: u64,
    doorbells: u64,
    context_size: usize,
    ports: u8,
    dcbaa: Page,
    _scratchpad: Vec<Page>,
    commands: Ring,
    events: EventBlock,
    _event_table: Page,
    event_next: usize,
    event_cycle: bool,
    devices: Vec<Device>,
    /// Ports whose device could not be set up: left alone until unplugged.
    failed: Vec<(u8, DeviceInfo)>,
    infos: Vec<DeviceInfo>,
    command_done: Option<(u8, u8)>,
    control_done: Option<(u8, u8, u32)>,
    control_short: Option<u32>,
    ports_changed: bool,
    /// Mouse movement since the last tick: buttons, right, down.
    mouse: Option<(u8, i32, i32)>,
    /// The list `lsusb` shows is out of date.
    stale: bool,
    video: Option<Video>,
    /// Camera buffers the controller did not confirm it let go of: never freed.
    kept: Vec<Video>,
    /// Switching the camera on failed: not tried again until it is switched off.
    video_failed: bool,
}

fn read32(addr: u64) -> u32 {
    unsafe { (addr as *const u32).read_volatile() }
}

fn write32(addr: u64, value: u32) {
    unsafe { (addr as *mut u32).write_volatile(value) }
}

fn write64(addr: u64, value: u64) {
    write32(addr, value as u32);
    write32(addr + 4, (value >> 32) as u32);
}

/// Wait until `done()` holds, at most `ms` milliseconds; other threads run meanwhile.
fn wait_for(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = time::uptime_ms() + ms + 10; // the clock ticks every 10 ms
    loop {
        if done() {
            return true;
        }
        if time::uptime_ms() >= end {
            return false;
        }
        thread::yield_now();
        core::hint::spin_loop();
    }
}

/// Take the controller over from the firmware (which may be using it to
/// imitate a PS/2 keyboard) and stop the firmware's own interrupts.
fn take_from_firmware(base: u64, capabilities: u32) {
    let mut offset = (capabilities >> 16) as u64 * 4;
    for _ in 0..64 {
        if offset == 0 {
            return;
        }
        let cap = read32(base + offset);
        if cap & 0xFF == 1 {
            write32(base + offset, cap | 1 << 24); // "the OS owns it"
            wait_for(1000, || read32(base + offset) & 1 << 16 == 0);
            let control = read32(base + offset + 4);
            write32(base + offset + 4, control & (7 << 1 | 0xFF << 5 | 7 << 17) | 7 << 29);
            return;
        }
        let next = (cap >> 8 & 0xFF) as u64 * 4;
        if next == 0 {
            return;
        }
        offset += next;
    }
}

impl Xhci {
    fn start(card: &pci::Device) -> Result<Xhci, String> {
        let Bar::Memory { addr: base, .. } = card.bar(0) else { return Err(String::from("no memory area")) };
        if base == 0 || base + 0x1_0000 > BELOW_4G {
            return Err(String::from("its registers are above 4 GiB"));
        }
        card.enable(pci::CMD_MEMORY | pci::CMD_BUS_MASTER);
        let cap_length = read32(base) & 0xFF;
        let structure1 = read32(base + 0x04);
        let structure2 = read32(base + 0x08);
        let capabilities = read32(base + 0x10);
        let op = base + cap_length as u64;
        let doorbells = base + (read32(base + 0x14) & !3) as u64;
        let runtime = base + (read32(base + 0x18) & !0x1F) as u64;
        let (slots, ports) = (structure1 & 0xFF, (structure1 >> 24) as u8);
        if cap_length < 0x20 || slots == 0 || ports == 0 {
            return Err(String::from("its registers make no sense"));
        }
        take_from_firmware(base, capabilities);

        // Stop, reset, and wait until it is ready.
        write32(op + USBCMD, read32(op + USBCMD) & !CMD_RUN);
        if !wait_for(200, || read32(op + USBSTS) & STS_HALTED != 0) {
            return Err(String::from("it does not stop"));
        }
        write32(op + USBCMD, CMD_RESET);
        if !wait_for(1000, || read32(op + USBCMD) & CMD_RESET == 0 && read32(op + USBSTS) & STS_NOT_READY == 0) {
            return Err(String::from("it does not finish its reset"));
        }
        if read32(op + PAGESIZE) & 1 == 0 {
            return Err(String::from("it does not work with 4 KiB pages"));
        }
        let no_memory = || String::from("no memory below 4 GiB");
        write32(op + CONFIG, slots);

        // The table of device contexts; its first place lists pages the
        // controller wants for itself.
        let dcbaa = Page::new().ok_or_else(no_memory)?;
        let scratch_count = ((structure2 >> 21 & 0x1F) << 5 | structure2 >> 27 & 0x1F) as usize;
        let mut scratchpad = Vec::new();
        if scratch_count > 0 {
            if scratch_count > PAGE / 8 {
                return Err(String::from("it wants too much memory for itself"));
            }
            let list = Page::new().ok_or_else(no_memory)?;
            for i in 0..scratch_count {
                let page = Page::new().ok_or_else(no_memory)?;
                list.write64(i * 8, page.addr());
                scratchpad.push(page);
            }
            dcbaa.write64(0, list.addr());
            scratchpad.push(list);
        }
        write64(op + DCBAAP, dcbaa.addr());

        let commands = Ring::new().ok_or_else(no_memory)?;
        write64(op + CRCR, commands.addr() | 1);

        // The event ring: one block, described by a one-line table.
        let events = EventBlock::new().ok_or_else(no_memory)?;
        let event_table = Page::new().ok_or_else(no_memory)?;
        event_table.write64(0, events.addr());
        event_table.write32(8, EVENT_TRBS as u32);
        let interrupter = runtime + 0x20;
        write32(interrupter + 0x08, 1);
        write64(interrupter + 0x18, events.addr());
        write64(interrupter + 0x10, event_table.addr());

        write32(op + USBCMD, CMD_RUN);
        if !wait_for(200, || read32(op + USBSTS) & STS_HALTED == 0) {
            return Err(String::from("it does not start"));
        }
        if card.vendor == 0x8086 && INTEL_SWITCHABLE.contains(&card.device) {
            card.config_write(INTEL_USB3_ENABLE, card.config_read(INTEL_USB3_ENABLE_MASK));
            card.config_write(INTEL_USB2_ROUTE, card.config_read(INTEL_USB2_ROUTE_MASK));
        }
        Ok(Xhci {
            op,
            runtime,
            doorbells,
            context_size: if capabilities & 1 << 2 != 0 { 64 } else { 32 },
            ports,
            dcbaa,
            _scratchpad: scratchpad,
            commands,
            events,
            _event_table: event_table,
            event_next: 0,
            event_cycle: true,
            devices: Vec::new(),
            failed: Vec::new(),
            infos: Vec::new(),
            command_done: None,
            control_done: None,
            control_short: None,
            ports_changed: true,
            mouse: None,
            stale: true,
            video: None,
            kept: Vec::new(),
            video_failed: false,
        })
    }

    fn port_register(&self, port: u8) -> u64 {
        self.op + PORTSC + (port as u64 - 1) * 0x10
    }

    fn ring_doorbell(&self, slot: u8, target: u32) {
        write32(self.doorbells + slot as u64 * 4, target);
    }

    /// Read what the controller reported since last time.
    fn process_events(&mut self) {
        let mut seen = false;
        // At most one lap: a controller gone wrong cannot keep us here.
        for _ in 0..EVENT_TRBS {
            let at = self.event_next * 16;
            let control = self.events.read32(at + 12);
            if (control & CYCLE != 0) != self.event_cycle {
                break;
            }
            let event = Trb {
                param: self.events.read32(at) as u64 | (self.events.read32(at + 4) as u64) << 32,
                status: self.events.read32(at + 8),
                control,
            };
            seen = true;
            self.event_next += 1;
            if self.event_next == EVENT_TRBS {
                self.event_next = 0;
                self.event_cycle = !self.event_cycle;
            }
            match event.kind() {
                EVENT_COMMAND => self.command_done = Some((event.code(), event.slot())),
                EVENT_PORT => self.ports_changed = true,
                EVENT_TRANSFER => {
                    let (endpoint, left) = ((event.control >> 16 & 0x1F) as u8, event.status & 0xFF_FFFF);
                    let video = self.video.as_mut().filter(|v| v.slot == event.slot() && v.dci == endpoint);
                    if let Some(video) = video {
                        if event.code() == CODE_RING_UNDERRUN || event.code() == CODE_RING_OVERRUN {
                            video.ring_bell = true; // ran dry: start again when refilled
                            video.stats.overruns += 1;
                        } else {
                            video.completed(event.param, event.code(), left);
                        }
                    } else if endpoint != 1 {
                        self.on_report(event.slot(), endpoint, event.code(), left);
                    } else if event.code() == CODE_SHORT_PACKET && self.control_short.is_none() {
                        // The data stage came up short; the status stage follows.
                        self.control_short = Some(left);
                    } else {
                        self.control_done = Some((event.slot(), event.code(), self.control_short.take().unwrap_or(0)));
                    }
                }
                _ => {}
            }
        }
        if seen {
            // Tell the controller how far we read (bit 3 clears its "busy" flag).
            write64(self.runtime + 0x20 + 0x18, self.events.addr() + self.event_next as u64 * 16 | 1 << 3);
        }
    }

    /// A report arrived from a keyboard or mouse (or the transfer failed).
    fn on_report(&mut self, slot: u8, dci: u8, code: u8, left: u32) {
        let Some(device) = self.devices.iter_mut().find(|d| d.slot == slot) else { return };
        let Some(ep) = device.endpoints.iter_mut().find(|e| e.dci == dci) else { return };
        if code != CODE_SUCCESS && code != CODE_SHORT_PACKET {
            ep.failed = true; // stalled or unplugged: stop asking
            return;
        }
        device.reports = device.reports.wrapping_add(1);
        self.stale = true;
        let got = (ep.length as usize).saturating_sub(left as usize);
        let data = ep.buffer.bytes(got);
        match ep.kind {
            Kind::Keyboard if data.len() >= 3 => {
                let mut report = [0u8; 8];
                let n = data.len().min(8);
                report[..n].copy_from_slice(&data[..n]);
                for code in usb::keyboard_changes(&ep.last_keys, &report) {
                    keyboard::push_scancode(code);
                }
                let held = usb::repeat_key(&report);
                if held != ep.repeat.map(|(key, _)| key) {
                    ep.repeat = held.map(|key| (key, time::uptime_ms() + usb::REPEAT_DELAY_MS));
                }
                ep.last_keys = report;
            }
            Kind::Mouse => {
                if let Some((buttons, dx, dy)) = usb::mouse_report(&data) {
                    let (_, x, y) = self.mouse.unwrap_or((0, 0, 0));
                    self.mouse = Some((buttons, x + dx, y + dy));
                }
            }
            _ => {}
        }
        // Ask for the next report.
        ep.ring.push(Trb::new(TRB_NORMAL, ep.buffer.addr(), ep.length as u32, INTERRUPT_ON_COMPLETION));
        write32(self.doorbells + slot as u64 * 4, dci as u32);
    }

    /// Run a command; returns the slot number the controller answers with.
    fn command(&mut self, trb: Trb) -> Result<u8, &'static str> {
        self.command_done = None;
        self.commands.push(trb);
        self.ring_doorbell(0, 0);
        let mut result = None;
        wait_for(1000, || {
            self.process_events();
            result = self.command_done.take();
            result.is_some()
        });
        match result {
            Some((CODE_SUCCESS, slot)) => Ok(slot),
            Some(_) => Err("the controller refused a command"),
            None => Err("the controller did not answer a command"),
        }
    }

    /// A control transfer to the device at `index` in `self.devices`. Data
    /// travels through the device's buffer page; returns how many bytes moved.
    fn control(&mut self, index: usize, request_type: u8, request: u8, value: u16, at: u16, length: u16) -> Result<usize, &'static str> {
        let length = length.min(PAGE as u16);
        let device = &mut self.devices[index];
        let slot = device.slot;
        for trb in control_trbs(request_type, request, value, at, device.buffer.addr(), length) {
            device.control.push(trb);
        }
        self.control_done = None;
        self.control_short = None;
        self.ring_doorbell(slot, 1);
        let mut result = None;
        wait_for(1000, || {
            self.process_events();
            result = self.control_done.take().filter(|(s, _, _)| *s == slot);
            result.is_some()
        });
        match result {
            Some((_, CODE_SUCCESS | CODE_SHORT_PACKET, left)) => Ok((length as usize).saturating_sub(left as usize)),
            Some(_) => Err("the device refused a request"),
            None => Err("the device did not answer"),
        }
    }

    /// Ask for a descriptor (type 1: device, 2: configuration).
    fn descriptor(&mut self, index: usize, kind: u8, length: u16) -> Result<Vec<u8>, &'static str> {
        let got = self.control(index, 0x80, 6, (kind as u16) << 8, 0, length)?;
        Ok(self.devices[index].buffer.bytes(got))
    }

    /// Fill in the slot part and the control endpoint of the input context.
    fn write_input(&self, device: &Device, add: u32, entries: u32, max_packet: u16) {
        let cs = self.context_size;
        device.input.clear();
        device.input.write32(4, add);
        device.input.write32(cs, (device.speed as u32) << 20 | entries << 27);
        device.input.write32(cs + 4, (device.port as u32) << 16);
        let ep0 = cs * 2;
        device.input.write32(ep0 + 4, 3 << 1 | 4 << 3 | (max_packet as u32) << 16);
        device.input.write64(ep0 + 8, device.control.addr() | device.control.cycle as u64);
        device.input.write32(ep0 + 16, 8);
    }

    /// A device appeared on a port: give it an address, read what it is,
    /// and start its keyboard or mouse.
    fn attach(&mut self, port: u8) -> Result<(), (&'static str, DeviceInfo)> {
        let mut info = DeviceInfo {
            port,
            speed: "unknown speed",
            vendor: 0,
            product: 0,
            what: String::from("device"),
            state: String::new(),
        };
        macro_rules! check {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => return Err((e, info)),
                }
            };
        }
        let register = self.port_register(port);
        if read32(register) & PORT_ENABLED == 0 {
            // USB 2 ports need a reset before the device answers.
            write32(register, port_write(read32(register), PORT_RESET));
            wait_for(500, || read32(register) & (PORT_RESET_CHANGE | PORT_ENABLED) != 0);
            write32(register, port_write(read32(register), PORT_RESET_CHANGE));
        }
        thread::sleep_ms(20); // the device may take a moment after a reset
        let status = read32(register);
        if status & PORT_CONNECTED == 0 || status & PORT_ENABLED == 0 {
            return Err(("the port did not come up", info));
        }
        let speed = (status >> 10 & 0xF) as u8;
        info.speed = speed_name(speed);

        let slot = check!(self.command(Trb::new(TRB_ENABLE_SLOT, 0, 0, 0)));
        let pages = (Page::new(), Page::new(), Ring::new(), Page::new());
        let (Some(context), Some(input), Some(control), Some(buffer)) = pages else {
            return Err(("no memory below 4 GiB", info));
        };
        let device = Device { slot, port, speed, context, input, control, buffer, endpoints: Vec::new(), reports: 0 };
        let mut max_packet = default_max_packet(speed);
        self.write_input(&device, 0b11, 1, max_packet);
        self.dcbaa.write64(slot as usize * 8, device.context.addr());
        let input_addr = device.input.addr();
        self.devices.push(device);
        let index = self.devices.len() - 1;
        // From here on a failure must give the slot back: see `attach_or_drop`.
        check!(self.command(Trb::new(TRB_ADDRESS_DEVICE, input_addr, 0, (slot as u32) << 24)));

        // The first 8 bytes say how large packets on the control endpoint may be.
        let head = check!(self.descriptor(index, 1, 8));
        if head.len() < 8 {
            return Err(("the device's description is cut short", info));
        }
        if speed == SPEED_FULL && matches!(head[7], 16 | 32 | 64) && head[7] as u16 != max_packet {
            max_packet = head[7] as u16;
            self.write_input(&self.devices[index], 0b10, 1, max_packet);
            check!(self.command(Trb::new(TRB_EVALUATE_CONTEXT, input_addr, 0, (slot as u32) << 24)));
        }
        let descriptor = check!(self.descriptor(index, 1, 18));
        if descriptor.len() < 18 {
            return Err(("the device's description is cut short", info));
        }
        info.vendor = u16::from_le_bytes([descriptor[8], descriptor[9]]);
        info.product = u16::from_le_bytes([descriptor[10], descriptor[11]]);
        let device_class = descriptor[4];

        let head = check!(self.descriptor(index, 2, 9));
        if head.len() < 9 {
            return Err(("the device's configuration is cut short", info));
        }
        let total = u16::from_le_bytes([head[2], head[3]]).clamp(9, PAGE as u16);
        let raw = check!(self.descriptor(index, 2, total));
        let config = usb::parse_config(&raw);
        if let Some(camera) = beenet::uvc::parse(&raw) {
            usb::CAMERAS.get().retain(|c| c.port != port);
            usb::CAMERAS.get().push(usb::CameraInfo { port, vendor: info.vendor, product: info.product, camera, raw });
        }
        let mut names: Vec<&str> = config.classes.iter().map(|&(c, s, p)| usb::class_name(c, s, p)).collect();
        names.dedup();
        info.what = match (names.is_empty(), device_class) {
            (false, _) => names.join(" + "),
            (true, usb::CLASS_HUB) => String::from("hub"),
            (true, _) => String::from("device"),
        };
        if config.interfaces.is_empty() {
            let hub = device_class == usb::CLASS_HUB || config.classes.iter().any(|c| c.0 == usb::CLASS_HUB);
            info.state = String::from(if hub { "hubs are not supported: plug devices into the machine itself" } else { "no driver" });
            self.infos.push(info);
            return Ok(());
        }
        check!(self.control(index, 0x00, 9, config.value as u16, 0, 0)); // select the configuration
        let mut entries = 1u32;
        let mut add = 1u32;
        let mut endpoints = Vec::new();
        for HidInterface { number, kind, endpoint, max_packet, interval } in config.interfaces.iter().copied().take(4) {
            // Boot protocol (simple fixed reports), and reports only when something changes.
            check!(self.control(index, 0x21, 0x0B, 0, number as u16, 0));
            let _ = self.control(index, 0x21, 0x0A, 0, number as u16, 0);
            let dci = (endpoint & 0x0F) * 2 + 1;
            let (Some(ring), Some(buffer)) = (Ring::new(), Page::new()) else {
                return Err(("no memory below 4 GiB", info));
            };
            let length = max_packet.min(64);
            if add & 1 << dci != 0 {
                continue; // two interfaces naming one endpoint: keep the first
            }
            add |= 1 << dci;
            entries = entries.max(dci as u32);
            let cs = self.context_size;
            let device = &self.devices[index];
            if endpoints.is_empty() {
                self.write_input(device, 0, entries, max_packet_of(device, cs));
            }
            let at = cs * (dci as usize + 1);
            device.input.write32(at, endpoint_interval(speed, interval) << 16);
            device.input.write32(at + 4, 3 << 1 | 7 << 3 | (length as u32) << 16);
            device.input.write64(at + 8, ring.addr() | 1);
            device.input.write32(at + 16, length as u32 | (length as u32) << 16);
            endpoints.push(Endpoint { dci, kind, ring, buffer, length, last_keys: [0; 8], repeat: None, failed: false });
        }
        let cs = self.context_size;
        let device = &self.devices[index];
        device.input.write32(4, add);
        device.input.write32(cs, (speed as u32) << 20 | entries << 27);
        check!(self.command(Trb::new(TRB_CONFIGURE_ENDPOINT, input_addr, 0, (slot as u32) << 24)));
        for ep in endpoints.iter_mut() {
            ep.ring.push(Trb::new(TRB_NORMAL, ep.buffer.addr(), ep.length as u32, INTERRUPT_ON_COMPLETION));
            self.ring_doorbell(slot, ep.dci as u32);
        }
        self.devices[index].endpoints = endpoints;
        info.state = String::from("in use");
        self.infos.push(info);
        Ok(())
    }

    /// Switch the first camera on: agree on a format and size with it,
    /// pick the alternate setting with enough bandwidth, and fill its
    /// endpoint with transfers. Returns what it sends.
    fn start_video(&mut self, want: (u16, u16)) -> Result<String, String> {
        let info = usb::CAMERAS.get().first().cloned().ok_or("no camera plugged in")?;
        let index = self.devices.iter().position(|d| d.port == info.port).ok_or("the camera is not set up")?;
        let camera = &info.camera;
        let (kind, format, frame) = match camera.pick(uvc::Kind::Mjpeg, want.0, want.1) {
            Some((f, fr)) => (uvc::Kind::Mjpeg, f.clone(), fr.clone()),
            None => {
                let (f, fr) = camera.pick(uvc::Kind::Yuy2, want.0, want.1).ok_or("the camera offers neither MJPEG nor YUY2")?;
                (uvc::Kind::Yuy2, f.clone(), fr.clone())
            }
        };
        let interface = camera.streaming_interface;
        usb::note(&format!("start: {:?} {}x{} (format {}, frame {})", kind, frame.width, frame.height, format.index, frame.index));
        self.control(index, 0x00, 9, camera.configuration as u16, 0, 0)?;
        usb::note("configuration set");
        // Probe: propose; the camera answers with what it will really do. Then commit.
        let mut block = uvc::probe(camera.version, format.index, frame.index, frame.default_interval);
        let len = block.len() as u16;
        self.devices[index].buffer.write_bytes(&block);
        self.control(index, 0x21, 0x01, 0x0100, interface as u16, len)?;
        let got = self.control(index, 0xA1, 0x81, 0x0100, interface as u16, len)?;
        let answer = self.devices[index].buffer.bytes(got);
        let (max_frame, payload) = uvc::negotiated(&answer).ok_or("the camera's answer is cut short")?;
        usb::note(&format!("probe: {got} bytes back, largest picture {max_frame}, payload {payload}"));
        let n = got.min(block.len()).min(answer.len());
        block[..n].copy_from_slice(&answer[..n]);
        self.devices[index].buffer.write_bytes(&block);
        self.control(index, 0x21, 0x01, 0x0200, interface as u16, len)?;
        usb::note("committed");
        let alt = camera.alt_for(payload).ok_or("the camera has no isochronous setting")?;
        self.control(index, 0x01, 11, alt.setting as u16, interface as u16, 0)?;
        usb::note(&format!("setting {} ({} x {} bytes)", alt.setting, alt.per_frame, alt.packet));

        let td_len = alt.bandwidth() as usize;
        let per_page = (PAGE / td_len.max(1)).max(1);
        let mut pages = Vec::new();
        for _ in 0..VIDEO_TDS.div_ceil(per_page) {
            pages.push(Page::new().ok_or("no memory below 4 GiB")?);
        }
        let ring = IsoRing::new(VIDEO_TDS.div_ceil(RING_TRBS - 1) + 1).ok_or("no memory below 4 GiB")?;
        let dci = (alt.endpoint & 0x0F) * 2 + 1;
        let cs = self.context_size;
        let device = &self.devices[index];
        let (slot, speed) = (device.slot, device.speed);
        let entries = (device.context.read32(0) >> 27).max(dci as u32);
        self.write_input(device, 0, entries, max_packet_of(device, cs));
        // Drop the endpoint first if a previous run left it enabled.
        let enabled = device.context.read32(cs * dci as usize) & 7 != 0;
        device.input.write32(0, if enabled { 1 << dci } else { 0 });
        device.input.write32(4, 1 | 1 << dci);
        let at = cs * (dci as usize + 1);
        let bandwidth = alt.bandwidth();
        device.input.write32(at, endpoint_interval(speed, alt.interval) << 16 | (bandwidth >> 16) << 24);
        device.input.write32(at + 4, 5 << 3 | (alt.per_frame as u32 - 1) << 8 | (alt.packet as u32) << 16);
        device.input.write64(at + 8, ring.addr() | 1);
        device.input.write32(at + 16, bandwidth & 0xFFFF | (bandwidth & 0xFFFF) << 16);
        let input_addr = device.input.addr();
        if let Err(e) = self.command(Trb::new(TRB_CONFIGURE_ENDPOINT, input_addr, 0, (slot as u32) << 24)) {
            usb::note(&format!("configure endpoint: {e}"));
            let _ = self.control(index, 0x01, 11, 0, interface as u16, 0);
            return Err(String::from(e));
        }
        usb::note(&format!("endpoint {dci} configured, {VIDEO_TDS} transfers of {td_len} bytes"));
        let mut video = Video {
            slot,
            dci,
            interface,
            ring,
            pages,
            td_len,
            per_page,
            burst: alt.per_frame as u32 - 1,
            queued: VecDeque::new(),
            frames: uvc::Frames::new((max_frame as usize).clamp(64 * 1024, 8 * 1024 * 1024)),
            kind,
            width: frame.width,
            height: frame.height,
            seq: 0,
            ring_bell: false,
            stats: usb::CameraStats::default(),
        };
        for n in 0..VIDEO_TDS {
            video.queue(n);
        }
        self.ring_doorbell(slot, dci as u32);
        video.ring_bell = false;
        self.video = Some(video);
        let name = if kind == uvc::Kind::Mjpeg { "MJPEG" } else { "YUY2" };
        Ok(format!(
            "{}x{} {name}, {}/s, setting {} ({} bytes per microframe)",
            frame.width,
            frame.height,
            uvc::frame_rate(frame.default_interval),
            alt.setting,
            bandwidth
        ))
    }

    /// Switch the camera off: back to its zero-bandwidth setting, and the
    /// endpoint dropped (the controller lets go of our buffers).
    /// The buffers are given back only when the controller confirmed it let
    /// go of them; otherwise they are kept for ever (memory the controller
    /// may still write into must never be handed out again).
    fn stop_video(&mut self) {
        let Some(video) = self.video.take() else { return };
        let Some(index) = self.devices.iter().position(|d| d.slot == video.slot) else {
            self.kept.push(video);
            return;
        };
        let (slot, dci) = (video.slot, video.dci);
        let stopped = self.command(Trb::new(TRB_STOP_ENDPOINT, 0, 0, (slot as u32) << 24 | (dci as u32) << 16)).is_ok();
        usb::note(&format!("stop: endpoint stopped {stopped}; {}", video.stats.describe()));
        let _ = self.control(index, 0x01, 11, 0, video.interface as u16, 0);
        let cs = self.context_size;
        let device = &self.devices[index];
        let entries = device.context.read32(0) >> 27;
        self.write_input(device, 0, entries, max_packet_of(device, cs));
        device.input.write32(0, 1 << dci);
        device.input.write32(4, 1);
        let input_addr = device.input.addr();
        let dropped = self.command(Trb::new(TRB_CONFIGURE_ENDPOINT, input_addr, 0, (slot as u32) << 24)).is_ok();
        usb::note(&format!("stop: endpoint dropped {dropped}"));
        if !(stopped || dropped) {
            self.kept.push(video);
        }
    }

    /// Follow `usb::CAMERA_WANT`.
    fn video_tick(&mut self) {
        let want = *usb::CAMERA_WANT.get();
        match (want, self.video.is_some()) {
            (Some(size), false) if !self.video_failed || *usb::CAMERA_STATE.get() == usb::CameraState::Off => {
                self.video_failed = false;
                *usb::CAMERA_PICTURE.get() = None;
                *usb::CAMERA_STATS.get() = usb::CameraStats::default();
                match self.start_video(size) {
                    Ok(what) => *usb::CAMERA_STATE.get() = usb::CameraState::On(what),
                    Err(why) => {
                        self.video_failed = true;
                        *usb::CAMERA_STATE.get() = usb::CameraState::Failed(why);
                    }
                }
            }
            (None, true) => {
                self.stop_video();
                *usb::CAMERA_STATE.get() = usb::CameraState::Off;
            }
            (None, false) => {
                self.video_failed = false;
                if *usb::CAMERA_STATE.get() != usb::CameraState::Off {
                    *usb::CAMERA_STATE.get() = usb::CameraState::Off;
                }
            }
            _ => {}
        }
        if let Some(video) = self.video.as_mut() {
            *usb::CAMERA_STATS.get() = video.stats;
            if video.ring_bell {
                video.ring_bell = false;
                let (slot, dci) = (video.slot, video.dci);
                self.ring_doorbell(slot, dci as u32);
            }
        }
    }

    /// Give a device's slot back and forget it; keys it held are released.
    fn detach(&mut self, port: u8) {
        self.infos.retain(|i| i.port != port);
        usb::CAMERAS.get().retain(|c| c.port != port);
        self.failed.retain(|(p, _)| *p != port);
        let Some(index) = self.devices.iter().position(|d| d.port == port) else { return };
        let slot = self.devices[index].slot;
        let video = if self.video.as_ref().is_some_and(|v| v.slot == slot) { self.video.take() } else { None };
        if video.is_some() {
            *usb::CAMERA_STATE.get() = usb::CameraState::Failed(String::from("the camera was unplugged"));
            self.video_failed = true;
        }
        for ep in self.devices[index].endpoints.iter() {
            for code in usb::keyboard_changes(&ep.last_keys, &[0; 8]) {
                keyboard::push_scancode(code);
            }
        }
        let disabled = self.command(Trb::new(TRB_DISABLE_SLOT, 0, 0, (slot as u32) << 24)).is_ok();
        if let Some(video) = video {
            // The slot is gone, and with it the endpoint: only then are the buffers free.
            if !disabled {
                self.kept.push(video);
            }
        }
        self.dcbaa.write64(slot as usize * 8, 0);
        self.devices.remove(index);
    }

    /// Look at every port: set up what was plugged in, drop what was pulled out.
    fn scan_ports(&mut self) {
        for port in 1..=self.ports {
            let register = self.port_register(port);
            let status = read32(register);
            if status & PORT_CHANGES != 0 {
                write32(register, port_write(status, status & PORT_CHANGES));
            }
            let connected = status & PORT_CONNECTED != 0;
            let known = self.devices.iter().any(|d| d.port == port) || self.failed.iter().any(|(p, _)| *p == port);
            if known && (!connected || status & PORT_CONNECT_CHANGE != 0) {
                if usb::CAMERAS.get().iter().any(|c| c.port == port) {
                    usb::note(&format!("port {port}: camera gone (port status {status:#x})"));
                }
                self.detach(port);
            }
            let known = self.devices.iter().any(|d| d.port == port) || self.failed.iter().any(|(p, _)| *p == port);
            if connected && !known {
                let before = self.devices.len();
                if let Err((why, mut info)) = self.attach(port) {
                    if self.devices.len() > before {
                        self.detach(port); // gives the slot back
                    }
                    info.state = format!("not usable: {why}");
                    self.failed.push((port, info));
                }
            }
        }
        self.stale = true;
    }

    /// Bring the list `lsusb` shows up to date.
    fn publish(&mut self) {
        self.stale = false;
        let mut all = self.infos.clone();
        for info in all.iter_mut() {
            if let Some(device) = self.devices.iter().find(|d| d.port == info.port && !d.endpoints.is_empty()) {
                let stopped = device.endpoints.iter().any(|e| e.failed);
                info.state = format!("{}, {} reports", if stopped { "stopped after an error" } else { "in use" }, device.reports);
            }
        }
        all.extend(self.failed.iter().map(|(_, info)| info.clone()));
        all.sort_by_key(|i| i.port);
        *usb::DEVICES.get() = all;
    }

    /// Once per timer tick.
    fn tick(&mut self, now: u64) {
        self.process_events();
        self.video_tick();
        if self.ports_changed {
            self.ports_changed = false;
            self.scan_ports();
        }
        for ep in self.devices.iter_mut().flat_map(|d| d.endpoints.iter_mut()) {
            if let Some((key, when)) = ep.repeat {
                if now >= when && !ep.failed {
                    if let Some((prefix, code)) = usb::usage_to_scancode(key) {
                        if prefix {
                            keyboard::push_scancode(0xE0);
                        }
                        keyboard::push_scancode(code);
                    }
                    ep.repeat = Some((key, now + usb::REPEAT_EVERY_MS));
                }
            }
        }
        if let Some((buttons, dx, dy)) = self.mouse.take() {
            console::mouse_event(buttons, dx, dy);
        }
        if self.stale {
            self.publish();
        }
    }
}

/// The control endpoint's packet size as the controller has it now.
fn max_packet_of(device: &Device, context_size: usize) -> u16 {
    (device.context.read32(context_size + 4) >> 16) as u16
}

/// Is this PCI device an xHCI controller? (serial bus, USB, xHCI)
pub fn is_xhci(class: u8, subclass: u8, prog_if: u8) -> bool {
    (class, subclass, prog_if) == (0x0C, 0x03, 0x30)
}

/// Find the controller and start the "usb" thread, which does everything
/// else. Returns a line for the boot log, or nothing without a controller.
pub fn start() -> Option<String> {
    let card = pci::scan().into_iter().find(|d| is_xhci(d.class, d.subclass, d.prog_if()))?;
    let name = format!("xHCI controller {:04x}:{:04x}", card.vendor, card.device);
    if crate::bootargs::value("usb") == Some("off") {
        *usb::STATUS.get() = Some(format!("{name}: not used (usb=off)"));
        return Some(format!("{name}: not used (usb=off)"));
    }
    *usb::STATUS.get() = Some(format!("{name}: starting"));
    let thread_name = name.clone();
    thread::spawn("usb", None, move || {
        let mut controller = match Xhci::start(&card) {
            Ok(controller) => controller,
            Err(why) => {
                *usb::STATUS.get() = Some(format!("{thread_name}: not usable: {why}"));
                *usb::READY.get() = true;
                return;
            }
        };
        *usb::STATUS.get() = Some(format!("{thread_name}, {} ports", controller.ports));
        let mut last_scan = 0;
        loop {
            let now = time::uptime_ms();
            if now >= last_scan + 1000 {
                // Also look without being told: a missed event must not lose a keyboard.
                controller.ports_changed = true;
                last_scan = now;
            }
            controller.tick(now);
            *usb::READY.get() = true;
            // Wait for the next tick, also while a camera sends: its ring holds
            // an eighth of a second. (Never `yield_now` in a loop: with nothing
            // else to run it does not switch, and the watchdog rightly calls
            // that a hang. It restarted the HP twice.)
            let want = usb::CAMERA_WANT.get().is_some();
            thread::wait_until(|| time::uptime_ms() != now || usb::CAMERA_WANT.get().is_some() != want);
        }
    })?;
    // Wait for the first look at the ports, so a USB keyboard works when the
    // prompt appears. A controller that hangs costs three seconds, once.
    let give_up = time::uptime_ms() + 3000;
    thread::wait_until(|| *usb::READY.get() || time::uptime_ms() >= give_up);
    let status = usb::STATUS.get().clone().unwrap_or(name);
    let devices = usb::DEVICES.get().len();
    Some(format!("{status}, {devices} device{} ('lsusb' shows them)", if devices == 1 { "" } else { "s" }))
}
