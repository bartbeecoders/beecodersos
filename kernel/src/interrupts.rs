//! IDT, legacy 8259 PIC and interrupt dispatch.
//!
//! CPU exceptions (vectors 0-31) print a diagnostic and halt. Hardware IRQs are
//! remapped to vectors 32-47; unmasked: IRQ0 (timer), IRQ1 (keyboard),
//! IRQ3 (COM2, AI bridge) and IRQ4 (COM1).

use crate::port::{inb, outb};
use crate::vga::Color;
use crate::{acpi, aibridge, console, crash, keyboard, serial, time};
use core::arch::asm;
use core::cell::UnsafeCell;

core::arch::global_asm!(include_str!("interrupts.s"));

extern "C" {
    static isr_table: [u64; 48];
}

pub const IRQ_BASE: u8 = 32;
const IRQ_TIMER: u8 = 0;
const IRQ_KEYBOARD: u8 = 1;
const IRQ_CASCADE: u8 = 2;
const IRQ_COM2: u8 = 3;
const IRQ_COM1: u8 = 4;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// Register state pushed by `isr_common` (lowest address first).
#[repr(C)]
pub struct InterruptFrame {
    r11: u64, r10: u64, r9: u64, r8: u64,
    rdi: u64, rsi: u64, rdx: u64, rcx: u64, rax: u64,
    vector: u64,
    error: u64,
    rip: u64, cs: u64, rflags: u64, rsp: u64, ss: u64,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const MISSING: IdtEntry = IdtEntry {
        offset_low: 0, selector: 0, ist: 0, type_attr: 0, offset_mid: 0, offset_high: 0, reserved: 0,
    };

    fn interrupt_gate(handler: u64) -> Self {
        IdtEntry {
            offset_low: handler as u16,
            selector: 0x08, // 64-bit kernel code segment from boot.s
            ist: 0,
            type_attr: 0x8E, // present, ring 0, 64-bit interrupt gate
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

#[repr(C, align(16))]
struct Idt(UnsafeCell<[IdtEntry; 256]>);
unsafe impl Sync for Idt {}

static IDT: Idt = Idt(UnsafeCell::new([IdtEntry::MISSING; 256]));

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

/// Install the IDT and remap the PIC. Interrupts stay disabled until `enable()`.
pub fn init() {
    unsafe {
        let idt = &mut *IDT.0.get();
        for (v, handler) in isr_table.iter().enumerate() {
            idt[v] = IdtEntry::interrupt_gate(*handler);
        }
        let ptr = IdtPointer { limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16, base: idt.as_ptr() as u64 };
        asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags));
        init_pic();
    }
}

unsafe fn init_pic() {
    let io_wait = || outb(0x80, 0);
    outb(PIC1_CMD, 0x11); io_wait(); // ICW1: init + ICW4 needed
    outb(PIC2_CMD, 0x11); io_wait();
    outb(PIC1_DATA, IRQ_BASE); io_wait(); // ICW2: vector offsets
    outb(PIC2_DATA, IRQ_BASE + 8); io_wait();
    outb(PIC1_DATA, 4); io_wait(); // ICW3: slave on IRQ2
    outb(PIC2_DATA, 2); io_wait();
    outb(PIC1_DATA, 0x01); io_wait(); // ICW4: 8086 mode
    outb(PIC2_DATA, 0x01); io_wait();
    let enabled = (1 << IRQ_TIMER) | (1 << IRQ_KEYBOARD) | (1 << IRQ_CASCADE) | (1 << IRQ_COM2) | (1 << IRQ_COM1);
    outb(PIC1_DATA, !enabled);
    outb(PIC2_DATA, 0xFF);
}

/// Unmask a PIC interrupt line (0-15).
pub fn unmask_irq(irq: u8) {
    if irq >= 16 {
        return;
    }
    unsafe {
        let (port, bit) = if irq < 8 { (PIC1_DATA, irq) } else { (PIC2_DATA, irq - 8) };
        outb(port, inb(port) & !(1 << bit));
    }
}

pub fn enable() {
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// Sleep until an interrupt arrives, unless `ready()` already holds.
/// Checking with interrupts off and then `sti; hlt` avoids missing a wakeup.
pub fn wait_until(ready: impl Fn() -> bool) {
    unsafe {
        asm!("cli", options(nomem, nostack));
        if ready() {
            asm!("sti", options(nomem, nostack));
        } else {
            asm!("sti; hlt", options(nomem, nostack));
        }
    }
}

#[no_mangle]
extern "C" fn interrupt_dispatch(frame: &mut InterruptFrame) {
    let vector = frame.vector as u8;
    if vector < IRQ_BASE {
        exception(frame);
    }
    let irq = vector - IRQ_BASE;
    unsafe {
        // Spurious IRQ 7/15: the in-service bit is not set; don't acknowledge it.
        if irq == 7 || irq == 15 {
            let cmd = if irq == 7 { PIC1_CMD } else { PIC2_CMD };
            outb(cmd, 0x0B);
            if inb(cmd) & 0x80 == 0 {
                if irq == 15 {
                    outb(PIC1_CMD, 0x20);
                }
                return;
            }
        }
        if irq == acpi::sci_irq() {
            acpi::on_interrupt(); // may share its line with another device
        }
        match irq {
            IRQ_TIMER => time::on_tick(),
            IRQ_KEYBOARD => keyboard::on_interrupt(),
            IRQ_COM2 => aibridge::on_interrupt(),
            IRQ_COM1 => serial::on_interrupt(),
            _ => {}
        }
        if irq >= 8 {
            outb(PIC2_CMD, 0x20);
        }
        outb(PIC1_CMD, 0x20);
    }
}

const EXCEPTIONS: [&str; 32] = [
    "Divide error", "Debug", "NMI", "Breakpoint", "Overflow", "Bound range exceeded",
    "Invalid opcode", "Device not available", "Double fault", "Coprocessor segment overrun",
    "Invalid TSS", "Segment not present", "Stack-segment fault", "General protection fault",
    "Page fault", "Reserved", "x87 floating-point", "Alignment check", "Machine check",
    "SIMD floating-point", "Virtualization", "Control protection", "Reserved", "Reserved",
    "Reserved", "Reserved", "Reserved", "Reserved", "Hypervisor injection", "VMM communication",
    "Security exception", "Reserved",
];

fn exception(f: &InterruptFrame) -> ! {
    console::set_color(Color::White, Color::Red);
    println!();
    println!("CPU EXCEPTION {}: {}", f.vector, EXCEPTIONS[f.vector as usize]);
    println!("  error={:#x} rip={:#x} rsp={:#x} rflags={:#x}", f.error, f.rip, f.rsp, f.rflags);
    if f.vector == 14 {
        let cr2: u64;
        unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)) };
        println!("  faulting address (cr2)={:#x}", cr2);
    }
    println!("  rax={:#x} rcx={:#x} rdx={:#x} rsi={:#x} rdi={:#x}", f.rax, f.rcx, f.rdx, f.rsi, f.rdi);
    let _ = (f.r8, f.r9, f.r10, f.r11, f.cs, f.ss);
    let name = EXCEPTIONS[f.vector as usize];
    crash::record("exception", format_args!("CPU exception {}: {} at rip={:#x}", f.vector, name, f.rip));
    crate::testmode::on_fatal_error();
    crate::halt_forever()
}
