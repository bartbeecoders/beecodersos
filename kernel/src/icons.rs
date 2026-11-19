//! The file manager's icon view (`v` switches): a grid of icons, a
//! picture's own thumbnail for pictures, drawn in the window's pixels.
//!
//! The thumbnails of a folder are made on every processor at once: the
//! files are read one after the other (the disk is one), then decoded and
//! shrunk in parallel. Layout and choosing (`Grid`, `step`, `cell_at`) and
//! shrinking (`thumbnail`) need no screen and are covered by self-tests.

use crate::filemgr::{self, Browser, Entry, Opener};
use crate::gfx::{Canvas, Rect};
use crate::image::Image;
use crate::input::Key;
use crate::storage::Storage;
use alloc::string::String;
use alloc::vec::Vec;

pub const CELL_W: usize = 112;
pub const CELL_H: usize = 104;
/// Thumbnails fit in a square this large.
pub const THUMB: usize = 64;
const TOP: usize = 22;
const BOTTOM: usize = 20;
/// Pictures larger than this (pixels, or bytes in the file) get the plain picture icon.
const MAX_PIXELS: usize = 2 * 1024 * 1024;
const MAX_BYTES: u64 = 8 * 1024 * 1024;

const BACKGROUND: u32 = 0x1C2230;
const BAR: u32 = 0x2B3A55;
const WARNING: u32 = 0x8A3A2A;
const CHOSEN: u32 = 0x3A6EA5;
const LABEL: u32 = 0xE8E8E8;
const HELP: u32 = 0x9AA4B8;

/// How the grid fits in a window of `width` x `height` pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    /// Space on the left so that the grid is in the middle.
    pub left: usize,
}

impl Grid {
    pub fn new(width: usize, height: usize) -> Grid {
        let cols = (width / CELL_W).max(1);
        let rows = (height.saturating_sub(TOP + BOTTOM) / CELL_H).max(1);
        Grid { cols, rows, left: width.saturating_sub(cols * CELL_W) / 2 }
    }

    /// The first entry shown, so that `chosen` is on screen (whole rows;
    /// `top` is where it was before).
    pub fn top_for(&self, chosen: usize, top: usize) -> usize {
        let (row, top_row) = (chosen / self.cols, top / self.cols);
        let first = if row < top_row {
            row
        } else if row >= top_row + self.rows {
            row + 1 - self.rows
        } else {
            top_row
        };
        first * self.cols
    }

    /// The entry under the pixel (x, y), with `top` the first one shown.
    pub fn cell_at(&self, x: usize, y: usize, top: usize, count: usize) -> Option<usize> {
        if x < self.left || y < TOP {
            return None;
        }
        let (col, row) = ((x - self.left) / CELL_W, (y - TOP) / CELL_H);
        if col >= self.cols || row >= self.rows {
            return None;
        }
        let at = top + row * self.cols + col;
        (at < count).then_some(at)
    }
}

/// Where the arrow keys move the choice in a grid of `cols` columns and
/// `count` entries; `None` for other keys.
pub fn step(chosen: usize, key: Key, cols: usize, rows: usize, count: usize) -> Option<usize> {
    let last = count.saturating_sub(1);
    Some(match key {
        Key::Left => chosen.saturating_sub(1),
        Key::Right => (chosen + 1).min(last),
        Key::Up if chosen >= cols => chosen - cols,
        Key::Up => chosen,
        Key::Down if chosen + cols <= last => chosen + cols,
        // On the last row already, or the row below is short: the last one.
        Key::Down => if chosen / cols < last / cols { last } else { chosen },
        Key::PageUp => chosen.saturating_sub(cols * rows),
        Key::PageDown => (chosen + cols * rows).min(last),
        Key::Home => 0,
        Key::End => last,
        _ => return None,
    })
}

/// A picture shrunk to fit in `THUMB` x `THUMB` (never made larger): each
/// pixel the average of the ones it covers.
pub fn thumbnail(picture: &Image) -> Image {
    let (w, h) = (picture.width.max(1), picture.height.max(1));
    let (tw, th) = if w >= h { (THUMB.min(w), (h * THUMB.min(w) / w).max(1)) } else { ((w * THUMB.min(h) / h).max(1), THUMB.min(h)) };
    let mut pixels = Vec::with_capacity(tw * th);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1));
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for &p in &picture.pixels[y * w + x0..y * w + x1] {
                    r += p >> 16 & 0xFF;
                    g += p >> 8 & 0xFF;
                    b += p & 0xFF;
                    n += 1;
                }
            }
            pixels.push((r / n) << 16 | (g / n) << 8 | b / n);
        }
    }
    Image { width: tw, height: th, pixels }
}

/// The thumbnails of a folder's pictures, in the order of its entries.
pub struct Thumbs {
    folder: String,
    names: Vec<String>,
    images: Vec<Option<Image>>,
}

impl Thumbs {
    pub fn new() -> Thumbs {
        Thumbs { folder: String::new(), names: Vec::new(), images: Vec::new() }
    }

