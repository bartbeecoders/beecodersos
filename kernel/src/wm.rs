//! A simple window system: a desktop with a plain background and windows
//! with a border and a title bar. It only knows where windows are and how
//! their frames look; what is inside a window is drawn by its owner (for the
//! terminal: the framebuffer console, fbcon.rs, in the window's content area).
//!
//! There is no mouse yet (USB mouse: U5 in Docs/drivers-todo.html), so
//! windows cannot be moved, and the only window is the terminal.

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
        if inner_width > 2 * GLYPH_WIDTH + box_size {
            let x = bar.x + inner_width - GLYPH_WIDTH - box_size;
            canvas.fill_rect(Rect::new(x, bar.y + 6, box_size, box_size), CLOSE_BOX);
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

pub struct Desktop {
    /// Back to front: the last window is on top and has the focus.
    windows: Vec<Window>,
}

impl Desktop {
    pub fn new() -> Self {
        Desktop { windows: Vec::new() }
    }

    /// Add a window on top of the others; returns its number.
    pub fn open(&mut self, title: &str, frame: Rect) -> usize {
        self.windows.push(Window { title: String::from(title), frame });
        self.windows.len() - 1
    }

    pub fn window(&self, id: usize) -> Option<&Window> {
        self.windows.get(id)
    }

    /// Draw the background and every window frame. The owners then draw
    /// their content again.
    pub fn draw(&self, canvas: &Canvas) {
        canvas.fill_rect(canvas.bounds(), BACKGROUND);
        for (i, window) in self.windows.iter().enumerate() {
            window.draw(canvas, i + 1 == self.windows.len());
        }
    }
}

/// Where the terminal window goes on a screen of this size: in the middle,
/// with some desktop around it, and a content area of whole characters.
/// `None`: the screen is too small for a window.
pub fn terminal_frame(width: usize, height: usize) -> Option<Rect> {
    let margin = (width.min(height) / 16).max(8);
    let extra_width = 2 * (BORDER + PADDING);
    let extra_height = extra_width + TITLE_HEIGHT;
    let cols = width.checked_sub(2 * margin + extra_width)? / GLYPH_WIDTH;
    let rows = height.checked_sub(2 * margin + extra_height)? / GLYPH_HEIGHT;
    if cols < MIN_COLS || rows < MIN_ROWS {
        return None;
    }
    let (w, h) = (cols * GLYPH_WIDTH + extra_width, rows * GLYPH_HEIGHT + extra_height);
    Some(Rect::new((width - w) / 2, (height - h) / 2, w, h))
}
