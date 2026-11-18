//! The remote desktop: a VNC server (RFB, beenet::rfb), so that the screen
//! can be seen and the machine used from another computer with any VNC
//! viewer (TigerVNC, Remmina, macOS Screen Sharing...).
//!
//! OFF unless switched on with `vnc on` (saved in /system/vnc). It is NOT
//! encrypted: what is on the screen and what is typed cross the network as
//! they are, so it is for your own network only. What guards it, as for the
//! remote shell (remote.rs):
//!   * A password made up by the OS (8 characters: the most VNC allows),
//!     kept in /system/vnc-password; checked with VNC Authentication, so it
//!     never crosses the network itself.
//!   * One viewer at a time; a pause after a wrong password; 30 s to log in.
//!   * Switched on or off, or given a new password, only on the machine's
//!     own console. Logins are counted; addresses are never logged.
//!
//! What the viewer sees is the screen as drawn in memory, sent as Raw
//! rectangles: only the 64x64 tiles that changed since the last update. Its
//! keys are typed as on this machine's keyboard (no layout applies: the
//! viewer sends characters), its mouse moves the pointer.

use crate::input::Key;
use crate::ring::Global;
use crate::{console, keyboard, net, rand, storage, thread, time};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beenet::rfb::{self, ClientMessage, KeyName, Modifier, PixelFormat};

pub const DEFAULT_PORT: u16 = 5900;
const SETTING_FILE: &str = "/system/vnc";
const PASSWORD_FILE: &str = "/system/vnc-password";
/// Letters and digits that are hard to mix up (no 0/o, 1/l/i).
const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
/// VNC Authentication uses the first 8 characters only.
const PASSWORD_CHARS: usize = 8;
const LOGIN_TIMEOUT_MS: u64 = 30_000;
const WRONG_DELAY_MS: u64 = 2_000;
const IDLE_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const SEND_STALL_MS: u64 = 15_000;
/// Updates are sent at most this often.
const UPDATE_EVERY_MS: u64 = 50;
pub const TILE: usize = 64;

struct State {
    wanted: Option<u16>,
    thread: Option<thread::ThreadId>,
    connected: bool,
    logins: u32,
    failures: u32,
}

static STATE: Global<State> = Global::new(State { wanted: None, thread: None, connected: false, logins: 0, failures: 0 });

pub fn port() -> Option<u16> {
    STATE.get().wanted
}

/// (logins, wrong passwords) since boot, and whether a viewer is connected now.
pub fn counters() -> (u32, u32, bool) {
    let s = STATE.get();
    (s.logins, s.failures, s.connected)
}

fn new_password() -> String {
    let mut text = String::new();
    while text.len() < PASSWORD_CHARS {
        let value = ((rand::next_u64() ^ time::rdtsc().rotate_left(17)) >> 32) as usize % 32;
        if value < ALPHABET.len() {
            text.push(ALPHABET[value] as char);
        }
    }
    text
}

fn saved_password() -> Option<String> {
    let data = storage::handle()?.fs().read_file(PASSWORD_FILE).ok()?;
    let password = String::from(String::from_utf8_lossy(&data).trim());
    (password.len() == PASSWORD_CHARS).then_some(password)
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

/// Switch the remote desktop on (None: off) and save the choice. Switching
/// on makes a password if there is none yet: it is returned to be shown once.
pub fn set(port: Option<u16>) -> Result<Option<String>, String> {
    let mut fresh = None;
    if port.is_some() {
        if !net::present() {
            return Err(String::from("no network card"));
        }
        if console::pointer_at().is_none() && !console::desktop_on() {
            return Err(String::from("needs the desktop (desktop on)"));
        }
        if saved_password().is_none() {
            fresh = Some(change_password()?);
        }
    }
    start(port)?;
    save(SETTING_FILE, &port.map_or(String::from("off\n"), |p| format!("on {p}\n")))?;
    Ok(fresh)
}

fn start(port: Option<u16>) -> Result<(), String> {
    STATE.get().wanted = None;
    if let Some(id) = STATE.get().thread.take() {
        thread::join(id);
    }
    let Some(port) = port else { return Ok(()) };
    net::tcp_listen(port).map_err(String::from)?;
    STATE.get().wanted = Some(port);
    let id = thread::spawn("vnc", None, move || serve(port));
    if id.is_none() {
        net::tcp_unlisten(port);
        STATE.get().wanted = None;
        return Err(String::from("too many threads"));
    }
    STATE.get().thread = id;
    Ok(())
}

/// At boot: start the remote desktop if it was left on and has a password.
pub fn load() -> Option<u16> {
    let data = storage::handle()?.fs().read_file(SETTING_FILE).ok()?;
    let port = crate::remote::parse_setting(&String::from_utf8_lossy(&data))?;
    saved_password()?;
    start(Some(port)).ok()?;
    Some(port)
}

fn wanted(port: u16) -> bool {
    STATE.get().wanted == Some(port)
}

fn serve(port: u16) {
    while wanted(port) {
        match net::tcp_accept(port) {
            Some(stream) => {
                STATE.get().connected = true;
                let mut link = Link { stream: &stream, input: Vec::new(), out: Vec::new(), last_progress: time::uptime_ms(), gone: false };
                if let Some(minor) = handshake(&mut link, port) {
                    session(&mut link, port, minor);
                }
                link.flush(1_000);
                STATE.get().connected = false;
            }
            None => {
                thread::pause(|| false);
            }
        }
    }
    net::tcp_unlisten(port);
}

struct Link<'a> {
    stream: &'a net::TcpStream,
    input: Vec<u8>,
    out: Vec<u8>,
    last_progress: u64,
    gone: bool,
}

