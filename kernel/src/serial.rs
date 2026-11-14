//! 16550 UART driver on COM1 (0x3F8): output, plus interrupt-driven input so
//! the terminal can be used from `qemu -serial stdio` / `-nographic`.

use crate::input::Key;
use crate::port::{inb, outb};
use crate::ring::{Global, Ring};
use alloc::vec::Vec;

const COM1: u16 = 0x3F8;

static RX: Ring<256> = Ring::new();
static DECODER: Global<SerialDecoder> = Global::new(SerialDecoder::new());

pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00); // disable interrupts
        outb(COM1 + 3, 0x80); // enable DLAB
        outb(COM1, 0x01);     // divisor low  (115200 baud)
        outb(COM1 + 1, 0x00); // divisor high
        outb(COM1 + 3, 0x03); // 8N1, DLAB off
        outb(COM1 + 2, 0xC7); // enable + clear FIFOs
        outb(COM1 + 4, 0x0B); // DTR, RTS, OUT2 (OUT2 gates the IRQ line)
    }
}

/// Raise IRQ4 when a byte is received.
pub fn enable_rx_interrupt() {
    unsafe {
        while inb(COM1 + 5) & 0x01 != 0 {
            inb(COM1); // drop stale input
        }
        outb(COM1 + 1, 0x01);
    }
}

pub fn write_byte(b: u8) {
    unsafe {
        while inb(COM1 + 5) & 0x20 == 0 {}
        outb(COM1, b);
    }
}

pub fn write_str(s: &str) {
    s.bytes().for_each(write_byte);
}

/// Called from the IRQ4 handler.
pub fn on_interrupt() {
    unsafe {
        while inb(COM1 + 5) & 0x01 != 0 {
            RX.push(inb(COM1));
        }
    }
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

struct SerialDecoder {
    esc: [u8; 8],
    esc_len: usize,
    utf8: [u8; 4],
    utf8_len: usize,
    last_cr: bool,
}

impl SerialDecoder {
    const fn new() -> Self {
        SerialDecoder { esc: [0; 8], esc_len: 0, utf8: [0; 4], utf8_len: 0, last_cr: false }
    }

    fn feed(&mut self, b: u8) -> Option<Key> {
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
