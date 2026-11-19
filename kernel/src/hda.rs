//! Intel High Definition Audio: the sound controller in nearly every PC
//! since 2004 (the HP's is 8086:8c20), and QEMU's `intel-hda`.
//!
//! The controller talks to codecs (the chips that make the sound) with
//! commands ("verbs") through two rings in memory, CORB out and RIRB in.
//! At first use this driver resets the controller, asks each codec for
//! its nodes, and finds a way (sound.rs `route`) from a converter (DAC)
//! to every speaker, headphone and line-out pin; it unmutes and switches
//! on everything on those ways. A sound is played by one output stream
//! from a buffer list (BDL) in memory, all DACs on the same stream tag,
//! and stopped when its time is up. No interrupts: the caller waits.
//!
//! Nothing happens at boot: the controller is only touched by `sound`,
//! `beep` and `play`. Memory the controller reads comes from the heap
//! (physical = virtual, below 4 GiB, as for xHCI).

use crate::pci::{self, Bar};
use crate::sound::{self, Kind, Node};
use crate::time;
use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const GCAP: usize = 0x00;
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0E;
const INTCTL: usize = 0x20;
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4A;
const CORBCTL: usize = 0x4C;
const CORBSIZE: usize = 0x4E;
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5A;
const RIRBCTL: usize = 0x5C;
const RIRBSTS: usize = 0x5D;
const RIRBSIZE: usize = 0x5E;
const DPLBASE: usize = 0x70;
const DPUBASE: usize = 0x74;

// Parameters (GET_PARAMETER, verb 0xF00).
const P_VENDOR: u32 = 0x00;
const P_NODES: u32 = 0x04;
const P_FUNCTION: u32 = 0x05;
const P_CAPS: u32 = 0x09;
const P_PIN_CAPS: u32 = 0x0C;
const P_CONN_LEN: u32 = 0x0E;
const P_OUT_AMP: u32 = 0x12;
const P_IN_AMP: u32 = 0x0D;

/// Memory the controller reads or writes: zeroed, aligned to 128 bytes.
struct Dma {
    ptr: *mut u8,
    len: usize,
}

impl Dma {
    fn new(len: usize) -> Option<Dma> {
        let layout = Layout::from_size_align(len.max(128), 4096).ok()?;
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() || ptr as u64 + len as u64 > 1 << 32 {
            return None;
        }
        Some(Dma { ptr, len: len.max(128) })
    }

    fn addr(&self) -> u64 {
        self.ptr as u64
    }

    fn read32(&self, at: usize) -> u32 {
        assert!(at + 4 <= self.len);
        unsafe { (self.ptr.add(at) as *const u32).read_volatile() }
    }

    fn write32(&self, at: usize, v: u32) {
        assert!(at + 4 <= self.len);
        unsafe { (self.ptr.add(at) as *mut u32).write_volatile(v) }
    }

    fn fill(&self, data: &[u8]) {
        let n = data.len().min(self.len);
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr, n) };
    }
}

impl Drop for Dma {
    fn drop(&mut self) {
        if let Ok(layout) = Layout::from_size_align(self.len, 4096) {
            unsafe { dealloc(self.ptr, layout) };
        }
    }
}

/// An output: a pin and the way to its converter.
#[derive(Debug, Clone)]
pub struct Output {
    pub codec: u8,
    pub what: &'static str,
    /// Pin first, DAC last, with the input taken at each.
    pub way: Vec<(u8, usize)>,
}

pub struct Controller {
    base: u64,
    pub pci: pci::Device,
    corb: Dma,
    rirb: Dma,
    rirb_read: u16,
    pub codecs: Vec<(u8, u32)>,
    pub outputs: Vec<Output>,
    /// Every pin: codec, node, configuration default, capabilities.
    pub pins: Vec<(u8, u8, u32, u32)>,
    /// The first output stream's register block.
    stream: usize,
    /// VC1 was switched off to match the chipset (`match_vc1`).
    vc1_off: bool,
    /// Where the card writes stream positions.
    posbuf: Dma,
    /// FIFORDY came at the last start.
    fifo_ready: bool,
    /// Volume of the DAC amplifiers in percent.
    pub volume: u32,
}

