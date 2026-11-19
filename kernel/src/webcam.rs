//! The camera window (`camera view`): what the USB camera sees, live, as in
//! a mirror. Each picture the usb thread puts together (usb::CAMERA_PICTURE)
//! is decoded on every processor and drawn; pictures that come while one is
//! being drawn are skipped, so the window always shows the latest.
//! Keys: `s` saves the picture as a JPEG next to the others, `m` turns the
//! mirror off and on, `q` closes the window (and switches the camera off).

use crate::input::{self, Key};
use crate::storage::Storage;
use crate::usb::{self, CameraState, Picture};
use crate::{console, image, session, thread, time};
use alloc::format;
use alloc::string::String;

/// A camera picture as pixels; `None` when it is damaged.
pub fn pixels(picture: &Picture) -> Option<image::Image> {
    match picture.kind {
        beenet::uvc::Kind::Mjpeg => image::decode(&picture.data).ok(),
        _ => {
            let (w, h) = (picture.width as usize, picture.height as usize);
            (picture.data.len() >= w * h * 2)
                .then(|| image::Image { width: w, height: h, pixels: beenet::uvc::yuy2_to_rgb(&picture.data, w, h) })
        }
    }
}

/// Left and right swapped, as in a mirror.
pub fn mirror(img: &mut image::Image) {
    for row in img.pixels.chunks_mut(img.width.max(1)) {
        row.reverse();
    }
}

/// The picture as a JPEG file: the camera's own when it sent JPEG (with the
/// standard tables written in), else encoded here.
pub fn as_jpeg(picture: &Picture, img: &image::Image) -> alloc::vec::Vec<u8> {
    if picture.kind == beenet::uvc::Kind::Mjpeg {
        return beenet::jpeg::with_tables(&picture.data);
    }
    let w = img.width;
    image::encode_jpeg(img.width, img.height, 90, &|x, y| img.pixels[y * w + x])
}

/// The thread of a camera window.
pub fn run(window: usize, folder: String, mut storage: Storage) {
    if usb::CAMERAS.get().is_empty() {
        console::pane_write(window, "\n No camera is plugged in.\n Plug a USB webcam in, then click Camera again.\n q closes this window.\n");
        while console::pane_exists(window) {
            thread::pause(|| input::pending() || !console::pane_exists(window));
            if input::pending() && matches!(input::read_key(), Key::Char('q') | Key::Ctrl('q')) {
                break;
            }
        }
        return;
    }
    *usb::CAMERA_STATE.get() = CameraState::Off;
    *usb::CAMERA_PICTURE.get() = None;
    *usb::CAMERA_WANT.get() = Some((640, 480));
    let (mut last, mut mirrored, mut saved) = (0u32, true, 0u32);
    let mut current: Option<Picture> = None;
    let mut deadline = time::uptime_ms() + 5000;
    loop {
        thread::pause(|| {
            input::pending()
                || !console::pane_exists(window)
                || time::uptime_ms() >= deadline
                || matches!(*usb::CAMERA_STATE.get(), CameraState::Failed(_))
                || usb::CAMERA_PICTURE.get().as_ref().is_some_and(|p| p.seq != last)
        });
        if !console::pane_exists(window) {
            break;
        }
        if let CameraState::Failed(why) = usb::CAMERA_STATE.get().clone() {
            console::pane_write(window, &format!("\n The camera does not work: {why}\n q closes this window.\n"));
            deadline = u64::MAX;
        }
        let picture = usb::CAMERA_PICTURE.get().as_ref().filter(|p| p.seq != last).cloned();
        if let Some(picture) = picture {
            last = picture.seq;
            deadline = time::uptime_ms() + 5000;
            if let Some(mut img) = pixels(&picture) {
                if mirrored {
                    mirror(&mut img);
                }
                if console::pane_picture(window, &img).is_none() {
                    break;
                }
                current = Some(picture);
            }
        } else if time::uptime_ms() >= deadline {
            console::pane_write(window, "\n No picture from the camera for 5 seconds.\n q closes this window.\n");
            deadline = u64::MAX;
        }
        if input::pending() {
            match input::read_key() {
                Key::Char('q') | Key::Ctrl('q') => break,
                Key::Ctrl('d') if session::current().is_none_or(session::is_closed) => break,
                Key::Char('m') => mirrored = !mirrored,
                Key::Char('s') => {
                    if let Some(picture) = &current {
                        if let Some(img) = pixels(picture) {
                            saved += 1;
                            let path = format!("{}/camera-{}-{saved}.jpg", folder.trim_end_matches('/'), time::uptime_ms() / 1000);
                            let _ = storage.fs().write_file(&path, &as_jpeg(picture, &img));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    *usb::CAMERA_WANT.get() = None;
}
