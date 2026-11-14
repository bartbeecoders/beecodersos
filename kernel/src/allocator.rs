//! Kernel heap: a linked-list allocator placed in free RAM found through the
//! Multiboot memory map. Enables `alloc` (Vec, String, Box, ...) in the kernel.

use linked_list_allocator::LockedHeap;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Upper bound for the heap; the rest of RAM stays free for later use.
const MAX_HEAP: u64 = 64 * 1024 * 1024;
const MIN_HEAP: u64 = 4 * 1024 * 1024;
/// Only memory covered by the boot page tables (identity-mapped) is usable.
const MAPPED_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

static mut HEAP_SIZE: usize = 0;

extern "C" {
    static __kernel_end: u8;
}

/// Pick the largest free RAM region above the kernel and use it as heap.
/// Returns (start, size).
pub fn init(multiboot_info: u32) -> Result<(u64, usize), &'static str> {
    let kernel_end = unsafe { &__kernel_end as *const u8 as u64 };
    let mut best: Option<(u64, u64)> = None;
    for_each_free_region(multiboot_info, |base, len| {
        let start = align_up(base.max(kernel_end), 4096);
        let end = (base + len).min(MAPPED_LIMIT) & !4095;
        if end > start && best.is_none_or(|(s, e)| end - start > e - s) {
            best = Some((start, end));
        }
    })?;
    let (start, end) = best.ok_or("no usable memory found")?;
    let size = (end - start).min(MAX_HEAP);
    if size < MIN_HEAP {
        return Err("not enough free memory for the kernel heap");
    }
    // SAFETY: the region is RAM marked free by the bootloader, lies above the
    // kernel image and is identity-mapped. Multiboot data inside it is no
    // longer needed: the memory map was fully read above.
    unsafe {
        ALLOCATOR.lock().init(start as *mut u8, size as usize);
        HEAP_SIZE = size as usize;
    }
    Ok((start, size as usize))
}

pub fn heap_size() -> usize {
    unsafe { HEAP_SIZE }
}

pub fn used() -> usize {
    ALLOCATOR.lock().used()
}

fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

/// Walk the Multiboot (v1) memory map, calling `f(base, len)` for free RAM.
fn for_each_free_region(mbi: u32, mut f: impl FnMut(u64, u64)) -> Result<(), &'static str> {
    let read32 = |addr: u64| unsafe { core::ptr::read_unaligned(addr as *const u32) };
    let read64 = |addr: u64| unsafe { core::ptr::read_unaligned(addr as *const u64) };
    let mbi = mbi as u64;
    let flags = read32(mbi);
    if flags & (1 << 6) != 0 {
        // mmap_length @44, mmap_addr @48; entries: size u32, base u64, len u64, type u32
        let (len, addr) = (read32(mbi + 44) as u64, read32(mbi + 48) as u64);
        let mut p = addr;
        while p < addr + len {
            let entry_size = read32(p) as u64;
            if read32(p + 20) == 1 {
                f(read64(p + 4), read64(p + 12));
            }
            p += entry_size + 4;
        }
        Ok(())
    } else if flags & 1 != 0 {
        // Only basic info: mem_upper (KiB above 1 MiB) @8.
        f(0x10_0000, read32(mbi + 8) as u64 * 1024);
        Ok(())
    } else {
        Err("bootloader provided no memory information")
    }
}
