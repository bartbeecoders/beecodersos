//! The remote shell: BeeShell over the network, Telnet style (phase N6).
//!
//! OFF unless switched on with `remote on` (saved in /system/remote). It is
//! NOT encrypted: the password and everything typed cross the network as
//! plain text, so it is for your own network only.
//!
//! What guards it:
//!   * A password is always required. The OS makes it up (`new_password`),
//!     so it is never a password somebody also uses elsewhere, and never a
//!     weak one. It is kept in /system/remote-password.
//!   * Guessing is slow: one client at a time, a pause after every wrong
//!     password, three tries per connection, 30 s to log in.
//!   * One session at a time; it ends after 10 minutes without input.
//!   * It can only be switched on or off, or given a new password, from the
//!     machine's own console (see `Shell::remote_command`).
//!   * Logins are counted and shown on the console; client addresses are
//!     never logged.
//!
//! It runs in a thread of its own ("remote"); the shell of a session runs in
//! a second one ("remote-shell") inside a console session (session.rs).

use crate::input::Key;
use crate::ring::Global;
use crate::serial::SerialDecoder;
use crate::shell::Shell;
use crate::{net, rand, session, storage, thread, time};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beenet::telnet;

pub const DEFAULT_PORT: u16 = 23;
const SETTING_FILE: &str = "/system/remote";
const PASSWORD_FILE: &str = "/system/remote-password";
/// Letters and digits that are hard to mix up (no 0/o, 1/l/i).
const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
/// 12 characters from 31: about 59 bits.
const PASSWORD_CHARS: usize = 12;
const MAX_TRIES: usize = 3;
const LOGIN_TIMEOUT_MS: u64 = 30_000;
/// The pause after a wrong password.
const WRONG_DELAY_MS: u64 = 2_000;
const IDLE_TIMEOUT_MS: u64 = 10 * 60 * 1000;
/// A client that takes nothing for this long is dropped.
const SEND_STALL_MS: u64 = 10_000;
const MAX_TYPED: usize = 64;
/// Output waiting for a slow client beyond this stays in the session (which cuts off the oldest).
const MAX_PENDING: usize = 16 * 1024;

struct State {
    /// Should be running, and on which port.
    wanted: Option<u16>,
    thread: Option<thread::ThreadId>,
    connected: bool,
    logins: u32,
    failures: u32,
    /// Logins and wrong passwords the console has not been told about yet.
    news: (u32, u32),
}

static STATE: Global<State> =
    Global::new(State { wanted: None, thread: None, connected: false, logins: 0, failures: 0, news: (0, 0) });

/// The port the remote shell listens on, if it is on.
pub fn port() -> Option<u16> {
    STATE.get().wanted
}

/// (logins, wrong passwords) since boot, and whether someone is connected now.
pub fn counters() -> (u32, u32, bool) {
    let s = STATE.get();
    (s.logins, s.failures, s.connected)
}

/// Logins and wrong passwords since the last call (for the console).
pub fn take_news() -> (u32, u32) {
    core::mem::take(&mut STATE.get().news)
}

/// A fresh password, like "abcd-efgh-jkmn".
pub fn new_password() -> String {
    let mut text = String::new();
    while text.len() < PASSWORD_CHARS + 2 {
        // Mix the clock's fine grain into every draw; take only values that
        // map evenly onto the alphabet.
        let value = ((rand::next_u64() ^ time::rdtsc().rotate_left(17)) >> 32) as usize % 32;
        if value < ALPHABET.len() {
            if text.len() == 4 || text.len() == 9 {
                text.push('-');
            }
            text.push(ALPHABET[value] as char);
        }
    }
    text
}

/// Is `typed` the password? Dashes, spaces and capitals do not matter. Takes
/// the same time whatever part is wrong.
pub fn password_matches(typed: &str, real: &str) -> bool {
    let clean = |s: &str| -> Vec<u8> {
        s.bytes().filter(|b| *b != b'-' && !b.is_ascii_whitespace()).map(|b| b.to_ascii_lowercase()).collect()
    };
    let (typed, real) = (clean(typed), clean(real));
    if real.len() < PASSWORD_CHARS {
        return false; // no password, or a damaged file: nobody gets in
    }
    let mut difference = (typed.len() != real.len()) as u8;
    for (i, r) in real.iter().enumerate() {
        difference |= r ^ typed.get(i).copied().unwrap_or(0xFF);
    }
    difference == 0
}

