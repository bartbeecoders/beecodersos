//! Framebuffer console: a grid of characters drawn as pixels with the
//! built-in font (font.rs). It offers what the VGA text screen (vga.rs)
//! offers, for any resolution a display driver (display.rs) sets.
//!
//! Every cell is remembered in RAM in the VGA format (color in the high
//! byte, code page 437 character in the low byte) and only cells that change
//! are drawn. Nothing is ever read from the framebuffer: reading video
//! memory is very slow on real hardware. Drawing needs no heap and never
//! waits, so it also works while the kernel panics.
//!
//! The console fills the whole screen, or one area of it: the inside of the
//! terminal window on the desktop (wm.rs).

use crate::display::Mode;
use crate::font::{GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::gfx::{Canvas, Rect};
use crate::vga::{self, Color};

/// Most cells the screen keeps (320x100: enough for 2560x1600). A larger
/// screen shows fewer lines than would fit.
pub const MAX_CELLS: usize = 320 * 100;

/// The 16 VGA colors as 0xRRGGBB, in the order of `vga::Color`.
pub const PALETTE: [u32; 16] = [
    0x000000, 0x0000AA, 0x00AA00, 0x00AAAA, 0xAA0000, 0xAA00AA, 0xAA5500, 0xAAAAAA,
    0x555555, 0x5555FF, 0x55FF55, 0x55FFFF, 0xFF5555, 0xFF55FF, 0xFFFF55, 0xFFFFFF,
];

/// The colors of `display test`, left to right. It covers the whole screen,
/// not only the console's area.
pub const BARS: [Color; 8] = [
    Color::White, Color::Yellow, Color::LightCyan, Color::LightGreen,
    Color::Pink, Color::LightRed, Color::LightBlue, Color::Black,
];

/// Rows of a cell the cursor (an underline) covers.
const CURSOR_ROWS: core::ops::Range<usize> = 13..15;

pub struct FbCon<'a> {
    canvas: Canvas,
    /// The part of the canvas the console draws in.
    area: Rect,
    cells: &'a mut [u16],
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    attr: u8,
    scrolls: usize,
    /// Cell that shows the cursor.
    cursor: Option<usize>,
}

impl<'a> FbCon<'a> {
    /// A console over the whole screen. `framebuffer` must be valid for
    /// `mode.bytes()` bytes. A mode whose lines cannot hold their pixels
    /// gives a screen of 0x0 characters: nothing is drawn.
    pub fn new(framebuffer: *mut u8, mode: Mode, cells: &'a mut [u16]) -> Self {
        let canvas = Canvas::new(framebuffer, mode);
        Self::in_area(canvas, canvas.bounds(), cells)
    }

    /// A console in one area of the canvas (what lies outside the canvas is cut off).
    pub fn in_area(canvas: Canvas, area: Rect, cells: &'a mut [u16]) -> Self {
        let area = area.clip(&canvas.bounds());
        let cols = area.width / GLYPH_WIDTH;
        let mut rows = area.height / GLYPH_HEIGHT;
        if cols == 0 || rows == 0 {
            rows = 0;
        } else {
            rows = rows.min(cells.len() / cols);
        }
        let cols = if rows == 0 { 0 } else { cols };
        FbCon { canvas, area, cells, cols, rows, col: 0, row: 0, attr: 0x07, scrolls: 0, cursor: None }
    }

    pub fn canvas(&self) -> Canvas {
        self.canvas
    }

    pub fn width(&self) -> usize {
        self.cols
    }

    pub fn height(&self) -> usize {
        self.rows
    }

    pub fn set_color(&mut self, fg: Color, bg: Color) {
        self.attr = (bg as u8) << 4 | fg as u8;
    }

    pub fn attr(&self) -> u8 {
        self.attr
    }

    /// Every cell, line after line (`width()` cells per line).
    pub fn cells(&self) -> &[u16] {
        &self.cells[..self.cols * self.rows]
    }

    pub fn set_attr(&mut self, attr: u8) {
        self.attr = attr;
    }

    fn blank(&self) -> u16 {
        (self.attr as u16) << 8 | b' ' as u16
    }

