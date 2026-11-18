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
use crate::{console, filemgr, keyboard, session, storage, textedit, thread, web};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

struct Terminal {
    window: usize,
    session: u32,
    thread: thread::ThreadId,
    /// A shell prints, and that text goes to its window. A program that
    /// draws its window itself (the editor) has nothing to bring over.
    prints: bool,
}

static TERMINALS: Global<Vec<Terminal>> = Global::new(Vec::new());
static STARTED: Global<bool> = Global::new(false);

/// Open a terminal window with a new shell in it; returns the window's number.
pub fn open_terminal() -> Result<usize, &'static str> {
    let storage = storage::handle().ok_or("no file system is mounted")?;
    open("Terminal", "terminal", true, move |_| Shell::remote(storage).run_session())
}

/// Open a window with the text editor on a file (`path` is absolute).
pub fn open_editor(path: &str) -> Result<usize, &'static str> {
    let mut storage = storage::handle().ok_or("no file system is mounted")?;
    let text = textedit::load(&mut storage, path)?;
    let path = String::from(path);
    let name = path.rsplit('/').next().unwrap_or(&path);
    open(&format!("Edit: {name}"), "editor", false, move |window| textedit::run(window, path, text, storage))
}

/// Open a file manager window on a folder (`folder` is absolute).
pub fn open_files(folder: &str) -> Result<usize, &'static str> {
    let mut storage = storage::handle().ok_or("no file system is mounted")?;
    if !storage.fs().is_dir(folder) {
        return Err("no such folder");
    }
    let folder = String::from(folder);
    open("Files", "files", false, move |window| filemgr::run(window, folder, storage))
}

/// Open a web page viewer on an address (an http:// URL or an absolute path).
pub fn open_web(address: &str) -> Result<usize, &'static str> {
    let storage = storage::handle().ok_or("no file system is mounted")?;
    let address = String::from(address);
    open("Web", "browse", false, move |window| web::run(window, address, storage))
}

/// The folder new windows start in: the data partition if there is one.
fn home() -> &'static str {
    if storage::has_data() { storage::DATA_DIR } else { "/" }
}

/// What a click on a desktop button starts (`console::BUTTONS`): a
/// terminal, the file manager on the home folder, or the editor on
/// `notes.txt` there.
pub fn launch(button: usize) -> Result<usize, &'static str> {
    match button {
        0 => open_terminal(),
        1 => open_files(home()),
        2 => open_editor(&filemgr::join(home(), "notes.txt")),
        _ => Err("no such button"),
    }
}

/// The mouse may ask for a window before any was opened with a command:
/// the thread that looks after them must be there.
pub fn start() {
    if !*STARTED.get() {
        *STARTED.get() = thread::spawn("desktop", None, serve).is_some();
    }
}

/// A window, a session for its keys, and a thread that runs `program` in it.
fn open(title: &str, thread_name: &str, prints: bool, program: impl FnOnce(usize) + 'static) -> Result<usize, &'static str> {
    let session = session::open().ok_or("too many sessions")?;
    let window = match console::open_pane(title) {
        Ok(window) => window,
        Err(why) => {
            session::remove(session);
            return Err(why);
        }
    };
    let Some(thread) = thread::spawn(thread_name, Some(session), move || program(window)) else {
        console::close_pane(window);
        session::remove(session);
        return Err("could not start a thread");
    };
    TERMINALS.get().push(Terminal { window, session, thread, prints });
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

/// Keys from the host (remote control, hivectl.rs), typed into the window
/// that has the focus. Alt+Tab changes window as on the keyboard. The
/// console's own window takes none: it is the machine's own console.
/// Returns how many keys were typed.
pub fn type_keys(keys: &[Key]) -> Result<usize, &'static str> {
    if !console::desktop_on() {
        return Err("no desktop on this machine: there is no window to type in");
    }
    for key in keys {
        if *key == Key::NextWindow {
            console::next_window();
            continue;
        }
        let terminal = console::focused_pane().and_then(|window| TERMINALS.get().iter().find(|t| t.window == window));
        let Some(terminal) = terminal else {
            return Err("the console's own window has the focus: keys from the host only go to the other windows ({alt+tab} changes window)");
        };
        session::push_key(terminal.session, *key);
    }
    Ok(keys.len())
}

fn work_waiting() -> bool {
    console::mouse_work()
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
    for (window, col, row) in console::take_clicks() {
        if let Some(terminal) = TERMINALS.get().iter().find(|t| t.window == window) {
            session::push_key(terminal.session, Key::Click(col, row));
        }
    }
    for button in console::take_launches() {
        // Nothing to tell when it does not work (too many windows): the
        // desktop simply stays as it is.
        let _ = launch(button);
    }
    let mut i = 0;
    while i < TERMINALS.get().len() {
        let (window, session, thread, prints) = {
            let t = &TERMINALS.get()[i];
            (t.window, t.session, t.thread, t.prints)
        };
        let text = session::take_output(session);
        if !text.is_empty() && prints {
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
