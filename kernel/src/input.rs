//! Unified input: key events from the PS/2 keyboard and the serial port.

use crate::{acpi, interrupts, keyboard, serial};

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
    if acpi::take_power_button() {
        return Some(Key::PowerButton);
    }
    keyboard::poll().or_else(serial::poll_key)
}

/// True when a key (or the power button) is waiting.
pub fn pending() -> bool {
    keyboard::pending() || serial::pending() || acpi::power_button_pending()
}

/// Block (sleeping with `hlt`) until a key arrives from any input source.
pub fn read_key() -> Key {
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
        interrupts::wait_until(|| keyboard::pending() || serial::pending() || acpi::power_button_pending());
    }
}
