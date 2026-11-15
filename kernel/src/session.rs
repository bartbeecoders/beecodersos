//! Console sessions: a shell that is not on the real screen and keyboard.
//!
//! A thread started with a session (see `thread::spawn`) prints into the
//! session's output buffer and reads its keys from the session's input queue.
//! Whoever owns the session (a test now, a network connection in phase N6)
//! feeds keys in and takes the output out.

use crate::input::Key;
use crate::ring::Global;
use crate::thread;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

/// Output nobody collects is cut off here (the oldest text goes first).
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_INPUT: usize = 4096;
pub const MAX_SESSIONS: usize = 8;

struct Session {
    id: u32,
    input: VecDeque<Key>,
    output: String,
    /// No more input will come: the shell ends when the queue is empty.
    closed: bool,
}

static SESSIONS: Global<Vec<Session>> = Global::new(Vec::new());
static NEXT_ID: Global<u32> = Global::new(1);

fn find(id: u32) -> Option<&'static mut Session> {
    SESSIONS.get().iter_mut().find(|s| s.id == id)
}

/// The session of the running thread, if it has one.
pub fn current() -> Option<u32> {
    thread::current_session()
}

pub fn open() -> Option<u32> {
    let sessions = SESSIONS.get();
    if sessions.len() >= MAX_SESSIONS {
        return None;
    }
    let id = *NEXT_ID.get();
    *NEXT_ID.get() += 1;
    sessions.push(Session { id, input: VecDeque::new(), output: String::new(), closed: false });
    Some(id)
}

/// Forget the session (after its thread has ended).
pub fn remove(id: u32) {
    SESSIONS.get().retain(|s| s.id != id);
}

/// Type text into the session; a newline is the Enter key.
pub fn type_text(id: u32, text: &str) {
    let Some(s) = find(id) else { return };
    for c in text.chars() {
        if s.input.len() < MAX_INPUT {
            s.input.push_back(if c == '\n' { Key::Enter } else { Key::Char(c) });
        }
    }
}

/// No more input will come.
pub fn close(id: u32) {
    if let Some(s) = find(id) {
        s.closed = true;
    }
}

/// Everything printed in the session since the last call.
pub fn take_output(id: u32) -> String {
    find(id).map(|s| core::mem::take(&mut s.output)).unwrap_or_default()
}

// --- the session's own side (called through console.rs and input.rs) ------------------

pub fn write(id: u32, text: &str) {
    let Some(s) = find(id) else { return };
    s.output.push_str(text);
    if s.output.len() > MAX_OUTPUT {
        let mut cut = s.output.len() - MAX_OUTPUT;
        while !s.output.is_char_boundary(cut) {
            cut += 1;
        }
        s.output.drain(..cut);
    }
}

pub fn poll_key(id: u32) -> Option<Key> {
    find(id)?.input.pop_front()
}

pub fn pending(id: u32) -> bool {
    find(id).is_some_and(|s| !s.input.is_empty())
}

/// The next key, waiting for it; None when the session is closed and empty.
pub fn read_key(id: u32) -> Option<Key> {
    loop {
        let s = find(id)?;
        if let Some(key) = s.input.pop_front() {
            return Some(key);
        }
        if s.closed {
            return None;
        }
        thread::wait_until(|| find(id).is_none_or(|s| !s.input.is_empty() || s.closed));
    }
}
