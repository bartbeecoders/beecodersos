//! Cooperative kernel threads (phase T of Docs/network-stack-plan.html).
//!
//! Each thread has its own stack; the CPU moves between them only where a
//! thread says so: `pause` / `wait_until` (waiting for a key, the network, a
//! timer), `yield_now`, or when its function returns. Nothing is ever cut off
//! halfway, so the kernel's "one flow of control at a time" state keeps
//! working, under two rules:
//!
//!  * Don't keep a reference to shared state (`Global::get()`) across a call
//!    that can switch: fetch it again afterwards.
//!  * No switch happens while the file system is in use (`crash::FS_BUSY`):
//!    a waiting thread then simply sleeps until the next interrupt.
//!
//! Interrupt handlers are not involved. A thread that never waits blocks the
//! others; that is accepted for kernel code.
//!
//! How waiting works: a waiting thread tests its own condition each time it
//! runs. `generation` counts "something may have changed" moments (an
//! interrupt arrived, or a thread ran). A waiting thread that has tested its
//! condition in the current generation is skipped; when every thread has,
//! the CPU sleeps (`hlt`) until the next interrupt. The whole round runs with
//! interrupts off and ends in an atomic `sti; hlt`, so no wakeup is missed.

use crate::crash;
use crate::ring::Global;
use crate::time;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::sync::atomic::Ordering;

pub type ThreadId = u32;

/// Stack of a spawned thread: 256 KiB (the boot stack of the main thread is 64 KiB).
const STACK_WORDS: usize = 32 * 1024;
/// Written at the lowest word of every stack; checked at every switch.
const GUARD: u64 = 0xBEEC_0DE5_57AC_6A2D;
pub const MAX_THREADS: usize = 16;

global_asm!(
    r#"
// thread_switch(save_rsp: *mut u64, new_rsp: u64)
// Saves the registers a function must preserve on the current stack, stores
// the stack pointer, continues on the other stack.
.global thread_switch
thread_switch:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
"#
);

extern "C" {
    fn thread_switch(save_rsp: *mut u64, new_rsp: u64);
}

struct Thread {
    id: ThreadId,
    name: String,
    /// Saved stack pointer while the thread is not running.
    rsp: u64,
    /// None: the main thread, which runs on the boot stack.
    stack: Option<Vec<u64>>,
    entry: Option<Box<dyn FnOnce()>>,
    /// Inside `pause`, its condition was false when last tested...
    waiting: bool,
    /// ...in this generation.
    checked: u64,
    dead: bool,
    /// Console session (see session.rs); None: the real screen and keyboard.
    session: Option<u32>,
    /// How often the thread was given the CPU.
    runs: u64,
}

struct Scheduler {
    threads: Vec<Box<Thread>>,
    current: usize,
    generation: u64,
    next_id: ThreadId,
}

static SCHEDULER: Global<Option<Scheduler>> = Global::new(None);

/// Start threading: what runs now becomes the thread "main". Needs the heap.
pub fn init() {
    let main = Thread {
        id: 0,
        name: String::from("main"),
        rsp: 0,
        stack: None,
        entry: None,
        waiting: false,
        checked: 0,
        dead: false,
        session: None,
        runs: 1,
    };
    *SCHEDULER.get() = Some(Scheduler { threads: vec![Box::new(main)], current: 0, generation: 1, next_id: 1 });
    arm_watchdog();
}

fn scheduler() -> Option<&'static mut Scheduler> {
    SCHEDULER.get().as_mut()
}

fn interrupts_off() {
    unsafe { asm!("cli", options(nomem, nostack)) };
}

