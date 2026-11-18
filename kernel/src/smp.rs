//! More than one processor: starting the others, and running work on all.
//!
//! The kernel proper (threads, drivers, the file system) runs on the
//! processor the machine started on, as before. The other processors
//! (`start`, the `cpu start` command) only do one thing: `run` hands out
//! the pieces of a job, numbered 0..count, to every processor, and comes
//! back when all are done. A piece must be work on memory only: no
//! printing, no files, no devices, nothing that waits.
//!
//! Starting a processor: it wakes up in 16-bit mode at an address below
//! 1 MiB, so a small piece of code (the trampoline, below) is copied to
//! 0x8000; it switches to 64-bit mode with the kernel's page tables and
//! calls `ap_entry` on a stack of its own. The wake-up call goes through
//! the local APIC (INIT, then STARTUP).
//!
//! Waiting processors sleep (MONITOR/MWAIT on the job counter) where the
//! processor can, and spin otherwise (virtual machines). They take no
//! interrupts.

use crate::cpu;
use crate::ring::Global;
use crate::time;
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};

/// Where the trampoline is copied to (must be below 1 MiB, on a page boundary).
const TRAMPOLINE: usize = crate::allocator::LOW_PAGE as usize;
const STACK_SIZE: usize = 64 * 1024;
/// Never more than this many processors, whatever the machine has.
pub const MAX_PROCESSORS: usize = 64;

global_asm!(
    r#"
.section .rodata
.global ap_trampoline_start
.global ap_trampoline_end
.set TRAMP, 0x8000
/* What `start` fills in, at fixed places in the same page (see DATA_AT). */
.set TRAMP_CR3,     0x8F00
.set TRAMP_STACK,   0x8F08
.set TRAMP_ENTRY,   0x8F10
.set TRAMP_GDT_PTR, 0x8F40
.code16
ap_trampoline_start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    /* lgdt with a 32-bit base: 66 0F 01 16 disp16 */
    .byte 0x66, 0x0F, 0x01, 0x16
    .word TRAMP_GDT_PTR
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    /* far jump to 32-bit code (selector 0x18): 66 EA offset32 selector16 */
    .byte 0x66, 0xEA
    .long TRAMP + (ap_tramp_32 - ap_trampoline_start)
    .word 0x18
.code32
ap_tramp_32:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov eax, dword ptr [TRAMP_CR3]
    mov cr3, eax
    mov eax, cr4
    or eax, 1 << 5
    mov cr4, eax
    mov ecx, 0xC0000080
    rdmsr
    or eax, 1 << 8
    wrmsr
    mov eax, cr0
    or eax, 0x80000001
    mov cr0, eax
    /* far jump to 64-bit code (selector 0x08): EA offset32 selector16 */
    .byte 0xEA
    .long TRAMP + (ap_tramp_64 - ap_trampoline_start)
    .word 0x08
.code64
ap_tramp_64:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    xor ax, ax
    mov fs, ax
    mov gs, ax
    mov rsp, qword ptr [TRAMP_STACK]
    mov rax, qword ptr [TRAMP_ENTRY]
    call rax
1:  cli
    hlt
    jmp 1b
ap_trampoline_end:
.text
"#
);

extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
}

/// The trampoline's data, in its page after the code: the page tables, the
/// stack and the function for the processor being started, and the
/// descriptor table for its way to 64-bit mode (the same numbers as in
/// the trampoline's `.set` lines).
const DATA_AT: usize = TRAMPOLINE + 0xF00;
const CR3_AT: usize = DATA_AT;
const STACK_AT: usize = DATA_AT + 0x08;
const ENTRY_AT: usize = DATA_AT + 0x10;
const GDT_AT: usize = DATA_AT + 0x20;
const GDT_PTR_AT: usize = DATA_AT + 0x40;
/// Null; 0x08: 64-bit code, as in the kernel's table; 0x10: data; 0x18:
/// 32-bit code, for the step in between.
const GDT: [u64; 4] = [0, 0x00AF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF, 0x00CF_9A00_0000_FFFF];

/// Processors that reached `ap_entry` (the first one not counted).
static ONLINE: AtomicUsize = AtomicUsize::new(0);
/// What every started processor copies from the first one.
static SETUP: Global<Setup> = Global::new(Setup { gdt: [0; 10], idt: [0; 10], cr0: 0, cr4: 0, pat: 0, mwait: false });

struct Setup {
    gdt: [u8; 10],
    idt: [u8; 10],
    cr0: u64,
    cr4: u64,
    pat: u64,
    mwait: bool,
}

// --- the local APIC (xAPIC: registers in memory) ---------------------------------------

const MSR_APIC_BASE: u32 = 0x1B;
const MSR_PAT: u32 = 0x277;
const APIC_ID: usize = 0x20;
const APIC_SVR: usize = 0xF0;
const APIC_ICR_LOW: usize = 0x300;
const APIC_ICR_HIGH: usize = 0x310;

unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    (hi as u64) << 32 | lo as u64
}

unsafe fn wrmsr(msr: u32, value: u64) {
    asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack, preserves_flags));
}

struct Apic(usize);

impl Apic {
    /// The local APIC of this processor, if it can be driven through memory
    /// below 4 GiB (the usual case).
    fn find() -> Result<Apic, &'static str> {
        if cpu::cpuid(1, 0).3 & (1 << 9) == 0 {
            return Err("this processor has no local APIC");
        }
        let base = unsafe { rdmsr(MSR_APIC_BASE) };
        if base & (1 << 11) == 0 {
            return Err("the local APIC is switched off");
        }
        if base & (1 << 10) != 0 {
            return Err("the local APIC is in x2APIC mode, which this OS cannot drive yet");
        }
        let addr = base & 0x000F_FFFF_FFFF_F000;
        if addr == 0 || addr >= 0xFFFF_F000 {
            return Err("the local APIC is not below 4 GiB");
        }
        Ok(Apic(addr as usize))
    }

    fn read(&self, reg: usize) -> u32 {
        unsafe { ((self.0 + reg) as *const u32).read_volatile() }
    }

    fn write(&self, reg: usize, value: u32) {
        unsafe { ((self.0 + reg) as *mut u32).write_volatile(value) }
    }

    fn id(&self) -> u32 {
        self.read(APIC_ID) >> 24
    }

    /// Send an INIT (`0x4500`) or STARTUP (`0x4600 | page`) to one processor
    /// and wait until it is on its way (at most a few milliseconds).
    fn send(&self, to: u32, command: u32) {
        self.write(APIC_ICR_HIGH, to << 24);
        self.write(APIC_ICR_LOW, command);
        let give_up = time::micros() + 20_000;
        while self.read(APIC_ICR_LOW) & (1 << 12) != 0 && time::micros() < give_up {
            core::hint::spin_loop();
        }
    }
}

fn wait_micros(micros: u64) {
    let end = time::micros() + micros;
    while time::micros() < end {
        core::hint::spin_loop();
    }
}

/// How many processors run this OS now (1 until `start`).
pub fn running() -> usize {
    1 + ONLINE.load(SeqCst)
}

/// Start the machine's other processors. Returns how many run afterwards
/// (those that did not come up within a second are left alone), or why
/// none could be started. Calling it again starts nothing twice.
pub fn start() -> Result<usize, &'static str> {
    if running() > 1 {
        return Ok(running());
    }
    let processors = cpu::processors();
    if processors.len() < 2 {
        return Ok(1);
    }
    let apic = Apic::find()?;
    if !crate::allocator::low_page_free() {
        return Err("the memory the start-up code needs (at 0x8000) is not free on this machine");
    }
    if processors.iter().any(|id| *id > 0xFE) {
        return Err("processor numbers above 254 need x2APIC mode, which this OS cannot drive yet");
    }
    let me = apic.id();
    // What the others copy: descriptor tables, control registers, the
    // memory types (PAT) the screen's write-combining depends on.
    let setup = SETUP.get();
    unsafe {
        asm!("sgdt [{}]", in(reg) setup.gdt.as_mut_ptr(), options(nostack, preserves_flags));
        asm!("sidt [{}]", in(reg) setup.idt.as_mut_ptr(), options(nostack, preserves_flags));
        asm!("mov {}, cr0", out(reg) setup.cr0, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) setup.cr4, options(nomem, nostack, preserves_flags));
        setup.pat = if cpu::cpuid(1, 0).3 & (1 << 16) != 0 { rdmsr(MSR_PAT) } else { 0 };
    }
    setup.mwait = cpu::cpuid(1, 0).2 & (1 << 3) != 0 && cpu::cpuid(1, 0).2 & (1 << 31) == 0;
    // The APIC must be switched on (in software too) to send anything.
    let svr = apic.read(APIC_SVR);
    if svr & 0x100 == 0 {
        apic.write(APIC_SVR, svr | 0x1FF);
    }
    let cr3: u64;
    unsafe {
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
        let (from, to) = (&ap_trampoline_start as *const u8, &ap_trampoline_end as *const u8);
        let len = to as usize - from as usize;
        if len > DATA_AT - TRAMPOLINE || cr3 >= 1 << 32 {
            return Err("the start-up code does not fit");
        }
        core::ptr::copy_nonoverlapping(from, TRAMPOLINE as *mut u8, len);
        (CR3_AT as *mut u64).write_unaligned(cr3);
        (ENTRY_AT as *mut u64).write_unaligned(ap_entry as *const () as u64);
        (GDT_AT as *mut [u64; 4]).write_unaligned(GDT);
        (GDT_PTR_AT as *mut u16).write_unaligned(4 * 8 - 1);
        ((GDT_PTR_AT + 2) as *mut u32).write_unaligned(GDT_AT as u32);
        for id in processors.iter().copied().filter(|id| *id != me).take(MAX_PROCESSORS - 1) {
            // A stack that stays for good.
            let stack = Vec::<u8>::with_capacity(STACK_SIZE).leak().as_mut_ptr() as usize;
            (STACK_AT as *mut u64).write_unaligned(((stack + STACK_SIZE) & !15) as u64);
            let before = ONLINE.load(SeqCst);
            apic.send(id, 0x4500);
            wait_micros(10_000);
            for _ in 0..2 {
                if ONLINE.load(SeqCst) != before {
                    break;
                }
                apic.send(id, 0x4600 | (TRAMPOLINE >> 12) as u32);
                let give_up = time::micros() + 500_000;
                while ONLINE.load(SeqCst) == before && time::micros() < give_up {
                    core::hint::spin_loop();
                }
            }
        }
    }
    Ok(running())
}

