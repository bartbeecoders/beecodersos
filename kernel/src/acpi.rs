//! Minimal ACPI support: find the FADT and DSDT, switch the chipset to ACPI
//! mode, handle the power button (SCI interrupt), and provide ACPI power-off
//! (S5) and reset.
//!
//! This is what makes "Shut Down" / "Reboot" in virt-manager, `virsh shutdown`
//! and QEMU's `system_powerdown` work: they press a virtual ACPI power button.

use crate::port::{inw, outb, outw};
use crate::ring::Global;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};

const PM1_PWRBTN: u16 = 1 << 8;
const PM1_CNT_SCI_EN: u16 = 1 << 0;
const PM1_CNT_SLP_EN: u16 = 1 << 13;
const FADT_RESET_REG_SUP: u32 = 1 << 10;
/// Only tables in the identity-mapped first 4 GiB are reachable.
const MAPPED_LIMIT: u64 = 1 << 32;

/// Values the SCI interrupt handler needs (lock-free).
static SCI_IRQ: AtomicU8 = AtomicU8::new(0xFF);
static PM1A_EVT: AtomicU16 = AtomicU16::new(0);
static PM1B_EVT: AtomicU16 = AtomicU16::new(0);
static PM1_EVT_HALF: AtomicU16 = AtomicU16::new(0);
static POWER_BUTTON: AtomicBool = AtomicBool::new(false);

static INFO: Global<Option<Acpi>> = Global::new(None);

#[derive(Clone, Copy)]
struct Acpi {
    pm1a_cnt: u16,
    pm1b_cnt: u16,
    /// SLP_TYPa / SLP_TYPb for the S5 (soft-off) state, from the DSDT.
    s5: Option<(u16, u16)>,
    /// Reset register: (address space, address, value).
    reset: Option<(u8, u64, u8)>,
}

/// Summary of what was found, for the boot log.
pub struct Status {
    pub sci_irq: u8,
    pub s5: bool,
    pub reset: bool,
}

/// Locate and initialize ACPI. Returns `None` when no ACPI tables are found
/// (e.g. UEFI boots, where the RSDP is not in legacy BIOS memory).
pub fn init() -> Option<Status> {
    let rsdp = find_rsdp()?;
    let fadt = find_table(rsdp, b"FACP")?;
    let len = read32(fadt + 4) as u64;
    let field8 = |off: u64| if off < len { read8(fadt + off) } else { 0 };
    let field32 = |off: u64| if off + 4 <= len { read32(fadt + off) } else { 0 };

    let sci_irq = read16(fadt + 46) as u8;
    let smi_cmd = field32(48) as u16;
    let acpi_enable = field8(52);
    let pm1a_evt = field32(56) as u16;
    let pm1b_evt = field32(60) as u16;
    let pm1a_cnt = field32(64) as u16;
    let pm1b_cnt = field32(68) as u16;
    let pm1_evt_len = field8(88) as u16;
    let flags = field32(112);
    if pm1a_evt == 0 || pm1a_cnt == 0 || pm1_evt_len < 4 {
        return None;
    }

    let mut dsdt = field32(40) as u64;
    if len >= 148 {
        let x_dsdt = read64(fadt + 140);
        if x_dsdt != 0 && x_dsdt < MAPPED_LIMIT {
            dsdt = x_dsdt;
        }
    }
    let s5 = if dsdt != 0 && dsdt < MAPPED_LIMIT { find_s5(dsdt) } else { None };

    let reset = if flags & FADT_RESET_REG_SUP != 0 && len >= 129 {
        let space = read8(fadt + 116);
        let addr = read64(fadt + 120);
        (addr != 0).then(|| (space, addr, read8(fadt + 128)))
    } else {
        None
    };

    unsafe {
        // Switch from legacy (SMM) to ACPI mode if the firmware hasn't yet.
        if inw(pm1a_cnt) & PM1_CNT_SCI_EN == 0 && smi_cmd != 0 && acpi_enable != 0 {
            outb(smi_cmd, acpi_enable);
            for _ in 0..1_000_000 {
                if inw(pm1a_cnt) & PM1_CNT_SCI_EN != 0 {
                    break;
                }
            }
        }
        // Clear stale events, then enable only the power button event.
        let half = pm1_evt_len / 2;
        for evt in [pm1a_evt, pm1b_evt] {
            if evt != 0 {
                outw(evt, inw(evt)); // status bits are write-1-to-clear
                outw(evt + half, PM1_PWRBTN);
            }
        }
        PM1A_EVT.store(pm1a_evt, Ordering::Relaxed);
        PM1B_EVT.store(pm1b_evt, Ordering::Relaxed);
        PM1_EVT_HALF.store(half, Ordering::Relaxed);
    }
    SCI_IRQ.store(sci_irq, Ordering::Release);
    *INFO.get() = Some(Acpi { pm1a_cnt, pm1b_cnt, s5, reset });
    crate::interrupts::unmask_irq(sci_irq);
    Some(Status { sci_irq, s5: s5.is_some(), reset: reset.is_some() })
}

pub fn sci_irq() -> u8 {
    SCI_IRQ.load(Ordering::Acquire)
}