static CONTROLLER: crate::ring::Global<Option<Controller>> = crate::ring::Global::new(None);
/// A sound is playing (the controller is not to be used by anyone else).
static PLAYING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn wait_us(us: u64, mut done: impl FnMut() -> bool) -> bool {
    let start = time::micros();
    loop {
        if done() {
            return true;
        }
        if time::micros() - start > us {
            return done();
        }
        core::hint::spin_loop();
    }
}

impl Controller {
    fn r8(&self, at: usize) -> u8 {
        unsafe { ((self.base + at as u64) as *const u8).read_volatile() }
    }
    fn r16(&self, at: usize) -> u16 {
        unsafe { ((self.base + at as u64) as *const u16).read_volatile() }
    }
    fn r32(&self, at: usize) -> u32 {
        unsafe { ((self.base + at as u64) as *const u32).read_volatile() }
    }
    fn w8(&self, at: usize, v: u8) {
        unsafe { ((self.base + at as u64) as *mut u8).write_volatile(v) }
    }
    fn w16(&self, at: usize, v: u16) {
        unsafe { ((self.base + at as u64) as *mut u16).write_volatile(v) }
    }
    fn w32(&self, at: usize, v: u32) {
        unsafe { ((self.base + at as u64) as *mut u32).write_volatile(v) }
    }