fn saved_password() -> Option<String> {
    let data = storage::handle()?.fs().read_file(PASSWORD_FILE).ok()?;
    Some(String::from(String::from_utf8_lossy(&data).trim()))
}

fn save(path: &str, text: &str) -> Result<(), String> {
    let mut storage = storage::handle().ok_or("no file system")?;
    let mut fs = storage.fs();
    let _ = fs.mkdir_all("/system");
    fs.write_file(path, text.as_bytes()).map_err(|e| format!("can't save {path}: {e}"))
}

/// Make a new password and save it. Returned to be shown once.
pub fn change_password() -> Result<String, String> {
    let password = new_password();
    save(PASSWORD_FILE, &format!("{password}\n"))?;
    Ok(password)
}

/// Switch the remote shell on (None: off) and save the choice. Switching on
/// makes a password if there is none yet: it is returned to be shown once.
pub fn set(port: Option<u16>) -> Result<Option<String>, String> {
    let mut fresh = None;
    if port.is_some() {
        if !net::present() {
            return Err(String::from("no network card"));
        }
        if !saved_password().is_some_and(|p| password_matches(&p, &p)) {
            fresh = Some(change_password()?);
        }
    }
    start(port)?;
    save(SETTING_FILE, &port.map_or(String::from("off\n"), |p| format!("on {p}\n")))?;
    Ok(fresh)
}

fn start(port: Option<u16>) -> Result<(), String> {
    // Let a running server stop first: it ends its session and frees the port.
    STATE.get().wanted = None;
    if let Some(id) = STATE.get().thread.take() {
        thread::join(id);
    }
    let Some(port) = port else { return Ok(()) };
    net::tcp_listen(port).map_err(String::from)?;
    STATE.get().wanted = Some(port);
    let id = thread::spawn("remote", None, move || serve(port));
    if id.is_none() {
        net::tcp_unlisten(port);
        STATE.get().wanted = None;
        return Err(String::from("too many threads"));
    }
    STATE.get().thread = id;
    Ok(())
}

/// At boot: start the remote shell if it was left switched on and has a
/// password. Returns its port.
pub fn load() -> Option<u16> {
    let data = storage::handle()?.fs().read_file(SETTING_FILE).ok()?;
    let port = parse_setting(&String::from_utf8_lossy(&data))?;
    let password = saved_password()?;
    if !password_matches(&password, &password) {
        return None;
    }
    start(Some(port)).ok()?;
    Some(port)
}

/// "on PORT" (or "on"): the port. Anything else means off.
pub fn parse_setting(text: &str) -> Option<u16> {
    let mut words = text.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("on"), None, _) => Some(DEFAULT_PORT),
        (Some("on"), Some(port), None) => port.parse().ok().filter(|p| *p != 0),
        _ => None,
    }
}

fn wanted(port: u16) -> bool {
    STATE.get().wanted == Some(port)
}

fn serve(port: u16) {
    while wanted(port) {
        match net::tcp_accept(port) {
            Some(stream) => {
                let mut link = Link::new(&stream);
                link.raw(&telnet::GREETING);
                link.text("BeeCodersOs remote shell (not encrypted)\n");
                if login(&mut link, port) {
                    run_shell(&mut link, port);
                }
                link.flush(1_000);
            }
            None => {
                thread::pause(|| false); // until the next timer tick
            }
        }
    }
    net::tcp_unlisten(port);
}

/// One client: its connection, what it types (decoded) and what is still to
/// be sent to it.
struct Link<'a> {
    stream: &'a net::TcpStream,
    telnet: telnet::Decoder,
    keys: SerialDecoder,
    out: Vec<u8>,
    last_progress: u64,
    gone: bool,
}

