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
//!
//! A machine without that serial port reaches beehive over the network
//! instead (`ai connect`, Docs/beehive-network-plan.html): the same messages
//! inside a TCP connection the OS opens, kept by the "hive" thread at the end
//! of this file. Each side proves it knows the pairing key and every line is
//! sealed (beenet::hive). Not encrypted. When a network link is set up it is
//! the one that is used; otherwise COM2.

use crate::input::{self, Key};
use crate::json::{self, Obj, Object};
use crate::ring::{Global, Ring};
use crate::uart::{Hardware, Uart, COM2};
use crate::{thread, time};
use crate::{net, rand};
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beenet::hive;
use core::sync::atomic::{AtomicBool, Ordering};

/// Max time for beehive to acknowledge a request (it answers at once).
const ACK_TIMEOUT_MS: u64 = 3_000;
/// Max silence while an answer is being produced.
const REPLY_TIMEOUT_MS: u64 = 120_000;
const MAX_LINE: usize = 8192;

static PORT: Global<Uart> = Global::new(Uart::new(COM2));
/// Room for two of the longest lines the host sends (pieces of a kernel).
static RX: Ring<16384> = Ring::new();
static PRESENT: AtomicBool = AtomicBool::new(false);
static STATE: Global<State> = Global::new(State { line: Vec::new(), peer: None, next_id: 1, status: None });

struct State {
    line: Vec<u8>,
    /// "backend model" from the last welcome message.
    peer: Option<String>,
    next_id: i64,
    /// Sent again whenever beehive (re)connects, so it is never lost.
    status: Option<String>,
}

/// Detect and set up COM2. Returns true when the port exists.
pub fn init() -> bool {
    let port = PORT.get();
    if !port.init(&mut Hardware) {
        return false;
    }
    port.enable_rx_interrupt(&mut Hardware);
    PRESENT.store(true, Ordering::SeqCst);
    true
}

/// Is there a way to reach beehive: a COM2 port, or a network link set up?
pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed) || LINK.get().settings.is_some()
}

/// Called from the IRQ3 handler.
pub fn on_interrupt() {
    PORT.get().receive(&mut Hardware, |b| {
        RX.push(b);
    });
}

pub fn pending() -> bool {
    !RX.is_empty() || !LINK.get().incoming.is_empty() || !ANSWERS.get().is_empty()
}

