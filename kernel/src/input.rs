//! Unified input: key events from the PS/2 keyboard and the serial port.

use crate::{acpi, aibridge, keyboard, serial, session, thread};

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
    /// The (ACPI) power button was pressed, e.g. virt-manager "Shut Down".
    PowerButton,
}

/// The next key if one is waiting, without blocking.
pub fn poll_key() -> Option<Key> {
    if let Some(id) = session::current() {
        return session::poll_key(id);
    }
    if !thread::on_console() {
        return None; // a background thread: the keyboard is not its to read
    }
    if acpi::take_power_button() {
        return Some(Key::PowerButton);
    }
    keyboard::poll().or_else(serial::poll_key)
}

/// True when a key (or the power button) is waiting.
pub fn pending() -> bool {
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
    if let Some(id) = session::current() {
        return session::read_key(id).unwrap_or(Key::Ctrl('d'));
    }
    loop {
        if acpi::take_power_button() {
            return Key::PowerButton;
        }
        if let Some(k) = keyboard::poll() {
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