fn interrupts_on() {
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// Sleep until the next interrupt. Interrupts must be off: `sti; hlt` is
/// atomic, so an interrupt that arrives just before is not missed.
fn sleep_until_interrupt() {
    unsafe { asm!("sti; hlt", options(nomem, nostack)) };
}

// --- the watchdog ---------------------------------------------------------------------------
//
// Threads switch only when one waits (`pause`, `yield_now`). A thread that
// loops without ever waiting starves all the others, the network included:
// the machine looks dead and only its power button helps. So the timer
// interrupt checks when a thread last gave up the processor (a switch, or
// a sleep with nothing to do); after WATCHDOG_MS without, it panics on
// purpose, and the crash handling restarts the machine. Long work that does
// not wait on purpose (writing a whole disk) calls `still_working`.

/// When a thread last gave up the processor (ms of uptime; 0: not started).
static LAST_YIELD_MS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The limit; boot flag `watchdog=SECONDS` changes it (tests use a short one).
static WATCHDOG_MS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(30_000);

fn gave_way() {
    LAST_YIELD_MS.store(crate::time::uptime_ms().max(1), Ordering::Relaxed);
}

/// For long work that never waits on purpose: it is busy, not stuck.
pub fn still_working() {
    gave_way();
}

/// Set the watchdog's limit from the boot flags (2 to 600 seconds).
fn arm_watchdog() {
    if let Some(secs) = crate::bootargs::value("watchdog").and_then(|v| v.parse::<u64>().ok()).filter(|s| (2..=600).contains(s)) {
        WATCHDOG_MS.store(secs * 1000, Ordering::Relaxed);
    }
    gave_way();
}

/// From the timer interrupt: panic if no thread has given up the processor
/// for too long (the panic restarts the machine).
pub fn watchdog_tick() {
    let last = LAST_YIELD_MS.load(Ordering::Relaxed);
    if last == 0 {
        return; // threads have not started
    }
    let now = crate::time::uptime_ms();
    let limit = WATCHDOG_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) > limit {
        gave_way(); // once
        let name = scheduler().and_then(|s| s.threads.get(s.current)).map_or("?", |t| t.name.as_str());
        panic!("watchdog: thread '{name}' kept the processor for {} s without letting the others run", limit / 1000);
    }
}

fn fs_busy() -> bool {
    crash::FS_BUSY.load(Ordering::SeqCst) > 0
}

impl Scheduler {
    /// The next thread worth running after the current one (round robin):
    /// one that is not waiting, or has not tested its condition lately.
    fn next(&self) -> Option<usize> {
        let n = self.threads.len();
        (1..n).map(|step| (self.current + step) % n).find(|&i| {
            let t = &self.threads[i];
            !t.dead && (!t.waiting || t.checked != self.generation)
        })
    }
}

/// Continue with thread `next`. Returns when this thread runs again.
/// Interrupts must be off; they still are on return.
fn switch_to(next: usize) {
    let Some(s) = scheduler() else { return };
    let me = &mut s.threads[s.current];
    if let Some(stack) = &me.stack {
        let here = &stack as *const _ as u64; // a local: close to the stack pointer
        if stack[0] != GUARD || here < stack.as_ptr() as u64 + 4096 {
            panic!("stack overflow in thread '{}'", me.name);
        }
    }
    let save: *mut u64 = &mut me.rsp;
    let target = &mut s.threads[next];
    target.runs += 1;
    let new_rsp = target.rsp;
    s.current = next;
    gave_way();
    unsafe { thread_switch(save, new_rsp) };
    // Running again (possibly much later). Clear away threads that ended:
    // never the current one, whose stack we are standing on.
    if let Some(s) = scheduler() {
        let me = s.threads[s.current].id;
        s.threads.retain(|t| !t.dead || t.id == me);
        s.current = s.threads.iter().position(|t| t.id == me).unwrap_or(0);
    }
}

/// Unless `ready()` holds, let other threads run, or sleep until the next
/// interrupt when none has anything to do. Returns whether `ready()` held.
/// Like `interrupts::wait_until`, it returns after one such pause: callers
/// loop, doing their own periodic work (timeouts, polling) in between.
pub fn pause(mut ready: impl FnMut() -> bool) -> bool {
    interrupts_off();
    let is_ready = ready();
    match scheduler() {
        Some(s) if is_ready => {
            // This thread goes on and may change what others wait for.
            s.generation += 1;
            s.threads[s.current].waiting = false;
        }
        Some(s) => {
            let generation = s.generation;
            let me = &mut s.threads[s.current];
            me.waiting = true;
            me.checked = generation;
            match s.next() {
                Some(next) if !fs_busy() => switch_to(next),
                _ => {
                    sleep_until_interrupt();
                    interrupts_off();
                    gave_way(); // nothing else to do: idle, not stuck
                    if let Some(s) = scheduler() {
                        s.generation += 1; // an interrupt arrived: everyone tests again
                    }
                }
            }
            if let Some(s) = scheduler() {
                s.threads[s.current].waiting = false;
            }
        }
        None if is_ready => {}
        None => crate::interrupts::wait_until(|| false), // before `init`: a plain wait
    }
    interrupts_on();
    is_ready
}

/// Wait until `ready()` holds, letting other threads run meanwhile.
pub fn wait_until(mut ready: impl FnMut() -> bool) {
    while !pause(&mut ready) {}
}

