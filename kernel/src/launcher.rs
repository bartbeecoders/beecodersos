//! The launcher (Super+Space, as on Omarchy): a small window that lists
//! the programs. Typing narrows the list, the arrows choose, Enter starts
//! the chosen one (and the launcher closes), Esc closes it. Also the list
//! of keyboard shortcuts (Super+K).

use crate::input::{self, Key};
use crate::{console, desk, session, vga};
use alloc::string::String;
use alloc::vec::Vec;

/// The keyboard shortcuts of the desktop (Super+K shows them).
pub const KEYS: [(&str, &str); 14] = [
    ("Super+Space", "the launcher: programs by name"),
    ("Super+Enter", "a new terminal"),
    ("Super+W", "close the window"),
    ("Super+arrows", "go to the window that way"),
    ("Super+Shift+arrows", "swap the window with that one"),
    ("Super+F", "fill the screen, and back"),
    ("Super+T", "tile the windows, or let them float"),
    ("Super+1-9", "show another workspace"),
    ("Super+Shift+1-9", "send the window to a workspace"),
    ("Super+K", "these keys"),
    ("Alt+Tab", "the next window"),
    ("Shift+PgUp/PgDn", "look back in a terminal"),
    ("Ctrl+C", "stop a command"),
    ("q / Esc", "leave most windows"),
];

/// The thread of the keys window: the list until a key or its close box.
pub fn run_keys(window: usize) {
    loop {
        let Some((cols, rows)) = console::pane_size(window) else { return };
        let line = |text: &str, attr: u8| {
            let mut cells: Vec<u16> = text.chars().take(cols).map(|c| (attr as u16) << 8 | vga::cp437(c) as u16).collect();
            cells.resize(cols, (attr as u16) << 8 | b' ' as u16);
            cells
        };
        let mut lines = alloc::vec![line(" Keys", TITLE_ATTR), line("", TEXT_ATTR)];
        for (keys, what) in KEYS.iter() {
            lines.push(line(&alloc::format!("  {keys:<20} {what}"), TEXT_ATTR));
        }
        while lines.len() + 1 < rows {
            lines.push(line("", TEXT_ATTR));
        }
        lines.truncate(rows.saturating_sub(1));
        lines.push(line(" any key closes this window", HELP_ATTR));
        console::pane_show(window, &lines, (0, cols.saturating_sub(1)));
        match input::read_key() {
            Key::Resized | Key::Click(..) => {}
            _ => return,
        }
    }
}

/// Name and what it is, in the order shown.
pub const PROGRAMS: [(&str, &str); 8] = [
    ("Terminal", "a shell in a window"),
    ("Files", "the file manager on your home folder"),
    ("Notes", "the editor on notes.txt"),
    ("Web", "the web viewer on your start page"),
    ("Camera", "the USB camera, live"),
    ("Monitor", "processors, memory, network, threads"),
    ("Pictures", "the pictures in your home folder"),
    ("Keys", "the keyboard shortcuts"),
];

const TITLE_ATTR: u8 = 0x70;
const TEXT_ATTR: u8 = 0x07;
const CHOSEN_ATTR: u8 = 0x1F;
const HELP_ATTR: u8 = 0x08;

/// The programs whose name or description contains `query` (any case), by
/// their place in `PROGRAMS`; names that start with it come first.
pub fn matches(query: &str) -> Vec<usize> {
    let q = query.trim().to_ascii_lowercase();
    let mut starts = Vec::new();
    let mut contains = Vec::new();
    for (i, (name, what)) in PROGRAMS.iter().enumerate() {
        let name = name.to_ascii_lowercase();
        if name.starts_with(&q) {
            starts.push(i);
        } else if name.contains(&q) || what.to_ascii_lowercase().contains(&q) {
            contains.push(i);
        }
    }
    starts.extend(contains);
    starts
}

fn start(program: usize) {
    let _ = match program {
        0 => desk::launch(0),
        1 => desk::launch(1),
        2 => desk::launch(2),
        3 => desk::launch(3),
        4 => desk::launch(4),
        5 => desk::open_monitor(),
        6 => desk::open_pictures(),
        _ => desk::open_keys(),
    };
}

/// The thread of the launcher's window.
pub fn run(window: usize) {
    let mut query = String::new();
    let mut chosen = 0usize;
    loop {
        let Some((cols, rows)) = console::pane_size(window) else { return };
        let found = matches(&query);
        chosen = chosen.min(found.len().saturating_sub(1));
        let line = |text: &str, attr: u8| {
            let mut cells: Vec<u16> = text.chars().take(cols).map(|c| (attr as u16) << 8 | vga::cp437(c) as u16).collect();
            cells.resize(cols, (attr as u16) << 8 | b' ' as u16);
            cells
        };
        let mut lines = alloc::vec![line(&alloc::format!(" Start: {query}"), TITLE_ATTR), line("", TEXT_ATTR)];
        for (k, &i) in found.iter().enumerate().take(rows.saturating_sub(3)) {
            let (name, what) = PROGRAMS[i];
            let attr = if k == chosen { CHOSEN_ATTR } else { TEXT_ATTR };
            lines.push(line(&alloc::format!("  {name:<10} {what}"), attr));
        }
        if found.is_empty() {
            lines.push(line("  nothing by that name", HELP_ATTR));
        }
        while lines.len() + 1 < rows {
            lines.push(line("", TEXT_ATTR));
        }
        lines.push(line(" type to search   arrows choose   Enter starts   Esc closes", HELP_ATTR));
        console::pane_show(window, &lines, (0, (8 + query.chars().count()).min(cols.saturating_sub(1))));
        match input::read_key() {
            Key::Escape | Key::Ctrl('q') => return,
            Key::Ctrl('d') if session::current().is_none_or(session::is_closed) => return,
            Key::Enter => {
                if let Some(&program) = found.get(chosen) {
                    start(program);
                    return;
                }
            }
            Key::Down => chosen = (chosen + 1).min(found.len().saturating_sub(1)),
            Key::Up => chosen = chosen.saturating_sub(1),
            Key::Backspace => {
                query.pop();
                chosen = 0;
            }
            Key::Char(c) if !c.is_control() && query.chars().count() < 30 => {
                query.push(c);
                chosen = 0;
            }
            Key::Click(_, row) if row >= 2 && (row as usize - 2) < found.len() => {
                start(found[row as usize - 2]);
                return;
            }
            _ => {}
        }
    }
}
