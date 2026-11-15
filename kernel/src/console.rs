//! Kernel console: `print!` / `println!` to the screen and the serial port
//! (with ANSI colors), plus helpers the line editor uses to redraw input.
//! A thread with a console session (session.rs) prints into that instead.
//!
//! The screen is the VGA text screen (vga.rs) until `start_display` finds a
//! display driver; from then on it is the framebuffer console (fbcon.rs),
//! over the whole screen or inside the terminal window of the desktop (wm.rs).

use crate::display::{self, DisplayError, Target};
use crate::fbcon::{self, FbCon};
use crate::font::{GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::gfx::Canvas;
use crate::ring::Global;
use crate::{bootargs, serial, session, vga, wm};
use alloc::format;
use alloc::string::String;
use core::fmt::{self, Write};

/// What characters are drawn on. Both kinds offer the same operations.
enum Screen {
    Text(vga::Writer),
    Graphics(FbCon<'static>),
}

/// Call the same method on whichever screen is in use.
macro_rules! on_screen {
    ($screen:expr, $s:ident => $call:expr) => {
        match $screen {
            Screen::Text($s) => $call,
            Screen::Graphics($s) => $call,
        }
    };
}

impl Screen {
    fn width(&self) -> usize {
        match self {
            Screen::Text(_) => vga::WIDTH,
            Screen::Graphics(s) => s.width(),
        }
    }

    fn height(&self) -> usize {
        match self {
            Screen::Text(_) => vga::HEIGHT,
            Screen::Graphics(s) => s.height(),
        }
    }

    fn clear(&mut self) {
        on_screen!(self, s => s.clear())
    }

    fn set_color(&mut self, fg: vga::Color, bg: vga::Color) {
        on_screen!(self, s => s.set_color(fg, bg))
    }

    fn pos(&self) -> (usize, usize) {
        on_screen!(self, s => s.pos())
    }

    fn set_pos(&mut self, row: usize, col: usize) {
        on_screen!(self, s => s.set_pos(row, col))
    }

    fn scrolls(&self) -> usize {
        on_screen!(self, s => s.scrolls())
    }

    fn write_char(&mut self, c: char) {
        on_screen!(self, s => s.write_char(c))
    }

    fn write_str(&mut self, text: &str) {
        on_screen!(self, s => s.write_str(text))
    }

    /// Show the cursor at the write position.
    fn sync_cursor(&mut self) {
        on_screen!(self, s => s.sync_cursor())
    }

    /// Show the cursor at a cell, counted from the top left.
    fn set_cursor(&mut self, index: usize) {
        match self {
            Screen::Text(_) => vga::set_hw_cursor(index),
            Screen::Graphics(s) => s.set_cursor(index),
        }
    }
}

static SCREEN: Global<Screen> = Global::new(Screen::Text(vga::Writer::new()));

/// The framebuffer console's cells: static, so drawing never needs the heap.
static CELLS: Global<[u16; fbcon::MAX_CELLS]> = Global::new([0; fbcon::MAX_CELLS]);

/// The desktop, when it is on: the console is then its terminal window.
static DESKTOP: Global<Option<wm::Desktop>> = Global::new(None);

pub fn init() {
    serial::init();
    SCREEN.get().clear();
    vga::enable_cursor();
}

/// Look for a display driver and move the console to its framebuffer; what
/// the text screen showed so far is copied over. Needs the heap. Returns
/// lines for the boot log: what the screen is now, and anything that went
/// wrong on the way.
pub fn start_display() -> String {
    // Remember the text screen first: setting a graphics mode wipes it.
    let Screen::Text(text) = SCREEN.get() else { return describe_display() };
    let mut saved = [0u16; vga::WIDTH * vga::HEIGHT];
    for (i, cell) in saved.iter_mut().enumerate() {
        *cell = text.cell(i / vga::WIDTH, i % vga::WIDTH);
    }
    let ((row, col), attr) = (text.pos(), text.attr());

    let (target, notes) = display::init();
    if let Some(target) = target {
        lay_out(target, bootargs::has("desktop"));
        if let Screen::Graphics(screen) = SCREEN.get() {
            for (i, cell) in saved.iter().enumerate() {
                screen.put_cell(i / vga::WIDTH, i % vga::WIDTH, *cell);
            }
            screen.set_pos(row, col);
            screen.set_attr(attr);
            screen.sync_cursor();
        }
    }
    let mut lines = String::new();
    for note in notes {
        lines += &format!("{note}\n       ");
    }
    lines + &describe_display().replace('\n', ", ")
}

/// Driver, resolution and size in characters, one per line.
pub fn describe_display() -> String {
    let (cols, rows) = size();
    match display::current() {
        Some((name, mode)) => format!(
            "driver: {name}\nmode: {}x{}, {} bits per pixel\ntext: {cols}x{rows} characters ({GLYPH_WIDTH}x{GLYPH_HEIGHT} font)\ndesktop: {}",
            mode.width,
            mode.height,
            mode.format.bytes() * 8,
            if desktop_on() { "on" } else { "off" }
        ),
        None => format!("driver: text (VGA text mode, no graphics)\ntext: {cols}x{rows} characters"),
    }
}

/// Characters per line, and lines, of the machine's own screen.
pub fn size() -> (usize, usize) {
    let screen = SCREEN.get();
    (screen.width(), screen.height())
}

/// Put an empty console on the framebuffer: over the whole screen, or with
/// `desktop` in the terminal window of a new desktop. Returns whether the
/// desktop is on now (a screen too small for a window gets none).
fn lay_out(target: Target, desktop: bool) -> bool {
    let canvas = Canvas::new(target.framebuffer, target.mode);
    let frame = if desktop { wm::terminal_frame(canvas.width(), canvas.height()) } else { None };
    let mut screen = match frame {
        Some(frame) => {
            let mut desk = wm::Desktop::new();
            let id = desk.open("Terminal", frame);
            desk.draw(&canvas);
            let area = desk.window(id).map_or(frame, |w| w.content());
            *DESKTOP.get() = Some(desk);
            FbCon::in_area(canvas, area, CELLS.get())
        }
        None => {
            *DESKTOP.get() = None;
            FbCon::in_area(canvas, canvas.bounds(), CELLS.get())
        }
    };
    screen.clear();
    *SCREEN.get() = Screen::Graphics(screen);
    frame.is_some()
}

/// Change the resolution (`display mode`). The screen starts empty again;
/// the desktop stays on if the new screen has room for it.
pub fn set_display_mode(width: u32, height: u32) -> Result<(), DisplayError> {
    let target = display::set_mode(width, height)?;
    lay_out(target, desktop_on());
    Ok(())
}

pub fn desktop_on() -> bool {
    DESKTOP.get().is_some()
}

/// Switch the desktop on or off (`desktop on` / `desktop off`). The
/// terminal starts empty.
pub fn set_desktop(on: bool) -> Result<(), &'static str> {
    let target = display::target().ok_or("the desktop needs a graphics mode, and this is the text screen (see display)")?;
    if lay_out(target, on) == on {
        Ok(())
    } else {
        Err("the screen is too small for a window")
    }
}

/// Color bars over the whole screen (`display test`); nothing on the text screen.
pub fn show_color_bars() {
    if let Screen::Graphics(screen) = SCREEN.get() {
        screen.color_bars();
    }
}

/// Draw the whole screen again; returns how long it took in microseconds.
pub fn redraw() -> u64 {
    let start = crate::time::micros();
    if let Screen::Graphics(screen) = SCREEN.get() {
        if let Some(desk) = DESKTOP.get() {
            desk.draw(&screen.canvas());
        }
        screen.redraw();
    }
    crate::time::micros() - start
}

pub fn clear() {
    if session::current().is_some() {
        return;
    }
    SCREEN.get().clear();
    serial::write_str("\x1b[2J\x1b[H");
}

pub fn set_color(fg: vga::Color, bg: vga::Color) {
    if session::current().is_some() {
        return; // sessions get plain text
    }
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
        if let Some(id) = session::current() {
            session::write(id, s);
            return Ok(());
        }
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
    fn start(&self, screen: &Screen) -> usize {
        let shift = (screen.scrolls() - self.scrolls) * screen.width();
        (self.row * screen.width() + self.col).saturating_sub(shift)
    }
}

/// Redraw an input line after an edit.
/// `old_len`/`old_cursor` describe what is currently displayed; `tail_edit`
/// means only the end of the line changed (a char typed or erased there).
pub fn redraw_line(mark: &Mark, text: &[char], old_len: usize, cursor: usize, old_cursor: usize, tail_edit: bool) {
    // Screen: rewrite from the line start, blank leftovers, place the cursor.
    let screen = SCREEN.get();
    let start = mark.start(screen);
    let width = screen.width().max(1);
    screen.set_pos(start / width, start % width);
    text.iter().for_each(|&c| screen.write_char(c));
    let end = screen.pos();
    for _ in text.len()..old_len {
        screen.write_char(' ');
    }
    screen.set_pos(end.0, end.1);
    screen.set_cursor(mark.start(screen) + cursor);

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
