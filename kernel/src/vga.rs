//! VGA text mode (80x25) at physical 0xB8000, with a hardware cursor.
//! Text is UTF-8; characters are translated to the VGA font (code page 437).

use crate::port::outb;

const BUFFER: *mut u16 = 0xB8000 as *mut u16;
pub const WIDTH: usize = 80;
pub const HEIGHT: usize = 25;

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0, Blue = 1, Green = 2, Cyan = 3, Red = 4, Magenta = 5, Brown = 6,
    LightGray = 7, DarkGray = 8, LightBlue = 9, LightGreen = 10, LightCyan = 11,
    LightRed = 12, Pink = 13, Yellow = 14, White = 15,
}

pub struct Writer {
    col: usize,
    row: usize,
    attr: u8,
    /// Number of times the screen scrolled (lets callers track moving text).
    scrolls: usize,
}

impl Writer {
    pub const fn new() -> Self {
        Writer { col: 0, row: 0, attr: 0x07, scrolls: 0 }
    }

    pub fn set_color(&mut self, fg: Color, bg: Color) {
        self.attr = (bg as u8) << 4 | fg as u8;
    }

    pub fn clear(&mut self) {
        for i in 0..WIDTH * HEIGHT {
            self.put(i, b' ');
        }
        self.col = 0;
        self.row = 0;
        self.sync_cursor();
    }

    pub fn pos(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn set_pos(&mut self, row: usize, col: usize) {
        self.row = row.min(HEIGHT - 1);
        self.col = col.min(WIDTH);
    }

    pub fn scrolls(&self) -> usize {
        self.scrolls
    }

    pub fn attr(&self) -> u8 {
        self.attr
    }

    /// What is on screen at a position: color in the high byte, character in the low.
    pub fn cell(&self, row: usize, col: usize) -> u16 {
        unsafe { BUFFER.add(row.min(HEIGHT - 1) * WIDTH + col.min(WIDTH - 1)).read_volatile() }
    }

    pub fn write_char(&mut self, c: char) {
        match c {
            '\n' => self.new_line(),
            '\r' => self.col = 0,
            '\t' => {
                for _ in 0..4 - self.col % 4 {
                    self.write_char(' ');
                }
            }
            c => {
                if self.col >= WIDTH {
                    self.new_line();
                }
                self.put(self.row * WIDTH + self.col, cp437(c));
                self.col += 1;
            }
        }
    }

    pub fn write_str(&mut self, s: &str) {
        s.chars().for_each(|c| self.write_char(c));
    }

    /// Move the blinking hardware cursor to the current write position.
    pub fn sync_cursor(&self) {
        set_hw_cursor(self.row * WIDTH + self.col.min(WIDTH - 1));
    }

    fn put(&self, index: usize, byte: u8) {
        let cell = (self.attr as u16) << 8 | byte as u16;
        unsafe { BUFFER.add(index).write_volatile(cell) };
    }

    fn new_line(&mut self) {
        self.col = 0;
        if self.row + 1 < HEIGHT {
            self.row += 1;
            return;
        }
        for i in 0..WIDTH * (HEIGHT - 1) {
            unsafe { BUFFER.add(i).write_volatile(BUFFER.add(i + WIDTH).read_volatile()) };
        }
        for i in WIDTH * (HEIGHT - 1)..WIDTH * HEIGHT {
            self.put(i, b' ');
        }
        self.scrolls += 1;
    }
}

/// Show the hardware cursor as an underline (scanlines 13-14).
pub fn enable_cursor() {
    unsafe {
        outb(0x3D4, 0x0A);
        outb(0x3D5, 13);
        outb(0x3D4, 0x0B);
        outb(0x3D5, 14);
    }
}

pub fn set_hw_cursor(index: usize) {
    let index = index.min(WIDTH * HEIGHT - 1);
    unsafe {
        outb(0x3D4, 0x0F);
        outb(0x3D5, index as u8);
        outb(0x3D4, 0x0E);
        outb(0x3D5, (index >> 8) as u8);
    }
}

/// Translate a Unicode character to its code page 437 glyph (■ if missing).
pub fn cp437(c: char) -> u8 {
    if (' '..='~').contains(&c) {
        return c as u8;
    }
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»";
    if let Some(i) = HIGH.chars().position(|h| h == c) {
        return 0x80 + i as u8;
    }
    match c {
        '§' => 0x15,
        '¶' => 0x14,
        'ß' => 0xE1,
        'µ' => 0xE6,
        '±' => 0xF1,
        '°' => 0xF8,
        '·' => 0xFA,
        '²' => 0xFD,
        _ => 0xFE, // ■
    }
}
