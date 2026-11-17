//! What the processor is: its name, what it can do, and how many
//! processors (cores and hardware threads) the machine has, from the ACPI
//! table "APIC" (the MADT). The OS runs on one of them so far; this is the
//! groundwork for using the others.

use crate::acpi;
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::asm;

/// One CPUID question: (eax, ebx, ecx, edx).
pub fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let (a, b, c, d): (u32, u32, u32, u32);
    unsafe {
        asm!(
            "mov {tmp:r}, rbx", "cpuid", "xchg {tmp:r}, rbx",
            tmp = out(reg) b, inout("eax") leaf => a, inout("ecx") sub => c, out("edx") d,
            options(nomem, nostack, preserves_flags)
        )
    };
    (a, b, c, d)
}

/// The processor's name as it gives it, e.g. "Intel(R) Core(TM) i5-4590
/// CPU @ 3.30GHz"; its maker's short name if it has no such text.
pub fn name() -> String {
    let mut bytes = Vec::new();
    if cpuid(0x8000_0000, 0).0 >= 0x8000_0004 {
        for leaf in 0x8000_0002..=0x8000_0004u32 {
            let (a, b, c, d) = cpuid(leaf, 0);
            for word in [a, b, c, d] {
                bytes.extend_from_slice(&word.to_le_bytes());
            }
        }
    } else {
        let (_, b, c, d) = cpuid(0, 0);
        for word in [b, d, c] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    clean_name(&bytes)
}

/// The text of a name: up to the first zero byte, printable ASCII only,
/// runs of spaces made one, none at the ends.
pub fn clean_name(bytes: &[u8]) -> String {
    let text: String = bytes.iter().take_while(|b| **b != 0).filter(|b| (0x20..0x7F).contains(*b)).map(|b| *b as char).collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The answers to CPUID that say what the processor can do.
#[derive(Clone, Copy)]
pub struct Raw {
    /// Leaf 1: ecx, edx.
    pub one: (u32, u32),
    /// Leaf 7, subleaf 0: ebx (0 if the processor has no leaf 7).
    pub seven_ebx: u32,
}

pub fn raw() -> Raw {
    let (_, _, c, d) = cpuid(1, 0);
    let seven_ebx = if cpuid(0, 0).0 >= 7 { cpuid(7, 0).1 } else { 0 };
    Raw { one: (c, d), seven_ebx }
}

/// The names of what the processor can do, of the things this OS may use
/// one day: fast copying and arithmetic on many numbers at once (sse2 ...
/// avx2), encryption (aes), hashing (sha), random numbers (rdrand), and
/// "vm" when it is not a real processor but a virtual machine's.
pub fn features(raw: Raw) -> Vec<&'static str> {
    let (c, d) = raw.one;
    let table = [
        (d & (1 << 26) != 0, "sse2"),
        (c & (1 << 0) != 0, "sse3"),
        (c & (1 << 9) != 0, "ssse3"),
        (c & (1 << 19) != 0, "sse4.1"),
        (c & (1 << 20) != 0, "sse4.2"),
        (c & (1 << 28) != 0, "avx"),
        (raw.seven_ebx & (1 << 5) != 0, "avx2"),
        (c & (1 << 25) != 0, "aes"),
        (raw.seven_ebx & (1 << 29) != 0, "sha"),
        (c & (1 << 30) != 0, "rdrand"),
        (c & (1 << 21) != 0, "x2apic"),
        (c & (1 << 31) != 0, "vm"),
    ];
    table.iter().filter(|(has, _)| *has).map(|(_, name)| *name).collect()
}

/// Most processors counted (a table that claims more is cut off there).
pub const MAX_PROCESSORS: usize = 256;

/// The processors an ACPI "APIC" table (MADT) lists as usable: their APIC
/// numbers, in the table's order. `table` is the whole table, header
/// included. Entries that are cut off or have a length that cannot be end
/// the list; nothing is read outside `table`.
pub fn madt_processors(table: &[u8]) -> Vec<u32> {
    let mut found = Vec::new();
    // 36 bytes of header, the local APIC address, flags; then the entries: type, length, ...
    let mut at = 44;
    while at + 2 <= table.len() && found.len() < MAX_PROCESSORS {
        let (kind, len) = (table[at], table[at + 1] as usize);
        if len < 2 || at + len > table.len() {
            break;
        }
        let entry = &table[at..at + len];
        let word = |i: usize| u32::from_le_bytes([entry[i], entry[i + 1], entry[i + 2], entry[i + 3]]);
        match kind {
            // Processor local APIC: processor number, APIC number, flags (bit 0: enabled, bit 1: can be).
            0 if len >= 8 && word(4) & 3 != 0 => found.push(entry[3] as u32),
            // Processor local x2APIC: (2 reserved), x2APIC number, flags, processor number.
            9 if len >= 16 && word(8) & 3 != 0 => found.push(word(4)),
            _ => {}
        }
        at += len;
    }
    found
}

/// The APIC numbers of the machine's processors; one (number 0) when the
/// machine has no such table.
pub fn processors() -> Vec<u32> {
    let found = acpi::table(b"APIC").map(madt_processors).unwrap_or_default();
    if found.is_empty() { alloc::vec![0] } else { found }
}
