//! Intel gigabit network cards. Polled: interrupts stay masked, `receive()`
//! is called from the network thread and from waiting network calls.
//!
//! Three generations share the same rings and most registers (see `CHIPS`):
//!   * e1000: 82540EM (PCI 8086:100E), QEMU's and libvirt's default card;
//!   * e1000e: 82574L (8086:10D3), QEMU's `-device e1000e`, the stand-in in
//!     the tests for the next kind;
//!   * the cards built into Intel chipsets ("PCH"): I217 (8086:153A/153B),
//!     as in the HP EliteDesk 800 G1. Their start-up differs: no EEPROM
//!     register, a reset that reloads settings from flash, and a separate PHY
//!     chip shared with the management firmware.
//!
//! The card and the driver share two rings of descriptors in memory. Each
//! descriptor points at a packet buffer; a "done" bit says the card has
//! finished with it. Memory is identity-mapped, so the addresses we hand to
//! the card are the pointers themselves.

use crate::net::{NetDevice, NetError, MAX_FRAME, MIN_FRAME};
use crate::pci::{self, Bar, Device};
use crate::port::outb;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

const VENDOR_INTEL: u16 = 0x8086;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    /// The original PCI cards.
    Classic,
    /// PCI Express cards (82574L).
    Express,
    /// Built into the chipset, with a separate PHY (I217).
    Chipset,
}

/// (PCI device number, name, family) of the cards this driver knows.
pub const CHIPS: [(u16, &str, Family); 5] = [
    (0x100E, "e1000 (Intel 82540EM)", Family::Classic),
    (0x100F, "e1000 (Intel 82545EM)", Family::Classic),
    (0x10D3, "e1000e (Intel 82574L)", Family::Express),
    (0x153A, "Intel I217-LM", Family::Chipset),
    (0x153B, "Intel I217-V", Family::Chipset),
];

pub fn chip(device: u16) -> Option<(&'static str, Family)> {
    CHIPS.iter().find(|c| c.0 == device).map(|c| (c.1, c.2))
}

// Registers (offsets into the memory-mapped BAR 0).
const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EERD: usize = 0x0014;
const CTRL_EXT: usize = 0x0018;
const MDIC: usize = 0x0020;
const EXTCNF_CTRL: usize = 0x0F00;
const RFCTL: usize = 0x5008;
const FWSM: usize = 0x5B54;
const IMC: usize = 0x00D8;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const TIPG: usize = 0x0410;
const RDBAL: usize = 0x2800;
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const TXDCTL: usize = 0x3828;
const TXDCTL1: usize = 0x3928;
const TARC0: usize = 0x3840;
// The card's own counters; each is set back to 0 when it is read.
const CRCERRS: usize = 0x4000;
const MPC: usize = 0x4010;
const GPRC: usize = 0x4074;
const GPTC: usize = 0x4080;
const MTA: usize = 0x5200;
const RAL0: usize = 0x5400;
const RAH0: usize = 0x5404;

const CTRL_ASDE: u32 = 1 << 5; // auto-speed detection
const CTRL_SLU: u32 = 1 << 6; // set link up
const CTRL_FORCE_SPEED: u32 = 1 << 11;
const CTRL_FORCE_DUPLEX: u32 = 1 << 12;
const CTRL_RST: u32 = 1 << 26;
const CTRL_EXT_DRIVER_LOADED: u32 = 1 << 28;
const STATUS_FULL_DUPLEX: u32 = 1 << 0;
const STATUS_LINK_UP: u32 = 1 << 1;
/// Chipset cards: the settings from flash are loaded again after a reset.
const STATUS_LAN_INIT_DONE: u32 = 1 << 9;
/// Chipset cards: "the PHY was reset"; cleared by writing it back as 0.
const STATUS_PHY_RESET: u32 = 1 << 10;
/// Extended receive descriptors (we use the plain ones).
const RFCTL_EXTENDED: u32 = 1 << 15;
/// Chipset cards: software owns the PHY while this is set (the management
/// firmware uses the PHY too).
const EXTCNF_SOFTWARE_FLAG: u32 = 1 << 5;
const MDIC_READ: u32 = 2 << 26;
const MDIC_READY: u32 = 1 << 28;
const MDIC_ERROR: u32 = 1 << 30;
/// Where the I217's PHY answers on the management bus.
const CHIPSET_PHY_ADDRESS: u32 = 2;
const RCTL_EN: u32 = 1 << 1;
const RCTL_BAM: u32 = 1 << 15; // accept broadcast
const RCTL_SECRC: u32 = 1 << 26; // strip the frame checksum
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3; // pad short packets