    /// Make them for what the browser shows, unless they are already made.
    pub fn update(&mut self, storage: &mut Storage, browser: &Browser) {
        let names: Vec<String> = browser.entries().iter().map(|e| e.name.clone()).collect();
        if self.folder == browser.folder() && self.names == names {
            return;
        }
        // Read the pictures that are not too large (one disk: one at a time).
        let files: Vec<Option<Vec<u8>>> = browser
            .entries()
            .iter()
            .map(|e| {
                let path = filemgr::join(browser.folder(), &e.name);
                let wanted = !e.folder && e.size <= MAX_BYTES && filemgr::opener(&e.name) == Opener::Picture;
                let data = if wanted { storage.fs().read_file(&path).ok() } else { None };
                data.filter(|d| crate::image::dimensions(d).is_some_and(|(w, h)| w * h <= MAX_PIXELS))
            })
            .collect();
        // Decode and shrink them on every processor.
        let mut images: Vec<Option<Image>> = (0..files.len()).map(|_| None).collect();
        let out = crate::smp::Disjoint(images.as_mut_ptr());
        let files = &files;
        crate::smp::run(files.len(), &|i| {
            let thumb = files[i].as_deref().and_then(|d| crate::image::decode(d).ok()).map(|p| thumbnail(&p));
            // SAFETY: each job sets its own entry.
            unsafe { *out.get().add(i) = thumb };
        });
        self.folder = String::from(browser.folder());
        self.names = names;
        self.images = images;
    }
}

/// Draw the icon view of `browser` (first entry shown: `top`).
pub fn draw(canvas: &Canvas, browser: &Browser, thumbs: &Thumbs, top: usize) {
    let (width, height) = (canvas.width(), canvas.height());
    canvas.fill_rect(canvas.bounds(), BACKGROUND);
    let (status, warning) = browser.status();
    let bar = if warning { WARNING } else { BAR };
    canvas.fill_rect(Rect::new(0, 0, width, TOP - 4), bar);
    canvas.text(4, 1, status.trim_start(), LABEL, bar, width.saturating_sub(8));
    let grid = Grid::new(width, height);
    for (i, entry) in browser.entries().iter().enumerate().skip(top).take(grid.cols * grid.rows) {
        let (col, row) = ((i - top) % grid.cols, (i - top) / grid.cols);
        let (x, y) = (grid.left + col * CELL_W, TOP + row * CELL_H);
        let chosen = i == browser.chosen_index();
        if chosen {
            canvas.fill_rect(Rect::new(x + 2, y, CELL_W - 4, CELL_H - 4), CHOSEN);
        }
        let back = if chosen { CHOSEN } else { BACKGROUND };
        let (ix, iy) = (x + (CELL_W - THUMB) / 2, y + 6);
        match thumbs.images.get(i).and_then(|t| t.as_ref()) {
            Some(thumb) => draw_thumbnail(canvas, thumb, ix, iy),
            None => draw_icon(canvas, entry, ix, iy),
        }
        // The name, cut to fit, in the middle under the icon.
        let max = (CELL_W - 8) / 8;
        let mut name: String = entry.name.chars().take(max).collect();
        if entry.name.chars().count() > max {
            name.pop();
            name.push('~');
        }
        let nx = x + (CELL_W - name.chars().count() * 8) / 2;
        canvas.text(nx, y + THUMB + 14, &name, LABEL, back, CELL_W);
    }
    if browser.entries().is_empty() {
        canvas.text(16, TOP + 16, "This folder is empty.", HELP, BACKGROUND, width);
    }
    let help = " arrows choose  Enter open  e edit  Backspace up  v list  n new folder  r rename  d delete  q leave";
    canvas.fill_rect(Rect::new(0, height.saturating_sub(BOTTOM - 2), width, BOTTOM - 2), BAR);
    canvas.text(0, height.saturating_sub(BOTTOM - 1), help, HELP, BAR, width);
}

fn draw_thumbnail(canvas: &Canvas, thumb: &Image, x: usize, y: usize) {
    let (ox, oy) = (x + (THUMB - thumb.width) / 2, y + (THUMB - thumb.height) / 2);
    canvas.fill_rect(Rect::new(ox.saturating_sub(1), oy.saturating_sub(1), thumb.width + 2, thumb.height + 2), 0xD0D0D0);
    for row in 0..thumb.height {
        canvas.put_row(ox, oy + row, &thumb.pixels[row * thumb.width..(row + 1) * thumb.width]);
    }
}

/// The drawn icons: a folder, a page of text, a picture, a sound.
fn draw_icon(canvas: &Canvas, entry: &Entry, x: usize, y: usize) {
    let r = |dx: usize, dy: usize, w: usize, h: usize, c: u32| canvas.fill_rect(Rect::new(x + dx, y + dy, w, h), c);
    if entry.folder {
        r(4, 10, 24, 8, 0xD9A63A); // the tab
        r(4, 16, 56, 40, 0xF2C14E);
        r(4, 52, 56, 4, 0xD9A63A);
        return;
    }
    match filemgr::opener(&entry.name) {
        Opener::Picture => {
            r(6, 8, 52, 48, 0xD0D0D0);
            r(8, 10, 48, 44, 0x6FA8DC); // sky
            r(8, 40, 48, 14, 0x5B8C3A); // grass
            for k in 0..14 {
                r(18 + k, 40 - k, 28 - 2 * k.min(14), 1, 0x7A6A55); // a hill
            }
            r(42, 14, 8, 8, 0xF6D04D); // the sun
        }
        Opener::Sound => {
            r(8, 8, 48, 48, 0x4A3A6A);
            r(18, 26, 8, 12, 0xE8E8E8); // a speaker
            for k in 0..6 {
                r(26 + 2 * k, 26 - k, 2, 12 + 2 * k, 0xE8E8E8); // its cone
            }
            r(42, 24, 3, 16, 0xB8A8E8); // sound waves
            r(48, 20, 3, 24, 0xB8A8E8);
        }
        Opener::Text => {
            r(12, 4, 40, 56, 0xF4F4F4);
            for line in 0..6 {
                r(18, 12 + line * 7, if line % 3 == 2 { 18 } else { 28 }, 2, 0x8890A0);
            }
        }
    }
}
