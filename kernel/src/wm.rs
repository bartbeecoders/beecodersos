//! A simple window system: a desktop with a plain background and windows
//! with a border and a title bar. It only knows where windows are and how
//! their frames look; what is inside a window is drawn by its owner (for the
//! terminal: the framebuffer console, fbcon.rs, in the window's content area).
//!
//! Windows lie on top of each other; the one on top has the focus (its
//! title bar is blue, it gets the keys). Each has a number that stays the
//! same while their order changes.
//!
//! With a mouse (usb.rs) there is a pointer: pressing the left button on a
//! window brings it to the top, on its title bar drags it, on the box at the
//! right of the title bar asks to close it.

use crate::font::{GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::gfx::{Canvas, Rect};
use alloc::string::String;
use alloc::vec::Vec;

/// Light blue.
pub const BACKGROUND: u32 = 0xA8D0F0;
pub const TITLE_BAR: u32 = 0x3A6EA5;
const TITLE_BAR_UNFOCUSED: u32 = 0x8A9BB0;
const TITLE_TEXT: u32 = 0xFFFFFF;
const BORDER_COLOR: u32 = 0x1F3A5F;
const SHADOW_COLOR: u32 = 0x86B4D8;
const CLOSE_BOX: u32 = 0xE06C5A;
/// Inside of a window before its owner draws: black, like the terminal.
pub const CONTENT: u32 = 0x000000;

/// Pixels: the line around a window, its title bar, and the room between
/// the frame and the content.
const BORDER: usize = 2;
const TITLE_HEIGHT: usize = GLYPH_HEIGHT + 8;
const PADDING: usize = 4;
const SHADOW: usize = 6;

/// The smallest terminal worth a window, in characters.
const MIN_COLS: usize = 20;
const MIN_ROWS: usize = 4;

pub struct Window {
    pub title: String,
    /// The whole window: border, title bar and content.
    pub frame: Rect,
}

impl Window {
    /// Where the owner draws: inside the border, below the title bar.
    pub fn content(&self) -> Rect {
        let side = BORDER + PADDING;
        Rect {
            x: self.frame.x + side,
            y: self.frame.y + BORDER + TITLE_HEIGHT + PADDING,
            width: self.frame.width.saturating_sub(2 * side),
            height: self.frame.height.saturating_sub(2 * side + TITLE_HEIGHT),
        }
    }

    /// The box at the right of the title bar; a very narrow window has none.
    pub fn close_box(&self) -> Option<Rect> {
        let inner_width = self.frame.width.saturating_sub(2 * BORDER);
        let box_size = GLYPH_HEIGHT - 4;
        (inner_width > 2 * GLYPH_WIDTH + box_size).then(|| {
            Rect::new(self.frame.x + BORDER + inner_width - GLYPH_WIDTH - box_size, self.frame.y + BORDER + 6, box_size, box_size)
        })
    }

    /// The frame: shadow, border, title bar with the title and a close box,
    /// and an empty content area.
    fn draw(&self, canvas: &Canvas, focused: bool) {
        let f = self.frame;
        canvas.fill_rect(Rect::new(f.x + SHADOW, f.y + SHADOW, f.width, f.height), SHADOW_COLOR);
        canvas.fill_rect(f, BORDER_COLOR);
        let inner_width = f.width.saturating_sub(2 * BORDER);
        let bar = Rect::new(f.x + BORDER, f.y + BORDER, inner_width, TITLE_HEIGHT);
        let bar_color = if focused { TITLE_BAR } else { TITLE_BAR_UNFOCUSED };
        canvas.fill_rect(bar, bar_color);
        let box_size = GLYPH_HEIGHT - 4;
        let text_room = inner_width.saturating_sub(3 * GLYPH_WIDTH + box_size);
        canvas.text(bar.x + GLYPH_WIDTH, bar.y + 4, &self.title, TITLE_TEXT, bar_color, text_room);
        if let Some(close) = self.close_box() {
            canvas.fill_rect(close, CLOSE_BOX);
        }
        let below = Rect::new(
            f.x + BORDER,
            f.y + BORDER + TITLE_HEIGHT,
            inner_width,
            f.height.saturating_sub(2 * BORDER + TITLE_HEIGHT),
        );
        canvas.fill_rect(below, CONTENT);
    }
}

/// The mouse pointer: an arrow, 12 by 19 pixels. X: black edge, o: white inside.
const POINTER: [&str; 19] = [
    "X           ",
    "XX          ",
    "XoX         ",
    "XooX        ",
    "XoooX       ",
    "XooooX      ",
    "XoooooX     ",
    "XooooooX    ",
    "XoooooooX   ",
    "XooooooooX  ",
    "XoooooooooX ",
    "XooooooXXXXX",
    "XoooXooX    ",
    "XooX XooX   ",
    "XoX  XooX   ",
    "XX    XooX  ",
    "X     XooX  ",
    "       XX   ",
    "            ",
];
pub const POINTER_WIDTH: usize = 12;
pub const POINTER_HEIGHT: usize = 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    /// The tip of the arrow.
    pub x: usize,
    pub y: usize,
    buttons: u8,
    /// While the top window is being dragged: where in its frame it was grabbed.
    grab: Option<(usize, usize)>,
}

