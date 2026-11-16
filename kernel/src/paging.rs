//! Page table changes after boot. For now one thing: mark the framebuffer
//! "write-combining", so the CPU collects pixel writes and sends them to
//! the graphics card in bursts. Without it every write to video memory goes
//! out on its own, which makes drawing on real hardware many times slower.
//!
//! boot.s maps the first 4 GiB with 2 MiB pages. This module only changes
//! the caching bits of entries that are already there; boot.s is untouched.
//!
//! How: the CPU's PAT register lists eight caching types and each page picks
//! one with three bits. Nothing uses type 1 (PWT bit set), so that one is
//! changed from "write-through" to "write-combining", and the framebuffer's
//! pages get the PWT bit.

use core::arch::asm;

const MSR_PAT: u32 = 0x277;
const PAT_WRITE_COMBINING: u64 = 0x01;

const PRESENT: u64 = 1 << 0;
const PWT: u64 = 1 << 3;
const PCD: u64 = 1 << 4;
const LARGE: u64 = 1 << 7;
/// The third type bit of a large page.
const PAT_LARGE: u64 = 1 << 12;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const PAGE_2M: u64 = 2 << 20;
const MAPPED: u64 = 4 << 30;

/// The PAT register with type 1 set to write-combining.
pub fn pat_with_wc(pat: u64) -> u64 {
    pat & !(0xFF << 8) | PAT_WRITE_COMBINING << 8
}

/// The 2 MiB pages that hold `addr..addr + len`: (number of the first, how
/// many). `None` when the area is empty or not inside the first 4 GiB. The
/// last page is taken whole: a framebuffer sits in a block of video memory
/// that is a power of two in size, so the rest of that page is video memory too.
pub fn pages_2m(addr: u64, len: u64) -> Option<(usize, usize)> {
    let end = addr.checked_add(len)?;
    if len == 0 || end > MAPPED {
        return None;
    }
    let first = addr / PAGE_2M;
    let last = (end - 1) / PAGE_2M;
    Some((first as usize, (last - first + 1) as usize))
}

/// A page directory entry with write-combining switched on. Entries that
/// are not a present 2 MiB page come back unchanged.
pub fn entry_with_wc(entry: u64) -> u64 {
    if entry & (PRESENT | LARGE) != PRESENT | LARGE {
        return entry;
    }
    entry & !(PCD | PAT_LARGE) | PWT
}

fn has_pat() -> bool {
    let edx: u32;
    unsafe {
        asm!("mov {tmp:r}, rbx", "cpuid", "mov rbx, {tmp:r}", tmp = out(reg) _,
             inout("eax") 1u32 => _, out("ecx") _, out("edx") edx, options(nostack, preserves_flags));
    }
    edx & 1 << 16 != 0
}

/// Mark `addr..addr + len` (a framebuffer) write-combining. Safe to call
/// again for the same or another area.
pub fn write_combine(addr: u64, len: u64) -> Result<(), &'static str> {
    let (first, count) = pages_2m(addr, len).ok_or("the area is not in the first 4 GiB")?;
    if !has_pat() {
        return Err("this CPU has no PAT register");
    }
    unsafe {
        let (low, high): (u32, u32);
        asm!("rdmsr", in("ecx") MSR_PAT, out("eax") low, out("edx") high, options(nostack, preserves_flags));
        let pat = pat_with_wc((high as u64) << 32 | low as u64);
        asm!("wrmsr", in("ecx") MSR_PAT, in("eax") pat as u32, in("edx") (pat >> 32) as u32,
             options(nostack, preserves_flags));

        // Physical addresses are virtual addresses here (identity mapping).
        let cr3: u64;
        asm!("mov {}, cr3", out(reg) cr3, options(nostack, preserves_flags));
        let pml4 = (cr3 & ADDR_MASK) as *const u64;
        let pml4e = pml4.read_volatile();
        if pml4e & PRESENT == 0 {
            return Err("unexpected page tables");
        }
        let pdpt = (pml4e & ADDR_MASK) as *const u64;
        for page in first..first + count {
            let pdpte = pdpt.add(page / 512).read_volatile();
            if pdpte & (PRESENT | LARGE) != PRESENT {
                return Err("unexpected page tables");
            }
            let entry = ((pdpte & ADDR_MASK) as *mut u64).add(page % 512);
            entry.write_volatile(entry_with_wc(entry.read_volatile()));
        }
        // Loading CR3 again makes the CPU forget the old entries.
        asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
    }
    Ok(())
}