    /// Reset the controller and start the command rings.
    fn start(card: pci::Device) -> Result<Controller, String> {
        let Bar::Memory { addr, .. } = card.bar(0) else { return Err(String::from("the sound card has no memory window")) };
        if addr == 0 || addr >= 1 << 32 {
            return Err(String::from("the sound card's registers are out of reach"));
        }
        card.enable(pci::CMD_MEMORY | pci::CMD_BUS_MASTER);
        let vc1_off = match_vc1(&card);
        // The card's interrupts stay inside it (PCI "interrupt disable"):
        // this driver polls, and the line may be another device's.
        card.config_write(0x04, card.config_read(0x04) & 0xFFFF | 1 << 10);
        // Intel: traffic class 0 for the card's memory reads (TCSEL, bits
        // 0-2 at 0x44), as Linux does; another class may never be served.
        // Intel: traffic class 0 (TCSEL, bits 0-2 at 0x44), as Linux does;
        // only where 0x44 looks like TCSEL (the other bits clear).
        let tcsel = card.config_read(0x44);
        if card.vendor == 0x8086 && tcsel & !7 == 0 && tcsel != 0 {
            card.config_write(0x44, 0);
        }
        let corb = Dma::new(1024).ok_or("out of memory")?;
        let rirb = Dma::new(2048).ok_or("out of memory")?;
        let posbuf = Dma::new(4096).ok_or("out of memory")?;
        let mut c = Controller { vc1_off, posbuf, fifo_ready: false, base: addr, pci: card, corb, rirb, rirb_read: 0, codecs: Vec::new(), outputs: Vec::new(), pins: Vec::new(), stream: 0, volume: 70 };
        // Stop the rings, then reset (CRST low, then high again).
        c.w8(CORBCTL, 0);
        c.w8(RIRBCTL, 0);
        wait_us(10_000, || c.r8(CORBCTL) & 2 == 0 && c.r8(RIRBCTL) & 2 == 0);
        c.w32(GCTL, c.r32(GCTL) & !1);
        if !wait_us(100_000, || c.r32(GCTL) & 1 == 0) {
            return Err(String::from("the sound card does not reset"));
        }
        wait_us(200, || false);
        c.w32(GCTL, c.r32(GCTL) | 1);
        if !wait_us(100_000, || c.r32(GCTL) & 1 == 1) {
            return Err(String::from("the sound card does not come out of reset"));
        }
        // Codecs announce themselves within 521 us of the reset.
        wait_us(2_000, || false);
        let present = c.r16(STATESTS);
        c.w16(STATESTS, present);
        c.w32(INTCTL, 0);
        c.w32(DPLBASE, 0);
        c.w32(DPUBASE, 0);
        // The command ring (CORB): 256 entries if the card can, else 16 or 2.
        let size_bits = |caps: u8| if caps & 0x40 != 0 { (2u8, 256u16) } else if caps & 0x20 != 0 { (1, 16) } else { (0, 2) };
        let (corb_bits, corb_n) = size_bits(c.r8(CORBSIZE) >> 4);
        let (rirb_bits, _) = size_bits(c.r8(RIRBSIZE) >> 4);
        c.w8(CORBSIZE, (c.r8(CORBSIZE) & !3) | corb_bits);
        c.w32(CORBLBASE, c.corb.addr() as u32);
        c.w32(CORBUBASE, 0);
        c.w16(CORBWP, 0);
        // Reset the read pointer: set bit 15, see it set, clear it, see it clear
        // (QEMU and some cards never show it set; go on after a while).
        c.w16(CORBRP, 1 << 15);
        wait_us(1_000, || c.r16(CORBRP) & (1 << 15) != 0);
        c.w16(CORBRP, 0);
        wait_us(1_000, || c.r16(CORBRP) & (1 << 15) == 0);
        c.w8(RIRBSIZE, (c.r8(RIRBSIZE) & !3) | rirb_bits);
        c.w32(RIRBLBASE, c.rirb.addr() as u32);
        c.w32(RIRBUBASE, 0);
        c.w16(RIRBWP, 1 << 15);
        // Answers before the card wants attention; it may hold back more
        // until RIRBSTS is cleared (QEMU does), so that is cleared after each.
        c.w16(RINTCNT, 0xFF);
        c.w8(CORBCTL, 2);
        c.w8(RIRBCTL, 2);
        c.rirb_read = 0;
        let _ = corb_n;
        let gcap = c.r16(GCAP);
        let (inputs, outputs) = ((gcap >> 8 & 0xF) as usize, (gcap >> 12 & 0xF) as usize);
        if outputs == 0 {
            return Err(String::from("the sound card has no output streams"));
        }
        c.stream = 0x80 + 0x20 * inputs;
        for codec in 0..15u8 {
            if present & (1 << codec) != 0 {
                if let Some(vendor) = c.param(codec, 0, P_VENDOR) {
                    c.codecs.push((codec, vendor));
                    c.find_outputs(codec);
                }
            }
        }
        Ok(c)
    }

    /// Send a verb and wait for its answer.
    fn command(&mut self, verb: u32) -> Option<u32> {
        let size = match self.r8(CORBSIZE) & 3 {
            2 => 256,
            1 => 16,
            _ => 2,
        };
        let wp = (self.r16(CORBWP) as usize + 1) % size;
        self.corb.write32(wp * 4, verb);
        self.w16(CORBWP, wp as u16);
        let rsize = match self.r8(RIRBSIZE) & 3 {
            2 => 256,
            1 => 16,
            _ => 2,
        };
        let want = (self.rirb_read as usize + 1) % rsize;
        let arrived = wait_us(20_000, || self.r16(RIRBWP) as usize % rsize == want);
        if !arrived {
            // Catch up, so the next command does not read this one's late answer.
            self.rirb_read = (self.r16(RIRBWP) as usize % rsize) as u16;
            self.w8(RIRBSTS, 0x05);
            return None;
        }
        self.rirb_read = want as u16;
        self.w8(RIRBSTS, 0x05);
        Some(self.rirb.read32(want * 8))
    }

    fn param(&mut self, codec: u8, nid: u8, which: u32) -> Option<u32> {
        self.command(sound::verb(codec, nid, 0xF00, which))
    }

