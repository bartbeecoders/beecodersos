//! PS/2 keyboard driver (i8042 controller, scancode set 1, IRQ1).
//!
//! The interrupt handler only queues raw scancodes; decoding (modifiers,
//! layouts, dead keys) happens in the main flow via `poll()`.

use crate::input::Key;
use crate::layouts::{compose, Layout, Mapped};
use crate::port::{inb, outb};
use crate::ring::{Global, Ring};
use alloc::vec::Vec;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64; // read
const COMMAND: u16 = 0x64; // write

const ST_OUTPUT_FULL: u8 = 0x01;
const ST_INPUT_FULL: u8 = 0x02;
const ST_AUX_DATA: u8 = 0x20; // byte came from the mouse port

static SCANCODES: Ring<128> = Ring::new();
static STATE: Global<Decoder> = Global::new(Decoder::new());

/// Configure the controller (IRQ1 on, scancode translation on) and set LEDs.
pub fn init() {
    unsafe {
        // Drop what is waiting. With a limit: where there is no controller
        // the status reads as "always full".
        for _ in 0..256 {
            if inb(STATUS) & ST_OUTPUT_FULL == 0 {
                break;
            }
            inb(DATA);
        }
        if write_cmd(0x20).is_some() {
            if let Some(cfg) = read_data() {
                let cfg = (cfg | 0x01 | 0x40) & !0x10; // IRQ1, translation, clock on
                if write_cmd(0x60).is_some() {
                    write_data(cfg);
                }
            }
        }
        write_cmd(0xAE); // enable first PS/2 port
    }
    STATE.get().update_leds();
}

pub fn set_layout(layout: Layout) {
    let s = STATE.get();
    s.layout = layout;
    s.dead = None;
}

pub fn layout() -> Layout {
    STATE.get().layout
}

/// Called from the IRQ1 handler.
pub fn on_interrupt() {
    unsafe {
        let st = inb(STATUS);
        if st & ST_OUTPUT_FULL != 0 {
            let b = inb(DATA);
            if st & ST_AUX_DATA == 0 {
                SCANCODES.push(b);
            }
        }
    }
}

/// A scancode from another kind of keyboard (USB, see usb.rs): it goes
/// through the same decoder and layouts as the PS/2 keyboard's.
pub fn push_scancode(code: u8) {
    SCANCODES.push(code);
}

/// Keys typed from elsewhere (the remote desktop, vnc.rs): already
/// decoded, so no keyboard layout applies to them. At most `MAX_INJECTED`.
static INJECTED: Global<alloc::collections::VecDeque<Key>> = Global::new(alloc::collections::VecDeque::new());
const MAX_INJECTED: usize = 64;

/// Type a key as if on this keyboard: it goes where keys go (the window
/// that has the focus, or the console).
pub fn push_key(key: Key) {
    let queue = INJECTED.get();
    if queue.len() < MAX_INJECTED {
        queue.push_back(key);
    }
}

pub fn pending() -> bool {
    !SCANCODES.is_empty() || STATE.get().queued.is_some() || !INJECTED.get().is_empty()
}

/// Decode queued scancodes into the next key event, if any.
pub fn poll() -> Option<Key> {
    let s = STATE.get();
    if let Some(c) = s.queued.take() {
        return Some(Key::Char(c));
    }
    if let Some(key) = INJECTED.get().pop_front() {
        return Some(key);
    }
    while let Some(sc) = SCANCODES.pop() {
        if let Some(k) = s.feed(sc) {
            return Some(k);
        }
    }
    None
}

/// Decode scancodes with a fresh decoder (for the self-tests; no hardware access
/// as long as Caps Lock / Num Lock are not pressed).
pub(crate) fn decode_for_test(layout: Layout, codes: &[u8]) -> Vec<Key> {
    let mut d = Decoder::new();
    d.layout = layout;
    let mut out = Vec::new();
    for &sc in codes {
        out.extend(d.feed(sc));
        out.extend(d.queued.take().map(Key::Char));
    }
    out
}

// --- controller helpers (with timeouts so a missing controller can't hang us) ---

unsafe fn wait_input_empty() -> Option<()> {
    (0..100_000).find(|_| inb(STATUS) & ST_INPUT_FULL == 0).map(|_| ())
}

unsafe fn write_cmd(cmd: u8) -> Option<()> {
    wait_input_empty()?;
    outb(COMMAND, cmd);
    Some(())
}

unsafe fn write_data(b: u8) -> Option<()> {
    wait_input_empty()?;
    outb(DATA, b);
    Some(())
}

unsafe fn read_data() -> Option<u8> {
    (0..100_000).find(|_| inb(STATUS) & ST_OUTPUT_FULL != 0).map(|_| inb(DATA))
}

// --- scancode decoder --------------------------------------------------------

struct Decoder {
    layout: Layout,
    e0: bool,
    e1: u8,
    lshift: bool,
    rshift: bool,
    lctrl: bool,
    rctrl: bool,
    alt: bool,
    altgr: bool,
    /// The Super (Windows) key: keys with it go to the desktop.
    gui: bool,
    caps: bool,
    num: bool,
    dead: Option<char>,
    queued: Option<char>,
}

impl Decoder {
    const fn new() -> Self {
        Decoder {
            layout: Layout::BelgianPeriod,
            e0: false,
            e1: 0,
            lshift: false,
            rshift: false,
            lctrl: false,
            rctrl: false,
            alt: false,
            altgr: false,
            gui: false,
            caps: false,
            num: true,
            dead: None,
            queued: None,
        }
    }

    fn update_leds(&self) {
        let leds = (self.num as u8) << 1 | (self.caps as u8) << 2;
        unsafe {
            // The keyboard's ACK bytes (0xFA) are ignored by `feed`.
            if write_data(0xED).is_some() {
                write_data(leds);
            }
        }
    }

