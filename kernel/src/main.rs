//! BeeCodersOs kernel
//!
//! Boot flow: `boot.s` (Multiboot entry, 32-bit -> 64-bit long mode) calls
//! `kernel_main`, which sets up the console, heap, interrupts and keyboard,
//! mounts storage and starts the terminal (BeeShell).

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod aibridge;
mod allocator;
mod ata;
mod bootargs;
#[macro_use]
mod console;
mod crash;
mod editor;
mod eventlog;
mod input;
mod json;
mod interrupts;
mod keyboard;
mod layouts;
mod port;
mod power;
mod ring;
mod selftest;
mod serial;
mod shell;
mod storage;
mod testmode;
mod time;
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
    bootargs::init(multiboot_info); // before the heap: it may reuse that memory
    if testmode::enabled() {
        println!("[test] test mode, kernel command line: {}", bootargs::cmdline());
    }

    match allocator::init(multiboot_info) {
        Ok((start, size)) => println!("[mem]  {} MiB kernel heap at {:#x}", size >> 20, start),
        Err(e) => panic!("heap: {}", e),
    }
    interrupts::init();
    time::init();
    keyboard::init();
    serial::enable_rx_interrupt();
    let bridge = aibridge::init();
    interrupts::enable();
    println!("[time] {}", time::timestamp());
    match acpi::init() {
        Some(s) => println!(
            "[acpi] power button on IRQ {}, power off: {}, reset: {}",
            s.sci_irq,
            if s.s5 { "S5" } else { "fallback" },
            if s.reset { "reset register" } else { "fallback" }
        ),
        None => println!("[acpi] no ACPI tables found (power button unsupported)"),
    }

    if bootargs::has("selftest") {
        let passed = selftest::run();
        if testmode::enabled() {
            testmode::exit(if passed { testmode::SUCCESS } else { testmode::FAILURE });
        }
    }

    let storage = storage::mount();
    let mut shell = shell::Shell::new(storage);
    shell.load_layout();
    let layout = keyboard::layout();
    println!("[kbd]  keyboard layout: {} ('layout' to change)", layout.name());
    if bridge {
        aibridge::hello(); // beehive answers if it runs; see the `ai` command
        println!("[ai]   AI bridge on COM2 ('ai' for status)");
    }
    shell.boot_report();
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
    crash::record("panic", format_args!("{}", info));
    testmode::on_fatal_error();
    halt_forever()
}
