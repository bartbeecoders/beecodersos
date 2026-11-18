//! Kernel console: `print!` / `println!` to the screen and the serial port
//! (with ANSI colors), plus helpers the line editor uses to redraw input.
//! A thread with a console session (session.rs) prints into that instead.
//!
//! The screen is the VGA text screen (vga.rs) until `start_display` finds a
//! display driver; from then on it is the framebuffer console (fbcon.rs),
//! over the whole screen or inside the terminal window of the desktop (wm.rs).
//!
//! Drawing goes to a copy of the screen in memory (`Shadow`), and the parts
//! that changed are copied to the video memory afterwards: at once for a
//! prompt or anything else that does not end a line, and at most every
//! 20 ms while lines pour out. Writing a line of text then costs memory
//! writes only, and nothing is ever read back from video memory.
//!
//! When the bootloader started a graphics mode there is no text screen: what
//! is printed before `start_display` only reaches the serial port, and is
//! shown on screen afterwards from a small buffer (`EARLY`).

use crate::display::{self, DisplayError, Target};
use crate::fbcon::{self, FbCon};
use crate::font::{GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::gfx::{Canvas, Rect, Surface};
use crate::ring::Global;
use crate::bootfb::{self, Boot};
use crate::wm::MouseEffect;
use crate::{bootargs, serial, session, thread, time, vga, wm};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// The screen in memory: everything is drawn on `back`, and `show` copies
/// what changed to `front`, the video memory.
struct Shadow {
    /// `back` draws in here.
    _pixels: Vec<u8>,
    front: Canvas,
    back: Canvas,
}

impl Shadow {
    /// `None` when there is no memory for it: drawing then goes straight
    /// to the screen, as it did before.
    fn new(target: &Target) -> Option<Shadow> {
        let size = usize::try_from(target.mode.bytes()).ok()?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(size).ok()?;
        pixels.resize(size, 0);
        let back = Canvas::new(pixels.as_mut_ptr(), target.mode);
        Some(Shadow { _pixels: pixels, front: Canvas::new(target.framebuffer, target.mode), back })
    }
}

static SHADOW: Global<Option<Shadow>> = Global::new(None);

/// Drawn in memory, not yet on the screen.
static PENDING: Global<Rect> = Global::new(Rect::new(0, 0, 0, 0));

/// When the screen last got what was drawn (uptime in ms).
static SHOWN_AT: Global<u64> = Global::new(0);

/// Lines pouring out reach the screen this often.
const SHOW_EVERY_MS: u64 = 20;

/// Show every change at once: during boot, when the clock does not tick
/// yet, and once the kernel is crashing.
static IMMEDIATE: AtomicBool = AtomicBool::new(true);

/// The boot is over: from now on, lines that pour out are shown in batches.
pub fn settle() {
    show(true);
    IMMEDIATE.store(false, Ordering::SeqCst);
}

/// Put everything drawn so far on the screen, and keep doing so after every
/// change: the first thing a crashing kernel does (crash.rs).
pub fn show_now() {
    IMMEDIATE.store(true, Ordering::SeqCst);
    show(true);
}

/// Bring everything drawn so far to the screen (before waiting for a key).
pub fn flush() {
    show(true);
}

/// The screen or the windows are being rebuilt (another size, a window
/// opened, closed or brought to the top): the timer keeps its hands off
/// meanwhile.
static REBUILDING: AtomicUsize = AtomicUsize::new(0);

struct Rebuilding;

impl Rebuilding {
    fn start() -> Rebuilding {
        REBUILDING.fetch_add(1, Ordering::SeqCst);
        Rebuilding
    }
}

impl Drop for Rebuilding {
    fn drop(&mut self) {
        REBUILDING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// From the timer interrupt, 100 times a second: lines that were held back
/// for a batch reach the screen even when the command that printed them is
/// busy and prints nothing more for a while. It only copies pixels.
pub fn on_tick() {
    if !IMMEDIATE.load(Ordering::Relaxed) && REBUILDING.load(Ordering::Relaxed) == 0 {
        show(false);
    }
}

/// Is a batch due? `force`, crash or boot: yes. Otherwise when the last one
/// is `SHOW_EVERY_MS` ago.
pub fn show_due(force: bool, immediate: bool, now_ms: u64, shown_at_ms: u64) -> bool {
    force || immediate || now_ms.saturating_sub(shown_at_ms) >= SHOW_EVERY_MS
}

/// Something was drawn on the canvas outside the console's cells.
fn damage(rect: Rect) {
    let pending = PENDING.get();
    *pending = pending.union(&rect);
}

/// Draw this part of the desktop again in memory: the background, and
/// every window with what it shows, back to front.
fn compose(rect: Rect) {
    let (Some(desk), Some(shadow)) = (DESKTOP.get().as_ref(), SHADOW.get().as_ref()) else { return };
    let canvas = shadow.back.clipped(rect);
    desk.draw_with(&canvas, |id, window| {
        let content = if id == TERMINAL {
            CONSOLE_SURFACE.get().as_ref().map(|s| s.canvas())
        } else {
            PANES.get().iter().find(|p| p.window == id).map(|p| p.surface.canvas())
        };
        if let Some(content) = content {
            let at = window.content();
            canvas.blit(&content, at.x, at.y);
        }
    });
    damage(rect.clip(&shadow.back.bounds()));
}

/// A window's owner drew on `rect` of its content: that part of the screen
/// is put together again.
fn window_changed(id: usize, rect: Rect) {
    if rect.is_empty() {
        return;
    }
    let Some(content) = DESKTOP.get().as_ref().and_then(|desk| desk.window(id)).map(|w| w.content()) else { return };
    compose(Rect::new(content.x + rect.x, content.y + rect.y, rect.width, rect.height).clip(&content));
}

/// Bring what was drawn to the screen (`force`: now, whatever the time).
fn show(force: bool) {
    let Screen::Graphics(screen) = SCREEN.get() else { return };
    let Some(shadow) = SHADOW.get() else {
        // Drawn straight on the screen: nothing to copy.
        screen.take_dirty();
        return;
    };
    let now = time::uptime_ms();
    if !show_due(force, IMMEDIATE.load(Ordering::Relaxed), now, *SHOWN_AT.get()) {
        return;
    }
    if DESKTOP.get().is_some() {
        window_changed(TERMINAL, screen.take_dirty());
        for i in 0..PANES.get().len() {
            let pane = &mut PANES.get()[i];
            let (id, dirty) = (pane.window, pane.con.take_dirty());
            window_changed(id, dirty);
        }
    } else {
        damage(screen.take_dirty());
    }
    let rect = *PENDING.get();
    if rect.is_empty() {
        return;
    }
    *PENDING.get() = Rect::new(0, 0, 0, 0);
    *SHOWN_AT.get() = now;
    shadow.front.copy_from(&shadow.back, rect);
    // The pointer lives on the screen only: put it back where it was covered.
    if let Some(desk) = DESKTOP.get() {
        if desk.pointer().is_some_and(|p| Rect::new(p.x, p.y, wm::POINTER_WIDTH, wm::POINTER_HEIGHT).overlaps(&rect)) {
            desk.draw_pointer(&shadow.front);
        }
    }
}

/// What the console's window shows; the console draws on it.
static CONSOLE_SURFACE: Global<Option<Surface>> = Global::new(None);

/// One more terminal window (`terminal`): the text it shows. The shell
/// that runs in it belongs to desk.rs.
struct Pane {
    window: usize,
    /// `con` keeps its characters in here.
    _cells: Vec<u16>,
    con: FbCon<'static>,
    surface: Surface,
}

static PANES: Global<Vec<Pane>> = Global::new(Vec::new());

/// Windows whose close box was clicked, for their owner (desk.rs) to end.
static CLOSING: Global<Vec<usize>> = Global::new(Vec::new());

static CLICKS: Global<Vec<(usize, u16, u16)>> = Global::new(Vec::new());
static LAUNCHES: Global<Vec<usize>> = Global::new(Vec::new());

/// The buttons on the desktop, left to right: what a click on each starts
/// is up to desk.rs.
pub const BUTTONS: [&str; 4] = ["Terminal", "Files", "Notes", "Web"];

pub const MAX_PANES: usize = 4;
/// The size of a new terminal window, in characters.
pub const PANE_SIZE: (usize, usize) = (80, 25);

/// Open one more terminal window on the desktop, on top; returns its
/// number. Each new one lies a little further right and down.
pub fn open_pane(title: &str) -> Result<usize, &'static str> {
    let (Some(shadow), Screen::Graphics(screen)) = (SHADOW.get().as_ref(), SCREEN.get()) else {
        return Err("windows need the desktop (see desktop)");
    };
    if DESKTOP.get().is_none() {
        return Err("windows need the desktop (see desktop)");
    }
    if PANES.get().len() >= MAX_PANES {
        return Err("too many terminal windows");
    }
    let (scale, bounds) = (screen.scale(), shadow.back.bounds());
    let (cols, rows) = PANE_SIZE;
    let (width, height) = wm::frame_size(cols * GLYPH_WIDTH * scale, rows * GLYPH_HEIGHT * scale);
    if width > bounds.width || height > bounds.height {
        return Err("the screen is too small for another window");
    }
    let step = 32 * (PANES.get().len() + 1);
    let frame = Rect::new(step.min(bounds.width - width), step.min(bounds.height - height), width, height);
    let surface = Surface::new(cols * GLYPH_WIDTH * scale, rows * GLYPH_HEIGHT * scale, shadow.back.format())
        .ok_or("not enough memory for another window")?;
    let mut cells = alloc::vec![0u16; cols * rows];
    // The console keeps a reference to its cells; they live as long as it does, in the same `Pane`.
    let slice: &'static mut [u16] = unsafe { core::slice::from_raw_parts_mut(cells.as_mut_ptr(), cells.len()) };
    let mut con = FbCon::in_area(surface.canvas(), surface.canvas().bounds(), slice, scale);
    con.clear();
    let _rebuilding = Rebuilding::start();
    let Some(desk) = DESKTOP.get().as_mut() else { return Err("windows need the desktop (see desktop)") };
    let window = desk.open(title, frame);
    PANES.get().push(Pane { window, _cells: cells, con, surface });
    compose(bounds);
    show(true);
    Ok(window)
}

/// Print text in a terminal window. A backspace character steps back one place.
pub fn pane_write(window: usize, text: &str) {
    let Some(pane) = PANES.get().iter_mut().find(|p| p.window == window) else { return };
    for c in text.chars() {
        if c == '\x08' {
            let (row, col) = pane.con.pos();
            pane.con.set_pos(row, col.saturating_sub(1));
        } else {
            pane.con.write_char(c);
        }
    }
    pane.con.sync_cursor();
    show(true);
}

/// Columns and lines of a terminal window.
pub fn pane_size(window: usize) -> Option<(usize, usize)> {
    PANES.get().iter().find(|p| p.window == window).map(|p| (p.con.width(), p.con.height()))
}

/// Fill a window with ready-made lines of cells (VGA format), from the top
/// left, and put the cursor in the cell (row, column): for programs that
/// draw their whole window themselves, like the editor.
pub fn pane_show(window: usize, lines: &[Vec<u16>], cursor: (usize, usize)) {
    let Some(pane) = PANES.get().iter_mut().find(|p| p.window == window) else { return };
    for (row, line) in lines.iter().enumerate() {
        for (col, cell) in line.iter().enumerate() {
            pane.con.put_cell(row, col, *cell);
        }
    }
    pane.con.set_cursor(cursor.0 * pane.con.width() + cursor.1);
    show(true);
}

pub fn pane_exists(window: usize) -> bool {
    PANES.get().iter().any(|p| p.window == window)
}

/// Take a terminal window off the desktop.
pub fn close_pane(window: usize) {
    let _rebuilding = Rebuilding::start();
    let before = PANES.get().len();
    PANES.get().retain(|p| p.window != window);
    if PANES.get().len() == before {
        return;
    }
    if let Some(desk) = DESKTOP.get().as_mut() {
        desk.close(window);
    }
    if let Some(bounds) = SHADOW.get().as_ref().map(|s| s.back.bounds()) {
        compose(bounds);
    }
    show(true);
}

/// The window that has the focus, if it is one of the extra terminals
/// (`None`: the console has it, or there is no desktop).
pub fn focused_pane() -> Option<usize> {
    DESKTOP.get().as_ref()?.top().filter(|id| *id != TERMINAL)
}

/// Give the focus to the next window (Alt+Tab).
pub fn next_window() {
    let _rebuilding = Rebuilding::start();
    let Some(desk) = DESKTOP.get().as_mut() else { return };
    if desk.cycle() {
        if let Some(bounds) = SHADOW.get().as_ref().map(|s| s.back.bounds()) {
            compose(bounds);
        }
        show(true);
    }
}

/// Windows whose close box was clicked since the last call.
pub fn take_closing() -> Vec<usize> {
    core::mem::take(CLOSING.get())
}

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
    match SCREEN.get() {
        Screen::Text(screen) => {
            let cells: Vec<u16> = (0..rows * cols).map(|i| screen.cell(i / cols, i % cols)).collect();
            text += &cells_text(&cells, cols);
        }
        Screen::Graphics(screen) => text += &cells_text(screen.cells(), cols),
    }
    // The other windows follow, each under a line with its number.
    for pane in PANES.get().iter() {
        text += &format!("--- window {} ---\n", pane.window);
        text += cells_text(pane.con.cells(), pane.con.width()).trim_end();
        text.push('\n');
    }
    text
}

/// Cells (VGA format) as text, line by line, trailing spaces left out.
fn cells_text(cells: &[u16], cols: usize) -> String {
    let mut text = String::new();
    for line in cells.chunks(cols.max(1)) {
        let line: String = line.iter().map(|cell| vga::from_cp437(*cell as u8)).collect();
        text += line.trim_end();
        text.push('\n');
    }
    text
}

/// A picture of the screen: every `scale`-th pixel, as (width, height, RGB
/// bytes). `None` on the text screen. It is read from the screen in memory,
/// so the mouse pointer is not in it.
pub fn capture(scale: usize) -> Option<(usize, usize, Vec<u8>)> {
    let Screen::Graphics(screen) = SCREEN.get() else { return None };
    let canvas = SHADOW.get().as_ref().map_or(screen.canvas(), |s| s.back);
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
    let rebuilding = Rebuilding::start();
    // The extra terminal windows do not move along: their shells end (desk.rs).
    PANES.get().clear();
    // The old in-memory screen stays until the console has moved off it.
    let shadow = Shadow::new(&target);
    let canvas = shadow.as_ref().map_or(Canvas::new(target.framebuffer, target.mode), |s| s.back);
    let scale = font_scale(*FONT.get(), canvas.height());
    // The desktop needs the screen in memory, and memory for what the window shows.
    let windowed = (if desktop && shadow.is_some() { wm::terminal_frame(canvas.width(), canvas.height(), scale) } else { None })
        .and_then(|frame| {
            let mut desk = wm::Desktop::new();
            let id = desk.open("Terminal", frame);
            let inside = desk.window(id)?.content();
            let surface = Surface::new(inside.width, inside.height, target.mode.format)?;
            desk.set_pointer(*POINTER.get(), canvas.bounds());
            desk.set_buttons(&BUTTONS, canvas.width());
            desk.set_clock(CLOCK.get());
            Some((desk, surface))
        });
    let (mut screen, desk, surface) = match windowed {
        Some((desk, surface)) => {
            let inside = surface.canvas();
            (FbCon::in_area(inside, inside.bounds(), CELLS.get(), scale), Some(desk), Some(surface))
        }
        None => (FbCon::in_area(canvas, canvas.bounds(), CELLS.get(), scale), None, None),
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
    let on = desk.is_some();
    *SCREEN.get() = Screen::Graphics(screen);
    *DESKTOP.get() = desk;
    *CONSOLE_SURFACE.get() = surface;
    *SHADOW.get() = shadow;
    *PENDING.get() = Rect::new(0, 0, 0, 0);
    drop(rebuilding);
    if on {
        compose(canvas.bounds());
    } else {
        damage(canvas.bounds());
    }
    show(true);
    on
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
    let _rebuilding = Rebuilding::start();
    let (Some(desk), Some(shadow)) = (DESKTOP.get().as_mut(), SHADOW.get().as_ref()) else { return };
    let bounds = shadow.back.bounds();
    match desk.mouse(buttons, dx, dy, bounds) {
        MouseEffect::Nothing => {}
        MouseEffect::PointerMoved(old) => {
            // What the pointer covered comes back from memory.
            shadow.front.copy_from(&shadow.back, old);
            desk.draw_pointer(&shadow.front);
        }
        MouseEffect::WindowMoved | MouseEffect::Raised => {
            compose(bounds);
            show(true);
        }
        // The console's own window stays.
        MouseEffect::Close(TERMINAL) => {}
        MouseEffect::Close(id) => CLOSING.get().push(id),
        MouseEffect::Click { id, x, y, raised } => {
            if raised {
                compose(bounds);
                show(true);
            }
            // For the program in that window: the character under the pointer.
            if let Some(pane) = PANES.get().iter().find(|p| p.window == id) {
                let (w, h) = (GLYPH_WIDTH * pane.con.scale(), GLYPH_HEIGHT * pane.con.scale());
                if CLICKS.get().len() < 16 {
                    CLICKS.get().push((id, (x / w) as u16, (y / h) as u16));
                }
            }
        }
        MouseEffect::Launch(button) => {
            if LAUNCHES.get().len() < 4 {
                LAUNCHES.get().push(button);
            }
            // The thread that opens it may not be there yet.
            crate::desk::start();
        }
    }
}

/// Clicks inside windows since the last call: (window, column, line).
pub fn take_clicks() -> Vec<(usize, u16, u16)> {
    core::mem::take(CLICKS.get())
}

/// The clock on the desktop shows this text (the clock thread sets it
/// when the minute changes; "" hides it). Nothing happens without a desktop.
static CLOCK: Global<String> = Global::new(String::new());

pub fn set_clock(text: &str) {
    *CLOCK.get() = String::from(text);
    let _rebuilding = Rebuilding::start();
    let Some(rect) = DESKTOP.get().as_mut().and_then(|desk| desk.set_clock(text)) else { return };
    compose(rect);
}

/// Desktop buttons clicked since the last call (their place in `BUTTONS`).
pub fn take_launches() -> Vec<usize> {
    core::mem::take(LAUNCHES.get())
}

/// Is there something the mouse asked for that desk.rs has not done yet?
pub fn mouse_work() -> bool {
    !CLICKS.get().is_empty() || !LAUNCHES.get().is_empty() || !CLOSING.get().is_empty()
}

/// Where the pointer is on the desktop: for `desktop` and the tests.
pub fn pointer() -> Option<(usize, usize)> {
    DESKTOP.get().as_ref()?.pointer().map(|p| (p.x, p.y))
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
    // What was printed so far goes to the screen first, so that it is not
    // put over the bars afterwards.
    show(true);
    if let Screen::Graphics(screen) = SCREEN.get() {
        match SHADOW.get() {
            // Over the whole screen, not only inside the console's window.
            Some(shadow) if DESKTOP.get().is_some() => {
                let (width, height) = (shadow.back.width(), shadow.back.height());
                for (i, color) in fbcon::BARS.iter().enumerate() {
                    let (from, to) = (width * i / fbcon::BARS.len(), width * (i + 1) / fbcon::BARS.len());
                    shadow.back.fill_rect(Rect::new(from, 0, to - from, height), fbcon::PALETTE[*color as usize]);
                }
                damage(shadow.back.bounds());
            }
            _ => screen.color_bars(),
        }
    }
    show(true);
}

/// Draw the whole screen again; returns how long it took in microseconds:
/// in all, and the part spent copying to video memory.
pub fn redraw() -> (u64, u64) {
    let start = crate::time::micros();
    if let Screen::Graphics(screen) = SCREEN.get() {
        screen.redraw();
        match SHADOW.get() {
            Some(shadow) if DESKTOP.get().is_some() => {
                for pane in PANES.get().iter_mut() {
                    pane.con.redraw();
                }
                compose(shadow.back.bounds());
            }
            _ => damage(screen.canvas().bounds()),
        }
    }
    let copy = crate::time::micros();
    show(true);
    let end = crate::time::micros();
    (end - start, end - copy)
}

pub fn clear() {
    if session::current().is_some() {
        return;
    }
    SCREEN.get().clear();
    show(true);
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

struct Console {
    /// The last piece written ended a line.
    ended_line: bool,
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if let Some(id) = session::current() {
            session::write(id, s);
            return Ok(());
        }
        let screen = SCREEN.get();
        screen.write_str(s);
        screen.sync_cursor();
        self.ended_line = s.ends_with('\n');
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
    let mut console = Console { ended_line: false };
    let _ = console.write_fmt(args);
    if session::current().is_none() {
        // A prompt, a progress dot, a word from a background thread: on the
        // screen at once. Whole lines from a running command: in batches.
        show(!console.ended_line || !thread::on_console());
    }
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
    show(true);

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