/// Send one JSON message (a line). Silently dropped without a COM2 port,
/// and cut short when the port is stuck: the OS never hangs on it.
pub fn send(msg: &str) {
    let link = LINK.get();
    if link.settings.is_some() {
        // Signals wait for the next connection; anything else is only worth
        // sending while beehive is there.
        let keep = link.connected || msg.contains(r#""type":"signal""#);
        if keep && link.outgoing.len() < MAX_QUEUED {
            link.outgoing.push_back(msg.into());
        }
        return;
    }
    for b in msg.bytes().chain(core::iter::once(b'\n')) {
        if !PORT.get().send(&mut Hardware, b) {
            return;
        }
    }
}

/// Answers waiting for whoever asked, and requests from the host (remote
/// control) waiting for the control thread. Both are bounded.
static ANSWERS: Global<VecDeque<Object>> = Global::new(VecDeque::new());
static REQUESTS: Global<VecDeque<Object>> = Global::new(VecDeque::new());
const MAX_WAITING: usize = 64;
/// `ai control on`: the host may run commands here and look at the screen.
static CONTROL: AtomicBool = AtomicBool::new(false);

/// Sort one message from the host: a welcome is handled at once, a request
/// goes to the control thread (or is refused), everything else is an answer.
fn sort(msg: Object) {
    match msg.str("type") {
        Some("welcome") => {
            let st = STATE.get();
            let backend = msg.str("backend").unwrap_or("?");
            let model = msg.str("model").unwrap_or("");
            st.peer = Some(alloc::format!("{backend} {model}").trim_end().into());
            if let Some(s) = st.status.clone() {
                send(&s);
            }
        }
        Some("run" | "screenshot" | "keys") => {
            let rid = msg.str("rid").unwrap_or("");
            // The kill switch (`ai off`) stops the host from acting here too.
            if !CONTROL.load(Ordering::Relaxed) || !LINK.get().enabled {
                let kind = if msg.str("type") == Some("run") { "ran" } else { "shot" };
                let why = "remote control is switched off on this machine (ai control on, typed on its own console)";
                send(&Obj::new().str("type", kind).str("rid", rid).bool("done", true).str("error", why).finish());
            } else if REQUESTS.get().len() < MAX_WAITING {
                REQUESTS.get().push_back(msg);
            }
        }
        _ => {
            if ANSWERS.get().len() >= MAX_WAITING {
                ANSWERS.get().pop_front(); // nobody is listening: the oldest goes
            }
            ANSWERS.get().push_back(msg);
        }
    }
}

/// Read what the serial port holds, without waiting for its interrupt. The
/// interrupt handler takes a limited number of bytes per interrupt; when a
/// long line arrives in one burst (a piece of a kernel), the port keeps its
/// interrupt line raised and no further interrupt comes. So whoever waits
/// for the host also looks at the port itself. Interrupts are off meanwhile:
/// the handler and this must not fill the ring at the same time.
fn poll_port() {
    if !PRESENT.load(Ordering::Relaxed) {
        return;
    }
    let flags: u64;
    unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags) };
    for _ in 0..64 {
        let mut taken = 0;
        PORT.get().receive(&mut Hardware, |b| {
            RX.push(b);
            taken += 1;
        });
        if taken < crate::uart::RECEIVE_LIMIT {
            break;
        }
    }
    if flags & 0x200 != 0 {
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    }
}

/// Take in what has arrived from the host, whichever way it came.
pub fn pump() {
    poll_port();
    // The network link hands over whole lines, already checked.
    while let Some(line) = LINK.get().incoming.pop_front() {
        if let Some(msg) = json::parse(&line) {
            sort(msg);
        }
    }
    while let Some(b) = RX.pop() {
        let st = STATE.get();
        if b != b'\n' {
            if st.line.len() < MAX_LINE {
                st.line.push(b);
            }
            continue;
        }
        let line = core::mem::take(&mut st.line);
        if let Some(msg) = core::str::from_utf8(&line).ok().and_then(json::parse) {
            sort(msg);
        }
    }
}

/// Next answer from the host.
fn poll_message() -> Option<Object> {
    pump();
    ANSWERS.get().pop_front()
}

/// Remote control (`ai control on|off`): may the host run commands here?
pub fn set_control(on: bool) {
    CONTROL.store(on, Ordering::SeqCst);
    if !on {
        REQUESTS.get().clear();
    }
}

/// False after `ai off`: nothing from the host is acted on.
pub fn enabled() -> bool {
    LINK.get().enabled
}

pub fn control() -> bool {
    CONTROL.load(Ordering::Relaxed)
}

/// The next request from the host, for the control thread (hivectl.rs).
pub fn take_request() -> Option<Object> {
    pump();
    REQUESTS.get().pop_front()
}

/// Send a message, waiting (a few seconds at most) while the network link's
/// queue is full: for answers that come in many pieces.
pub fn send_all(msg: &str) {
    let give_up = time::uptime_ms() + 5_000;
    thread::wait_until(|| {
        let link = LINK.get();
        link.settings.is_none() || !link.connected || link.outgoing.len() < MAX_QUEUED || time::uptime_ms() >= give_up
    });
    send(msg);
}

