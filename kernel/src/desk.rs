//! The extra terminal windows of the desktop (`terminal`): each runs a
//! shell of its own in a console session (session.rs). One thread,
//! "desktop", looks after all of them: it brings what a shell printed to its
//! window, hands the keys to the window that has the focus, and clears up
//! when a shell ends or a window is closed.
//!
//! The windows themselves, and what they show, belong to console.rs.

use crate::input::{Key, SuperKey};
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
    open("Terminal", "terminal", true, move |_| Shell::terminal(storage).run_session())
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

/// Open a picture viewer on a file (`path` is absolute); the picture is
/// read before the window opens, so a file that is no picture opens none.
pub fn open_picture(path: &str) -> Result<(usize, usize, usize, usize), String> {
    let mut storage = storage::handle().ok_or("no file system is mounted")?;
    let picture = crate::view::load_any(&mut storage, path)?;
    let (width, height) = picture.size();
    let frames = picture.frames();
    let name = path.rsplit('/').next().unwrap_or(path);
    let owned = String::from(path);
    let window = open(&format!("View: {name}"), "view", false, move |window| crate::view::run(window, picture, owned, storage))?;
    Ok((window, width, height, frames))
}

/// Open a video player on an AVI file (`path` is absolute): read and
/// checked before the window opens. Returns the window, size, frames and
/// whether it has sound.
pub fn open_video(path: &str) -> Result<(usize, usize, usize, usize, bool), String> {
    let mut storage = storage::handle().ok_or("no file system is mounted")?;
    let data = storage.fs().read_file(path).map_err(|e| format!("{e}"))?;
    let avi = beenet::avi::parse(&data).map_err(String::from)?;
    if !avi.is_mjpeg() {
        let codec = String::from_utf8_lossy(&avi.codec).into_owned();
        return Err(format!("a video in {codec} (only Motion-JPEG videos can be played)"));
    }
    if avi.frames.is_empty() {
        return Err(String::from("a video without frames"));
    }
    let (w, h, n, sound) = (avi.width, avi.height, avi.frames.len(), avi.sound.is_some());
    let name = path.rsplit('/').next().unwrap_or(path);
    let window = open(&format!("Video: {name}"), "video", false, move |window| crate::video::run(window, data, avi))?;
    Ok((window, w, h, n, sound))
}

/// Open the camera window: what the USB camera sees, live. Saved pictures
/// go to the home folder.
/// Without a camera the window opens anyway and says so (a desktop button
/// has nowhere else to say it).
pub fn open_camera() -> Result<usize, &'static str> {
    let storage = storage::handle().ok_or("no file system is mounted")?;
    let folder = String::from(home());
    open("Camera", "camera", false, move |window| crate::webcam::run(window, folder, storage))
}

/// A timer's time is up: a small window with its message until a key or
/// the close box.
pub fn open_timer_alert(id: u32, text: &str) -> Result<usize, &'static str> {
    let text = String::from(text);
    open(&format!("Timer {id}"), "timer-alert", false, move |window| {
        console::pane_write(window, &format!("\n  {text}\n\n  Any key closes this window.\n"));
        while console::pane_exists(window) {
            thread::wait_until(|| crate::input::pending() || !console::pane_exists(window));
            if crate::input::pending() {
                let _ = crate::input::read_key();
                break;
            }
        }
    })
}

/// The launcher (Super+Space): the programs to start, by name.
pub fn open_launcher() -> Result<usize, &'static str> {
    // Floating in the middle, over the (maybe tiled) windows, as on Omarchy.
    open_with("Start", "launcher", false, Some((64, 12)), crate::launcher::run)
}

/// The keyboard shortcuts (Super+K), floating in the middle.
pub fn open_keys() -> Result<usize, &'static str> {
    let rows = crate::launcher::KEYS.len() + 4;
    open_with("Keys", "keys", false, Some((64, rows)), crate::launcher::run_keys)
}

/// The pictures in the home folder: the first in the picture viewer (the
/// arrow keys show the others); without any, the file manager there.
pub fn open_pictures() -> Result<usize, &'static str> {
    let mut storage = storage::handle().ok_or("no file system is mounted")?;
    let names: Vec<String> = storage.fs().read_dir(home()).map(|es| es.into_iter().map(|e| e.name).collect()).unwrap_or_default();
    match crate::view::first_in(&names) {
        Some(first) => open_picture(&filemgr::join(home(), &first)).map(|(window, ..)| window).map_err(|_| "the picture could not be shown"),
        None => open_files(home()),
    }
}

/// Open the system monitor in a window.
pub fn open_monitor() -> Result<usize, &'static str> {
    open("Monitor", "monitor", false, crate::monitor::run)
}