impl<'a> Link<'a> {
    fn new(stream: &'a net::TcpStream) -> Self {
        Link {
            stream,
            telnet: telnet::Decoder::new(),
            keys: SerialDecoder::new(),
            out: Vec::new(),
            last_progress: time::uptime_ms(),
            gone: false,
        }
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    fn text(&mut self, text: &str) {
        telnet::encode(text.as_bytes(), &mut self.out);
    }

    /// Move bytes both ways, without waiting. Returns the keys that arrived.
    fn turn(&mut self) -> Vec<Key> {
        let mut keys = Vec::new();
        let mut buf = [0u8; 256];
        match self.stream.receive(&mut buf) {
            net::Received::Data(n) => {
                for &b in &buf[..n] {
                    match self.telnet.feed(b) {
                        Some(telnet::Event::Byte(b)) => keys.extend(self.keys.feed(b)),
                        Some(telnet::Event::Interrupt) => keys.push(Key::Ctrl('c')),
                        Some(telnet::Event::EndOfFile) => keys.push(Key::Ctrl('d')),
                        Some(telnet::Event::Reply(reply)) => self.out.extend_from_slice(&reply),
                        None => {}
                    }
                }
            }
            net::Received::Nothing => {}
            net::Received::End | net::Received::Failed(_) => self.gone = true,
        }
        let now = time::uptime_ms();
        if self.out.is_empty() {
            self.last_progress = now;
        } else if !self.gone {
            match self.stream.send_some(&self.out) {
                Ok(0) if now - self.last_progress > SEND_STALL_MS => self.gone = true,
                Ok(0) => {}
                Ok(n) => {
                    self.out.drain(..n);
                    self.last_progress = now;
                }
                Err(_) => self.gone = true,
            }
        }
        keys
    }

    /// Send what is waiting, for at most `ms`. What the client types meanwhile is dropped.
    fn flush(&mut self, ms: u64) {
        let end = time::uptime_ms() + ms;
        while !self.out.is_empty() && !self.gone && time::uptime_ms() < end {
            self.turn();
            thread::pause(|| false);
        }
    }
}

/// Ask for the password. True when the client may have a shell.
fn login(link: &mut Link, port: u16) -> bool {
    let Some(real) = saved_password() else { return false };
    let deadline = time::uptime_ms() + LOGIN_TIMEOUT_MS;
    for _ in 0..MAX_TRIES {
        link.text("password: ");
        let mut typed = String::new();
        'typing: loop {
            for key in link.turn() {
                match key {
                    Key::Enter => break 'typing,
                    Key::Char(c) if typed.len() < MAX_TYPED => typed.push(c), // never echoed
                    Key::Backspace => {
                        typed.pop();
                    }
                    Key::Ctrl('c' | 'd') => return false,
                    _ => {}
                }
            }
            if link.gone || !wanted(port) {
                return false;
            }
            if time::uptime_ms() > deadline {
                link.text("\ntoo slow\n");
                return false;
            }
            thread::pause(|| false);
        }
        if password_matches(&typed, &real) {
            let s = STATE.get();
            s.logins += 1;
            s.news.0 += 1;
            link.text("\n");
            return true;
        }
        let s = STATE.get();
        s.failures += 1;
        s.news.1 += 1;
        // Make guessing slow. What is typed meanwhile does not count.
        let end = time::uptime_ms() + WRONG_DELAY_MS;
        while time::uptime_ms() < end {
            link.turn();
            if link.gone || !wanted(port) {
                return false;
            }
            thread::pause(|| false);
        }
        link.text("\nwrong password\n");
    }
    false
}

/// Run a shell for a logged-in client until it leaves.
fn run_shell(link: &mut Link, port: u16) {
    let (Some(id), Some(storage)) = (session::open(), storage::handle()) else {
        link.text("no room for a session now\n");
        return;
    };
    let Some(shell) = thread::spawn("remote-shell", Some(id), move || Shell::remote(storage).run_session()) else {
        session::remove(id);
        link.text("no room for a session now\n");
        return;
    };
    STATE.get().connected = true;
    link.text("logged in ('exit' ends the session)\n");
    let mut last_input = time::uptime_ms();
    loop {
        for key in link.turn() {
            session::push_key(id, key);
            last_input = time::uptime_ms();
        }
        if link.out.len() < MAX_PENDING {
            link.text(&session::take_output(id));
        }
        if link.gone || !wanted(port) {
            break;
        }
        if !thread::is_alive(shell) {
            link.text(&session::take_output(id));
            break;
        }
        if time::uptime_ms() - last_input > IDLE_TIMEOUT_MS {
            link.text("\nno input for 10 minutes: closing\n");
            break;
        }
        thread::pause(|| false);
    }
    // Stop whatever the shell is doing and wait until it has ended.
    session::hang_up(id);
    thread::join(shell);
    session::remove(id);
    STATE.get().connected = false;
}
