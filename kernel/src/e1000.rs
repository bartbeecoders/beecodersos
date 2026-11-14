//! Intel e1000 network card (82540EM, PCI 8086:100E; QEMU's and libvirt's
//! default card). Polled: interrupts stay masked, `receive()` is called from
//! the idle loop and from waiting network calls.
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
/// 82540EM (QEMU "e1000") and 82545EM (VMware).
const DEVICE_IDS: [u16; 2] = [0x100E, 0x100F];

// Registers (offsets into the memory-mapped BAR 0).
const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EERD: usize = 0x0014;
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
const MTA: usize = 0x5200;
const RAL0: usize = 0x5400;
const RAH0: usize = 0x5404;

const CTRL_ASDE: u32 = 1 << 5; // auto-speed detection
const CTRL_SLU: u32 = 1 << 6; // set link up
const CTRL_RST: u32 = 1 << 26;
const STATUS_LINK_UP: u32 = 1 << 1;
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
}

/// Index after `i` in a ring of `len` slots.
pub fn ring_next(i: usize, len: usize) -> usize {
    (i + 1) % len
}

impl E1000 {
    /// Find an e1000 on the PCI bus and bring it up.
    pub fn find(devices: &[Device]) -> Option<Result<Self, &'static str>> {
        let dev = devices.iter().find(|d| d.vendor == VENDOR_INTEL && DEVICE_IDS.contains(&d.device))?;
        Some(Self::init(dev))
    }

    fn init(dev: &Device) -> Result<Self, &'static str> {
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
            E1000 { regs, mac: [0; 6], rx, rx_buf, rx_next: 0, tx, tx_buf, tx_next: 0, tx_started: [false; TX_RING], rx_tail_dirty: false };

        // Reset, with all interrupts masked before and after (we poll).
        card.write(IMC, 0xFFFF_FFFF);
        card.write(CTRL, card.read(CTRL) | CTRL_RST);
        let mut reset_done = false;
        for _ in 0..100_000 {
            unsafe { outb(0x80, 0) }; // ~1 microsecond
            if card.read(CTRL) & CTRL_RST == 0 {
                reset_done = true;
                break;
            }
        }
        if !reset_done {
            return Err("e1000: reset did not finish");
        }
        card.write(IMC, 0xFFFF_FFFF);
        card.write(CTRL, card.read(CTRL) | CTRL_SLU | CTRL_ASDE);

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
        card.write(TCTL, TCTL_EN | TCTL_PSP | 0x10 << 4 | 0x40 << 12); // collision settings from the manual
        card.write(TIPG, 10 | 8 << 10 | 6 << 20);
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
        let mut mac = [0u8; 6];
        for word in 0..3 {
            self.write(EERD, (word as u32) << 8 | 1);
            let value = (0..100_000).find_map(|_| Some(self.read(EERD)).filter(|v| v & (1 << 4) != 0))?;
            mac[word * 2..word * 2 + 2].copy_from_slice(&((value >> 16) as u16).to_le_bytes());
        }
        (mac != [0; 6] && mac != [0xFF; 6]).then_some(mac)
    }
}

impl NetDevice for E1000 {
    fn name(&self) -> &'static str {
        "e1000 (Intel 82540EM)"
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