/// Ask the host one thing and wait for the one answer of type `answer`
/// (matched by request id). No keys are read: this is for work in the
/// background. `fields` must not contain "type" or "id".
pub fn call(kind: &str, fields: Obj, answer: &str, timeout_ms: u64) -> Result<Object, &'static str> {
    if !present() {
        return Err("no link to the AI host");
    }
    if REQUEST_RUNNING.swap(true, Ordering::SeqCst) {
        return Err("the link to the AI host is busy with another request");
    }
    struct Running;
    impl Drop for Running {
        fn drop(&mut self) {
            REQUEST_RUNNING.store(false, Ordering::SeqCst);
        }
    }
    let _running = Running;
    let over_network = LINK.get().settings.is_some();
    if over_network && !LINK.get().connected {
        return Err("the AI host is not connected");
    }
    let st = STATE.get();
    let id = st.next_id;
    st.next_id += 1;
    send(&fields.str("type", kind).num("id", id).finish());
    let deadline = time::uptime_ms() + timeout_ms;
    loop {
        // Other answers stay for whoever waits for them; ours is taken out.
        pump();
        if let Some(at) = ANSWERS.get().iter().position(|m| m.num("id") == Some(id) && m.str("type") == Some(answer)) {
            return ANSWERS.get().remove(at).ok_or("the answer got lost");
        }
        if over_network && !LINK.get().connected {
            return Err("the connection to the AI host was lost");
        }
        if time::uptime_ms() > deadline {
            return Err("the AI host did not answer");
        }
        thread::pause(pending);
    }
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

/// Send a message now and again each time beehive connects (e.g. the health
/// result of a trial kernel, which the host's deploy watchdog waits for).
pub fn set_status(msg: String) {
    send(&msg);
    STATE.get().status = Some(msg);
}

pub fn hello() {
    if LINK.get().settings.is_some() {
        return; // the hive thread says hello as part of its proof
    }
    send(&Obj::new().str("type", "hello").str("version", crate::OS_VERSION).str("build", crate::OS_BUILD).finish());
}

/// Send hello and wait up to `ms` for a welcome. Returns the peer if online.
pub fn ping(ms: u64) -> Option<String> {
    if !present() {
        return None;
    }
    if LINK.get().settings.is_some() {
        return peer(); // the hive thread keeps this up to date
    }
    STATE.get().peer = None;
    hello();
    let deadline = time::uptime_ms() + ms;
    while time::uptime_ms() < deadline {
        if let Some(p) = peer() {
            return Some(p);
        }
        thread::pause(pending);
    }
    None
}

static REQUEST_RUNNING: AtomicBool = AtomicBool::new(false);

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
    // One conversation at a time: answers carry no thread, only a request id.
    if REQUEST_RUNNING.swap(true, Ordering::SeqCst) {
        return Outcome::Error("the AI is busy with a request from another session".into());
    }
    struct Running;
    impl Drop for Running {
        fn drop(&mut self) {
            REQUEST_RUNNING.store(false, Ordering::SeqCst);
        }
    }
    let _running = Running;
    let over_network = LINK.get().settings.is_some();
    if over_network && !LINK.get().connected {
        return Outcome::Offline;
    }
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
        if over_network && !LINK.get().connected {
            return Outcome::Error("the connection to the AI host was lost".into());
        }
        if time::uptime_ms() > deadline {
            return if acked { Outcome::Error("no answer (timed out)".into()) } else { Outcome::Offline };
        }
        // The 100 Hz timer wakes us too, so the deadline is checked.
        thread::pause(|| pending() || input::pending());
    }
}

// --- the link over the network -----------------------------------------------------

/// Where beehive is and who we are to it (`ai connect`, saved in /system/hive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    /// The name this machine was paired under.
    pub name: String,
    pub key: hive::Key,
}

impl Settings {
    /// The settings file: one "word value" per line.
    pub fn to_text(&self) -> String {
        format!("host {}\nport {}\nname {}\nkey {}\n", self.host, self.port, self.name, hive::key_text(&self.key))
    }

