//! Kernel console: `print!` / `println!` to the screen and the serial port
//! (with ANSI colors), plus helpers the line editor uses to redraw input.
//! A thread with a console session (session.rs) prints into that instead.
//!
//! The screen is the VGA text screen (vga.rs) until `start_display` finds a
//! display driver; from then on it is the framebuffer console (fbcon.rs),
//! over the whole screen or inside the terminal window of the desktop (wm.rs).
//!
//! When the bootloader started a graphics mode there is no text screen: what
//! is printed before `start_display` only reaches the serial port, and is
//! shown on screen afterwards from a small buffer (`EARLY`).

use crate::display::{self, DisplayError, Target};
use crate::fbcon::{self, FbCon};
use crate::font::{GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::gfx::Canvas;
use crate::ring::Global;
use crate::bootfb::{self, Boot};
use crate::wm::MouseEffect;
use crate::{bootargs, serial, session, vga, wm};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
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

/// Where the pointer was when the desktop was last laid out or switched
/// off: a mouse that was in use keeps its pointer.
static POINTER: Global<Option<wm::Pointer>> = Global::new(None);

/// The terminal's window on the desktop (the first one opened).
const TERMINAL: usize = 0;

/// The first text printed while the screen is the text screen, to show it
/// again when the bootloader's graphics mode hid that screen.
static EARLY: Global<([u8; 1024], usize)> = Global::new(([0; 1024], 0));

/// The font size asked for: 1, 2, or 0 = choose by the screen (`display font`).
static FONT: Global<usize> = Global::new(0);

/// The font size for a screen: the one asked for, or double above full HD,
/// where 8x16 letters get too small to read.
pub fn font_scale(wanted: usize, screen_height: usize) -> usize {
    match wanted {
        1 | 2 => wanted,
        _ if screen_height >= 1440 => 2,
        _ => 1,
    }
}

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
        // The disk is not mounted yet: the saved choice is applied later
        // (`apply_desktop_setting`).
        lay_out(target, desktop_choice(bootargs::cmdline(), None));
        if let Screen::Graphics(screen) = SCREEN.get() {
            if bootfb::boot() == Boot::Text {
                for (i, cell) in saved.iter().enumerate() {
                    screen.put_cell(i / vga::WIDTH, i % vga::WIDTH, *cell);
                }
                screen.set_pos(row, col);
            } else {
                // The text screen was never visible: print the early lines again.
                let (text, len) = EARLY.get();
                let text = &text[..*len];
                let valid = core::str::from_utf8(text).map_or_else(|e| e.valid_up_to(), |s| s.len());
                screen.write_str(core::str::from_utf8(&text[..valid]).unwrap_or(""));
            }
            screen.set_attr(attr);
            screen.sync_cursor();
        }
    }
    let mut lines = String::new();
    for note in notes {
        lines += &format!("{note}\n       ");
    }
    // The boot log gets the first four facts; `display` shows all.
    let facts = describe_display();
    lines + &facts.lines().take(4).collect::<Vec<_>>().join(", ")
}

/// Driver, resolution and size in characters, one per line.
pub fn describe_display() -> String {
    let (cols, rows) = size();
    let Some((name, mode)) = display::current() else {
        return format!("driver: text (VGA text mode, no graphics)\ntext: {cols}x{rows} characters");
    };
    let scale = match SCREEN.get() {
        Screen::Graphics(screen) => screen.scale(),
        Screen::Text(_) => 1,
    };
    let font = format!("{}x{} font{}", GLYPH_WIDTH * scale, GLYPH_HEIGHT * scale, if *FONT.get() == 0 { ", automatic" } else { "" });
    let fast = match display::write_combining() {
        Ok(()) => String::from("write-combining"),
        Err(why) => format!("plain ({why})"),
    };
    format!(
        "driver: {name}\nmode: {}x{}, {} bits per pixel\ntext: {cols}x{rows} characters ({font})\ndesktop: {}\nvideo memory: {fast}",
        mode.width,
        mode.height,
        mode.format.bytes() * 8,
        if desktop_on() { "on" } else { "off" }
    )
}

/// The text on the machine's own screen, line by line (trailing spaces
/// left out): for a remote look at it (hivectl.rs).
pub fn screen_text() -> String {
    let (cols, rows) = size();
    let mut text = String::new();
    for row in 0..rows {
        let mut line = String::new();
        for col in 0..cols {
            let cell = match SCREEN.get() {
                Screen::Text(screen) => screen.cell(row, col),
                Screen::Graphics(screen) => screen.cells().get(row * cols + col).copied().unwrap_or(0x20),
            };
            line.push(vga::from_cp437(cell as u8));
        }
        text += line.trim_end();
        text.push('\n');
    }
    text
}

/// A picture of the screen: every `scale`-th pixel, as (width, height, RGB
/// bytes). `None` on the text screen.
pub fn capture(scale: usize) -> Option<(usize, usize, Vec<u8>)> {
    let Screen::Graphics(screen) = SCREEN.get() else { return None };
    let canvas = screen.canvas();
    let scale = scale.max(1);
    let (width, height) = (canvas.width() / scale, canvas.height() / scale);
    let mut pixels = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for x in 0..width {
            let rgb = canvas.pixel(x * scale, y * scale);
            pixels.extend_from_slice(&[(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8]);
        }
    }
    Some((width, height, pixels))
}

/// Characters per line, and lines, of the machine's own screen.
pub fn size() -> (usize, usize) {
    let screen = SCREEN.get();
    (screen.width(), screen.height())
}