    /// Fill the whole area (also the edges no character reaches) with the
    /// background color.
    pub fn clear(&mut self) {
        if self.cols > 0 {
            self.canvas.fill_rect(self.area, PALETTE[(self.attr >> 4) as usize]);
        }
        let blank = self.blank();
        self.cells[..self.cols * self.rows].fill(blank);
        self.col = 0;
        self.row = 0;
        self.cursor = None;
        self.sync_cursor();
    }

    pub fn pos(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn set_pos(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows.saturating_sub(1));
        self.col = col.min(self.cols);
    }

    pub fn scrolls(&self) -> usize {
        self.scrolls
    }

    pub fn write_char(&mut self, c: char) {
        if self.cols == 0 {
            return;
        }
        match c {
            '\n' => self.new_line(),
            '\r' => self.col = 0,
            '\t' => {
                for _ in 0..4 - self.col % 4 {
                    self.write_char(' ');
                }
            }
            c => {
                if self.col >= self.cols {
                    self.new_line();
                }
                let cell = (self.attr as u16) << 8 | vga::cp437(c) as u16;
                self.set_cell(self.row * self.cols + self.col, cell);
                self.col += 1;
            }
        }
    }

    pub fn write_str(&mut self, s: &str) {
        s.chars().for_each(|c| self.write_char(c));
    }

    /// Put a ready-made cell (VGA format) on the screen; outside it: ignored.
    pub fn put_cell(&mut self, row: usize, col: usize, cell: u16) {
        if row < self.rows && col < self.cols {
            self.set_cell(row * self.cols + col, cell);
        }
    }

    /// Show the cursor at the write position.
    pub fn sync_cursor(&mut self) {
        self.set_cursor(self.row * self.cols + self.col.min(self.cols.saturating_sub(1)));
    }

    /// Show the cursor at a cell (counted from the top left).
    pub fn set_cursor(&mut self, index: usize) {
        if self.cols == 0 {
            return;
        }
        if let Some(old) = self.cursor {
            self.draw_cell(old);
        }
        let index = index.min(self.cols * self.rows - 1);
        self.cursor = Some(index);
        let color = PALETTE[(self.cells[index] >> 8) as usize & 0x0F];
        let (x, y) = self.origin(index);
        self.canvas.fill_rect(Rect::new(x, y + CURSOR_ROWS.start, GLYPH_WIDTH, CURSOR_ROWS.len()), color);
    }

    /// Draw every cell again (after something else drew on the screen).
    pub fn redraw(&mut self) {
        if self.cols == 0 {
            return;
        }
        self.canvas.fill_rect(self.area, PALETTE[(self.attr >> 4) as usize]);
        for index in 0..self.cols * self.rows {
            self.draw_cell(index);
        }
        if let Some(cursor) = self.cursor {
            self.set_cursor(cursor);
        }
    }

    /// Eight vertical color bars over the whole screen (`display test`).
    /// The cells are not changed: `redraw` brings the text back.
    pub fn color_bars(&mut self) {
        if self.cols == 0 {
            return;
        }
        let (width, height) = (self.canvas.width(), self.canvas.height());
        for (i, color) in BARS.iter().enumerate() {
            let (from, to) = (width * i / BARS.len(), width * (i + 1) / BARS.len());
            self.canvas.fill_rect(Rect::new(from, 0, to - from, height), PALETTE[*color as usize]);
        }
    }

    fn new_line(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
            return;
        }
        let (cols, last) = (self.cols, self.cols * (self.rows - 1));
        for i in 0..last {
            self.set_cell(i, self.cells[i + cols]);
        }
        let blank = self.blank();
        for i in last..last + cols {
            self.set_cell(i, blank);
        }
        self.scrolls += 1;
    }

    /// Change a cell; draw it only when it really changed.
    fn set_cell(&mut self, index: usize, cell: u16) {
        if self.cells[index] != cell {
            self.cells[index] = cell;
            self.draw_cell(index);
        }
    }

    /// Top left pixel of a cell.
    fn origin(&self, index: usize) -> (usize, usize) {
        (self.area.x + index % self.cols * GLYPH_WIDTH, self.area.y + index / self.cols * GLYPH_HEIGHT)
    }

    fn draw_cell(&mut self, index: usize) {
        let cell = self.cells[index];
        let (fg, bg) = (PALETTE[(cell >> 8) as usize & 0x0F], PALETTE[(cell >> 12) as usize]);
        let (x, y) = self.origin(index);
        self.canvas.glyph(x, y, cell as u8, fg, bg);
    }
}
