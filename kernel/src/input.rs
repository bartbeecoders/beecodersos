//! Unified input: key events from the PS/2 keyboard and the serial port.

use crate::ring::Global;
use crate::{acpi, aibridge, desk, keyboard, serial, session, thread};
use alloc::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Ctrl + letter (lowercase), e.g. `Ctrl('c')`.
    Ctrl(char),
    Enter,
    Backspace,
    Delete,
    Tab,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    /// Alt+Tab: the next window gets the focus (the desktop takes this key
    /// itself; no program sees it).
    NextWindow,
    /// The mouse was clicked in the program's window: column and line of
    /// the character under the pointer, counted from 0.
    Click(u16, u16),
    /// The (ACPI) power button was pressed, e.g. virt-manager "Shut Down".
    PowerButton,
    /// The program's window was resized: it draws its content again for
    /// the new size (`console::pane_size`).
    Resized,
    /// Shift+Page Up (true) or Shift+Page Down: the focused terminal looks
    /// back through the lines that scrolled off (the desktop does it).
    ScrollBack(bool),
    /// A key with the Super (Windows) key held: for the desktop, as on
    /// Omarchy (desk.rs); no program sees it.
    Super(SuperKey, bool),
}

/// The keys the desktop knows with Super; the bool of `Key::Super` is Shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperKey {
    Enter,
    Left,
    Right,
    Up,
    Down,
    /// A letter or digit (lowercase).
    Char(char),
}

/// The next key from the machine's keyboard that is meant for the console:
/// on the desktop, keys go to the window that has the focus (desk.rs).
fn console_key() -> Option<Key> {
    while let Some(key) = keyboard::poll() {
        if let Some(key) = desk::route(key) {
            return Some(key);
        }
    }
    None
}

/// Keys that were read while waiting for something else (an AI answer)
/// and are given back: the next read returns them first. Each belongs to
/// the session it was typed in (`None`: the console).
static AHEAD: Global<VecDeque<(Option<u32>, Key)>> = Global::new(VecDeque::new());
/// No more are kept than this.
const MAX_AHEAD: usize = 32;

/// Give a key back: the next `poll_key` / `read_key` of this session returns it.
pub fn unread(key: Key) {
    let ahead = AHEAD.get();
    if ahead.len() < MAX_AHEAD {
        ahead.push_back((session::current(), key));
    }
}

fn take_ahead() -> Option<Key> {
    let me = session::current();
    let ahead = AHEAD.get();
    let at = ahead.iter().position(|(owner, _)| *owner == me)?;
    ahead.remove(at).map(|(_, key)| key)
}

/// The next key if one is waiting, without blocking.
pub fn poll_key() -> Option<Key> {
    if let Some(key) = take_ahead() {
        return Some(key);
    }
    if let Some(id) = session::current() {
        return session::poll_key(id);
    }
    if !thread::on_console() {
        return None; // a background thread: the keyboard is not its to read
    }
    if acpi::take_power_button() {
        return Some(Key::PowerButton);
    }
    console_key().or_else(serial::poll_key)
}

/// True when a key (or the power button) is waiting.
pub fn pending() -> bool {
    let me = session::current();
    if AHEAD.get().iter().any(|(owner, _)| *owner == me) {
        return true;
    }
    if let Some(id) = session::current() {
        return session::pending(id);
    }
    if !thread::on_console() {
        return false;
    }
    keyboard::pending() || serial::pending() || acpi::power_button_pending()
}

/// Block until a key arrives from any input source; other threads run
/// meanwhile, and the CPU sleeps (`hlt`) when none has anything to do.
/// In a console session that has ended this returns Ctrl+D.
pub fn read_key() -> Key {
    if let Some(key) = take_ahead() {
        return key;
    }
    if let Some(id) = session::current() {
        return session::read_key(id).unwrap_or(Key::Ctrl('d'));
    }
    loop {
        if acpi::take_power_button() {
            return Key::PowerButton;
        }
        if let Some(k) = console_key() {
            return k;
        }
        if let Some(k) = serial::poll_key() {
            return k;
        }
        // Nothing more will be printed until a key comes: show it all.
        crate::console::flush();
        // Handle messages from beehive while idle (welcome after it connects).
        aibridge::drain();
        // The network is looked after by its own thread (`net::start_task`).
        thread::pause(|| keyboard::pending() || serial::pending() || acpi::power_button_pending() || aibridge::pending());
    }
}
