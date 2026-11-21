//! BeeCodersOs kernel
//!
//! Boot flow: `boot.s` (Multiboot entry, 32-bit -> 64-bit long mode) calls
//! `kernel_main`, which sets up the console, heap, interrupts and keyboard,
//! mounts storage and starts the terminal (BeeShell).

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod ahci;
mod aibridge;
mod allocator;
mod ata;
mod bochs;
mod bootargs;
mod bootfb;
mod bootmods;
#[macro_use]
mod console;
mod cpu;
mod smp;
mod desk;
mod filemgr;
mod textedit;
mod textops;
mod doplan;
mod monitor;
mod clock;
mod image;
mod view;
mod timer;
mod video;
mod webcam;
mod icons;
mod sound;
mod hda;
mod vnc;
mod users;
mod web;
// After `console`: uses println!.
mod bootslots;
mod crash;
mod disks;
mod display;
mod e1000;
mod editor;
mod eventlog;
mod hivectl;
mod fat;
mod fbcon;
mod font;
mod gfx;
mod httpd;
mod fileserver;
mod remote;
mod input;
mod install;
mod json;
mod interrupts;
mod keyboard;
mod launcher;
mod layouts;
mod net;
mod paging;
mod pci;
mod port;
mod power;
mod rand;
mod ring;
mod selftest;
mod serial;
mod session;
mod shell;
mod storage;
mod testmode;
mod thread;
mod time;
mod uart;
mod update;
mod usb;
mod vga;
mod wm;
mod xhci;

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
    if !serial::present() {
        println!("[com1] no serial port found: screen and keyboard only");
    }
    bootargs::init(multiboot_info); // before the heap: it may reuse that memory
    bootfb::remember(multiboot_info); // the same holds for the bootloader's framebuffer
    bootmods::remember(multiboot_info); // and the heap must stay away from loaded files
    if testmode::enabled() {
        println!("[test] test mode, kernel command line: {}", bootargs::cmdline());
    }

    match allocator::init(multiboot_info) {
        Ok((start, size)) => println!("[mem]  {} MiB kernel heap at {:#x}", size >> 20, start),
        Err(e) => panic!("heap: {}", e),
    }
    // Graphics, if a display driver knows this machine (it needs the heap).
    println!("[disp] {}", console::start_display());
    thread::init();
    // A/B slots: a trial kernel arms the rollback before anything risky.
    let slot_report = bootslots::on_boot();
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

    let storage = storage::mount();

    if bootargs::has("selftest") {
        let passed = selftest::run();
        if testmode::enabled() {
            testmode::exit(if passed { testmode::SUCCESS } else { testmode::FAILURE });
        }
    }

    match net::init() {
        Ok(line) => println!("[net]  {line}"),
        Err(e) => println!("[net]  {e}"),
    }
    net::start_task();
    if let Some(line) = xhci::start() {
        println!("[usb]  {line}");
    }

    let mut shell = shell::Shell::new(storage);
    shell.load_layout();
    shell.go_home();
    shell.load_display();
    shell.load_desktop();
    shell.load_wallpaper();
    shell.load_cpus();
    shell.load_timezone();
    clock::start();
    shell.load_network();
    shell.load_hive();
    shell.load_remote_work();
    let layout = keyboard::layout();
    println!("[kbd]  keyboard layout: {} ('layout' to change)", layout.name());
    if bridge {
        aibridge::hello(); // beehive answers if it runs; see the `ai` command
        println!("[ai]   AI bridge on COM2 ('ai' for status)");
    }
    if let Some(line) = timer::restore() {
        println!("[timer] {line}");
    }
    if let Some(port) = httpd::load() {
        println!("[httpd] status page on port {port} ('httpd' for details)");
    }
    if let Some(port) = fileserver::load() {
        println!("[fileserver] files on port {port}, with a key, not encrypted ('fileserver' for details)");
    }
    if let Some(port) = vnc::load() {
        println!("[vnc]  remote desktop on port {port}, not encrypted ('vnc' for details)");
    }
    if let Some(port) = remote::load() {
        println!("[remote] remote shell on port {port}, not encrypted ('remote' for details)");
    }
    shell.boot_report();
    match slot_report {
        Some(bootslots::Report::RolledBack(msg)) => shell.note("rollback", "crash", &msg),
        Some(bootslots::Report::Trial) => trial_health_check(&mut shell),
        None => {}
    }
    shell.show_banner();
    println!();
    println!("Type 'help' for a list of commands.");
    console::settle();
    shell.run()
}

/// A trial kernel becomes the good slot once the shell is up and the
/// self-tests pass; otherwise the good slot boots again.
fn trial_health_check(shell: &mut shell::Shell) {
    if bootargs::has("failboot") {
        panic!("failboot: simulated broken build (tests the rollback)");
    }
    let slot = bootslots::booted_slot().map(|s| s as char).unwrap_or('?');
    match bootslots::health_check(selftest::passes()) {
        Some(true) => {
            println!("[slots] slot {slot} passed its health check: it is now the good slot");
            shell.note("slot-ok", "health", &alloc::format!("slot {slot} b{} passed its health check", OS_BUILD));
        }
        Some(false) => {
            let msg = alloc::format!("trial of build b{} in slot {slot} failed its health check", OS_BUILD);
            println!("[slots] {msg}; rebooting into the good slot");
            shell.note("rollback", "crash", &msg);
            power::reboot();
        }
        None => {}
    }
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::set_color(vga::Color::White, vga::Color::Red);
    // Which thread it was matters for anything but the main one.
    let thread = thread::current_name();
    if thread == "main" {
        println!("KERNEL PANIC: {}", info);
        crash::record("panic", format_args!("{}", info));
    } else {
        println!("KERNEL PANIC in thread '{}': {}", thread, info);
        crash::record("panic", format_args!("[thread {}] {}", thread, info));
    }
    testmode::on_fatal_error();
    crash::finish()
}