    fn feed(&mut self, sc: u8) -> Option<Key> {
        match sc {
            0xE0 => {
                self.e0 = true;
                return None;
            }
            0xE1 => {
                self.e1 = 2; // Pause key: skip the next two bytes
                return None;
            }
            0x00 | 0xEE | 0xFA | 0xFE | 0xFF if !self.e0 => return None, // ACK, echo, errors
            _ => {}
        }
        if self.e1 > 0 {
            self.e1 -= 1;
            return None;
        }
        let e0 = core::mem::take(&mut self.e0);
        let pressed = sc & 0x80 == 0;
        let code = sc & 0x7F;

        // Modifier keys track both press and release.
        match (e0, code) {
            (false, 0x2A) => self.lshift = pressed,
            (false, 0x36) => self.rshift = pressed,
            (false, 0x1D) => self.lctrl = pressed,
            (true, 0x1D) => self.rctrl = pressed,
            (false, 0x38) => self.alt = pressed,
            (true, 0x38) => self.altgr = pressed,
            (true, 0x5B) | (true, 0x5C) => self.gui = pressed, // left and right Super
            (true, 0x2A) | (true, 0x36) => {} // fake shifts around PrtSc etc.
            _ if pressed => return self.press(e0, code),
            _ => {}
        }
        None
    }

    fn press(&mut self, e0: bool, code: u8) -> Option<Key> {
        if self.gui {
            use crate::input::SuperKey;
            let shift = self.shift();
            let key = match (e0, code) {
                (_, 0x1C) => SuperKey::Enter,
                (true, 0x4B) => SuperKey::Left,
                (true, 0x4D) => SuperKey::Right,
                (true, 0x48) => SuperKey::Up,
                (true, 0x50) => SuperKey::Down,
                // The top row by place (on AZERTY its unshifted keys are not digits).
                (false, 0x02..=0x0A) => SuperKey::Char((b'1' + code - 0x02) as char),
                (false, 0x0B) => SuperKey::Char('0'),
                (false, 0x39) => SuperKey::Char(' '),
                (false, _) => match self.layout.map(code, false, false)? {
                    Mapped::Char(c) if c.is_ascii_alphanumeric() => SuperKey::Char(c.to_ascii_lowercase()),
                    _ => return None,
                },
                _ => return None,
            };
            return Some(Key::Super(key, shift));
        }
        let nav = |code| match code {
            0x47 => Some(Key::Home),
            0x48 => Some(Key::Up),
            0x49 => Some(Key::PageUp),
            0x4B => Some(Key::Left),
            0x4D => Some(Key::Right),
            0x4F => Some(Key::End),
            0x50 => Some(Key::Down),
            0x51 => Some(Key::PageDown),
            0x52 => Some(Key::Insert),
            0x53 => Some(Key::Delete),
            _ => None,
        };
        if e0 && self.shift() && matches!(code, 0x49 | 0x51) {
            return Some(Key::ScrollBack(code == 0x49));
        }
        if e0 {
            return match code {
                0x1C => Some(Key::Enter),       // keypad Enter
                0x35 => self.char_key('/'),     // keypad /
                _ => nav(code),
            };
        }
        match code {
            0x01 => Some(Key::Escape),
            0x0E => Some(Key::Backspace),
            0x0F if self.alt => Some(Key::NextWindow),
            0x0F => Some(Key::Tab),
            0x1C => Some(Key::Enter),
            0x3A => {
                self.caps = !self.caps;
                self.update_leds();
                None
            }
            0x45 => {
                self.num = !self.num;
                self.update_leds();
                None
            }
            0x37 => self.char_key('*'),
            0x4A => self.char_key('-'),
            0x4E => self.char_key('+'),
            0x47..=0x53 => {
                if !self.num || self.shift() {
                    return nav(code);
                }
                let c = match code {
                    0x47 => '7', 0x48 => '8', 0x49 => '9',
                    0x4B => '4', 0x4C => '5', 0x4D => '6',
                    0x4F => '1', 0x50 => '2', 0x51 => '3',
                    0x52 => '0', 0x53 => self.layout.keypad_decimal(),
                    _ => return None,
                };
                self.char_key(c)
            }
            _ => {
                let altgr = self.altgr || (self.ctrl() && self.alt); // Ctrl+Alt acts as AltGr
                match self.layout.map(code, self.shift(), altgr)? {
                    Mapped::Dead(accent) => self.dead_key(accent),
                    Mapped::Char(mut c) => {
                        if self.caps && c.is_ascii_alphabetic() {
                            c = if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c.to_ascii_uppercase() };
                        }
                        if self.ctrl() && !altgr && c.is_ascii_alphabetic() {
                            self.dead = None;
                            return Some(Key::Ctrl(c.to_ascii_lowercase()));
                        }
                        self.char_key(c)
                    }
                }
            }
        }
    }

    fn shift(&self) -> bool {
        self.lshift || self.rshift
    }

    fn ctrl(&self) -> bool {
        self.lctrl || self.rctrl
    }

    fn dead_key(&mut self, accent: char) -> Option<Key> {
        // A second dead key releases the first one as a plain character.
        let prev = self.dead.replace(accent);
        prev.map(Key::Char)
    }

    /// Emit a character, combining it with a pending dead key.
    fn char_key(&mut self, c: char) -> Option<Key> {
        let Some(accent) = self.dead.take() else { return Some(Key::Char(c)) };
        if let Some(composed) = compose(accent, c) {
            return Some(Key::Char(composed));
        }
        if c == ' ' {
            return Some(Key::Char(accent)); // dead key + space = the accent itself
        }
        self.queued = Some(c); // no combination: accent, then the character
        Some(Key::Char(accent))
    }
}
