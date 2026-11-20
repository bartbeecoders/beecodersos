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
//! right of the title bar asks to close it, and on the grip in its bottom
//! right corner resizes it (in whole characters; its owner then lays out
//! its content again).

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
const GRIP_COLOR: u32 = 0x8A9BB0;
const COLLAPSE_BOX: u32 = 0xF2C14E;
const SCROLL_TRACK: u32 = 0x2B3A55;
const SCROLL_THUMB: u32 = 0x8A9BB0;
/// How wide a scroll bar is (over the right edge of the content).
pub const SCROLL_WIDTH: usize = 10;
/// Inside of a window before its owner draws: black, like the terminal.
pub const CONTENT: u32 = 0x000000;

/// Pixels: the line around a window, its title bar, and the room between
/// the frame and the content.
const BORDER: usize = 2;
const TITLE_HEIGHT: usize = GLYPH_HEIGHT + 8;
const PADDING: usize = 4;
const SHADOW: usize = 6;
/// The square in the bottom right corner that resizes a window.
pub const GRIP: usize = 12;

/// The smallest terminal worth a window, in characters.
const MIN_COLS: usize = 20;
const MIN_ROWS: usize = 4;

pub struct Window {
    pub title: String,
    /// The whole window: border, title bar and content.
    pub frame: Rect,
    /// It has a grip and can be resized (the console's own window cannot).
    pub resizable: bool,
    /// Folded up to its title bar: the height it had before.
    pub collapsed: Option<usize>,
    /// What its program shows of a longer text: (first line shown, lines
    /// shown, lines in all). A scroll bar is drawn when not all fit.
    pub scroll: Option<(usize, usize, usize)>,
    /// Filling the screen (Super+F): the frame it had before.
    pub saved: Option<Rect>,
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

    /// The scroll bar along the right edge of the content (above the grip),
    /// when the program shows only part of what it has.
    pub fn scroll_bar(&self) -> Option<Rect> {
        let (_, shown, total) = self.scroll?;
        let c = self.content();
        (total > shown && c.width > 4 * SCROLL_WIDTH && c.height > 3 * GRIP)
            .then(|| Rect::new(c.x + c.width - SCROLL_WIDTH, c.y, SCROLL_WIDTH, c.height.saturating_sub(GRIP)))
    }

    /// The thumb: where in the track the part shown lies, and how much of it.
    pub fn scroll_thumb(&self) -> Option<Rect> {
        let bar = self.scroll_bar()?;
        let (first, shown, total) = self.scroll?;
        let height = (bar.height * shown / total.max(1)).clamp(GRIP.min(bar.height), bar.height);
        let room = bar.height - height;
        let most = total.saturating_sub(shown).max(1);
        let y = bar.y + room * first.min(most) / most;
        Some(Rect::new(bar.x + 2, y, bar.width - 4, height))
    }

    fn draw_scroll_bar(&self, canvas: &Canvas) {
        let (Some(bar), Some(thumb)) = (self.scroll_bar(), self.scroll_thumb()) else { return };
        canvas.fill_rect(bar, SCROLL_TRACK);
        canvas.fill_rect(thumb, SCROLL_THUMB);
    }

    /// The box left of the close box: folds the window up to its title bar and back.
    pub fn collapse_box(&self) -> Option<Rect> {
        let close = self.close_box()?;
        let x = close.x.checked_sub(close.width + GLYPH_WIDTH / 2)?;
        (x > self.frame.x + BORDER + 4 * GLYPH_WIDTH).then(|| Rect::new(x, close.y, close.width, close.height))
    }

    /// The height of a window folded up to its title bar.
    pub fn collapsed_height() -> usize {
        2 * BORDER + TITLE_HEIGHT
    }

    /// Fold up to the title bar, or back to the height it had.
    pub fn toggle_collapsed(&mut self) {
        match self.collapsed.take() {
            Some(height) => self.frame.height = height,
            None => {
                self.collapsed = Some(self.frame.height);
                self.frame.height = Self::collapsed_height();
            }
        }
    }

    /// The grip in the bottom right corner (over the content's corner).
    pub fn grip(&self) -> Option<Rect> {
        let f = self.frame;
        (self.resizable && self.collapsed.is_none() && f.width > 3 * GRIP && f.height > 3 * GRIP)
            .then(|| Rect::new(f.x + f.width - BORDER - GRIP, f.y + f.height - BORDER - GRIP, GRIP, GRIP))
    }