    /// Every node of the codec's audio function group, and the ways from
    /// its output pins to converters.
    fn find_outputs(&mut self, codec: u8) {
        let Some(groups) = self.param(codec, 0, P_NODES) else { return };
        let mut nodes: Vec<Node> = Vec::new();
        let mut afg = None;
        for fg in (groups >> 16 & 0xFF)..(groups >> 16 & 0xFF) + (groups & 0xFF).min(8) {
            if self.param(codec, fg as u8, P_FUNCTION).is_some_and(|t| t & 0xFF == 1) {
                afg = Some(fg as u8);
            }
        }
        let Some(afg) = afg else { return };
        // Power the function group on (D0).
        let _ = self.command(sound::verb(codec, afg, 0x705, 0));
        let Some(widgets) = self.param(codec, afg, P_NODES) else { return };
        let first = (widgets >> 16 & 0xFF) as u8;
        for nid in first..first.saturating_add((widgets & 0xFF).min(128) as u8) {
            let Some(caps) = self.param(codec, nid, P_CAPS) else { continue };
            let kind = Kind::from_caps(caps);
            let inputs = if caps & (1 << 8) != 0 {
                let len = self.param(codec, nid, P_CONN_LEN).unwrap_or(0);
                let mut answers: Vec<(u32, u32)> = Vec::new();
                let count = (len & 0x7F).min(32);
                let per = if len & 0x80 != 0 { 2 } else { 4 };
                let mut off = 0;
                while off < count {
                    let a = self.command(sound::verb(codec, nid, 0xF02, off)).unwrap_or(0);
                    answers.push((off, a));
                    off += per;
                }
                sound::connections(len, |o| answers.iter().find(|(k, _)| *k == o).map_or(0, |(_, a)| *a))
            } else {
                Vec::new()
            };
            let (config, pin_caps) = if kind == Kind::Pin {
                (self.command(sound::verb(codec, nid, 0xF1C, 0)).unwrap_or(0), self.param(codec, nid, P_PIN_CAPS).unwrap_or(0))
            } else {
                (0, 0)
            };
            nodes.push(Node { nid, kind, caps, inputs, config, pin_caps });
        }
        self.pins.extend(nodes.iter().filter(|n| n.kind == Kind::Pin).map(|n| (codec, n.nid, n.config, n.pin_caps)));
        for pin in nodes.iter().filter(|n| n.kind == Kind::Pin && n.pin_caps & (1 << 4) != 0) {
            let Some(what) = sound::output_use(pin.config) else { continue };
            if let Some(way) = sound::route(&nodes, pin.nid) {
                self.outputs.push(Output { codec, what, way });
            }
        }
        self.switch_on(codec, &nodes);
    }

    /// Unmute and power everything on the ways out; select the inputs the
    /// ways take; let the pins drive their jacks and speakers.
    fn switch_on(&mut self, codec: u8, nodes: &[Node]) {
        let ways: Vec<Vec<(u8, usize)>> = self.outputs.iter().filter(|o| o.codec == codec).map(|o| o.way.clone()).collect();
        for way in &ways {
            for (step, &(nid, input)) in way.iter().enumerate() {
                let Some(node) = nodes.iter().find(|n| n.nid == nid) else { continue };
                let _ = self.command(sound::verb(codec, nid, 0x705, 0)); // power D0
                if node.kind == Kind::Pin || node.kind == Kind::Selector {
                    let _ = self.command(sound::verb(codec, nid, 0x701, input as u32));
                }
                if node.kind == Kind::Pin {
                    let headphones = sound::output_use(node.config) == Some("headphones");
                    let _ = self.command(sound::verb(codec, nid, 0x707, if headphones { 0xC0 } else { 0x40 }));
                    if node.pin_caps & (1 << 16) != 0 {
                        let _ = self.command(sound::verb(codec, nid, 0x70C, 2)); // external amplifier on
                    }
                }
                // Output amplifier: unmuted at 0 dB (the DAC's follows the volume).
                if node.caps & (1 << 2) != 0 && step + 1 < way.len() {
                    let amp = self.param(codec, nid, P_OUT_AMP).unwrap_or(0);
                    let _ = self.command(sound::verb16(codec, nid, 0x3, 0xB000 | (amp & 0x7F)));
                }
                // Mixers: unmute the input the way takes.
                if node.caps & (1 << 1) != 0 && node.kind == Kind::Mixer {
                    let amp = self.param(codec, nid, P_IN_AMP).unwrap_or(0);
                    let _ = self.command(sound::verb16(codec, nid, 0x3, 0x7000 | (input as u32) << 8 | (amp & 0x7F)));
                }
            }
        }
        let volume = self.volume;
        self.set_volume(volume);
    }