/// What a mouse event changed on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEffect {
    Nothing,
    /// The pointer moved: this is where it was.
    PointerMoved(Rect),
    /// The top window moved (and the pointer with it).
    WindowMoved,
    /// Another window came to the top (the pointer may have moved too).
    Raised,
    /// The button went down on the close box of this window (its number).
    Close(usize),
    /// The button went down inside a window, on what it shows: the
    /// window's number and the place, in pixels from the top left of its
    /// content. `raised`: the window also came to the top by it.
    Click { id: usize, x: usize, y: usize, raised: bool },
    /// The button went down on a button of the desktop (its place in the row).
    Launch(usize),
}

pub struct Desktop {
    /// Back to front: the last window is on top and has the focus.
    windows: Vec<Window>,
    /// The number of each window, in the same order.
    ids: Vec<usize>,
    next_id: usize,
    /// `None` until a mouse moves.
    pointer: Option<Pointer>,
    /// Buttons on the desktop itself, in a row at the top: label and place.
    buttons: Vec<(String, Rect)>,
}

/// Where the row of desktop buttons starts, and how high a button is.
const BUTTONS_AT: (usize, usize) = (100, 6);
const BUTTON_HEIGHT: usize = GLYPH_HEIGHT + 8;
const BUTTON_GAP: usize = 8;

impl Desktop {
    pub fn new() -> Self {
        Desktop { windows: Vec::new(), ids: Vec::new(), next_id: 0, pointer: None, buttons: Vec::new() }
    }

    /// Put a row of buttons on the desktop (to start programs with the
    /// mouse). Those that do not fit on a screen `width` pixels wide are
    /// left out. Returns where each one is.
    pub fn set_buttons(&mut self, labels: &[&str], width: usize) -> Vec<Rect> {
        self.buttons.clear();
        let mut x = BUTTONS_AT.0;
        for label in labels {
            let w = (label.chars().count() + 2) * GLYPH_WIDTH;
            if x + w > width {
                break;
            }
            self.buttons.push((String::from(*label), Rect::new(x, BUTTONS_AT.1, w, BUTTON_HEIGHT)));
            x += w + BUTTON_GAP;
        }
        self.buttons.iter().map(|(_, rect)| *rect).collect()
    }

    /// Add a window on top of the others; returns its number (the first
    /// window gets 0, and numbers are not used again).
    pub fn open(&mut self, title: &str, frame: Rect) -> usize {
        self.windows.push(Window { title: String::from(title), frame });
        self.ids.push(self.next_id);
        self.next_id += 1;
        self.next_id - 1
    }