/// SCI interrupt handler: acknowledge events, remember power button presses.
pub fn on_interrupt() {
    let half = PM1_EVT_HALF.load(Ordering::Relaxed);
    for evt in [PM1A_EVT.load(Ordering::Relaxed), PM1B_EVT.load(Ordering::Relaxed)] {
        if evt == 0 {
            continue;
        }
        unsafe {
            let status = inw(evt);
            let enabled = inw(evt + half);
            if status & enabled & PM1_PWRBTN != 0 {
                POWER_BUTTON.store(true, Ordering::Release);
            }
            outw(evt, status); // acknowledge everything that was set
        }
    }
}

pub fn power_button_pending() -> bool {
    POWER_BUTTON.load(Ordering::Acquire)
}

/// Returns true once per power button press.
pub fn take_power_button() -> bool {
    POWER_BUTTON.swap(false, Ordering::AcqRel)
}

/// Enter S5 (soft off). Returns only if that did not work.
pub fn power_off() {
    let Some(acpi) = *INFO.get() else { return };
    let Some((typ_a, typ_b)) = acpi.s5 else { return };
    unsafe {
        for (cnt, typ) in [(acpi.pm1a_cnt, typ_a), (acpi.pm1b_cnt, typ_b)] {
            if cnt != 0 {
                let v = (inw(cnt) & !(0x7 << 10)) | (typ & 0x7) << 10;
                outw(cnt, v | PM1_CNT_SLP_EN);
            }
        }
        for _ in 0..10_000_000 {
            core::hint::spin_loop();
        }
    }
}

/// Use the FADT reset register. Returns only if that did not work.
pub fn reset() {
    let Some(Acpi { reset: Some((space, addr, value)), .. }) = *INFO.get() else { return };
    unsafe {
        match space {
            0 if addr < MAPPED_LIMIT => core::ptr::write_volatile(addr as *mut u8, value),
            1 => outb(addr as u16, value),
            _ => return,
        }
        for _ in 0..10_000_000 {
            core::hint::spin_loop();
        }
    }
}

// --- table discovery -------------------------------------------------------------

fn read8(a: u64) -> u8 {
    unsafe { core::ptr::read_volatile(a as *const u8) }
}
fn read16(a: u64) -> u16 {
    unsafe { core::ptr::read_unaligned(a as *const u16) }
}
fn read32(a: u64) -> u32 {
    unsafe { core::ptr::read_unaligned(a as *const u32) }
}
fn read64(a: u64) -> u64 {
    unsafe { core::ptr::read_unaligned(a as *const u64) }
}

fn checksum_ok(addr: u64, len: u64) -> bool {
    (0..len).fold(0u8, |s, i| s.wrapping_add(read8(addr + i))) == 0
}

/// Search the EBDA and the BIOS area 0xE0000-0xFFFFF for "RSD PTR ".
fn find_rsdp() -> Option<u64> {
    let ebda = (read16(0x40E) as u64) << 4;
    let ranges = [(ebda, ebda + 1024), (0xE0000, 0x100000)];
    for (start, end) in ranges {
        if start == 0 {
            continue;
        }
        let mut p = start & !15;
        while p + 20 <= end {
            if unsafe { core::slice::from_raw_parts(p as *const u8, 8) } == b"RSD PTR " && checksum_ok(p, 20) {
                return Some(p);
            }
            p += 16;
        }
    }
    None
}

/// Find a table by signature through the XSDT (ACPI 2+) or RSDT.
fn find_table(rsdp: u64, sig: &[u8; 4]) -> Option<u64> {
    let revision = read8(rsdp + 15);
    let (root, entry_size) = if revision >= 2 && read64(rsdp + 24) != 0 && read64(rsdp + 24) < MAPPED_LIMIT {
        (read64(rsdp + 24), 8)
    } else {
        (read32(rsdp + 16) as u64, 4)
    };
    if root == 0 || !checksum_ok(root, read32(root + 4) as u64) {
        return None;
    }
    let count = (read32(root + 4) as u64).saturating_sub(36) / entry_size;
    (0..count)
        .map(|i| if entry_size == 8 { read64(root + 36 + i * 8) } else { read32(root + 36 + i * 4) as u64 })
        .filter(|&t| t != 0 && t < MAPPED_LIMIT)
        .find(|&t| unsafe { core::slice::from_raw_parts(t as *const u8, 4) } == sig)
}

/// Extract SLP_TYPa/b from the `_S5_` package in the DSDT's AML.
fn find_s5(dsdt: u64) -> Option<(u16, u16)> {
    let len = read32(dsdt + 4) as usize;
    let aml = unsafe { core::slice::from_raw_parts(dsdt as *const u8, len) };
    let pos = aml.windows(4).position(|w| w == b"_S5_")?;
    // Expect: NameOp (0x08) [optional '\'] "_S5_" PackageOp (0x12)
    let name_op = pos >= 1 && (aml[pos - 1] == 0x08 || (pos >= 2 && aml[pos - 2] == 0x08 && aml[pos - 1] == b'\\'));
    if !name_op || aml.get(pos + 4) != Some(&0x12) {
        return None;
    }
    let mut p = pos + 5;
    p += ((aml.get(p)? & 0xC0) >> 6) as usize + 1; // PkgLength
    p += 1; // NumElements
    let mut value = || -> Option<u16> {
        let v = match *aml.get(p)? {
            0x0A => {
                p += 1; // BytePrefix
                *aml.get(p)? as u16
            }
            0x00 => 0, // ZeroOp
            0x01 => 1, // OneOp
            b => b as u16,
        };
        p += 1;
        Some(v)
    };
    let a = value()?;
    let b = value()?;
    Some((a, b))
}
