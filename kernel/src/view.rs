//! The picture viewer (`view FILE` or `view FOLDER`): a window that shows a
//! JPEG, PNG, GIF, BMP or PPM picture from the disk, as large as fits, and
//! plays an animated GIF (space pauses and goes on, `n` shows the next
//! frame). The arrow keys (and Page Up / Page Down) go to the previous and
//! next picture in the same folder, in name order, round and round; `s`
//! starts and stops a slideshow (the next picture every 5 seconds). `q` leaves.

use crate::input::{self, Key};
use crate::storage::Storage;
use crate::{console, image, session, thread, time};
use alloc::string::String;
use alloc::vec::Vec;
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

/// What ended the showing of one picture.
enum Then {
    Leave,
    Step(i32),
}

/// Which way a key moves through the folder.
fn step_of(key: &Key) -> Option<i32> {
    match key {
        Key::Right | Key::Down | Key::PageDown => Some(1),
        Key::Left | Key::Up | Key::PageUp => Some(-1),
        _ => None,
    }
}

/// The picture `step` places from `current` among `names` (only pictures
/// count, in name order, round and round); `None` when there is no other.
pub fn neighbour(names: &[String], current: &str, step: i32) -> Option<String> {
    let mut pictures: Vec<&String> = names.iter().filter(|n| crate::filemgr::opener(n) == crate::filemgr::Opener::Picture).collect();
    pictures.sort();
    let at = pictures.iter().position(|n| n.as_str() == current)?;
    if pictures.len() < 2 {
        return None;
    }
    let len = pictures.len() as i32;
    Some(pictures[(at as i32 + step).rem_euclid(len) as usize].clone())
}

/// The first picture in a folder (name order), for `view FOLDER`.
pub fn first_in(names: &[String]) -> Option<String> {
    let mut pictures: Vec<&String> = names.iter().filter(|n| crate::filemgr::opener(n) == crate::filemgr::Opener::Picture).collect();
    pictures.sort();
    pictures.first().map(|n| (*n).clone())
}

/// The thread of a viewer window: shows the picture at `path`, and the
/// others in its folder on the arrow keys, until `q` or the window closes.
pub fn run(window: usize, picture: Picture, path: String, mut storage: Storage) {
    let (folder, mut name) = match path.rsplit_once('/') {
        Some((f, n)) => (String::from(if f.is_empty() { "/" } else { f }), String::from(n)),
        None => (String::from("/"), path.clone()),
    };
    let mut picture = Some(picture);
    let mut slideshow = false;
    loop {
        let then = match picture.take() {
            Some(Picture::Still(still)) => show(window, &still, &mut slideshow),
            Some(Picture::Moving(moving)) => play(window, &moving),
            None => wait_for_key(window),
        };
        let Then::Step(step) = then else { return };
        let names: Vec<String> = storage.fs().read_dir(&folder).map(|es| es.into_iter().map(|e| e.name).collect()).unwrap_or_default();
        // Pictures that do not decode are passed over (at most one lap).
        let mut tries = names.len();
        let mut at = name.clone();
        while tries > 0 {
            tries -= 1;
            let Some(next) = neighbour(&names, &at, step) else { break };
            at = next;
            if let Ok(p) = load_any(&mut storage, &crate::filemgr::join(&folder, &at)) {
                picture = Some(p);
                name = at.clone();
                console::pane_title(window, &alloc::format!("View: {name}"));
                break;
            }
        }
        if picture.is_none() {
            // No other picture: show this one again.
            picture = load_any(&mut storage, &crate::filemgr::join(&folder, &name)).ok();
        }
    }
}

fn wait_for_key(window: usize) -> Then {
    loop {
        if !console::pane_exists(window) {
            return Then::Leave;
        }
        let key = input::read_key();
        if leaves(&key) {
            return Then::Leave;
        }
        if let Some(step) = step_of(&key) {
            return Then::Step(step);
        }
    }
}

/// How long a slideshow shows each picture.
pub const SLIDE_MS: u64 = 5000;

/// One still picture until a key (or the slideshow) says what next. Any
/// other key shows it again.
fn show(window: usize, still: &image::Image, slideshow: &mut bool) -> Then {
    if console::pane_picture(window, still).is_none() {
        return Then::Leave;
    }
    let mut due = time::uptime_ms() + SLIDE_MS;
    loop {
        let on = *slideshow;
        thread::wait_until(|| input::pending() || !console::pane_exists(window) || (on && time::uptime_ms() >= due));
        if !console::pane_exists(window) {
            return Then::Leave;
        }
        if !input::pending() {
            if on && time::uptime_ms() >= due {
                return Then::Step(1); // the slideshow's next picture
            }
            continue;
        }
        let key = input::read_key();
        if leaves(&key) {
            return Then::Leave;
        }
        if let Some(step) = step_of(&key) {
            return Then::Step(step);
        }
        if key == Key::Char('s') {
            *slideshow = !*slideshow;
            due = time::uptime_ms() + SLIDE_MS;
        }
        if console::pane_picture(window, still).is_none() {
            return Then::Leave;
        }
    }
}

/// Play an animation, frame after frame at the speed the file asks for,
/// round and round.
fn play(window: usize, moving: &gif::Gif) -> Then {
    let mut player = gif::Player::new(moving);
    let mut paused = false;
    let mut step = true; // paint the next frame now
    let mut due = time::uptime_ms();
    loop {
        if step {
            let (pixels, delay) = player.advance();
            if console::pane_pixels(window, moving.width, moving.height, pixels).is_none() {
                return Then::Leave;
            }
            due = time::uptime_ms() + delay as u64;
            step = false;
        }
        thread::pause(|| input::pending() || (!paused && time::uptime_ms() >= due) || !console::pane_exists(window));
        if !console::pane_exists(window) {
            return Then::Leave;
        }
        if input::pending() {
            let key = input::read_key();
            if let Some(step) = step_of(&key) {
                return Then::Step(step);
            }
            match key {
                _ if leaves(&key) => return Then::Leave,
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