    /// Read the settings file; `None` unless everything is there and valid.
    pub fn parse(text: &str) -> Option<Settings> {
        let (mut host, mut port, mut name, mut key) = (None, hive::DEFAULT_PORT, None, None);
        for line in text.lines() {
            let mut words = line.split_whitespace();
            match (words.next(), words.next(), words.next()) {
                (Some("host"), Some(v), None) => host = Some(v),
                (Some("port"), Some(v), None) => port = v.parse().ok().filter(|p| *p != 0)?,
                (Some("name"), Some(v), None) => name = Some(v),
                (Some("key"), Some(v), None) => key = Some(hive::key_bytes(v)?),
                _ => {}
            }
        }
        Settings::new(host?, port, name?, key?)
    }

    /// `None` when the host or the name cannot be right.
    pub fn new(host: &str, port: u16, name: &str, key: hive::Key) -> Option<Settings> {
        let host_ok = !host.is_empty() && host.len() <= 253 && host.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-');
        (host_ok && port != 0 && hive::valid_name(name)).then(|| Settings { host: host.into(), port, name: name.into(), key })
    }
}

/// Lines waiting in either direction: more are dropped (outgoing) or left
/// in the connection until there is room (incoming).
const MAX_QUEUED: usize = 128;
/// The proof must be done within this time.
const PROOF_MS: u64 = 10_000;
/// "Still there?" this often; no word from beehive for the longer time: start over.
const PING_EVERY_MS: u64 = 30_000;
const SILENCE_MS: u64 = 75_000;
/// Wait this long before connecting again, doubling up to the second value.
const RETRY_MS: u64 = 2_000;
const RETRY_MAX_MS: u64 = 60_000;