/// Let the other threads have a turn, without sleeping. For loops that keep
/// the CPU busy on purpose (timing a ping).
pub fn yield_now() {
    if fs_busy() {
        return;
    }
    interrupts_off();
    if let Some(s) = scheduler() {
        s.generation += 1;
        s.threads[s.current].waiting = false;
        if let Some(next) = s.next() {
            switch_to(next);
        }
    }
    interrupts_on();
}

pub fn sleep_ms(ms: u64) {
    let end = time::uptime_ms() + ms;
    wait_until(|| time::uptime_ms() >= end);
}

/// Where a new thread starts (reached by the `ret` in `thread_switch`).
extern "C" fn thread_entry() -> ! {
    interrupts_on();
    let entry = scheduler().and_then(|s| s.threads[s.current].entry.take());
    if let Some(entry) = entry {
        entry();
    }
    // Finished: mark it and leave for good. Another thread frees the stack.
    interrupts_off();
    loop {
        if let Some(s) = scheduler() {
            s.generation += 1;
            s.threads[s.current].dead = true;
            if let Some(next) = s.next() {
                switch_to(next);
            }
        }
        sleep_until_interrupt(); // not reached: the main thread never ends
        interrupts_off();
    }
}

/// Start a thread that runs `entry`. `session`: the console session its
/// output and input belong to (None: the real console). None when there are
/// too many threads.
pub fn spawn(name: &str, session: Option<u32>, entry: impl FnOnce() + 'static) -> Option<ThreadId> {
    let s = scheduler()?;
    if s.threads.iter().filter(|t| !t.dead).count() >= MAX_THREADS {
        return None;
    }
    let mut stack = vec![0u64; STACK_WORDS];
    stack[0] = GUARD;
    // The top must be 16-byte aligned (the x86-64 calling convention).
    let mut top = STACK_WORDS;
    if (stack.as_ptr() as usize + top * 8) % 16 != 0 {
        top -= 1;
    }
    // What `thread_switch` pops: six registers, then the address it returns to.
    // The word above stands in for `thread_entry`'s own return address.
    stack[top - 1] = 0;
    stack[top - 2] = thread_entry as *const () as u64;
    let rsp = &stack[top - 8] as *const u64 as u64;
    let id = s.next_id;
    s.next_id += 1;
    s.threads.push(Box::new(Thread {
        id,
        name: String::from(name),
        rsp,
        stack: Some(stack),
        entry: Some(Box::new(entry)),
        waiting: false,
        checked: 0,
        dead: false,
        session,
        runs: 0,
    }));
    Some(id)
}

pub fn is_alive(id: ThreadId) -> bool {
    scheduler().is_some_and(|s| s.threads.iter().any(|t| t.id == id && !t.dead))
}

/// Wait until thread `id` has ended.
pub fn join(id: ThreadId) {
    wait_until(|| !is_alive(id));
}

pub fn current() -> ThreadId {
    scheduler().map_or(0, |s| s.threads[s.current].id)
}

/// The name of the running thread ("main" before threading starts).
pub fn current_name() -> String {
    scheduler().map_or_else(|| String::from("main"), |s| s.threads[s.current].name.clone())
}

/// True in the main thread: the one the real screen and keyboard belong to.
pub fn on_console() -> bool {
    current() == 0
}

/// The console session of the running thread, if it has one.
pub fn current_session() -> Option<u32> {
    scheduler().and_then(|s| s.threads[s.current].session)
}

pub struct Info {
    pub id: ThreadId,
    pub name: String,
    pub state: &'static str,
    pub runs: u64,
    /// Deepest stack use so far and the stack's size, in bytes (None: main).
    pub stack: Option<(usize, usize)>,
}

/// For the `threads` command.
pub fn list() -> Vec<Info> {
    let Some(s) = scheduler() else { return Vec::new() };
    let current = s.current;
    s.threads
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.dead)
        .map(|(i, t)| Info {
            id: t.id,
            name: t.name.clone(),
            state: if i == current { "running" } else if t.waiting { "waiting" } else { "ready" },
            runs: t.runs,
            // Stacks start out as zeros: the first word that is not shows how deep it got.
            stack: t.stack.as_ref().map(|st| {
                let untouched = st[1..].iter().take_while(|w| **w == 0).count() + 1;
                ((st.len() - untouched) * 8, st.len() * 8)
            }),
        })
        .collect()
}