/// The folder new windows start in: the data partition if there is one.
fn home() -> &'static str {
    if storage::has_data() { storage::DATA_DIR } else { "/" }
}

/// What a click on a desktop button starts (`console::BUTTONS`): a
/// terminal, the file manager on the home folder, the editor on
/// `notes.txt` there, the web page viewer on `start.html` there, or the
/// camera window.
pub fn launch(button: usize) -> Result<usize, &'static str> {
    match button {
        0 => open_terminal(),
        1 => open_files(home()),
        2 => open_editor(&filemgr::join(home(), "notes.txt")),
        3 => {
            let start = filemgr::join(home(), web::START_NAME);
            let mut storage = storage::handle().ok_or("no file system is mounted")?;
            let existing = storage.fs().read_file(&start).ok();
            // A page of its own to start from (or an earlier version's page that
            // was never changed, brought up to date); a disk that is full simply has none.
            if let Some(page) = web::start_page_update(existing.as_deref()) {
                let _ = storage.fs().write_file(&start, page.as_bytes());
            }
            open_web(&start)
        }
        4 => open_camera(),
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
    open_with(title, thread_name, prints, None, program)
}

/// `open`, with a size (characters) for a floating window in the middle.
fn open_with(
    title: &str,
    thread_name: &str,
    prints: bool,
    floating: Option<(usize, usize)>,
    program: impl FnOnce(usize) + 'static,
) -> Result<usize, &'static str> {
    let session = session::open().ok_or("too many sessions")?;
    let opened = match floating {
        Some(size) => console::open_pane_sized(title, size, true),
        None => console::open_pane(title),
    };
    let window = match opened {
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
    if let Key::Super(k, shift) = key {
        super_key(k, shift);
        return None;
    }
    if let Key::ScrollBack(up) = key {
        console::scroll_focused(up);
        return None;
    }
    if console::nothing_focused() {
        return None; // an empty workspace: no window to type in
    }
    let Some(window) = console::focused_pane() else { return Some(key) };
    if let Some(terminal) = TERMINALS.get().iter().find(|t| t.window == window) {
        session::push_key(terminal.session, key);
    }
    None
}

/// Keys with Super, as on Omarchy: Super+Enter a terminal, Super+W closes
/// the focused window, Super+arrows move the focus, Super+Shift+arrows move
/// the window, Super+F fills the screen with it, Super+T tiles or floats,
/// Super+1-9 shows a workspace, Super+Shift+1-9 sends the window there,
/// Super+Space opens the launcher, Super+K lists these keys.
pub fn super_key(key: SuperKey, shift: bool) {
    use crate::wm::Dir;
    let dir = match key {
        SuperKey::Left => Some(Dir::Left),
        SuperKey::Right => Some(Dir::Right),
        SuperKey::Up => Some(Dir::Up),
        SuperKey::Down => Some(Dir::Down),
        _ => None,
    };
    match (key, dir) {
        (_, Some(dir)) if shift => console::swap_towards(dir),
        (_, Some(dir)) => console::focus_towards(dir),
        (SuperKey::Enter, _) => {
            let _ = open_terminal();
        }
        (SuperKey::Char(d @ '1'..='9'), _) if shift => console::send_to_workspace(d as usize - '0' as usize),
        (SuperKey::Char(d @ '1'..='9'), _) => console::switch_workspace(d as usize - '0' as usize),
        (SuperKey::Char(' '), _) => {
            let _ = open_launcher();
        }
        (SuperKey::Char('k'), _) => {
            let _ = open_keys();
        }
        (SuperKey::Char('w'), _) => console::close_focused(),
        (SuperKey::Char('f'), _) => console::toggle_fullscreen(),
        (SuperKey::Char('t'), _) => {
            let on = !console::tiling_on();
            console::set_tiling(on);
            if let Some(mut storage) = storage::handle() {
                let _ = storage.fs().write_file("/system/desktop-tile", if on { b"on" } else { b"off" });
            }
        }
        _ => {}
    }
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
        if let Key::Super(k, shift) = *key {
            super_key(k, shift);
            continue;
        }
        if let Key::ScrollBack(up) = *key {
            console::scroll_focused(up);
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
    for (window, up) in console::take_scrolls() {
        if let Some(terminal) = TERMINALS.get().iter().find(|t| t.window == window) {
            session::push_key(terminal.session, if up { Key::PageUp } else { Key::PageDown });
        }
    }
    for window in console::take_resized() {
        if let Some(terminal) = TERMINALS.get().iter().find(|t| t.window == window) {
            session::push_key(terminal.session, Key::Resized);
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
