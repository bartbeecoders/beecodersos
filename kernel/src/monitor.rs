//! The system monitor (`monitor`): a window that shows, every second, what
//! the machine is doing: the work on each processor, memory, network
//! traffic and the kernel's threads.
//!
//! `Sample` is a reading of the counters and `render` turns two readings
//! into the lines to show, without windows or clocks, so the self-tests can
//! cover it. `run` is the window's thread: it reads, draws and waits a
//! second (or for `q`).

use crate::input::{self, Key};
use crate::{allocator, console, net, session, smp, thread, time};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const TITLE_ATTR: u8 = 0x70;
const TEXT_ATTR: u8 = 0x07;
const HEAD_ATTR: u8 = 0x0E;
const BAR_ATTR: u8 = 0x0A;
const HELP_ATTR: u8 = 0x08;

/// One reading of the counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    pub micros: u64,
    pub tsc: u64,
    /// Cycles each processor after the first spent on jobs (smp.rs).
    pub busy: Vec<u64>,
    pub heap_used: usize,
    pub heap_size: usize,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// (name, state) of every thread.
    pub threads: Vec<(String, String)>,
}

impl Sample {
    pub fn now() -> Sample {
        let (rx_bytes, tx_bytes) = net::traffic();
        Sample {
            micros: time::micros(),
            tsc: time::rdtsc(),
            busy: smp::busy_cycles(),
            heap_used: allocator::used(),
            heap_size: allocator::heap_size(),
            rx_bytes,
            tx_bytes,
            threads: thread::list().into_iter().map(|t| (String::from(t.name), String::from(t.state))).collect(),
        }
    }
}

/// `part` of `whole` as a bar of `width` characters, e.g. "[####......]".
pub fn bar(part: u64, whole: u64, width: usize) -> String {
    let filled = if whole == 0 { 0 } else { ((part.min(whole) as u128 * width as u128 + whole as u128 / 2) / whole as u128) as usize };
    format!("[{}{}]", "#".repeat(filled), ".".repeat(width - filled))
}

/// A number of bytes, short: "512 B", "3.4 KiB", "12.0 MiB".
pub fn bytes(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1048575 => format!("{}.{} KiB", n / 1024, n % 1024 * 10 / 1024),
        _ => format!("{}.{} MiB", n >> 20, (n & 0xFFFFF) * 10 >> 20),
    }
}

/// The lines to show for the change from `before` to `now`, at most
/// `cols` characters wide, with a color attribute for each.
pub fn render(before: &Sample, now: &Sample, cols: usize) -> Vec<(String, u8)> {
    let cols = cols.max(30);
    // Room for the longest line around a bar: "  heap   [...] 999.9 KiB of 999.9 MiB".
    let width = cols.saturating_sub(34).clamp(8, 40);
    let mut out = Vec::new();
    let line = |text: String, attr: u8| (text.chars().take(cols).collect::<String>(), attr);
    let up = time::format_duration(now.micros / 1000);
    out.push(line(format!(" System monitor   up {up}"), TITLE_ATTR));
    out.push(line(String::new(), TEXT_ATTR));
    let cycles = now.tsc.saturating_sub(before.tsc);
    out.push(line(format!(" Processors: {} in use", now.busy.len() + 1), HEAD_ATTR));
    out.push(line(String::from("  cpu 0  runs the kernel: threads, drivers, the screen"), TEXT_ATTR));
    for (i, busy) in now.busy.iter().enumerate() {
        let was = before.busy.get(i).copied().unwrap_or(0);
        let used = busy.saturating_sub(was).min(cycles);
        let percent = if cycles == 0 { 0 } else { used * 100 / cycles };
        out.push(line(format!("  cpu {}  {} {percent:>3}% on jobs", i + 1, bar(used, cycles, width)), BAR_ATTR));
    }
    out.push(line(String::new(), TEXT_ATTR));
    let (used, size) = (now.heap_used as u64, now.heap_size as u64);
    out.push(line(String::from(" Memory"), HEAD_ATTR));
    out.push(line(format!("  heap   {} {} of {}", bar(used, size, width), bytes(used), bytes(size)), BAR_ATTR));
    out.push(line(String::new(), TEXT_ATTR));
    let secs_x10 = (now.micros.saturating_sub(before.micros) / 100_000).max(1);
    let rate = |a: u64, b: u64| bytes(b.saturating_sub(a) * 10 / secs_x10);
    out.push(line(String::from(" Network"), HEAD_ATTR));
    out.push(line(
        format!("  in  {}/s  ({} so far)   out {}/s  ({} so far)", rate(before.rx_bytes, now.rx_bytes), bytes(now.rx_bytes), rate(before.tx_bytes, now.tx_bytes), bytes(now.tx_bytes)),
        TEXT_ATTR,
    ));
    out.push(line(String::new(), TEXT_ATTR));
    out.push(line(format!(" Threads: {}", now.threads.len()), HEAD_ATTR));
    for (name, state) in &now.threads {
        out.push(line(format!("  {name:<12} {state}"), TEXT_ATTR));
    }
    out
}

/// The thread of a monitor window: draws every second until `q`, or until
/// the window is closed.
pub fn run(window: usize) {
    let mut before = Sample::now();
    // A first picture after a short while, so the rates mean something.
    let mut wait_ms = 200;
    loop {
        let deadline = time::uptime_ms() + wait_ms;
        thread::pause(|| input::pending() || time::uptime_ms() >= deadline);
        if input::pending() {
            match input::read_key() {
                Key::Char('q') | Key::Ctrl('q') => return,
                Key::Ctrl('d') if session::current().is_none_or(session::is_closed) => return,
                _ => {}
            }
        }
        let Some((cols, rows)) = console::pane_size(window) else { return };
        let now = Sample::now();
        let mut lines = render(&before, &now, cols);
        lines.truncate(rows.saturating_sub(1));
        while lines.len() < rows.saturating_sub(1) {
            lines.push((String::new(), TEXT_ATTR));
        }
        lines.push((String::from(" q leave   (updated every second)"), HELP_ATTR));
        let cells: Vec<Vec<u16>> = lines
            .iter()
            .map(|(text, attr)| {
                let mut row: Vec<u16> = text.chars().take(cols).map(|c| (*attr as u16) << 8 | crate::vga::cp437(c) as u16).collect();
                row.resize(cols, (*attr as u16) << 8 | b' ' as u16);
                row
            })
            .collect();
        console::pane_show(window, &cells, (rows.saturating_sub(1), cols.saturating_sub(1)));
        before = now;
        wait_ms = 1000;
    }
}