const DESC_DONE: u8 = 1 << 0;
const RX_END_OF_PACKET: u8 = 1 << 1;
const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1; // card appends the frame checksum
const TX_CMD_RS: u8 = 1 << 3; // report status (sets DESC_DONE)

pub const RX_RING: usize = 32;
pub const TX_RING: usize = 16;
pub const BUFFER_SIZE: usize = 2048;

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
pub struct RxDesc {
    addr: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
pub struct TxDesc {
    addr: u64,
    length: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Buffer([u8; BUFFER_SIZE]);

pub struct E1000 {
    name: &'static str,
    family: Family,
    regs: *mut u8,
    mac: [u8; 6],
    rx: &'static mut [RxDesc],
    rx_buf: &'static mut [Buffer],
    rx_next: usize,
    tx: &'static mut [TxDesc],
    tx_buf: &'static mut [Buffer],
    tx_next: usize,
    tx_started: [bool; TX_RING],
    /// The receive tail was written since the last write flush.
    rx_tail_dirty: bool,
    /// The card's own counts since start: frames it put on the wire, good
    /// frames it took off it, frames with a bad checksum, frames it had no
    /// room for.
    counted: core::cell::Cell<[u64; 4]>,
}

/// Index after `i` in a ring of `len` slots.
pub fn ring_next(i: usize, len: usize) -> usize {
    (i + 1) % len
}

impl E1000 {
    /// Find an e1000 on the PCI bus and bring it up.
    pub fn find(devices: &[Device]) -> Option<Result<Self, &'static str>> {
        let (dev, (name, family)) = devices
            .iter()
            .filter(|d| d.vendor == VENDOR_INTEL && d.class == 0x02)
            .find_map(|d| chip(d.device).map(|c| (d, c)))?;
        // Tests only: run the start-up of a chipset card on whatever card is
        // there (no emulator has an I217), to show that it cannot hang.
        let family = if crate::bootargs::has("nic=chipset") { Family::Chipset } else { family };
        Some(Self::init(dev, name, family))
    }

