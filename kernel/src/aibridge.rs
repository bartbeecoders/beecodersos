//! AI bridge: a line-based JSON protocol on the second serial port (COM2,
//! 0x2F8, IRQ3) to the `beehive` daemon on the host, which talks to an LLM.
//! The OS never holds API keys; without beehive everything else still works.
//!
//! Guest -> host                                  Host -> guest
//!   {"type":"hello","build":..}                    {"type":"welcome","backend":..,"model":..}
//!   {"type":"ask","id":N,"text":..,"cwd":..}       {"type":"ack","id":N}
//!   {"type":"explain","id":N,"cmd":..,"error":..}  {"type":"line","id":N,"text":..}  (one per screen line)
//!   {"type":"cancel","id":N}                       {"type":"done","id":N}
//!   {"type":"signal","kind":..,"text":..}          {"type":"error","id":N,"text":..}
//!
//! QEMU: `-serial ... -chardev socket,id=ai,host=127.0.0.1,port=4556,server=on,wait=off -serial chardev:ai`

use crate::input::{self, Key};
use crate::json::{self, Obj, Object};
use crate::port::{inb, outb};
use crate::ring::{Global, Ring};
use crate::{interrupts, time};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

const COM2: u16 = 0x2F8;
/// Max time for beehive to acknowledge a request (it answers at once).
const ACK_TIMEOUT_MS: u64 = 3_000;
/// Max silence while an answer is being produced.
const REPLY_TIMEOUT_MS: u64 = 120_000;
const MAX_LINE: usize = 8192;

static RX: Ring<4096> = Ring::new();
static PRESENT: AtomicBool = AtomicBool::new(false);
static STATE: Global<State> = Global::new(State { line: Vec::new(), peer: None, next_id: 1 });

struct State {
    line: Vec<u8>,
    /// "backend model" from the last welcome message.
    peer: Option<String>,
    next_id: i64,
}

/// Detect and set up COM2. Returns true when the port exists.
pub fn init() -> bool {
    unsafe {
        // A UART has a scratch register; missing hardware reads back 0xFF.
        outb(COM2 + 7, 0x5A);
        if inb(COM2 + 7) != 0x5A {
            return false;
        }
        outb(COM2 + 1, 0x00); // interrupts off
        outb(COM2 + 3, 0x80); // DLAB
        outb(COM2, 0x01); // 115200 baud
        outb(COM2 + 1, 0x00);
        outb(COM2 + 3, 0x03); // 8N1
        outb(COM2 + 2, 0xC7); // FIFOs on, cleared
        outb(COM2 + 4, 0x0B); // DTR, RTS, OUT2 (IRQ gate)
        while inb(COM2 + 5) & 0x01 != 0 {
            inb(COM2);
        }
        outb(COM2 + 1, 0x01); // IRQ on received data
    }
    PRESENT.store(true, Ordering::SeqCst);
    true
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Called from the IRQ3 handler.
pub fn on_interrupt() {
    unsafe {
        while inb(COM2 + 5) & 0x01 != 0 {
            RX.push(inb(COM2));
        }
    }
}

pub fn pending() -> bool {
    !RX.is_empty()
}

/// Send one JSON message (a line). Silently dropped without a COM2 port.
pub fn send(msg: &str) {
    if !present() {
        return;
    }
    for b in msg.bytes().chain(core::iter::once(b'\n')) {
        unsafe {
            // Bounded wait: never hang the OS on a stuck port.
            if (0..1_000_000).find(|_| inb(COM2 + 5) & 0x20 != 0).is_none() {
                return;
            }
            outb(COM2, b);
        }
    }
}

/// Next complete message from the host. Welcome messages are handled here.
fn poll_message() -> Option<Object> {
    let st = STATE.get();
    while let Some(b) = RX.pop() {
        if b != b'\n' {
            if st.line.len() < MAX_LINE {
                st.line.push(b);
            }
            continue;
        }
        let line = core::mem::take(&mut st.line);
        let Some(msg) = core::str::from_utf8(&line).ok().and_then(json::parse) else { continue };
        if msg.str("type") == Some("welcome") {
            let backend = msg.str("backend").unwrap_or("?");
            let model = msg.str("model").unwrap_or("");
            st.peer = Some(alloc::format!("{backend} {model}").trim_end().into());
            continue;
        }
        return Some(msg);
    }
    None
}

/// Handle anything the host sent while we were not listening.
pub fn drain() {
    while poll_message().is_some() {}
}

/// Who answered the last hello ("backend model"), if anyone.
pub fn peer() -> Option<String> {
    drain();
    STATE.get().peer.clone()
}

pub fn hello() {
    send(&Obj::new().str("type", "hello").str("version", crate::OS_VERSION).str("build", crate::OS_BUILD).finish());
}

/// Send hello and wait up to `ms` for a welcome. Returns the peer if online.
pub fn ping(ms: u64) -> Option<String> {
    if !present() {
        return None;
    }
    STATE.get().peer = None;
    hello();
    let deadline = time::uptime_ms() + ms;
    while time::uptime_ms() < deadline {
        if let Some(p) = peer() {
            return Some(p);
        }
        interrupts::wait_until(pending);
    }
    None
}

pub enum Outcome {
    Done,
    Cancelled,
    /// No acknowledgement: beehive is not running or not connected.
    Offline,
    Error(String),
}

/// Send a request (`fields` must not contain "type" or "id") and stream the
/// answer: `on_line` is called for every line. Ctrl+C cancels.
pub fn request(kind: &str, fields: Obj, mut on_line: impl FnMut(&str)) -> Outcome {
    drain();
    let st = STATE.get();
    let id = st.next_id;
    st.next_id += 1;
    let msg = fields.str("type", kind).num("id", id).finish();
    send(&msg);
    let mut acked = false;
    let mut deadline = time::uptime_ms() + ACK_TIMEOUT_MS;
    loop {
        while let Some(m) = poll_message() {
            if m.num("id") != Some(id) {
                continue; // a late reply to an earlier, cancelled request
            }
            match m.str("type") {
                Some("ack") => acked = true,
                Some("line") => on_line(m.str("text").unwrap_or("")),
                Some("done") => return Outcome::Done,
                Some("error") => return Outcome::Error(m.str("text").unwrap_or("unknown error").into()),
                _ => {}
            }
            deadline = time::uptime_ms() + REPLY_TIMEOUT_MS;
        }
        while let Some(k) = input::poll_key() {
            if matches!(k, Key::Ctrl('c') | Key::Escape) {
                send(&Obj::new().str("type", "cancel").num("id", id).finish());
                return Outcome::Cancelled;
            }
        }
        if time::uptime_ms() > deadline {
            return if acked { Outcome::Error("no answer (timed out)".into()) } else { Outcome::Offline };
        }
        // The 100 Hz timer wakes us too, so the deadline is checked.
        interrupts::wait_until(|| pending() || input::pending());
    }
}
