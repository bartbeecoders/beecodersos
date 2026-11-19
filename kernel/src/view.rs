//! The picture viewer (`view FILE`): a window that shows a PNG, GIF, BMP or
//! PPM picture from the disk, as large as fits, and plays an animated GIF
//! (space pauses and goes on, `n` shows the next frame). `q` leaves.

use crate::input::{self, Key};
use crate::storage::Storage;
use crate::{console, image, session, thread, time};
use alloc::string::String;
use beenet::gif;

/// Read and decode a picture file (an animated GIF: its first frame).
pub fn load(storage: &mut Storage, path: &str) -> Result<image::Image, String> {
    let data = storage.fs().read_file(path).map_err(|e| alloc::format!("{e}"))?;
    image::decode(&data).map_err(String::from)
}

/// What a viewer shows: one picture, or an animation.
pub enum Picture {
    Still(image::Image),
    Moving(gif::Gif),
}

impl Picture {
    pub fn size(&self) -> (usize, usize) {
        match self {
            Picture::Still(p) => (p.width, p.height),
            Picture::Moving(g) => (g.width, g.height),
        }
    }

    /// Frames (1 for a still picture).
    pub fn frames(&self) -> usize {
        match self {
            Picture::Still(_) => 1,
            Picture::Moving(g) => g.frames.len(),
        }
    }
}

/// Read a picture file for the viewer: a GIF with more than one frame stays an animation.
pub fn load_any(storage: &mut Storage, path: &str) -> Result<Picture, String> {
    let data = storage.fs().read_file(path).map_err(|e| alloc::format!("{e}"))?;
    if gif::is_gif(&data) {
        let moving = gif::decode(&data).map_err(String::from)?;
        if moving.frames.len() > 1 {
            return Ok(Picture::Moving(moving));
        }
    }
    image::decode(&data).map(Picture::Still).map_err(String::from)
}

/// Is this a key that ends the viewer?
fn leaves(key: &Key) -> bool {
    match key {
        Key::Char('q') | Key::Ctrl('q') => true,
        Key::Ctrl('d') => session::current().is_none_or(session::is_closed),
        _ => false,
    }
}

/// The thread of a viewer window: shows the picture until `q`, or until
/// the window is closed. Any other key shows it again.
pub fn run(window: usize, picture: Picture) {
    let still = match picture {
        Picture::Still(still) => still,
        Picture::Moving(moving) => return play(window, &moving),
    };
    loop {
        if console::pane_picture(window, &still).is_none() {
            return;
        }
        if leaves(&input::read_key()) {
            return;
        }
    }
}

/// Play an animation, frame after frame at the speed the file asks for,
/// round and round.
fn play(window: usize, moving: &gif::Gif) {
    let mut player = gif::Player::new(moving);
    let mut paused = false;
    let mut step = true; // paint the next frame now
    let mut due = time::uptime_ms();
    loop {
        if step {
            let (pixels, delay) = player.advance();
            if console::pane_pixels(window, moving.width, moving.height, pixels).is_none() {
                return;
            }
            due = time::uptime_ms() + delay as u64;
            step = false;
        }
        thread::pause(|| input::pending() || (!paused && time::uptime_ms() >= due) || !console::pane_exists(window));
        if !console::pane_exists(window) {
            return;
        }
        if input::pending() {
            let key = input::read_key();
            match key {
                _ if leaves(&key) => return,
                Key::Char(' ') => paused = !paused,
                Key::Char('n') => {
                    paused = true;
                    step = true;
                }
                _ => {}
            }
        }
        if !paused && time::uptime_ms() >= due {
            step = true;
        }
    }
}
