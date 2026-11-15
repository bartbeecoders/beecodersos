//! PCI: find devices on the bus and read their configuration (legacy
//! configuration access through I/O ports 0xCF8 / 0xCFC).

use crate::port::{inl, outl};
use alloc::vec::Vec;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Command register bits.
pub const CMD_MEMORY: u16 = 1 << 1;
pub const CMD_BUS_MASTER: u16 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub irq: u8,
}

/// A decoded base address register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    Memory { addr: u64, is_64bit: bool },
    Io { port: u16 },
    None,
}

/// Value for CONFIG_ADDRESS selecting a register (offset is rounded down to 4).
pub fn config_address(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    0x8000_0000 | (bus as u32) << 16 | ((dev & 0x1F) as u32) << 11 | ((func & 7) as u32) << 8 | (offset & 0xFC) as u32
}

fn read32(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    unsafe {
        outl(CONFIG_ADDRESS, config_address(bus, dev, func, offset));
        inl(CONFIG_DATA)
    }
}

fn write32(bus: u8, dev: u8, func: u8, offset: u8, value: u32) {
    unsafe {
        outl(CONFIG_ADDRESS, config_address(bus, dev, func, offset));
        outl(CONFIG_DATA, value);
    }
}

fn read8(bus: u8, dev: u8, func: u8, offset: u8) -> u8 {
    (read32(bus, dev, func, offset) >> ((offset & 3) * 8)) as u8
}

fn probe(bus: u8, dev: u8, func: u8) -> Option<Device> {
    let id = read32(bus, dev, func, 0x00);
    let vendor = id as u16;
    if vendor == 0xFFFF {
        return None; // nothing there
    }
    let class = read32(bus, dev, func, 0x08);
    Some(Device {
        bus,
        dev,
        func,
        vendor,
        device: (id >> 16) as u16,
        class: (class >> 24) as u8,
        subclass: (class >> 16) as u8,
        irq: read8(bus, dev, func, 0x3C),
    })
}

/// All devices, following PCI-to-PCI bridges from bus 0.
pub fn scan() -> Vec<Device> {
    let mut found = Vec::new();
    let mut buses = alloc::vec![0u8];
    let mut next = 0;
    while next < buses.len() {
        let bus = buses[next];
        next += 1;
        for dev in 0..32 {
            let Some(first) = probe(bus, dev, 0) else { continue };
            let multi = read8(bus, dev, 0, 0x0E) & 0x80 != 0;
            for func in 0..if multi { 8 } else { 1 } {
                let Some(d) = (if func == 0 { Some(first) } else { probe(bus, dev, func) }) else { continue };
                found.push(d);
                if d.class == 0x06 && d.subclass == 0x04 {
                    let secondary = read8(bus, dev, func, 0x19);
                    if secondary != 0 && !buses.contains(&secondary) && buses.len() < 32 {
                        buses.push(secondary);
                    }
                }
            }
        }
    }
    found
}

/// Decode a BAR from its raw value(s). `high` is the next BAR (for 64-bit ones).
pub fn decode_bar(low: u32, high: u32) -> Bar {
    if low == 0 {
        Bar::None
    } else if low & 1 == 1 {
        Bar::Io { port: (low & 0xFFFC) as u16 }
    } else {
        let is_64bit = (low >> 1) & 3 == 2;
        let addr = (low & 0xFFFF_FFF0) as u64 | if is_64bit { (high as u64) << 32 } else { 0 };
        Bar::Memory { addr, is_64bit }
    }
}

/// Most capabilities one device can have: the list lives in 192 bytes of
/// 4-byte-aligned entries, so a longer one goes in circles.
pub const MAX_CAPABILITIES: usize = 48;

/// Capability ids we name or use.
pub const CAP_POWER: u8 = 0x01;
pub const CAP_MSI: u8 = 0x05;
pub const CAP_PCIE: u8 = 0x10;
pub const CAP_MSIX: u8 = 0x11;

/// A device's capability list: (id, offset in the configuration space) of
/// each entry. `read` gives one byte of the configuration space. The list
/// comes from the device, so it is not trusted: offsets outside the device's
/// own area end it, and so does a list that points back into itself.
pub fn walk_capabilities(read: impl Fn(u8) -> u8) -> Vec<(u8, u8)> {
    let mut found = Vec::new();
    // Status register bit 4: "has a capability list". Header type 2
    // (CardBus bridges) keeps the pointer elsewhere: not supported.
    if read(0x06) & 1 << 4 == 0 || read(0x0E) & 0x7F > 1 {
        return found;
    }
    let mut at = read(0x34) & !3;
    while at >= 0x40 && found.len() < MAX_CAPABILITIES && !found.iter().any(|&(_, seen)| seen == at) {
        let id = read(at);
        if id == 0xFF {
            break; // nothing answers there
        }
        found.push((id, at));
        at = read(at + 1) & !3;
    }
    found
}

pub fn capability_name(id: u8) -> Option<&'static str> {
    Some(match id {
        CAP_POWER => "power management",
        CAP_MSI => "MSI",
        CAP_PCIE => "PCI Express",
        CAP_MSIX => "MSI-X",
        0x09 => "vendor specific",
        0x0A => "debug port",
        0x0D => "bridge subsystem id",
        0x12 => "SATA",
        0x13 => "advanced features",
        _ => return None,
    })
}

impl Device {
    /// (id, offset) of each capability: MSI, PCI Express, power management, ...
    pub fn capabilities(&self) -> Vec<(u8, u8)> {
        walk_capabilities(|offset| read8(self.bus, self.dev, self.func, offset))
    }

    pub fn bar(&self, n: u8) -> Bar {
        let off = 0x10 + n * 4;
        let low = read32(self.bus, self.dev, self.func, off);
        let high = if n < 5 { read32(self.bus, self.dev, self.func, off + 4) } else { 0 };
        decode_bar(low, high)
    }

    /// Set bits in the command register (memory access, bus mastering, ...).
    pub fn enable(&self, bits: u16) {
        let reg = read32(self.bus, self.dev, self.func, 0x04);
        // The upper half is the status register: its bits clear when written as 1.
        write32(self.bus, self.dev, self.func, 0x04, (reg & 0xFFFF) | bits as u32);
    }

    pub fn class_name(&self) -> &'static str {
        match (self.class, self.subclass) {
            (0x01, 0x01) => "IDE controller",
            (0x01, 0x06) => "SATA controller",
            (0x01, _) => "Storage controller",
            (0x02, 0x00) => "Ethernet controller",
            (0x02, _) => "Network controller",
            (0x03, _) => "Display controller",
            (0x04, _) => "Multimedia device",
            (0x06, 0x00) => "Host bridge",
            (0x06, 0x01) => "ISA bridge",
            (0x06, 0x04) => "PCI bridge",
            (0x06, _) => "Bridge",
            (0x07, _) => "Communication controller",
            (0x08, _) => "System peripheral",
            (0x09, _) => "Input device",
            (0x0C, 0x03) => "USB controller",
            (0x0C, 0x05) => "SMBus controller",
            (0x0C, _) => "Serial bus controller",
            _ => "Device",
        }
    }
}