    /// The DACs' output amplifiers: 0 % is muted, 100 % their highest step.
    pub fn set_volume(&mut self, percent: u32) {
        self.volume = percent.min(100);
        let dacs: Vec<(u8, u8)> = self.outputs.iter().filter_map(|o| o.way.last().map(|&(n, _)| (o.codec, n))).collect();
        for (codec, dac) in dacs {
            let amp = self.param(codec, dac, P_OUT_AMP).unwrap_or(0);
            let steps = amp >> 8 & 0x7F;
            let gain = steps * self.volume / 100;
            let mute = if self.volume == 0 { 0x80 } else { 0 };
            let _ = self.command(sound::verb16(codec, dac, 0x3, 0xB000 | mute | gain));
        }
    }

    fn sd_r8(&self, at: usize) -> u8 {
        self.r8(self.stream + at)
    }

    fn sd_w8(&self, at: usize, v: u8) {
        self.w8(self.stream + at, v)
    }

    fn sd_w16(&self, at: usize, v: u16) {
        self.w16(self.stream + at, v)
    }

    fn sd_w32(&self, at: usize, v: u32) {
        self.w32(self.stream + at, v)
    }

    pub fn position(&self) -> u32 {
        self.r32(self.stream + 0x04)
    }

    fn stop_stream(&self) {
        self.sd_w8(0, self.sd_r8(0) & !2);
        wait_us(10_000, || self.sd_r8(0) & 2 == 0);
        // Reset the stream.
        self.sd_w8(0, 1);
        wait_us(10_000, || self.sd_r8(0) & 1 != 0);
        self.sd_w8(0, 0);
        wait_us(10_000, || self.sd_r8(0) & 1 == 0);
        self.sd_w8(3, 0x1C); // clear the status bits
    }

    /// Start playing `buffer` (48k frames) round and round on stream tag 1.
    fn start_stream(&mut self, buffer: &Dma, bdl: &Dma, len: usize) {
        self.stop_stream();
        // Two halves (the list needs at least two entries).
        let half = (len / 2) & !127;
        let parts = [(buffer.addr(), half), (buffer.addr() + half as u64, len - half)];
        for (i, (addr, n)) in parts.iter().enumerate() {
            bdl.write32(i * 16, *addr as u32);
            bdl.write32(i * 16 + 4, (*addr >> 32) as u32);
            bdl.write32(i * 16 + 8, *n as u32);
            bdl.write32(i * 16 + 12, 0);
        }
        self.sd_w32(0x18, bdl.addr() as u32);
        self.sd_w32(0x1C, 0);
        self.sd_w32(0x08, len as u32);
        self.sd_w16(0x0C, 1); // last entry: 1
        self.sd_w16(0x12, sound::FORMAT_48K_16_STEREO);
        // Stream tag 1, written as Linux does (the whole control register).
        let ctl = self.r32(self.stream) & !(0xF << 20) & !0xFF00_0000;
        self.sd_w32(0, ctl | 1 << 20);
        // The DMA position buffer (Linux reads positions from it first).
        self.w32(DPLBASE, self.posbuf.addr() as u32 | 1);
        self.w32(DPUBASE, 0);
        let dacs: Vec<(u8, u8)> = self.outputs.iter().filter_map(|o| o.way.last().map(|&(n, _)| (o.codec, n))).collect();
        for (codec, dac) in dacs {
            let _ = self.command(sound::verb16(codec, dac, 0x2, sound::FORMAT_48K_16_STEREO as u32));
            let _ = self.command(sound::verb(codec, dac, 0x706, 1 << 4));
        }
        // Start as Linux does: hold the stream (SSYNC, 0x38), set RUN, let
        // the FIFO fill (FIFORDY), then let go.
        let bit = 1u32 << ((self.stream - 0x80) / 0x20);
        self.w32(0x38, self.r32(0x38) | bit);
        // The stream's interrupt bit (no global enable: nothing reaches the CPU).
        self.w32(INTCTL, self.r32(INTCTL) | bit | 1 << 31 | 1 << 30);
        self.sd_w8(0, 2 | 0x1C); // run, with completion and error reports
        let ready = wait_us(20_000, || self.sd_r8(3) & 0x20 != 0);
        self.w32(0x38, self.r32(0x38) & !bit);
        self.fifo_ready = ready;
    }