struct Link {
    settings: Option<Settings>,
    /// The kill switch: `ai off` closes the connection and keeps it closed.
    enabled: bool,
    /// Counts changes of settings and switch: a running connection ends when it moves.
    generation: u32,
    connected: bool,
    /// Why there is no connection, in words.
    problem: Option<String>,
    /// When the state last changed (ms since boot).
    since: u64,
    incoming: VecDeque<String>,
    outgoing: VecDeque<String>,
    thread: bool,
    /// Connections made and lost that the console has not been told about.
    news: Vec<(&'static str, String)>,
}

static LINK: Global<Link> = Global::new(Link {
    settings: None,
    enabled: true,
    generation: 0,
    connected: false,
    problem: None,
    since: 0,
    incoming: VecDeque::new(),
    outgoing: VecDeque::new(),
    thread: false,
    news: Vec::new(),
});

/// Set up, change or remove (`None`) the network link. The hive thread
/// follows: it drops a running connection and connects with the new settings.
pub fn configure(settings: Option<Settings>) {
    let link = LINK.get();
    link.settings = settings;
    link.generation = link.generation.wrapping_add(1);
    link.outgoing.clear();
    link.incoming.clear();
    link.problem = None;
    if link.settings.is_some() && !link.thread {
        link.thread = thread::spawn("hive", None, hive_thread).is_some();
    }
}

/// The kill switch (`ai on` / `ai off`): off closes the connection to
/// beehive and makes no new one.
pub fn set_enabled(enabled: bool) {
    let link = LINK.get();
    if link.enabled != enabled {
        link.enabled = enabled;
        link.generation = link.generation.wrapping_add(1);
        if !enabled {
            link.outgoing.clear();
            // Off at once for everyone who asks; the hive thread closes the
            // connection at its next look.
            set_state(false, None);
        }
    }
}

/// The network link for `ai`: the name we are paired under, whether we are
/// connected, the reason if not, and for how many ms. `None`: no network link.
pub fn link_status() -> Option<(String, bool, Option<String>, u64)> {
    let link = LINK.get();
    let settings = link.settings.as_ref()?;
    let problem = if link.enabled { link.problem.clone() } else { Some(String::from("AI features are switched off")) };
    Some((settings.name.clone(), link.connected, problem, time::uptime_ms().saturating_sub(link.since)))
}

/// Connections made and lost since the last call: ("connected" or "lost",
/// reason). For the console and the event log; never an address.
pub fn take_news() -> Vec<(&'static str, String)> {
    core::mem::take(&mut LINK.get().news)
}

fn set_state(connected: bool, problem: Option<String>) {
    let link = LINK.get();
    if link.connected != connected {
        link.news.push((if connected { "connected" } else { "lost" }, problem.clone().unwrap_or_default()));
        if link.news.len() > 16 {
            link.news.remove(0);
        }
    }
    if link.connected != connected || link.problem != problem {
        link.since = time::uptime_ms();
    }
    link.connected = connected;
    link.problem = problem;
    if !connected {
        STATE.get().peer = None;
        link.incoming.clear();
    }
}

/// The thread that keeps the connection: connect, prove, carry lines, and
/// start over when it breaks, waiting longer each time.
fn hive_thread() {
    let mut wait = RETRY_MS;
    loop {
        let (settings, enabled, generation) = {
            let link = LINK.get();
            (link.settings.clone(), link.enabled, link.generation)
        };
        let Some(settings) = settings.filter(|_| enabled) else {
            set_state(false, None);
            thread::sleep_ms(200);
            continue;
        };
        let (was_connected, why) = session(&settings, generation);
        set_state(false, Some(why));
        if was_connected {
            wait = RETRY_MS;
        }
        // Wait before the next try, unless the settings change meanwhile, or
        // the network gets the address it was missing.
        let until = time::uptime_ms() + wait;
        let no_address = net::address().is_none();
        thread::wait_until(|| {
            time::uptime_ms() >= until || LINK.get().generation != generation || (no_address && net::address().is_some())
        });
        wait = (wait * 2).min(RETRY_MAX_MS);
    }
}

/// Why there is no connection while the network is still starting.
pub const NO_ADDRESS: &str = "the network has no address yet (see net)";

/// One line from the connection, waiting until `deadline`. `Err`: why not.
fn read_line(stream: &net::TcpStream, pending: &mut Vec<u8>, deadline: u64) -> Result<String, &'static str> {
    loop {
        if let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            return String::from_utf8(line[..end].to_vec()).map_err(|_| "beehive sent something unreadable");
        }
        if pending.len() > hive::MAX_LINE {
            return Err("beehive sent a line that is too long");
        }
        let mut buf = [0u8; 512];
        match stream.receive(&mut buf) {
            net::Received::Data(n) => pending.extend_from_slice(&buf[..n]),
            net::Received::End => return Err("closed"),
            net::Received::Failed(e) => return Err(e),
            net::Received::Nothing => {
                if time::uptime_ms() >= deadline {
                    return Err("no answer");
                }
                let tick = time::uptime_ms();
                thread::wait_until(|| time::uptime_ms() != tick);
            }
        }
    }
}