    fn init(dev: &Device, name: &'static str, family: Family) -> Result<Self, &'static str> {
        let Bar::Memory { addr, .. } = dev.bar(0) else { return Err("e1000: no memory BAR") };
        // Boot page tables identity-map the first 4 GiB only.
        if addr == 0 || addr + 0x20000 > 0x1_0000_0000 {
            return Err("e1000: registers are outside the mapped 4 GiB");
        }
        dev.enable(pci::CMD_MEMORY | pci::CMD_BUS_MASTER);
        let regs = addr as *mut u8;
        let rx: &'static mut [RxDesc] = Box::leak(vec![RxDesc::default(); RX_RING].into_boxed_slice());
        let rx_buf: &'static mut [Buffer] = Box::leak(vec![Buffer([0; BUFFER_SIZE]); RX_RING].into_boxed_slice());
        let tx: &'static mut [TxDesc] = Box::leak(vec![TxDesc::default(); TX_RING].into_boxed_slice());
        let tx_buf: &'static mut [Buffer] = Box::leak(vec![Buffer([0; BUFFER_SIZE]); TX_RING].into_boxed_slice());
        let mut card =
            E1000 { name, family, regs, mac: [0; 6], rx, rx_buf, rx_next: 0, tx, tx_buf, tx_next: 0, tx_started: [false; TX_RING], rx_tail_dirty: false,
                    counted: core::cell::Cell::new([0; 4]) };

        // Reset, with all interrupts masked before and after (we poll).
        card.write(IMC, 0xFFFF_FFFF);
        if family != Family::Classic {
            // Stop receiving and sending first, and let what is under way finish.
            card.write(RCTL, 0);
            card.write(TCTL, TCTL_PSP);
            card.write_flush();
            delay_ms(10);
        }
        card.write(CTRL, card.read(CTRL) | CTRL_RST);
        if family == Family::Chipset {
            delay_ms(20); // the registers must be left alone while it resets
        }
        let mut reset_done = false;
        for _ in 0..100_000 {
            delay_us(1);
            if card.read(CTRL) & CTRL_RST == 0 {
                reset_done = true;
                break;
            }
        }
        if !reset_done {
            return Err("e1000: reset did not finish");
        }
        card.write(IMC, 0xFFFF_FFFF);
        match family {
            Family::Classic => card.write(CTRL, card.read(CTRL) | CTRL_SLU | CTRL_ASDE),
            Family::Express | Family::Chipset => {
                if family == Family::Chipset {
                    // The card loads its settings from flash again: give it
                    // up to 150 ms, and go on regardless.
                    for _ in 0..150 {
                        if card.read(STATUS) & STATUS_LAN_INIT_DONE != 0 {
                            break;
                        }
                        delay_ms(1);
                    }
                    card.write(STATUS, card.read(STATUS) & !(STATUS_LAN_INIT_DONE | STATUS_PHY_RESET));
                }
                if family == Family::Chipset {
                    // Tell the management firmware (which shares the card)
                    // that a driver has taken over.
                    card.write(CTRL_EXT, card.read(CTRL_EXT) | CTRL_EXT_DRIVER_LOADED);
                }
                // Link up, at the speed the PHY negotiates.
                let ctrl = card.read(CTRL) & !(CTRL_FORCE_SPEED | CTRL_FORCE_DUPLEX);
                card.write(CTRL, ctrl | CTRL_SLU);
                card.write(RFCTL, card.read(RFCTL) & !RFCTL_EXTENDED);
            }
        }

        card.mac = card.read_mac().ok_or("e1000: no MAC address")?;
        let m = card.mac;
        card.write(RAL0, u32::from_le_bytes([m[0], m[1], m[2], m[3]]));
        card.write(RAH0, u16::from_le_bytes([m[4], m[5]]) as u32 | 1 << 31); // address valid
        for i in 0..128 {
            card.write(MTA + i * 4, 0); // no multicast groups
        }

        // Receive ring: every descriptor gets a buffer; the card owns all but one.
        for i in 0..RX_RING {
            card.rx[i].addr = card.rx_buf[i].0.as_ptr() as u64;
        }
        let rx_base = card.rx.as_ptr() as u64;
        card.write(RDBAL, rx_base as u32);
        card.write(RDBAH, (rx_base >> 32) as u32);
        card.write(RDLEN, (RX_RING * core::mem::size_of::<RxDesc>()) as u32);
        card.write(RDH, 0);
        card.write(RDT, (RX_RING - 1) as u32);
        card.write(RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC); // 2048-byte buffers

        for i in 0..TX_RING {
            card.tx[i].addr = card.tx_buf[i].0.as_ptr() as u64;
        }
        let tx_base = card.tx.as_ptr() as u64;
        card.write(TDBAL, tx_base as u32);
        card.write(TDBAH, (tx_base >> 32) as u32);
        card.write(TDLEN, (TX_RING * core::mem::size_of::<TxDesc>()) as u32);
        card.write(TDH, 0);
        card.write(TDT, 0);
        if family == Family::Chipset {
            // Settings these cards need before they transmit reliably (the
            // same ones other drivers apply): count descriptors, on both
            // transmit queues, and the documented transmit arbitration bits.
            // Thresholds as other drivers set them: fetch descriptors early
            // (bits 0-5), report each one when done (bits 16-21 = 1, bit 24).
            for queue in [TXDCTL, TXDCTL1] {
                let value = card.read(queue) & !(0x3F | 0x3F << 16);
                card.write(queue, value | 0x1F | 1 << 16 | 1 << 22 | 1 << 24);
            }
            card.write(TARC0, card.read(TARC0) | 1 << 23 | 1 << 24 | 1 << 26 | 1 << 27);
        }
        // Collision and frame-gap settings from each family's manual.
        match family {
            Family::Classic => {
                card.write(TCTL, TCTL_EN | TCTL_PSP | 0x10 << 4 | 0x40 << 12);
                card.write(TIPG, 10 | 8 << 10 | 6 << 20);
            }
            Family::Express | Family::Chipset => {
                // Only the fields we know: real chips keep settings of their
                // own in the upper bits, and need them.
                let kept = card.read(TCTL) & !(0xFF << 4 | 0x3FF << 12);
                card.write(TCTL, kept | TCTL_EN | TCTL_PSP | 0x0F << 4 | 0x3F << 12);
                card.write(TIPG, 8 | 8 << 10 | 6 << 20);
            }
        }
        card.write_flush();
        Ok(card)
    }

    fn read(&self, reg: usize) -> u32 {
        unsafe { read_volatile(self.regs.add(reg) as *const u32) }
    }

    fn write(&self, reg: usize, value: u32) {
        unsafe { write_volatile(self.regs.add(reg) as *mut u32, value) }
    }

    /// Make sure earlier register writes have reached the card. Under QEMU/KVM
    /// most e1000 register writes (the receive tail among them) are buffered
    /// ("coalesced MMIO") until the CPU next exits to QEMU; an idle OS that only
    /// sleeps and polls never does, and the card would stop receiving after one
    /// ring of frames. Reads are never buffered, so one read flushes the writes.
    /// (The same idiom as E1000_WRITE_FLUSH in other drivers.)
    fn write_flush(&self) {
        let _ = self.read(STATUS);
    }

    /// The MAC address: from the receive-address register (loaded from the
    /// EEPROM at reset), or read from the EEPROM directly.
    fn read_mac(&self) -> Option<[u8; 6]> {
        let (lo, hi) = (self.read(RAL0).to_le_bytes(), self.read(RAH0).to_le_bytes());
        let mac = [lo[0], lo[1], lo[2], lo[3], hi[0], hi[1]];
        if mac != [0; 6] && mac != [0xFF; 6] {
            return Some(mac);
        }
        if self.family == Family::Chipset {
            return None; // these have no EEPROM register: the address is in flash
        }
        let mut mac = [0u8; 6];
        for word in 0..3 {
            self.write(EERD, (word as u32) << 8 | 1);
            let value = (0..100_000).find_map(|_| Some(self.read(EERD)).filter(|v| v & (1 << 4) != 0))?;
            mac[word * 2..word * 2 + 2].copy_from_slice(&((value >> 16) as u16).to_le_bytes());
        }
        (mac != [0; 6] && mac != [0xFF; 6]).then_some(mac)
    }
}

impl E1000 {
    /// Read a register of the PHY chip over the management bus. Chipset
    /// cards only, and only for `details`: the driver does not depend on it.
    fn read_phy(&self, register: u32) -> Option<u16> {
        // The management firmware uses the PHY too: take the flag first.
        self.write(EXTCNF_CTRL, self.read(EXTCNF_CTRL) | EXTCNF_SOFTWARE_FLAG);
        let ours = (0..1_000).any(|_| {
            delay_us(100);
            self.read(EXTCNF_CTRL) & EXTCNF_SOFTWARE_FLAG != 0
        });
        let mut value = None;
        if ours {
            self.write(MDIC, register << 16 | CHIPSET_PHY_ADDRESS << 21 | MDIC_READ);
            let answer = (0..2_000).find_map(|_| {
                delay_us(50);
                Some(self.read(MDIC)).filter(|v| v & (MDIC_READY | MDIC_ERROR) != 0)
            });
            value = answer.filter(|v| v & MDIC_ERROR == 0).map(|v| v as u16);
        }
        self.write(EXTCNF_CTRL, self.read(EXTCNF_CTRL) & !EXTCNF_SOFTWARE_FLAG);
        value
    }
}

/// "1000 Mb/s, full duplex" from the status register.
pub fn link_speed(status: u32) -> alloc::string::String {
    let speed = match (status >> 6) & 3 {
        0 => 10,
        1 => 100,
        _ => 1000,
    };
    alloc::format!("{speed} Mb/s, {} duplex", if status & STATUS_FULL_DUPLEX != 0 { "full" } else { "half" })
}

/// Wait about `us` microseconds (a write to the unused port 0x80 takes about one).
fn delay_us(us: u32) {
    for _ in 0..us {
        unsafe { outb(0x80, 0) };
    }
}

fn delay_ms(ms: u32) {
    delay_us(ms * 1000);
}

impl NetDevice for E1000 {
    fn name(&self) -> &'static str {
        self.name
    }