    /// Three short diagonal lines in the grip, drawn over the content.
    fn draw_grip(&self, canvas: &Canvas) {
        let Some(g) = self.grip() else { return };
        for k in 0..3 {
            let at = 4 * k + 2;
            for d in 0..(GRIP - at) {
                canvas.fill_rect(Rect::new(g.x + at + d, g.y + GRIP - 1 - d, 1, 1), GRIP_COLOR);
            }
        }
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
        let text_room = inner_width.saturating_sub(4 * GLYPH_WIDTH + 2 * box_size);
        canvas.text(bar.x + GLYPH_WIDTH, bar.y + 4, &self.title, TITLE_TEXT, bar_color, text_room);
        if let Some(close) = self.close_box() {
            canvas.fill_rect(close, CLOSE_BOX);
        }
        if let Some(fold) = self.collapse_box() {
            canvas.fill_rect(fold, COLLAPSE_BOX);
            // A short bar: fold up / unfold.
            let line = Rect::new(fold.x + 2, fold.y + fold.height / 2, fold.width.saturating_sub(4), 2);
            canvas.fill_rect(line, BORDER_COLOR);
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
    /// While the top window is being resized: its frame and the pointer when it started.
    resize: Option<(Rect, usize, usize)>,
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
    /// The button was let go after resizing this window: its owner lays
    /// out its content for the new size.
    Resized(usize),
    /// The button went down on a window's scroll bar, above (`up`) or
    /// below its thumb: its program shows the page before or after.
    Scroll { id: usize, up: bool },
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
    /// The clock at the right end of the row (empty: none), and the screen's width.
    clock: String,
    width: usize,
    /// The size of a character in windows: resizing goes in whole ones.
    cell: (usize, usize),
    /// Windows are tiled (`tile`): they cannot be dragged or resized by hand.
    pub tiling: bool,
    /// The order windows are tiled in (oldest first; Super+Shift+arrow swaps).
    tile_order: Vec<usize>,
}

/// A direction for moving the focus or a window with the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Where the row of desktop buttons starts, and how high a button is.
const BUTTONS_AT: (usize, usize) = (100, 6);
const BUTTON_HEIGHT: usize = GLYPH_HEIGHT + 8;
const BUTTON_GAP: usize = 8;

impl Desktop {
    pub fn new() -> Self {
        Desktop {
            windows: Vec::new(),
            ids: Vec::new(),
            next_id: 0,
            pointer: None,
            buttons: Vec::new(),
            clock: String::new(),
            width: 0,
            cell: (GLYPH_WIDTH, GLYPH_HEIGHT),
            tiling: false,
            tile_order: Vec::new(),
        }
    }

    /// Lay the windows out side by side in `area` (oldest first, see
    /// `tile_frames`); folded windows unfold. Returns the windows whose
    /// size changed.
    pub fn tile(&mut self, area: Rect) -> Vec<usize> {
        self.tile_order.retain(|id| self.ids.contains(id));
        let order: Vec<usize> = self.tile_order.iter().filter_map(|id| self.position(*id)).collect();
        let frames = tile_frames(order.len(), area, self.cell);
        let mut changed = Vec::new();
        for (&i, frame) in order.iter().zip(frames) {
            let window = &mut self.windows[i];
            window.collapsed = None;
            window.saved = None;
            if (window.frame.width, window.frame.height) != (frame.width, frame.height) {
                changed.push(self.ids[i]);
            }
            window.frame = frame;
        }
        changed
    }

    /// The size of a character in windows (the font, scaled): resizing
    /// keeps whole characters.
    pub fn set_cell(&mut self, width: usize, height: usize) {
        self.cell = (width.max(1), height.max(1));
    }

    /// What a window's program shows of a longer text (see `Window::scroll`);
    /// true when that changes the scroll bar.
    pub fn set_scroll(&mut self, id: usize, scroll: Option<(usize, usize, usize)>) -> bool {
        let Some(at) = self.position(id) else { return false };
        let scroll = scroll.filter(|(_, shown, total)| total > shown);
        let changed = self.windows[at].scroll != scroll;
        self.windows[at].scroll = scroll;
        changed
    }

    /// A window that cannot be resized (no grip): the console's own.
    pub fn set_resizable(&mut self, id: usize, resizable: bool) {
        if let Some(at) = self.position(id) {
            self.windows[at].resizable = resizable;
        }
    }

    /// The frame `start` resized by (dx, dy) pixels: whole characters, at
    /// least MIN_COLS x MIN_ROWS of them, and within the screen.
    pub fn resized_frame(&self, start: Rect, dx: i64, dy: i64, screen: Rect) -> Rect {
        let (cw, ch) = self.cell;
        let (extra_w, extra_h) = frame_size(0, 0);
        let fit = |size: usize, by: i64, extra: usize, cell: usize, min: usize, room: usize| {
            let wanted = (size as i64 + by).max(0) as usize;
            let cells = (wanted.saturating_sub(extra) / cell).max(min);
            let most = room.saturating_sub(extra) / cell;
            cells.min(most.max(min)) * cell + extra
        };
        let width = fit(start.width, dx, extra_w, cw, MIN_COLS, screen.width.saturating_sub(start.x));
        let height = fit(start.height, dy, extra_h, ch, MIN_ROWS, screen.height.saturating_sub(start.y));
        Rect::new(start.x, start.y, width, height)
    }

    /// Put a row of buttons on the desktop (to start programs with the
    /// mouse). Those that do not fit on a screen `width` pixels wide are
    /// left out. Returns where each one is.
    pub fn set_buttons(&mut self, labels: &[&str], width: usize) -> Vec<Rect> {
        self.buttons.clear();
        self.width = width;
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

    /// Where the clock goes for this text: at the right end of the row of
    /// buttons; `None` if it would not fit next to them.
    fn clock_place(&self, text: &str) -> Option<Rect> {
        if text.is_empty() {
            return None;
        }
        let w = (text.chars().count() + 2) * GLYPH_WIDTH;
        let x = self.width.checked_sub(w + BUTTONS_AT.0 / 2)?;
        let end_of_buttons = self.buttons.last().map_or(BUTTONS_AT.0, |(_, r)| r.x + r.width + BUTTON_GAP);
        (x >= end_of_buttons).then(|| Rect::new(x, BUTTONS_AT.1, w, BUTTON_HEIGHT))
    }

    /// Show this text as the clock ("" hides it). Returns the part of the
    /// screen to draw again (nothing when the text did not change).
    pub fn set_clock(&mut self, text: &str) -> Option<Rect> {
        if self.clock == text {
            return None;
        }
        let old = self.clock_place(&self.clock);
        self.clock = String::from(text);
        match (old, self.clock_place(text)) {
            (Some(a), Some(b)) => Some(a.union(&b)),
            (a, b) => a.or(b),
        }
    }

    /// Add a window on top of the others; returns its number (the first
    /// window gets 0, and numbers are not used again).
    pub fn open(&mut self, title: &str, frame: Rect) -> usize {
        self.windows.push(Window { title: String::from(title), frame, resizable: true, collapsed: None, scroll: None, saved: None });
        self.ids.push(self.next_id);
        self.tile_order.push(self.next_id);
        self.next_id += 1;
        self.next_id - 1
    }

    /// Change a window's title; returns the part of the screen to draw
    /// again (nothing when the title did not change).
    pub fn set_title(&mut self, id: usize, title: &str) -> Option<Rect> {
        let at = self.position(id)?;
        let window = &mut self.windows[at];
        if window.title == title {
            return None;
        }
        window.title = String::from(title);
        Some(window.frame)
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
        self.tile_order.retain(|i| *i != id);
        true
    }

    /// The window next to `id` in direction `dir`: of those whose middle
    /// lies that way, the nearest (distance across counts double).
    pub fn neighbour(&self, id: usize, dir: Dir) -> Option<usize> {
        let middle = |r: Rect| ((r.x + r.width / 2) as i64, (r.y + r.height / 2) as i64);
        let (x, y) = middle(self.window(id)?.frame);
        self.ids
            .iter()
            .zip(self.windows.iter())
            .filter(|(other, _)| **other != id)
            .filter_map(|(other, w)| {
                let (ox, oy) = middle(w.frame);
                let (ahead, across) = match dir {
                    Dir::Left => (x - ox, (oy - y).abs()),
                    Dir::Right => (ox - x, (oy - y).abs()),
                    Dir::Up => (y - oy, (ox - x).abs()),
                    Dir::Down => (oy - y, (ox - x).abs()),
                };
                (ahead > 0).then_some((ahead + 2 * across, *other))
            })
            .min()
            .map(|(_, other)| other)
    }

    /// Put a window somewhere else (another size too).
    pub fn place(&mut self, id: usize, frame: Rect) {
        if let Some(at) = self.position(id) {
            self.windows[at].frame = frame;
            self.windows[at].collapsed = None;
        }
    }

    /// Swap the places of two windows in the tiling order.
    pub fn swap(&mut self, a: usize, b: usize) -> bool {
        let (Some(i), Some(j)) = (self.tile_order.iter().position(|x| *x == a), self.tile_order.iter().position(|x| *x == b)) else {
            return false;
        };
        self.tile_order.swap(i, j);
        true
    }

    /// Fill `area` with a window (on top), or give it back the frame it had.
    /// Returns its new frame.
    pub fn toggle_full(&mut self, id: usize, area: Rect) -> Option<Rect> {
        let at = self.position(id)?;
        let cell = self.cell;
        let window = &mut self.windows[at];
        window.collapsed = None;
        window.frame = match window.saved.take() {
            Some(frame) => frame,
            None => {
                window.saved = Some(window.frame);
                tile_frames(1, area, cell)[0]
            }
        };
        let frame = window.frame;
        self.raise(id);
        Some(frame)
    }

    /// Draw the background and every window frame. The owners then draw
    /// their content again.
    pub fn draw(&self, canvas: &Canvas) {
        self.draw_with(canvas, None, |_, _| {});
    }

    /// The same, for windows that overlap: after each frame, back to front,
    /// `content` is called with the window's number and the window, to
    /// draw what is inside before the next window goes over it.
    /// `wallpaper`: a picture of the screen's size behind everything, instead of the plain color.
    pub fn draw_with(&self, canvas: &Canvas, wallpaper: Option<&Canvas>, mut content: impl FnMut(usize, &Window)) {
        match wallpaper {
            Some(picture) => canvas.blit(picture, 0, 0),
            None => canvas.fill_rect(canvas.bounds(), BACKGROUND),
        }
        for (label, rect) in &self.buttons {
            canvas.fill_rect(*rect, BORDER_COLOR);
            let inside = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);
            canvas.fill_rect(inside, TITLE_BAR);
            canvas.text(rect.x + GLYPH_WIDTH, rect.y + 4, label, TITLE_TEXT, TITLE_BAR, rect.width);
        }
        if let Some(rect) = self.clock_place(&self.clock) {
            canvas.fill_rect(rect, BORDER_COLOR);
            canvas.text(rect.x + GLYPH_WIDTH, rect.y + 4, &self.clock, TITLE_TEXT, BORDER_COLOR, rect.width);
        }
        for (i, window) in self.windows.iter().enumerate() {
            window.draw(canvas, i + 1 == self.windows.len());
            content(self.ids[i], window);
            window.draw_scroll_bar(canvas);
            if !self.tiling {
                window.draw_grip(canvas);
            }
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
            resize: None,
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
        let old = self.pointer.unwrap_or(Pointer { x: screen.width / 2, y: screen.height / 2, buttons: 0, grab: None, resize: None });
        let step = |at: usize, by: i32, size: usize| {
            (at as i64 + by as i64).clamp(0, size.saturating_sub(1) as i64) as usize
        };
        let mut new = Pointer { x: step(old.x, dx, screen.width), y: step(old.y, dy, screen.height), buttons, grab: old.grab, resize: old.resize };
        let (was_down, is_down) = (old.buttons & 1 != 0, buttons & 1 != 0);
        let inside = |r: Rect| new.x >= r.x && new.x < r.x + r.width && new.y >= r.y && new.y < r.y + r.height;
        let (mut raised, mut close, mut click, mut launch) = (false, None, None, None);
        if !is_down {
            new.grab = None;
            if new.resize.take().is_some() {
                // Let go after resizing: its owner lays out the content again.
                self.pointer = Some(new);
                if let Some(id) = self.top() {
                    return MouseEffect::Resized(id);
                }
            }
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
            } else if !self.tiling && top.and_then(|w| w.collapse_box()).is_some_and(inside) {
                if let Some(window) = self.windows.last_mut() {
                    window.toggle_collapsed();
                }
                self.pointer = Some(new);
                return MouseEffect::WindowMoved;
            } else if let (Some(bar), Some(thumb)) = (top.and_then(|w| w.scroll_bar()), top.and_then(|w| w.scroll_thumb())) {
                if inside(bar) && !inside(thumb) {
                    let up = new.y < thumb.y;
                    self.pointer = Some(new);
                    if let Some(id) = self.top() {
                        return MouseEffect::Scroll { id, up };
                    }
                }
                if !inside(bar) {
                    new.grab = top.filter(|_| !self.tiling).and_then(|w| {
                        let f = w.frame;
                        (new.y < f.y + BORDER + TITLE_HEIGHT).then(|| (new.x - f.x, new.y - f.y))
                    });
                    click = top.map(|w| w.content()).filter(|c| inside(*c)).map(|c| (new.x - c.x, new.y - c.y));
                    if let Some(w) = top.filter(|w| !self.tiling && w.grip().is_some_and(inside)) {
                        new.resize = Some((w.frame, new.x, new.y));
                        click = None;
                    }
                }
            } else if let Some(w) = top.filter(|w| !self.tiling && w.grip().is_some_and(inside)) {
                new.resize = Some((w.frame, new.x, new.y));
            } else {
                new.grab = top.filter(|_| !self.tiling).and_then(|w| {
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
        if let Some((start, sx, sy)) = new.resize {
            let frame = self.resized_frame(start, new.x as i64 - sx as i64, new.y as i64 - sy as i64, screen);
            if let Some(window) = self.windows.last_mut() {
                if frame != window.frame {
                    window.frame = frame;
                    return MouseEffect::WindowMoved;
                }
            }
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

/// How much of the top of the screen the row of desktop buttons takes.
pub fn top_bar_height() -> usize {
    BUTTONS_AT.1 + BUTTON_HEIGHT
}

/// Room between tiled windows, and around them.
pub const GAP: usize = 8;

/// Where `n` tiled windows go in `area`, oldest first (Hyprland's
/// "dwindle", as on Omarchy): the first takes the left half, the second
/// the top half of what is left, the third the left half of the rest, and
/// so on; the last takes all that remains. `GAP` pixels between and around
/// them; each content area is whole characters (`cell`) wide and high.
pub fn tile_frames(n: usize, area: Rect, cell: (usize, usize)) -> Vec<Rect> {
    let mut frames = Vec::new();
    let mut rest = Rect::new(area.x + GAP, area.y + GAP, area.width.saturating_sub(2 * GAP), area.height.saturating_sub(2 * GAP));
    for i in 0..n {
        let mut take = rest;
        if i + 1 < n {
            if i % 2 == 0 {
                // Side by side: this one on the left.
                let w = rest.width.saturating_sub(GAP) / 2;
                take = Rect::new(rest.x, rest.y, w, rest.height);
                rest = Rect::new(rest.x + w + GAP, rest.y, rest.width.saturating_sub(w + GAP), rest.height);
            } else {
                // One above the other: this one on top.
                let h = rest.height.saturating_sub(GAP) / 2;
                take = Rect::new(rest.x, rest.y, rest.width, h);
                rest = Rect::new(rest.x, rest.y + h + GAP, rest.width, rest.height.saturating_sub(h + GAP));
            }
        }
        frames.push(snap(take, cell));
    }
    frames
}

/// A frame no larger than `room`, its content whole characters (and at
/// least one), in the top left of `room`.
fn snap(room: Rect, (cw, ch): (usize, usize)) -> Rect {
    let (extra_w, extra_h) = frame_size(0, 0);
    let cols = (room.width.saturating_sub(extra_w) / cw.max(1)).max(1);
    let rows = (room.height.saturating_sub(extra_h) / ch.max(1)).max(1);
    Rect::new(room.x, room.y, cols * cw + extra_w, rows * ch + extra_h)
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