/// Should the desktop be on? A boot flag decides first (`desktop=off`,
/// `desktop=on` or just `desktop`), then what was saved (`/system/desktop`:
/// "on" or "off"); without either it is on.
pub fn desktop_choice(cmdline: &str, saved: Option<&str>) -> bool {
    match bootargs::value_in(cmdline, "desktop") {
        Some("off") => return false,
        Some(_) => return true,
        None if cmdline.split_whitespace().any(|w| w == "desktop") => return true,
        None => {}
    }
    saved.map(str::trim) != Some("off")
}

/// After the disk is mounted: follow the saved choice (the screen was laid
/// out before the disk could be read). `saved` is the content of the
/// settings file, if there is one. Nothing happens on the text screen.
pub fn apply_desktop_setting(saved: Option<&str>) {
    let wanted = desktop_choice(bootargs::cmdline(), saved);
    if wanted != desktop_on() {
        let _ = set_desktop(wanted);
    }
}

/// Put a console on the framebuffer: over the whole screen, or with
/// `desktop` in the terminal window of a new desktop. The text of the
/// console before it moves along (its last lines, if the new one is smaller).
/// Returns whether the desktop is on now (a screen too small for a window
/// gets none).
fn lay_out(target: Target, desktop: bool) -> bool {
    let old = match SCREEN.get() {
        Screen::Graphics(s) if s.width() > 0 => Some((Vec::from(s.cells()), s.width(), s.pos(), s.attr())),
        _ => None,
    };
    if let Some(pointer) = DESKTOP.get().as_ref().and_then(|desk| desk.pointer()) {
        *POINTER.get() = Some(pointer);
    }
    let canvas = Canvas::new(target.framebuffer, target.mode);
    let scale = font_scale(*FONT.get(), canvas.height());
    let frame = if desktop { wm::terminal_frame(canvas.width(), canvas.height(), scale) } else { None };
    let mut screen = match frame {
        Some(frame) => {
            let mut desk = wm::Desktop::new();
            desk.open("Terminal", frame);
            desk.set_pointer(*POINTER.get(), canvas.bounds());
            desk.draw(&canvas);
            let area = desk.window(TERMINAL).map_or(frame, |w| w.content());
            *DESKTOP.get() = Some(desk);
            FbCon::in_area(canvas, area, CELLS.get(), scale)
        }
        None => {
            *DESKTOP.get() = None;
            FbCon::in_area(canvas, canvas.bounds(), CELLS.get(), scale)
        }
    };
    screen.clear();
    if let Some((cells, cols, (row, col), attr)) = old {
        let skip = (row + 1).saturating_sub(screen.height());
        for r in skip..=row {
            for c in 0..cols {
                screen.put_cell(r - skip, c, cells[r * cols + c]);
            }
        }
        screen.set_pos(row.saturating_sub(skip), col);
        screen.set_attr(attr);
        screen.sync_cursor();
    }
    *SCREEN.get() = Screen::Graphics(screen);
    pointer_on_top();
    frame.is_some()
}

/// Change the resolution (`display mode`). The desktop stays on if the new
/// screen has room for it.
pub fn set_display_mode(width: u32, height: u32) -> Result<(), DisplayError> {
    let target = display::set_mode(width, height)?;
    lay_out(target, desktop_on());
    Ok(())
}

/// Choose the font size (`display font`): 1, 2, or 0 for automatic.
pub fn set_font(size: usize) {
    *FONT.get() = size;
    if let Some(target) = display::target() {
        lay_out(target, desktop_on());
    }
}

/// The mouse moved or a button changed (from the usb thread, once per
/// timer tick): buttons, then how far right and down. Only the desktop has
/// a pointer; a full-screen terminal ignores the mouse.
pub fn mouse_event(buttons: u8, dx: i32, dy: i32) {
    let (Some(desk), Screen::Graphics(screen)) = (DESKTOP.get().as_mut(), SCREEN.get()) else { return };
    let canvas = screen.canvas();
    match desk.mouse(buttons, dx, dy, canvas.bounds()) {
        MouseEffect::Nothing => {}
        MouseEffect::PointerMoved(old) => {
            // Repair what the pointer covered, then draw it in its new place.
            desk.draw(&canvas.clipped(old));
            screen.redraw_in(old);
            desk.draw_pointer(&canvas);
        }
        MouseEffect::WindowMoved => {
            if let Some(window) = desk.window(TERMINAL) {
                let area = window.content();
                screen.move_to(area.x, area.y);
            }
            desk.draw(&canvas);
            screen.redraw();
            desk.draw_pointer(&canvas);
        }
    }
}

/// Where the pointer is on the desktop: for `desktop` and the tests.
pub fn pointer() -> Option<(usize, usize)> {
    DESKTOP.get().as_ref()?.pointer().map(|p| (p.x, p.y))
}

/// Text was drawn and may have covered the pointer: put it back on top.
fn pointer_on_top() {
    if let (Some(desk), Screen::Graphics(screen)) = (DESKTOP.get().as_ref(), SCREEN.get()) {
        desk.draw_pointer(&screen.canvas());
    }
}

pub fn desktop_on() -> bool {
    DESKTOP.get().is_some()
}

/// Switch the desktop on or off (`desktop on` / `desktop off`).
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
    pointer_on_top();
    crate::time::micros() - start
}

pub fn clear() {
    if session::current().is_some() {
        return;
    }
    SCREEN.get().clear();
    pointer_on_top();
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
        pointer_on_top();
        if let Screen::Text(_) = screen {
            let (early, len) = EARLY.get();
            let n = s.len().min(early.len() - *len);
            early[*len..*len + n].copy_from_slice(&s.as_bytes()[..n]);
            *len += n;
        }
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
    pointer_on_top();

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