impl Link<'_> {
    /// Move bytes both ways, without waiting; what arrived is added to `input`.
    fn turn(&mut self) {
        let mut buf = [0u8; 1024];
        match self.stream.receive(&mut buf) {
            net::Received::Data(n) => self.input.extend_from_slice(&buf[..n]),
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
    }

    /// Wait for `n` bytes (until `deadline`): `None` if they did not come.
    fn take(&mut self, n: usize, deadline: u64, port: u16) -> Option<Vec<u8>> {
        while self.input.len() < n {
            self.turn();
            if self.gone || !wanted(port) || time::uptime_ms() > deadline {
                return None;
            }
            thread::pause(|| false);
        }
        Some(self.input.drain(..n).collect())
    }

    fn flush(&mut self, ms: u64) {
        let end = time::uptime_ms() + ms;
        while !self.out.is_empty() && !self.gone && time::uptime_ms() < end {
            self.turn();
            thread::pause(|| false);
        }
    }
}

/// Version, password, ClientInit, ServerInit. Returns the protocol's minor version.
fn handshake(link: &mut Link, port: u16) -> Option<u8> {
    let deadline = time::uptime_ms() + LOGIN_TIMEOUT_MS;
    link.out.extend_from_slice(rfb::SERVER_VERSION);
    let minor = rfb::parse_version(&link.take(12, deadline, port)?)?;
    if minor == 3 {
        link.out.extend_from_slice(&(rfb::SECURITY_VNC_AUTH as u32).to_be_bytes());
    } else {
        link.out.extend_from_slice(&[1, rfb::SECURITY_VNC_AUTH]);
        if link.take(1, deadline, port)?[0] != rfb::SECURITY_VNC_AUTH {
            return None;
        }
    }
    let password = saved_password()?;
    let mut challenge = [0u8; 16];
    for half in challenge.chunks_mut(8) {
        half.copy_from_slice(&(rand::next_u64() ^ time::rdtsc().rotate_left(29)).to_le_bytes());
    }
    link.out.extend_from_slice(&challenge);
    let response = link.take(16, deadline, port)?;
    if !rfb::auth_ok(&password, &challenge, &response) {
        STATE.get().failures += 1;
        thread::sleep_ms(WRONG_DELAY_MS);
        rfb::security_result(false, minor, "wrong password", &mut link.out);
        return None;
    }
    STATE.get().logins += 1;
    rfb::security_result(true, minor, "", &mut link.out);
    link.take(1, deadline, port)?; // ClientInit: shared or not, one viewer anyway
    let mut screen = Vec::new();
    let (w, h) = console::capture_rgb(&mut screen)?;
    link.out.extend_from_slice(&rfb::server_init(w as u16, h as u16, &rfb::desktop_name("")));
    Some(minor)
}

/// The rectangles (x, y, w, h) of the TILE x TILE tiles where `new` differs
/// from `old` (both `width` x `height`); every tile if `old` is not that size.
pub fn changed_tiles(old: &[u32], new: &[u32], width: usize, height: usize) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    let all = old.len() != new.len() || new.len() != width * height;
    for ty in (0..height).step_by(TILE) {
        for tx in (0..width).step_by(TILE) {
            let (w, h) = (TILE.min(width - tx), TILE.min(height - ty));
            let differs = all || (ty..ty + h).any(|y| old[y * width + tx..y * width + tx + w] != new[y * width + tx..y * width + tx + w]);
            if differs {
                out.push((tx, ty, w, h));
            }
        }
    }
    out
}

/// The keys a viewer's keysyms mean, with Control and Alt held or not.
#[derive(Default)]
pub struct Keys {
    ctrl: bool,
    alt: bool,
}

