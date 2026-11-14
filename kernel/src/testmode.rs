//! Test mode (`test` on the kernel command line): lets automated tests end a
//! QEMU run with a pass/fail exit code through the `isa-debug-exit` device
//! (`-device isa-debug-exit,iobase=0xf4,iosize=0x04`).
//!
//! QEMU exits with status `(code << 1) | 1`: SUCCESS -> 33, FAILURE -> 35.

use crate::{bootargs, port, power};

const DEBUG_EXIT_PORT: u16 = 0xF4;
pub const SUCCESS: u8 = 0x10;
pub const FAILURE: u8 = 0x11;

pub fn enabled() -> bool {
    bootargs::has("test")
}

/// End the QEMU run with `code`. Only used in test mode and under a
/// hypervisor: on real hardware port 0xF4 may belong to something else.
pub fn exit(code: u8) -> ! {
    if power::running_in_vm() {
        unsafe { port::outb(DEBUG_EXIT_PORT, code) };
    }
    // No debug-exit device (or not a VM): power off instead.
    power::shutdown()
}

/// Called by the panic and CPU exception handlers.
pub fn on_fatal_error() {
    if enabled() {
        exit(FAILURE);
    }
}
