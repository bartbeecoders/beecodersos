//! Files the bootloader loaded into memory next to the kernel ("Multiboot
//! modules"). The install medium carries one: the image `install` writes to
//! the start of a disk (see install.rs, Docs/install-plan.html).
//!
//! The list is read before the heap is set up, and the heap stays out of the
//! memory the modules occupy (allocator.rs asks `span`).

use crate::ring::Global;

/// Multiboot info flag: the module fields are valid.
const FLAG_MODULES: u32 = 1 << 3;
const MAX_MODULES: usize = 4;
/// All that boot.s maps.
const MAPPED: u64 = 4 << 30;

/// (first byte, one past the last byte) of each module.
static MODULES: Global<([(u64, u64); MAX_MODULES], usize)> = Global::new(([(0, 0); MAX_MODULES], 0));

/// The list as the bootloader gives it: 16 bytes per module (start, end,
/// name, unused). Entries that make no sense are left out. `read32` reads
/// memory; the tests pass their own.
pub fn parse(flags: u32, count: u32, list: u64, read32: impl Fn(u64) -> u32) -> ([(u64, u64); MAX_MODULES], usize) {
    let mut found = ([(0, 0); MAX_MODULES], 0);
    if flags & FLAG_MODULES == 0 || list == 0 {
        return found;
    }
    for i in 0..(count as u64).min(MAX_MODULES as u64) {
        let (start, end) = (read32(list + i * 16) as u64, read32(list + i * 16 + 4) as u64);
        if start >= 0x10_0000 && end > start && end <= MAPPED {
            found.0[found.1] = (start, end);
            found.1 += 1;
        }
    }
    found
}

/// Must run before the heap is set up.
pub fn remember(multiboot_info: u32) {
    let read32 = |addr: u64| unsafe { core::ptr::read_unaligned(addr as *const u32) };
    let mbi = multiboot_info as u64;
    *MODULES.get() = parse(read32(mbi), read32(mbi + 20), read32(mbi + 24) as u64, read32);
}

/// The memory all modules lie in, from the lowest start to the highest end.
pub fn span() -> Option<(u64, u64)> {
    let (modules, count) = MODULES.get();
    let used = &modules[..*count];
    Some((used.iter().map(|m| m.0).min()?, used.iter().map(|m| m.1).max()?))
}

/// The modules' contents.
pub fn modules() -> impl Iterator<Item = &'static [u8]> {
    let (modules, count) = MODULES.get();
    modules[..*count].iter().map(|&(start, end)| unsafe { core::slice::from_raw_parts(start as *const u8, (end - start) as usize) })
}
