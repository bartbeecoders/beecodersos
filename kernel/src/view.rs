//! The picture viewer (`view FILE`): a window that shows a BMP or PPM
//! picture from the disk, as large as fits. `q` leaves.

use crate::input::{self, Key};
use crate::storage::Storage;
use crate::{console, image, session};
use alloc::string::String;

/// Read and decode a picture file.
pub fn load(storage: &mut Storage, path: &str) -> Result<image::Image, String> {
    let data = storage.fs().read_file(path).map_err(|e| alloc::format!("{e}"))?;
    image::decode(&data).map_err(String::from)
}

/// The thread of a viewer window: shows the picture until `q`, or until
/// the window is closed. Any other key shows it again.
pub fn run(window: usize, picture: image::Image) {
    loop {
        if console::pane_picture(window, &picture).is_none() {
            return;
        }
        match input::read_key() {
            Key::Char('q') | Key::Ctrl('q') => return,
            Key::Ctrl('d') if session::current().is_none_or(session::is_closed) => return,
            _ => {}
        }
    }
}
