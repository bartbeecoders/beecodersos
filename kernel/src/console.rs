//! Kernel console: `print!` / `println!` to the VGA screen and the serial port
//! (with ANSI colors), plus helpers the line editor uses to redraw input.

use crate::ring::Global;
use crate::{serial, vga};
use alloc::format;
use core::fmt::{self, Write};

static SCREEN: Global<vga::Writer> = Global::new(vga::Writer::new());

pub fn init() {
    serial::init();
    SCREEN.get().clear();
    vga::enable_cursor();
}

pub fn clear() {
    SCREEN.get().clear();
    serial::write_str("\x1b[2J\x1b[H");
}

pub fn set_color(fg: vga::Color, bg: vga::Color) {
    SCREEN.get().set_color(fg, bg);
    serial::write_str(ansi(fg));
}

fn ansi(c: vga::Color) -> &'static str {
    use vga::Color::*;
    match c {
        LightGray => "\x1b[0m",
        Red | LightRed => "\x1b[91m",
        Green | LightGreen => "\x1b[92m",
        Yellow | Brown => "\x1b[93m",
        Blue | LightBlue => "\x1b[94m",
        Magenta | Pink => "\x1b[95m",
        Cyan | LightCyan => "\x1b[96m",
        White => "\x1b[97m",
        Black | DarkGray => "\x1b[90m",
    }
}

struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let screen = SCREEN.get();
        screen.write_str(s);
        screen.sync_cursor();
        for b in s.bytes() {
            if b == b'\n' {
                serial::write_byte(b'\r');
            }
            serial::write_byte(b);
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = Console.write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::console::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

// --- line editing support ------------------------------------------------------

/// Where an input line starts on screen.
pub struct Mark {
    row: usize,
    col: usize,
    scrolls: usize,
}

pub fn mark() -> Mark {
    let s = SCREEN.get();
    let (row, col) = s.pos();
    Mark { row, col, scrolls: s.scrolls() }
}

impl Mark {
    /// Linear screen index of the line start, following any scrolling since.
    fn start(&self, screen: &vga::Writer) -> usize {
        let shift = (screen.scrolls() - self.scrolls) * vga::WIDTH;
        (self.row * vga::WIDTH + self.col).saturating_sub(shift)
    }
}

/// Redraw an input line after an edit.
/// `old_len`/`old_cursor` describe what is currently displayed; `tail_edit`
/// means only the end of the line changed (a char typed or erased there).
pub fn redraw_line(mark: &Mark, text: &[char], old_len: usize, cursor: usize, old_cursor: usize, tail_edit: bool) {
    // VGA: rewrite from the line start, blank leftovers, place the cursor.
    let screen = SCREEN.get();
    let start = mark.start(screen);
    screen.set_pos(start / vga::WIDTH, start % vga::WIDTH);
    text.iter().for_each(|&c| screen.write_char(c));
    let end = screen.pos();
    for _ in text.len()..old_len {
        screen.write_char(' ');
    }
    screen.set_pos(end.0, end.1);
    vga::set_hw_cursor(mark.start(screen) + cursor);

    // Serial fast paths: typing or backspacing at the end of the line.
    let mut buf = [0u8; 4];
    if tail_edit && cursor == text.len() && old_cursor == old_len {
        if text.len() == old_len + 1 {
            serial::write_str(text[cursor - 1].encode_utf8(&mut buf));
            return;
        }
        if text.len() + 1 == old_len {
            serial::write_str("\x08 \x08");
            return;
        }
    }
    // General case: ANSI cursor movement relative to the old cursor position.
    if old_cursor > 0 {
        serial::write_str(&format!("\x1b[{old_cursor}D"));
    }
    text.iter().for_each(|c| serial::write_str(c.encode_utf8(&mut buf)));
    serial::write_str("\x1b[K");
    if text.len() > cursor {
        serial::write_str(&format!("\x1b[{}D", text.len() - cursor));
    }
}

/// Finish an input line: move past its end and start a new line.
pub fn end_line(text_len: usize, cursor: usize) {
    if text_len > cursor {
        serial::write_str(&format!("\x1b[{}C", text_len - cursor));
    }
    crate::println!();
}
