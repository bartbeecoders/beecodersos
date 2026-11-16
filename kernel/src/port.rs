//! x86 I/O port access.

use core::arch::asm;

pub unsafe fn outb(port: u16, val: u8) {
    asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags));
    val
}

pub unsafe fn outw(port: u16, val: u16) {
    asm!("out dx, ax", in("dx") port, in("ax") val, options(nomem, nostack, preserves_flags));
}

pub unsafe fn inw(port: u16) -> u16 {
    let val: u16;
    asm!("in ax, dx", out("ax") val, in("dx") port, options(nomem, nostack, preserves_flags));
    val
}

/// Send a block of 16-bit words to a port in one go (`data.len()` must be even).
pub unsafe fn outsw(port: u16, data: &[u8]) {
    asm!("rep outsw", in("dx") port, inout("rsi") data.as_ptr() => _, inout("rcx") data.len() / 2 => _,
         options(nostack, preserves_flags, readonly));
}

/// Read a block of 16-bit words from a port in one go (`buf.len()` must be even).
pub unsafe fn insw(port: u16, buf: &mut [u8]) {
    asm!("rep insw", in("dx") port, inout("rdi") buf.as_mut_ptr() => _, inout("rcx") buf.len() / 2 => _,
         options(nostack, preserves_flags));
}

pub unsafe fn outl(port: u16, val: u32) {
    asm!("out dx, eax", in("dx") port, in("eax") val, options(nomem, nostack, preserves_flags));
}

pub unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    asm!("in eax, dx", out("eax") val, in("dx") port, options(nomem, nostack, preserves_flags));
    val
}