    fn position(&self, id: usize) -> Option<usize> {
        self.ids.iter().position(|i| *i == id)
    }

    pub fn window(&self, id: usize) -> Option<&Window> {
        self.windows.get(self.position(id)?)
    }

    /// The number of the window on top: the one with the focus.
    pub fn top(&self) -> Option<usize> {
        self.ids.last().copied()
    }

    /// Bring a window to the top. False: it already was, or does not exist.
    pub fn raise(&mut self, id: usize) -> bool {
        match self.position(id) {
            Some(at) if at + 1 < self.windows.len() => {
                let window = self.windows.remove(at);
                self.windows.push(window);
                self.ids.remove(at);
                self.ids.push(id);
                true
            }
            _ => false,
        }
    }

    /// The next window gets the focus: the one at the back comes to the
    /// top, so repeating it visits every window. False with fewer than two.
    pub fn cycle(&mut self) -> bool {
        match self.ids.first().copied() {
            Some(back) if self.windows.len() > 1 => self.raise(back),
            _ => false,
        }
    }

    /// Take a window away. False: there is no such window.
    pub fn close(&mut self, id: usize) -> bool {
        let Some(at) = self.position(id) else { return false };
        self.windows.remove(at);
        self.ids.remove(at);
        true
    }

    /// Draw the background and every window frame. The owners then draw
    /// their content again.
    pub fn draw(&self, canvas: &Canvas) {
        self.draw_with(canvas, |_, _| {});
    }

    /// The same, for windows that overlap: after each frame, back to front,
    /// `content` is called with the window's number and the window, to
    /// draw what is inside before the next window goes over it.
    pub fn draw_with(&self, canvas: &Canvas, mut content: impl FnMut(usize, &Window)) {
        canvas.fill_rect(canvas.bounds(), BACKGROUND);
        for (label, rect) in &self.buttons {
            canvas.fill_rect(*rect, BORDER_COLOR);
            let inside = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);
            canvas.fill_rect(inside, TITLE_BAR);
            canvas.text(rect.x + GLYPH_WIDTH, rect.y + 4, label, TITLE_TEXT, TITLE_BAR, rect.width);
        }
        for (i, window) in self.windows.iter().enumerate() {
            window.draw(canvas, i + 1 == self.windows.len());
            content(self.ids[i], window);
        }
    }
}

impl Desktop {
    pub fn pointer(&self) -> Option<Pointer> {
        self.pointer
    }

    /// Put the pointer somewhere (a new desktop takes over the old one's pointer).
    pub fn set_pointer(&mut self, pointer: Option<Pointer>, screen: Rect) {
        self.pointer = pointer.map(|p| Pointer {
            x: p.x.min(screen.width.saturating_sub(1)),
            y: p.y.min(screen.height.saturating_sub(1)),
            buttons: 0,
            grab: None,
        });
    }

    pub fn draw_pointer(&self, canvas: &Canvas) {
        let Some(p) = self.pointer else { return };
        for (dy, row) in POINTER.iter().enumerate() {
            for (dx, mark) in row.bytes().enumerate() {
                let color = match mark {
                    b'X' => 0x000000,
                    b'o' => 0xFFFFFF,
                    _ => continue,
                };
                canvas.fill_rect(Rect::new(p.x + dx, p.y + dy, 1, 1), color);
            }
        }
    }