    fn details(&self) -> Option<alloc::string::String> {
        let status = self.read(STATUS);
        let mut text = if status & STATUS_LINK_UP != 0 { link_speed(status) } else { alloc::string::String::from("no link") };
        if self.family == Family::Chipset {
            // What we need to see when the link does not come up on real hardware.
            let phy = match (self.read_phy(2), self.read_phy(3)) {
                (Some(high), Some(low)) => alloc::format!("{high:04x}{low:04x}"),
                _ => alloc::string::String::from("no answer"),
            };
            // The card's own view: did our frames reach the wire?
            let mut counted = self.counted.get();
            for (total, register) in counted.iter_mut().zip([GPTC, GPRC, CRCERRS, MPC]) {
                *total += self.read(register) as u64;
            }
            self.counted.set(counted);
            let waiting = (0..TX_RING)
                .filter(|&i| self.tx_started[i] && unsafe { read_volatile(&raw const self.tx[i].status) } & DESC_DONE == 0)
                .count();
            text += &alloc::format!(
                "\n         on the wire: {} sent ({waiting} stuck), {} received, {} damaged, {} missed\n         \
                 status {status:08x} ctrl {:08x} ext {:08x} fw {:08x}\n         phy {phy} txdctl {:08x} tctl {:08x} rctl {:08x}\n         \
                 queue: head {} tail {} at {:08x} length {} tarc {:08x}",
                counted[0],
                counted[1],
                counted[2],
                counted[3],
                self.read(CTRL),
                self.read(CTRL_EXT),
                self.read(FWSM),
                self.read(TXDCTL),
                self.read(TCTL),
                self.read(RCTL),
                self.read(TDH),
                self.read(TDT),
                self.read(TDBAL),
                self.read(TDLEN),
                self.read(TARC0)
            );
        }
        Some(text)
    }

    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn link_up(&self) -> bool {
        self.read(STATUS) & STATUS_LINK_UP != 0
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), NetError> {
        if frame.len() < 14 || frame.len() > MAX_FRAME {
            return Err(NetError::BadFrame);
        }
        let i = self.tx_next;
        let desc = &mut self.tx[i] as *mut TxDesc;
        // The slot is free if never used or the card marked it done.
        if self.tx_started[i] && unsafe { read_volatile(&raw const (*desc).status) } & DESC_DONE == 0 {
            return Err(NetError::Busy);
        }
        let len = frame.len().max(MIN_FRAME);
        let buf = &mut self.tx_buf[i].0;
        buf[..frame.len()].copy_from_slice(frame);
        buf[frame.len()..len].fill(0); // pad short frames
        unsafe {
            write_volatile(&raw mut (*desc).length, len as u16);
            write_volatile(&raw mut (*desc).status, 0);
            write_volatile(&raw mut (*desc).cmd, TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS);
        }
        self.tx_started[i] = true;
        self.tx_next = ring_next(i, TX_RING);
        self.write(TDT, self.tx_next as u32); // hand it to the card
        Ok(())
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        loop {
            let i = self.rx_next;
            let desc = &mut self.rx[i] as *mut RxDesc;
            let status = unsafe { read_volatile(&raw const (*desc).status) };
            if status & DESC_DONE == 0 {
                if self.rx_tail_dirty {
                    self.write_flush();
                    self.rx_tail_dirty = false;
                }
                return None;
            }
            let len = unsafe { read_volatile(&raw const (*desc).length) } as usize;
            let errors = unsafe { read_volatile(&raw const (*desc).errors) };
            let frame = (status & RX_END_OF_PACKET != 0 && errors == 0 && (14..=BUFFER_SIZE).contains(&len))
                .then(|| self.rx_buf[i].0[..len].to_vec());
            unsafe { write_volatile(&raw mut (*desc).status, 0) };
            self.rx_next = ring_next(i, RX_RING);
            self.write(RDT, i as u32); // give the descriptor back to the card
            self.rx_tail_dirty = true;
            if frame.is_some() {
                return frame;
            } // else: a bad or oversized frame was dropped; look at the next one
        }
    }
}