impl Keys {
    pub fn event(&mut self, down: bool, keysym: u32) -> Option<Key> {
        let name = rfb::keysym(keysym)?;
        match name {
            KeyName::Modifier(Modifier::Control) => self.ctrl = down,
            KeyName::Modifier(Modifier::Alt) => self.alt = down,
            _ => {}
        }
        if !down {
            return None;
        }
        Some(match name {
            KeyName::Char(c) if self.ctrl && c.is_ascii_alphabetic() => Key::Ctrl(c.to_ascii_lowercase()),
            KeyName::Char(c) => Key::Char(c),
            KeyName::Tab if self.alt => Key::NextWindow,
            KeyName::Enter => Key::Enter,
            KeyName::Backspace => Key::Backspace,
            KeyName::Tab => Key::Tab,
            KeyName::Escape => Key::Escape,
            KeyName::Delete => Key::Delete,
            KeyName::Insert => Key::Insert,
            KeyName::Left => Key::Left,
            KeyName::Right => Key::Right,
            KeyName::Up => Key::Up,
            KeyName::Down => Key::Down,
            KeyName::Home => Key::Home,
            KeyName::End => Key::End,
            KeyName::PageUp => Key::PageUp,
            KeyName::PageDown => Key::PageDown,
            KeyName::Modifier(_) => return None,
        })
    }
}

/// A logged-in viewer: its messages in, screen updates out.
fn session(link: &mut Link, port: u16, _minor: u8) {
    let mut reader = rfb::ClientReader::new();
    let mut format = PixelFormat::SERVER;
    let mut keys = Keys::default();
    let mut sent: Vec<u32> = Vec::new();
    let mut screen: Vec<u32> = Vec::new();
    let mut request: Option<bool> = None; // Some(incremental)
    let mut buttons = 0u8;
    let (mut last_update, mut last_input) = (0u64, time::uptime_ms());
    loop {
        link.turn();
        let input = core::mem::take(&mut link.input);
        let messages = match reader.feed(&input) {
            Ok(messages) => messages,
            Err(_) => return,
        };
        for message in messages {
            last_input = time::uptime_ms();
            match message {
                ClientMessage::SetPixelFormat(f) if f.supported() => {
                    format = f;
                    sent.clear(); // everything again, in the new format
                }
                ClientMessage::SetPixelFormat(_) => return, // a palette: not offered
                ClientMessage::SetEncodings(_) | ClientMessage::CutText => {}
                ClientMessage::UpdateRequest { incremental, .. } => {
                    // A full one wins over an incremental one still waiting.
                    request = Some(request.map_or(incremental, |r| r && incremental));
                }
                ClientMessage::Key { down, keysym } => {
                    if let Some(key) = keys.event(down, keysym) {
                        keyboard::push_key(key);
                    }
                }
                ClientMessage::Pointer { buttons: b, x, y } => {
                    if console::pointer_at().is_none() {
                        console::mouse_event(0, 0, 0); // the pointer appears (in the middle)
                    }
                    if let Some((px, py)) = console::pointer_at() {
                        let (dx, dy) = (x as i32 - px as i32, y as i32 - py as i32);
                        if dx != 0 || dy != 0 || (b & 1) != buttons {
                            buttons = b & 1;
                            console::mouse_event(buttons, dx, dy);
                        }
                    }
                }
            }
        }
        let now = time::uptime_ms();
        if link.gone || !wanted(port) || now - last_input > IDLE_TIMEOUT_MS {
            return;
        }
        if let Some(incremental) = request {
            if link.out.is_empty() && now - last_update >= UPDATE_EVERY_MS {
                let Some((w, h)) = console::capture_rgb(&mut screen) else { return };
                if !incremental {
                    sent.clear();
                }
                if !sent.is_empty() && sent.len() != w * h {
                    return; // the screen changed size: the viewer connects again
                }
                let tiles = changed_tiles(&sent, &screen, w, h);
                if !tiles.is_empty() {
                    let bytes = format.bytes();
                    let mut out = Vec::with_capacity(4 + tiles.len() * (12 + TILE * TILE * bytes));
                    rfb::update_header(tiles.len().min(u16::MAX as usize) as u16, &mut out);
                    for &(x, y, tw, th) in tiles.iter().take(u16::MAX as usize) {
                        rfb::rect_header(x as u16, y as u16, tw as u16, th as u16, rfb::ENCODING_RAW, &mut out);
                        for row in y..y + th {
                            for &rgb in &screen[row * w + x..row * w + x + tw] {
                                format.put(rgb, &mut out);
                            }
                        }
                    }
                    link.out = out;
                    core::mem::swap(&mut sent, &mut screen);
                    request = None;
                    last_update = now;
                }
            }
        }
        thread::pause(|| false);
    }
}
