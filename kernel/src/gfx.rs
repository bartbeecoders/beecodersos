//! Drawing on a framebuffer: filled rectangles and characters. Everything
//! that puts a pixel on the screen goes through `Canvas`, which checks every
//! position, so nothing is ever written outside the framebuffer. Used by the
//! framebuffer console (fbcon.rs) and the window system (wm.rs).

use crate::display::Mode;
use crate::font::{FONT, GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::vga;

/// A rectangle in pixels; (x, y) is its top left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    pub const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Rect { x, y, width, height }
    }

    /// The part of this rectangle that lies inside `outer`.
    pub fn clip(&self, outer: &Rect) -> Rect {
        let x = self.x.clamp(outer.x, outer.x.saturating_add(outer.width));
        let y = self.y.clamp(outer.y, outer.y.saturating_add(outer.height));
        let right = self.x.saturating_add(self.width).min(outer.x.saturating_add(outer.width));
        let bottom = self.y.saturating_add(self.height).min(outer.y.saturating_add(outer.height));
        Rect { x, y, width: right.saturating_sub(x), height: bottom.saturating_sub(y) }
    }
}

#[derive(Clone, Copy)]
pub struct Canvas {
    framebuffer: *mut u8,
    mode: Mode,
    /// Only this part is drawn in (the whole canvas, unless `clipped`).
    clip: Rect,
}

impl Canvas {
    /// `framebuffer` must be valid for `mode.bytes()` bytes. A mode whose
    /// lines cannot hold their pixels gives a canvas of 0x0 pixels.
    pub fn new(framebuffer: *mut u8, mut mode: Mode) -> Self {
        if (mode.pitch as usize) < mode.width as usize * mode.format.bytes() {
            mode.width = 0;
            mode.height = 0;
        }
        let clip = Rect::new(0, 0, mode.width as usize, mode.height as usize);
        Canvas { framebuffer, mode, clip }
    }

    /// The same canvas, but drawing only changes pixels inside `rect`: to
    /// repair one part of the screen with the usual drawing code.
    pub fn clipped(&self, rect: Rect) -> Canvas {
        Canvas { clip: rect.clip(&self.clip), ..*self }
    }

    pub fn width(&self) -> usize {
        self.mode.width as usize
    }

    pub fn height(&self) -> usize {
        self.mode.height as usize
    }

    /// The whole canvas.
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width(), self.height())
    }

    /// Fill a rectangle with a 0xRRGGBB color; the part outside the canvas is left out.
    pub fn fill_rect(&self, rect: Rect, rgb: u32) {
        let r = rect.clip(&self.clip);
        let value = self.mode.format.encode(rgb);
        for y in r.y..r.y + r.height {
            for x in r.x..r.x + r.width {
                self.put(x, y, value);
            }
        }
    }

    /// Draw one character of the font (code page 437 number) with its top
    /// left corner at (x, y). `scale` 2 draws every font pixel as 2x2 (1 to
    /// 4 are accepted). A character that does not fit completely is not drawn.
    pub fn glyph(&self, x: usize, y: usize, code: u8, fg: u32, bg: u32, scale: usize) {
        let scale = scale.clamp(1, 4);
        let fits = |at: usize, size: usize, limit: usize| at.checked_add(size).is_some_and(|end| end <= limit);
        if !fits(x, GLYPH_WIDTH * scale, self.width()) || !fits(y, GLYPH_HEIGHT * scale, self.height()) {
            return;
        }
        let cell = Rect::new(x, y, GLYPH_WIDTH * scale, GLYPH_HEIGHT * scale);
        let visible = cell.clip(&self.clip);
        if visible.width == 0 || visible.height == 0 {
            return;
        }
        let whole = visible == cell;
        let inside = |px: usize, py: usize| {
            whole || (px >= visible.x && px < visible.x + visible.width && py >= visible.y && py < visible.y + visible.height)
        };
        let (fg, bg) = (self.mode.format.encode(fg), self.mode.format.encode(bg));
        for (gy, bits) in FONT[code as usize].iter().enumerate() {
            for gx in 0..GLYPH_WIDTH {
                let value = if bits & (0x80 >> gx) != 0 { fg } else { bg };
                for sy in 0..scale {
                    for sx in 0..scale {
                        let (px, py) = (x + gx * scale + sx, y + gy * scale + sy);
                        if inside(px, py) {
                            self.put(px, py, value);
                        }
                    }
                }
            }
        }
    }

    /// Draw text on one line, starting at (x, y); it stops at `max_width` pixels.
    pub fn text(&self, x: usize, y: usize, text: &str, fg: u32, bg: u32, max_width: usize) {
        for (i, c) in text.chars().take(max_width / GLYPH_WIDTH).enumerate() {
            self.glyph(x.saturating_add(i * GLYPH_WIDTH), y, vga::cp437(c), fg, bg, 1);
        }
    }

    /// Callers have checked: x < width and y < height.
    fn put(&self, x: usize, y: usize, value: u32) {
        let bytes = self.mode.format.bytes();
        let at = y * self.mode.pitch as usize + x * bytes;
        unsafe {
            let p = self.framebuffer.add(at);
            if bytes == 4 {
                (p as *mut u32).write_unaligned(value);
            } else {
                for (i, b) in value.to_le_bytes()[..bytes].iter().enumerate() {
                    p.add(i).write(*b);
                }
            }
        }
    }
}