/// One connection, from connecting to its end. Returns whether the proof
/// succeeded, and why the connection ended.
fn session(settings: &Settings, generation: u32) -> (bool, String) {
    let fail = |why: &str| (false, String::from(why));
    if net::address().is_none() {
        return fail(NO_ADDRESS);
    }
    let ip = match net::resolve(&settings.host) {
        Ok(ip) => ip,
        Err(_) => return fail("the name of the AI host could not be looked up"),
    };
    let stream = match net::tcp_connect(ip, settings.port) {
        Ok(stream) => stream,
        Err(e) => return (false, format!("beehive is not reachable: {e}")),
    };
    // The proof (beenet::hive).
    let mut random = [0u8; 16];
    random[..8].copy_from_slice(&rand::next_u64().to_le_bytes());
    random[8..].copy_from_slice(&rand::next_u64().to_le_bytes());
    let ours = hive::hex(&random);
    let hello = Obj::new()
        .str("type", "hello")
        .str("name", &settings.name)
        .str("nonce", &ours)
        .str("version", crate::OS_VERSION)
        .str("build", crate::OS_BUILD)
        .finish();
    let deadline = time::uptime_ms() + PROOF_MS;
    let mut pending = Vec::new();
    if stream.send(format!("{hello}\n").as_bytes()).is_err() {
        return fail("the connection broke");
    }
    let theirs = match read_line(&stream, &mut pending, deadline).ok().and_then(|l| json::parse(&l)) {
        Some(m) if m.str("type") == Some("challenge") => match m.str("nonce") {
            Some(n) if hive::valid_nonce(n) => String::from(n),
            _ => return fail("the other side is not beehive"),
        },
        _ => return fail("the other side is not beehive (or it is ignoring this machine for a minute)"),
    };
    let auth = Obj::new().str("type", "auth").str("mac", &hive::proof(&settings.key, "os", &ours, &theirs)).finish();
    if stream.send(format!("{auth}\n").as_bytes()).is_err() {
        return fail("the connection broke");
    }
    let welcome = match read_line(&stream, &mut pending, deadline) {
        Ok(line) => json::parse(&line),
        Err(_) => return fail("refused: wrong key or name, or this machine is not paired (beehive.py pair NAME)"),
    };
    let expected = hive::proof(&settings.key, "hive", &theirs, &ours);
    let peer = match welcome {
        Some(m) if m.str("type") == Some("welcome") && hive::same(m.str("mac").unwrap_or("").as_bytes(), expected.as_bytes()) => {
            format!("{} {}", m.str("backend").unwrap_or("?"), m.str("model").unwrap_or("")).trim_end().into()
        }
        _ => return fail("the other side could not prove it is the beehive this machine was paired with"),
    };
    let key = hive::session_key(&settings.key, &ours, &theirs);
    STATE.get().peer = Some(peer);
    set_state(true, None);
    if let Some(status) = STATE.get().status.clone() {
        send(&status); // e.g. the health result the deploy watchdog waits for
    }

    let (mut sent, mut received) = (0u64, 0u64);
    let mut heard = time::uptime_ms();
    let mut pinged = heard;
    let ended = loop {
        if LINK.get().generation != generation {
            break "the settings changed";
        }
        let now = time::uptime_ms();
        if now.saturating_sub(pinged) >= PING_EVERY_MS {
            pinged = now;
            LINK.get().outgoing.push_back(String::from(r#"{"type":"ping"}"#));
        }
        if now.saturating_sub(heard) >= SILENCE_MS {
            break "beehive stopped answering";
        }
        // Out: everything that is waiting. The queue is fetched again after
        // each send, which may wait.
        let mut broke = false;
        while let Some(line) = LINK.get().outgoing.pop_front() {
            sent += 1;
            if stream.send(format!("{}\n", hive::seal(&key, "o", sent, &line)).as_bytes()).is_err() {
                broke = true;
                break;
            }
        }
        if broke {
            break "the connection broke";
        }
        // In: whole lines, as long as there is room for them.
        let mut problem = None;
        while LINK.get().incoming.len() < MAX_QUEUED {
            let line = match read_line(&stream, &mut pending, 0) {
                Ok(line) => line,
                Err("no answer") => break,
                Err("closed") => {
                    problem = Some("beehive closed the connection");
                    break;
                }
                Err(e) => {
                    problem = Some(e);
                    break;
                }
            };
            received += 1;
            let Some(text) = hive::unseal(&key, "h", received, &line) else {
                problem = Some("a message was not genuine");
                break;
            };
            heard = time::uptime_ms();
            if !text.contains(r#""type": "pong""#) && !text.contains(r#""type":"pong""#) {
                LINK.get().incoming.push_back(text.into());
            }
        }
        if let Some(problem) = problem {
            break problem;
        }
        let tick = time::uptime_ms();
        thread::wait_until(|| time::uptime_ms() != tick || !LINK.get().outgoing.is_empty());
    };
    (true, String::from(ended))
}