    fn release_dacs(&mut self) {
        let dacs: Vec<(u8, u8)> = self.outputs.iter().filter_map(|o| o.way.last().map(|&(n, _)| (o.codec, n))).collect();
        for (codec, dac) in dacs {
            let _ = self.command(sound::verb(codec, dac, 0x706, 0));
        }
    }
}

/// Intel chipsets: the DMI link's VC1 control and status (RCBA + 0x2020,
/// 0x2026), from the LPC bridge's RCBA register (00:1f.0, 0xF0).
fn dmi_vc1() -> Option<(u32, u16)> {
    let lpc = pci::scan().into_iter().find(|d| d.bus == 0 && d.dev == 0x1f && d.func == 0 && d.vendor == 0x8086)?;
    let rcba = (lpc.config_read(0xF0) & !0x3FFF) as u64;
    if rcba == 0 || lpc.config_read(0xF0) & 1 == 0 {
        return None;
    }
    let ctl = unsafe { ((rcba + 0x2020) as *const u32).read_volatile() };
    let sts = unsafe { ((rcba + 0x2026) as *const u16).read_volatile() };
    Some((ctl, sts))
}

/// Where a device's PCI Express configuration space is (ACPI MCFG), for
/// registers past the first 256 bytes.
fn ext_config_base(card: &pci::Device) -> Option<u64> {
    let mcfg = crate::acpi::table(b"MCFG")?;
    for entry in mcfg.get(44..)?.chunks_exact(16) {
        let base = u64::from_le_bytes(entry[0..8].try_into().ok()?);
        let (first, last) = (entry[10], entry[11]);
        if u16::from_le_bytes([entry[8], entry[9]]) == 0 && (first..=last).contains(&card.bus) && base < 1 << 32 {
            return Some(base + (((card.bus - first) as u64) << 20 | (card.dev as u64) << 15 | (card.func as u64) << 12));
        }
    }
    None
}

/// The card's VC1 control register (in its virtual channel capability),
/// if it has one.
fn vc1_control(card: &pci::Device) -> Option<*mut u32> {
    let base = ext_config_base(card)?;
    let read = |off: u64| unsafe { ((base + off) as *const u32).read_volatile() };
    let mut at = 0x100u64;
    for _ in 0..32 {
        let header = read(at);
        if header == 0 || header == 0xFFFF_FFFF {
            return None;
        }
        if header & 0xFFFF == 0x0002 {
            // VC0 resource control at +0x14, VC1's at +0x20 (if there is a VC1).
            return (read(at + 4) & 7 >= 1).then_some((base + at + 0x20) as *mut u32);
        }
        at = (header >> 20) as u64;
        if at < 0x100 {
            return None;
        }
    }
    None
}

