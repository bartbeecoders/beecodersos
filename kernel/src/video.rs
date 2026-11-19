//! The video player (`play FILE.avi`): Motion-JPEG AVI videos in a window,
//! each frame a JPEG decoded on every processor (image.rs) and shown at its
//! time; frames that would come too late are skipped. PCM sound plays on
//! the sound card at the same time, on a thread of its own. Space pauses
//! (the sound stops; it starts again at the next frame on), `q` leaves.

use crate::input::{self, Key};
use crate::{console, image, session, thread, time};
use alloc::vec::Vec;
use beenet::avi::Avi;
use core::sync::atomic::{AtomicU32, Ordering};

/// The sound that may play: a sound thread plays while this holds its number.
static SOUND: AtomicU32 = AtomicU32::new(0);

/// Play `sound` (48k frames) from `from_ms` on, until another sound or a stop.
fn start_sound(frames: &alloc::sync::Arc<Vec<u8>>, from_ms: u64) {
    let number = SOUND.fetch_add(1, Ordering::SeqCst) + 1;
    let at = ((from_ms * crate::sound::RATE as u64 / 1000) as usize * 4).min(frames.len());
    let frames = frames.clone();
    let _ = thread::spawn("sound", None, move || {
        let _ = crate::hda::play(&frames[at..], || SOUND.load(Ordering::SeqCst) != number);
    });
}

fn stop_sound() {
    SOUND.fetch_add(1, Ordering::SeqCst);
}

/// The thread of a player window.
pub fn run(window: usize, data: Vec<u8>, avi: Avi) {
    let sound = avi.sound.as_ref().filter(|_| crate::hda::available()).and_then(|s| {
        let wav = crate::sound::Wav { rate: s.rate, channels: s.channels, bits: s.bits, data: s.data.clone() };
        (wav.frames() > 0 && matches!(s.bits, 8 | 16) && (1..=2).contains(&s.channels) && (8_000..=192_000).contains(&s.rate)).then(|| alloc::sync::Arc::new(wav.to_48k()))
    });
    let frame_ms = |i: usize| i as u64 * avi.frame_us as u64 / 1000;
    let mut start = time::uptime_ms();
    let (mut i, mut paused) = (0usize, false);
    if let Some(s) = &sound {
        start_sound(s, 0);
    }
    loop {
        if !paused && i < avi.frames.len() {
            // The frame that is due now (skipping those that came too late).
            let now = time::uptime_ms() - start;
            while i + 1 < avi.frames.len() && frame_ms(i + 1) <= now {
                i += 1;
            }
            if let Some(picture) = avi.frame(&data, i).and_then(|f| image::decode(f).ok()) {
                if console::pane_picture(window, &picture).is_none() {
                    stop_sound();
                    return;
                }
            }
            i += 1;
        }
        // After the last frame (or while paused) only a key or the window's
        // end is waited for: a time already past would never let go of the
        // processor, and threads only switch when one waits.
        let ended = i >= avi.frames.len();
        let due = start + frame_ms(i);
        thread::pause(|| input::pending() || (!paused && !ended && time::uptime_ms() >= due) || !console::pane_exists(window));
        if !console::pane_exists(window) {
            stop_sound();
            return;
        }
        if input::pending() {
            match input::read_key() {
                Key::Char('q') | Key::Ctrl('q') => {
                    stop_sound();
                    return;
                }
                Key::Ctrl('d') if session::current().is_none_or(session::is_closed) => {
                    stop_sound();
                    return;
                }
                Key::Char(' ') if i < avi.frames.len() => {
                    paused = !paused;
                    if paused {
                        stop_sound();
                    } else {
                        // Go on from the frame where it stopped.
                        start = time::uptime_ms() - frame_ms(i);
                        if let Some(s) = &sound {
                            start_sound(s, frame_ms(i));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