/// Where a started processor arrives, in 64-bit mode on its own stack.
extern "C" fn ap_entry() -> ! {
    let setup = SETUP.get();
    unsafe {
        asm!("lgdt [{}]", in(reg) setup.gdt.as_ptr(), options(readonly, nostack, preserves_flags));
        asm!("lidt [{}]", in(reg) setup.idt.as_ptr(), options(readonly, nostack, preserves_flags));
        asm!("mov cr4, {}", in(reg) setup.cr4, options(nomem, nostack, preserves_flags));
        asm!("mov cr0, {}", in(reg) setup.cr0, options(nomem, nostack, preserves_flags));
        if setup.pat != 0 {
            wrmsr(MSR_PAT, setup.pat);
        }
    }
    let mwait = setup.mwait;
    ONLINE.fetch_add(1, SeqCst);
    let mut seen = 0;
    loop {
        // Sleep until a job is announced.
        while JOBS.load(SeqCst) == seen {
            if mwait {
                unsafe {
                    asm!("monitor", in("rax") &JOBS as *const AtomicU64, in("ecx") 0u32, in("edx") 0u32, options(nostack, preserves_flags));
                    if JOBS.load(SeqCst) == seen {
                        asm!("mwait", in("eax") 0u32, in("ecx") 0u32, options(nostack, preserves_flags));
                    }
                }
            } else {
                core::hint::spin_loop();
            }
        }
        seen = JOBS.load(SeqCst);
        work();
    }
}

// --- jobs --------------------------------------------------------------------------------

/// Counts the jobs announced: what sleeping processors watch.
static JOBS: AtomicU64 = AtomicU64::new(0);
/// The job: its pieces are `NEXT..TOTAL`; `DONE` counts those finished.
static NEXT: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
/// Processors that are looking for a piece or working on one.
static BUSY: AtomicUsize = AtomicUsize::new(0);
/// The job's function, as the two words of a `&dyn Fn(usize)`.
static JOB: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

/// Take pieces of the current job until none are left.
fn work() {
    BUSY.fetch_add(1, SeqCst);
    loop {
        let piece = NEXT.load(SeqCst);
        if piece >= TOTAL.load(SeqCst) {
            break;
        }
        if NEXT.compare_exchange(piece, piece + 1, SeqCst, SeqCst).is_err() {
            continue;
        }
        // SAFETY: `run` stored a reference that lives until every piece is
        // done and no processor is busy, and that may be shared (`Sync`).
        let job: &(dyn Fn(usize) + Sync) = unsafe { core::mem::transmute([JOB[0].load(SeqCst), JOB[1].load(SeqCst)]) };
        job(piece);
        DONE.fetch_add(1, SeqCst);
    }
    BUSY.fetch_sub(1, SeqCst);
}

/// Run `job(0)`, `job(1)`, ... `job(count - 1)`, spread over all running
/// processors (this one too), and return when all are done. The order is
/// not fixed, and pieces run at the same time: they share only what is
/// safe to share. One job at a time: only call this from the kernel's
/// threads (which take turns), never from a job.
pub fn run(count: usize, job: &(dyn Fn(usize) + Sync)) {
    if count == 0 {
        return;
    }
    let words: [usize; 2] = unsafe { core::mem::transmute(job) };
    JOB[0].store(words[0], SeqCst);
    JOB[1].store(words[1], SeqCst);
    DONE.store(0, SeqCst);
    NEXT.store(0, SeqCst);
    TOTAL.store(count, SeqCst);
    JOBS.fetch_add(1, SeqCst); // wakes the others
    work();
    while DONE.load(SeqCst) < count {
        core::hint::spin_loop();
    }
    // No more pieces; wait for latecomers that are still looking.
    TOTAL.store(0, SeqCst);
    while BUSY.load(SeqCst) != 0 {
        core::hint::spin_loop();
    }
}