/// Intel: output streams always ask for VC1 when the card has it on. If
/// the chipset's own link has VC1 off, that traffic is never served and
/// no sound is fetched (the HP's firmware leaves it so); switch the card's
/// VC1 off too, so the streams use VC0 like everything else.
fn match_vc1(card: &pci::Device) -> bool {
    let Some(reg) = vc1_control(card) else { return false };
    let card_on = unsafe { reg.read_volatile() } & (1 << 31) != 0;
    let link_on = dmi_vc1().is_some_and(|(ctl, _)| ctl & (1 << 31) != 0);
    if card.vendor == 0x8086 && card_on && !link_on {
        unsafe { reg.write_volatile(reg.read_volatile() & !(1 << 31)) };
        return true;
    }
    false
}

/// The HD Audio controllers in the machine, the ones with a codec that
/// has analog outputs first.
fn controllers() -> Vec<pci::Device> {
    pci::scan().into_iter().filter(|d| d.class == 0x04 && d.subclass == 0x03).collect()
}

/// Start the driver if it has not been started: the first controller with
/// a speaker, headphone or line-out.
fn ensure() -> Result<(), String> {
    if CONTROLLER.get().is_some() {
        return Ok(());
    }
    let cards = controllers();
    if cards.is_empty() {
        return Err(String::from("no sound card (HD Audio) found"));
    }
    let mut last_err = String::new();
    let mut silent = None;
    for card in cards {
        match Controller::start(card) {
            Ok(c) if !c.outputs.is_empty() => {
                *CONTROLLER.get() = Some(c);
                return Ok(());
            }
            Ok(c) => silent = silent.or(Some(c)),
            Err(e) => last_err = e,
        }
    }
    // A card without outputs is still worth describing.
    match silent {
        Some(c) => {
            *CONTROLLER.get() = Some(c);
            Ok(())
        }
        None => Err(last_err),
    }
}

/// Is there a sound card that can play? (Starts the driver if needed.)
pub fn available() -> bool {
    ensure().is_ok() && CONTROLLER.get().as_ref().is_some_and(|c| !c.outputs.is_empty())
}

/// What `sound` shows.
pub fn describe() -> Result<Vec<String>, String> {
    ensure()?;
    let c = CONTROLLER.get().as_ref().ok_or("no sound card")?;
    let mut lines = alloc::vec![format!(
        "HD Audio {:04x}:{:04x} at {:02x}:{:02x}.{}, volume {} %",
        c.pci.vendor, c.pci.device, c.pci.bus, c.pci.dev, c.pci.func, c.volume
    )];
    if c.vc1_off {
        lines.push(String::from("VC1 switched off on the card, as on the chipset's link"));
    }
    for (codec, vendor) in &c.codecs {
        lines.push(format!("codec {codec}: {:04x}:{:04x}", vendor >> 16, vendor & 0xFFFF));
    }
    for (codec, nid, config, caps) in &c.pins {
        let what = sound::output_use(*config).unwrap_or("-");
        lines.push(format!("  pin {codec}/{nid:#04x}: config {config:08x}, caps {caps:08x} ({what})"));
    }
    if c.outputs.is_empty() {
        lines.push(String::from("no way found from a converter to a speaker or headphones"));
    }
    for o in &c.outputs {
        let way: Vec<String> = o.way.iter().map(|(n, _)| format!("{n:#04x}")).collect();
        lines.push(format!("  {:<11} {}", o.what, way.join(" <- ")));
    }
    Ok(lines)
}

pub fn set_volume(percent: u32) -> Result<(), String> {
    ensure()?;
    if PLAYING.load(core::sync::atomic::Ordering::Acquire) {
        return Err(String::from("a sound is playing"));
    }
    CONTROLLER.get().as_mut().ok_or("no sound card")?.set_volume(percent);
    Ok(())
}

