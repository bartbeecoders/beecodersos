//! BeeCodersOs kernel
//!
//! Boot flow: `boot.s` (Multiboot entry, 32-bit -> 64-bit long mode) calls
//! `kernel_main`, which sets up the console, heap, interrupts and keyboard,
//! mounts storage and starts the terminal (BeeShell).

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod allocator;
mod ata;
#[macro_use]
mod console;
mod editor;
mod input;
mod interrupts;
mod keyboard;
mod layouts;
mod port;
mod power;
mod ring;
mod serial;
mod shell;
mod storage;
mod vga;

use core::panic::PanicInfo;

core::arch::global_asm!(include_str!("boot.s"));

/// OS version (set in the Makefile) and build number (BUILD_NUMBER file).
pub const OS_VERSION: &str = env!("BEECODERS_VERSION");
pub const OS_BUILD: &str = env!("BEECODERS_BUILD");

const MULTIBOOT_BOOTLOADER_MAGIC: u32 = 0x2BAD_B002;

#[no_mangle]
pub extern "C" fn kernel_main(multiboot_magic: u32, multiboot_info: u32) -> ! {
    console::init();
    console::set_color(vga::Color::Yellow, vga::Color::Black);
    println!("Hello World, from BeeCodersOs {} b{}", OS_VERSION, OS_BUILD);
    console::set_color(vga::Color::LightGray, vga::Color::Black);

    if multiboot_magic != MULTIBOOT_BOOTLOADER_MAGIC {
        panic!("not started by a Multiboot bootloader (magic {:#x})", multiboot_magic);
    }

    match allocator::init(multiboot_info) {
        Ok((start, size)) => println!("[mem]  {} MiB kernel heap at {:#x}", size >> 20, start),
        Err(e) => panic!("heap: {}", e),
    }
    interrupts::init();
    keyboard::init();
    serial::enable_rx_interrupt();
    interrupts::enable();
    match acpi::init() {
        Some(s) => println!(
            "[acpi] power button on IRQ {}, power off: {}, reset: {}",
            s.sci_irq,
            if s.s5 { "S5" } else { "fallback" },
            if s.reset { "reset register" } else { "fallback" }
        ),
        None => println!("[acpi] no ACPI tables found (power button unsupported)"),
    }

    let storage = storage::mount();
    let mut shell = shell::Shell::new(storage);
    shell.load_layout();
    let layout = keyboard::layout();
    println!("[kbd]  keyboard layout: {} ('layout' to change)", layout.name());
    println!();
    println!("Type 'help' for a list of commands.");
    shell.run()
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::set_color(vga::Color::White, vga::Color::Red);
    println!("KERNEL PANIC: {}", info);
    halt_forever()
}