    /// The mouse moved by (dx, dy) pixels and its buttons are now `buttons`
    /// (bit 0: left). The first event puts the pointer in the middle of the
    /// screen. Pressing the left button on a window brings it to the top;
    /// on the close box it asks to close the window; elsewhere on the title
    /// bar it grabs the window, which then follows the pointer until the
    /// button is let go, and stays completely on the screen.
    pub fn mouse(&mut self, buttons: u8, dx: i32, dy: i32, screen: Rect) -> MouseEffect {
        let first = self.pointer.is_none();
        let old = self.pointer.unwrap_or(Pointer { x: screen.width / 2, y: screen.height / 2, buttons: 0, grab: None });
        let step = |at: usize, by: i32, size: usize| {
            (at as i64 + by as i64).clamp(0, size.saturating_sub(1) as i64) as usize
        };
        let mut new = Pointer { x: step(old.x, dx, screen.width), y: step(old.y, dy, screen.height), buttons, grab: old.grab };
        let (was_down, is_down) = (old.buttons & 1 != 0, buttons & 1 != 0);
        let inside = |r: Rect| new.x >= r.x && new.x < r.x + r.width && new.y >= r.y && new.y < r.y + r.height;
        let (mut raised, mut close, mut click, mut launch) = (false, None, None, None);
        if !is_down {
            new.grab = None;
        } else if !was_down {
            // The topmost window under the pointer comes to the top.
            match self.windows.iter().rposition(|w| inside(w.frame)) {
                Some(at) => raised = self.raise(self.ids[at]),
                // On the desktop itself: one of its buttons?
                None => launch = self.buttons.iter().position(|(_, rect)| inside(*rect)),
            }
            let top = self.windows.last().filter(|w| inside(w.frame));
            if top.and_then(|w| w.close_box()).is_some_and(inside) {
                close = self.top();
            } else {
                new.grab = top.and_then(|w| {
                    let f = w.frame;
                    (new.y < f.y + BORDER + TITLE_HEIGHT).then(|| (new.x - f.x, new.y - f.y))
                });
                click = top.map(|w| w.content()).filter(|c| inside(*c)).map(|c| (new.x - c.x, new.y - c.y));
            }
        }
        self.pointer = Some(new);
        if let Some(id) = close {
            return MouseEffect::Close(id);
        }
        if let (Some((x, y)), Some(id)) = (click, self.top()) {
            return MouseEffect::Click { id, x, y, raised };
        }
        if let Some(button) = launch {
            return MouseEffect::Launch(button);
        }
        if let (Some((gx, gy)), Some(window)) = (new.grab, self.windows.last_mut()) {
            let f = window.frame;
            let x = new.x.saturating_sub(gx).min(screen.width.saturating_sub(f.width));
            let y = new.y.saturating_sub(gy).min(screen.height.saturating_sub(f.height));
            if (x, y) != (f.x, f.y) {
                window.frame = Rect::new(x, y, f.width, f.height);
                return MouseEffect::WindowMoved;
            }
        }
        if raised {
            return MouseEffect::Raised;
        }
        if first || (new.x, new.y) != (old.x, old.y) {
            return MouseEffect::PointerMoved(Rect::new(old.x, old.y, POINTER_WIDTH, POINTER_HEIGHT));
        }
        MouseEffect::Nothing
    }
}

/// The size of the window whose content area is `width` by `height` pixels.
pub fn frame_size(width: usize, height: usize) -> (usize, usize) {
    let side = 2 * (BORDER + PADDING);
    (width + side, height + side + TITLE_HEIGHT)
}

/// Where the terminal window goes on a screen of this size: in the middle,
/// with some desktop around it, and a content area of whole characters
/// (`scale`: the terminal's font size, 1 or 2). `None`: the screen is too
/// small for a window.
pub fn terminal_frame(width: usize, height: usize, scale: usize) -> Option<Rect> {
    let (cell_width, cell_height) = (GLYPH_WIDTH * scale.max(1), GLYPH_HEIGHT * scale.max(1));
    let margin = (width.min(height) / 16).max(8);
    let extra_width = 2 * (BORDER + PADDING);
    let extra_height = extra_width + TITLE_HEIGHT;
    let cols = width.checked_sub(2 * margin + extra_width)? / cell_width;
    let rows = height.checked_sub(2 * margin + extra_height)? / cell_height;
    if cols < MIN_COLS || rows < MIN_ROWS {
        return None;
    }
    let (w, h) = (cols * cell_width + extra_width, rows * cell_height + extra_height);
    Some(Rect::new((width - w) / 2, (height - h) / 2, w, h))
}