/// Play 48k frames; returns when they have been played, or early when
/// `stop()` says so. Gives how many milliseconds were played and how far
/// the controller's position went (proof the sound card took the data).
pub fn play(frames: &[u8], mut stop: impl FnMut() -> bool) -> Result<(u64, u64), String> {
    use core::sync::atomic::Ordering;
    ensure()?;
    if CONTROLLER.get().as_ref().is_none_or(|c| c.outputs.is_empty()) {
        return Err(String::from("the sound card has no speaker or headphone output this OS can use"));
    }
    if PLAYING.swap(true, Ordering::AcqRel) {
        return Err(String::from("another sound is playing"));
    }
    // 100 ms of silence after the sound: the stream is stopped in it.
    let tail = sound::RATE as usize / 10 * 4;
    let len = (frames.len() & !3) + tail;
    let len = (len + 255) & !255;
    let result = (|| {
        let buffer = Dma::new(len).ok_or("out of memory for the sound")?;
        buffer.fill(&frames[..frames.len() & !3]);
        let bdl = Dma::new(4096).ok_or("out of memory")?;
        let c = CONTROLLER.get().as_mut().ok_or("no sound card")?;
        c.start_stream(&buffer, &bdl, len);
        let ms = (frames.len() / 4) as u64 * 1000 / sound::RATE as u64;
        let start = time::uptime_ms();
        let (mut last, mut moved) = (0u32, 0u64);
        loop {
            let now = time::uptime_ms() - start;
            if now >= ms + 20 || stop() {
                break;
            }
            crate::thread::pause(|| time::uptime_ms() - start >= ms + 20);
            let pos = CONTROLLER.get().as_ref().map_or(0, |c| c.position());
            moved += (pos.wrapping_sub(last) as u64) % len as u64;
            last = pos;
        }
        let played = time::uptime_ms() - start;
        let c = CONTROLLER.get().as_mut().ok_or("no sound card")?;
        let pos = c.position();
        if moved == 0 && pos == 0 {
            let s = c.stream;
            let state = format!(
                "the sound card did not take the sound (stream at {:#x}: control {:06x}, status {:02x}, position {}, length {}, last {}, format {:04x}, list {:08x}, fifo {} (ready at start: {}); GCTL {:08x}, sync {:08x}/{:08x}, pci {:08x})",
                s,
                c.r32(s) & 0xFF_FFFF,
                c.r8(s + 3),
                c.r32(s + 4),
                c.r32(s + 8),
                c.r16(s + 0x0C),
                c.r16(s + 0x12),
                c.r32(s + 0x18),
                c.r16(s + 0x10),
                c.fifo_ready,
                c.r32(GCTL),
                c.r32(0x34),
                c.r32(0x38),
                c.pci.config_read(0x04)
            );
            let mut codec_state = String::new();
            let dacs: Vec<(u8, u8)> = c.outputs.iter().filter_map(|o| o.way.last().map(|&(n, _)| (o.codec, n))).collect();
            for (codec, dac) in dacs.into_iter().take(1) {
                let conv = c.command(sound::verb(codec, dac, 0xF06, 0));
                let fmt = c.command(sound::verb16(codec, dac, 0xA, 0));
                let power = c.command(sound::verb(codec, dac, 0xF05, 0));
                let afg_power = c.command(sound::verb(codec, 1, 0xF05, 0));
                let w1 = c.r32(0x30);
                wait_us(1000, || false);
                let w2 = c.r32(0x30);
                codec_state = format!("; wall clock {w1:08x} -> {w2:08x}, GCAP {:04x}, INTCTL {:08x}; position buffer {}; DAC {dac:#x}: stream {conv:x?}, format {fmt:x?}, power {power:x?}, group power {afg_power:x?}, INTSTS {:08x}", c.r16(GCAP), c.r32(INTCTL), c.posbuf.read32(((c.stream - 0x80) / 0x20) * 8), c.r32(0x24));
            }
            c.stop_stream();
            c.release_dacs();
            return Err(state + &codec_state);
        }
        moved += (pos.wrapping_sub(last) as u64) % len as u64;
        c.stop_stream();
        c.release_dacs();
        Ok::<(u64, u64), String>((played.min(ms), moved))
    })();
    PLAYING.store(false, Ordering::Release);
    result
}
