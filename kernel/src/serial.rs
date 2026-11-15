//! The serial console on COM1 (0x3F8): output, plus interrupt-driven input so
//! the terminal can be used from `qemu -serial stdio` / `-nographic`. The
//! port itself is in uart.rs; a machine without COM1 works without it.

use crate::input::Key;
use crate::ring::{Global, Ring};
use crate::uart::{Hardware, Uart, COM1};
use alloc::vec::Vec;

static PORT: Global<Uart> = Global::new(Uart::new(COM1));
static RX: Ring<256> = Ring::new();
static DECODER: Global<SerialDecoder> = Global::new(SerialDecoder::new());

/// Look for COM1 and set it up. Without it the OS simply has no serial console.
pub fn init() -> bool {
    PORT.get().init(&mut Hardware)
}

pub fn present() -> bool {
    PORT.get().present()
}

/// Raise IRQ4 when a byte is received.
pub fn enable_rx_interrupt() {
    PORT.get().enable_rx_interrupt(&mut Hardware);
}

/// Send a byte; dropped when there is no port or it is stuck (see uart.rs).
pub fn write_byte(b: u8) {
    PORT.get().send(&mut Hardware, b);
}

pub fn write_str(s: &str) {
    s.bytes().for_each(write_byte);
}

/// Called from the IRQ4 handler.
pub fn on_interrupt() {
    PORT.get().receive(&mut Hardware, |b| {
        RX.push(b);
    });
}

pub fn pending() -> bool {
    !RX.is_empty()
}

/// Decode received bytes (UTF-8 text, control keys, ANSI escape sequences).
pub fn poll_key() -> Option<Key> {
    let d = DECODER.get();
    while let Some(b) = RX.pop() {
        if let Some(k) = d.feed(b) {
            return Some(k);
        }
    }
    None
}

/// Decode bytes with a fresh decoder (for the self-tests).
pub(crate) fn decode_for_test(bytes: &[u8]) -> Vec<Key> {
    let mut d = SerialDecoder::new();
    bytes.iter().filter_map(|&b| d.feed(b)).collect()
}

/// Bytes to keys; also used for the remote shell's input (remote.rs).
pub(crate) struct SerialDecoder {
    esc: [u8; 8],
    esc_len: usize,
    utf8: [u8; 4],
    utf8_len: usize,
    last_cr: bool,
}

impl SerialDecoder {
    pub(crate) const fn new() -> Self {
        SerialDecoder { esc: [0; 8], esc_len: 0, utf8: [0; 4], utf8_len: 0, last_cr: false }
    }

    pub(crate) fn feed(&mut self, b: u8) -> Option<Key> {
        let after_cr = core::mem::take(&mut self.last_cr);
        if self.esc_len > 0 {
            return self.feed_escape(b);
        }
        if self.utf8_len > 0 || b >= 0x80 {
            return self.feed_utf8(b);
        }
        match b {
            0x1B => {
                self.esc[0] = b;
                self.esc_len = 1;
                None
            }
            b'\r' => {
                self.last_cr = true;
                Some(Key::Enter)
            }
            b'\n' if after_cr => None, // CR LF counts as one Enter
            b'\n' => Some(Key::Enter),
            0x7F | 0x08 => Some(Key::Backspace),
            b'\t' => Some(Key::Tab),
            0x01..=0x1A => Some(Key::Ctrl((b'a' + b - 1) as char)),
            0x20..=0x7E => Some(Key::Char(b as char)),
            _ => None,
        }
    }

    fn feed_escape(&mut self, b: u8) -> Option<Key> {
        self.esc[self.esc_len] = b;
        self.esc_len += 1;
        let seq = &self.esc[1..self.esc_len];
        let key = match seq {
            [b'['] | [b'O'] => return None,
            [b'[', b'0'..=b'9'] | [b'[', b'0'..=b'9', b'0'..=b'9'] if self.esc_len < self.esc.len() => return None,
            [b'[' | b'O', b'A'] => Some(Key::Up),
            [b'[' | b'O', b'B'] => Some(Key::Down),
            [b'[' | b'O', b'C'] => Some(Key::Right),
            [b'[' | b'O', b'D'] => Some(Key::Left),
            [b'[' | b'O', b'H'] => Some(Key::Home),
            [b'[' | b'O', b'F'] => Some(Key::End),
            [b'[', b'1' | b'7', b'~'] => Some(Key::Home),
            [b'[', b'4' | b'8', b'~'] => Some(Key::End),
            [b'[', b'2', b'~'] => Some(Key::Insert),
            [b'[', b'3', b'~'] => Some(Key::Delete),
            [b'[', b'5', b'~'] => Some(Key::PageUp),
            [b'[', b'6', b'~'] => Some(Key::PageDown),
            [0x1B] => Some(Key::Escape),
            _ => None, // unknown sequence: ignore it
        };
        self.esc_len = 0;
        key
    }

    fn feed_utf8(&mut self, b: u8) -> Option<Key> {
        self.utf8[self.utf8_len] = b;
        self.utf8_len += 1;
        let need = match self.utf8[0] {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => {
                self.utf8_len = 0; // invalid lead byte
                return None;
            }
        };
        if self.utf8_len < need {
            return None;
        }
        self.utf8_len = 0;
        core::str::from_utf8(&self.utf8[..need]).ok()?.chars().next().map(Key::Char)
    }
}
