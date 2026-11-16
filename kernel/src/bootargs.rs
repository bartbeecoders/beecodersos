//! Kernel command line from the bootloader (Multiboot `cmdline`), e.g.
//! `qemu -kernel ... -append "test selftest"` or `cmdline:` in limine.conf.
//!
//! Flags:
//!   `test`      test mode: a panic or CPU exception ends QEMU with a failure
//!               exit code (isa-debug-exit) instead of halting forever
//!   `selftest`  run the in-kernel self-tests at boot (in test mode: then exit)
//!   `slot=a|b`  which A/B kernel slot was booted (set by the boot disk)
//!   `failboot`  tests: a trial kernel panics after arming the rollback
//!   `nodhcp`    tests: don't request an address automatically at boot
//!   `net=poll`  don't use the network card's interrupt, only poll it (for
//!               a machine where the interrupt line misbehaves)
//!   `nic=chipset` tests: start the network card the way a chipset card
//!               (Intel I217) is started, whatever card it is
//!   `display=bochs|bootfb|text` which display driver to use (`text`: none, the
//!               80x25 VGA text screen); see display.rs
//!   `mode=1920x1080` the screen resolution to ask the display driver for
//!   `desktop=on|off` the desktop for this boot, whatever was saved (plain
//!               `desktop` means on); see console::desktop_choice

use crate::ring::Global;

const MAX_LEN: usize = 256;

static CMDLINE: Global<([u8; MAX_LEN], usize)> = Global::new(([0; MAX_LEN], 0));

/// Copy the command line out of the Multiboot info. Must run before the heap
/// is set up: the heap may reuse the memory the bootloader put it in.
pub fn init(multiboot_info: u32) {
    let mbi = multiboot_info as u64;
    let flags = unsafe { core::ptr::read_unaligned(mbi as *const u32) };
    if flags & (1 << 2) == 0 {
        return;
    }
    let mut p = unsafe { core::ptr::read_unaligned((mbi + 16) as *const u32) } as u64;
    let (buf, len) = CMDLINE.get();
    while *len < MAX_LEN {
        let b = unsafe { core::ptr::read(p as *const u8) };
        if b == 0 {
            break;
        }
        buf[*len] = b;
        *len += 1;
        p += 1;
    }
}

pub fn cmdline() -> &'static str {
    let (buf, len) = CMDLINE.get();
    core::str::from_utf8(&buf[..*len]).unwrap_or("")
}

/// True when `flag` appears as a separate word on the command line.
pub fn has(flag: &str) -> bool {
    cmdline().split_whitespace().any(|w| w == flag)
}

/// The value of a `key=value` word on the command line (the first one).
pub fn value(key: &str) -> Option<&'static str> {
    value_in(cmdline(), key)
}

pub fn value_in<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_whitespace().find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}
