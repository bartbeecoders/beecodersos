//! The extra terminal windows of the desktop (`terminal`): each runs a
//! shell of its own in a console session (session.rs). One thread,
//! "desktop", looks after all of them: it brings what a shell printed to its
//! window, hands the keys to the window that has the focus, and clears up
//! when a shell ends or a window is closed.
//!
//! The windows themselves, and what they show, belong to console.rs.

use crate::input::Key;
use crate::ring::Global;
use crate::shell::Shell;
use crate::{console, keyboard, session, storage, thread};
use alloc::vec::Vec;

struct Terminal {
    window: usize,
    session: u32,
    thread: thread::ThreadId,
}

static TERMINALS: Global<Vec<Terminal>> = Global::new(Vec::new());
static STARTED: Global<bool> = Global::new(false);

/// Open a terminal window with a new shell in it; returns the window's number.
pub fn open_terminal() -> Result<usize, &'static str> {
    let storage = storage::handle().ok_or("no file system is mounted")?;
    let session = session::open().ok_or("too many sessions")?;
    let window = match console::open_pane("Terminal") {
        Ok(window) => window,
        Err(why) => {
            session::remove(session);
            return Err(why);
        }
    };
    let Some(thread) = thread::spawn("terminal", Some(session), move || Shell::remote(storage).run_session()) else {
        console::close_pane(window);
        session::remove(session);
        return Err("could not start a thread");
    };
    TERMINALS.get().push(Terminal { window, session, thread });
    if !*STARTED.get() {
        *STARTED.get() = thread::spawn("desktop", None, serve).is_some();
    }
    Ok(window)
}

/// How many terminal windows are open.
pub fn count() -> usize {
    TERMINALS.get().len()
}

/// A key from the machine's keyboard: `Some` when it is for the console,
/// `None` when it went elsewhere. Alt+Tab gives the focus to the next
/// window; other keys go to the window that has the focus.
pub fn route(key: Key) -> Option<Key> {
    if key == Key::NextWindow {
        console::next_window();
        return None;
    }
    let Some(window) = console::focused_pane() else { return Some(key) };
    if let Some(terminal) = TERMINALS.get().iter().find(|t| t.window == window) {
        session::push_key(terminal.session, key);
    }
    None
}

fn work_waiting() -> bool {
    console::closing()
        || (console::focused_pane().is_some() && keyboard::pending())
        || TERMINALS.get().iter().any(|t| session::has_output(t.session) || !thread::is_alive(t.thread) || !console::pane_exists(t.window))
}

fn serve() {
    loop {
        thread::pause(work_waiting);
        step();
        // The shells get their turn even while there is more to do (a shell
        // that was told to end must run to do so).
        thread::yield_now();
    }
}

/// One round of looking after the terminal windows.
fn step() {
    // While another window has the focus, the keyboard is read here: the
    // console's shell may be busy and not asking for keys.
    while console::focused_pane().is_some() {
        let Some(key) = keyboard::poll() else { break };
        route(key);
    }
    for window in console::take_closing() {
        if let Some(terminal) = TERMINALS.get().iter().find(|t| t.window == window) {
            session::hang_up(terminal.session);
        }
    }
    let mut i = 0;
    while i < TERMINALS.get().len() {
        let (window, session, thread) = {
            let t = &TERMINALS.get()[i];
            (t.window, t.session, t.thread)
        };
        let text = session::take_output(session);
        if !text.is_empty() {
            console::pane_write(window, &text);
        }
        if !thread::is_alive(thread) {
            // The shell has ended (`exit`, or its window was closed).
            console::close_pane(window);
            session::remove(session);
            TERMINALS.get().remove(i);
            continue;
        }
        if !console::pane_exists(window) {
            // The window is gone (the screen was laid out anew): end the shell.
            session::hang_up(session);
        }
        i += 1;
    }
}
