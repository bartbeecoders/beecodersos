//! Reboot and power off: ACPI first, then progressively blunter fallbacks.

use crate::acpi;
use crate::port::{inb, outb, outw};
use core::arch::asm;

pub fn reboot() -> ! {
    println!("Rebooting...");
    unsafe { asm!("cli", options(nomem, nostack)) };
    acpi::reset();
    unsafe {
        // PCI reset control register (Intel PIIX/ICH chipsets): full reset.
        outb(0xCF9, 0x02);
        outb(0xCF9, 0x06);
        spin();
        // Pulse the CPU reset line through the keyboard controller.
        for _ in 0..100_000 {
            if inb(0x64) & 0x02 == 0 {
                break;
            }
        }
        outb(0x64, 0xFE);
        spin();
        // Last resort: triple fault with an empty IDT.
        let null_idt = [0u16; 5];
        asm!("lidt [{}]; int3", in(reg) &null_idt, options(nostack));
    }
    crate::halt_forever()
}

pub fn shutdown() -> ! {
    println!("Shutting down...");
    unsafe { asm!("cli", options(nomem, nostack)) };
    acpi::power_off();
    if running_in_vm() {
        // Without usable ACPI tables (e.g. UEFI boot): well-known power-off ports
        // of QEMU, Bochs and VirtualBox. Only tried under a hypervisor: on real
        // hardware these ports may mean anything.
        unsafe {
            outw(0x604, 0x2000);
            outw(0xB004, 0x2000);
            outw(0x4004, 0x3400);
        }
    }
    println!("System halted. It is now safe to turn off the machine.");
    crate::halt_forever()
}

fn spin() {
    for _ in 0..10_000_000 {
        core::hint::spin_loop();
    }
}

/// CPUID leaf 1, ECX bit 31: "running under a hypervisor".
fn running_in_vm() -> bool {
    let ecx: u32;
    unsafe {
        asm!(
            "mov {tmp:r}, rbx", "cpuid", "mov rbx, {tmp:r}",
            tmp = out(reg) _, inout("eax") 1u32 => _, out("ecx") ecx, out("edx") _,
            options(nomem, nostack, preserves_flags)
        )
    };
    ecx & (1 << 31) != 0
}
