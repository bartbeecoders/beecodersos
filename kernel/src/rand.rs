//! Random numbers for network identifiers (TCP sequence numbers, DNS and DHCP
//! IDs, ports): the CPU's RDRAND when it has one, otherwise a generator
//! seeded from the timestamp counter and the clock. Not for cryptography.

use crate::ring::Global;
use crate::time;
use core::arch::asm;

static FALLBACK: Global<u64> = Global::new(0);

fn has_rdrand() -> bool {
    let ecx: u32;
    unsafe {
        asm!(
            "mov {tmp:r}, rbx", "cpuid", "mov rbx, {tmp:r}",
            tmp = out(reg) _, inout("eax") 1u32 => _, out("ecx") ecx, out("edx") _,
            options(nomem, nostack, preserves_flags)
        )
    };
    ecx & (1 << 30) != 0
}

fn rdrand() -> Option<u64> {
    for _ in 0..10 {
        let (value, ok): (u64, u8);
        unsafe { asm!("rdrand {v}", "setc {ok}", v = out(reg) value, ok = out(reg_byte) ok, options(nomem, nostack)) };
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

/// One step of xorshift64*.
pub fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

pub fn next_u64() -> u64 {
    if has_rdrand() {
        if let Some(v) = rdrand() {
            return v;
        }
    }
    let state = FALLBACK.get();
    // Mix in fresh timing on every call; never let the state be zero.
    *state ^= rdtsc() ^ time::now_unix().unwrap_or(0).rotate_left(32);
    if *state == 0 {
        *state = 0x9E37_79B9_7F4A_7C15;
    }
    xorshift(state)
}

pub fn next_u32() -> u32 {
    (next_u64() >> 32) as u32
}
