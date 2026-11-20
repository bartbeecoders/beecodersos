//! The text editor (`edit FILE`): a window on the desktop showing a file,
//! with a status line at the top.
//!
//! `Editor` is the text and the cursor, and knows nothing about windows or
//! disks: keys go in, and `render` gives the characters to show. That part
//! is covered by the self-tests. `run` is the editor's thread: it reads the
//! keys of its window, draws, and saves the file.

use crate::input::{self, Key};
use crate::storage::Storage;
use crate::{console, session};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Larger files are not opened: the whole text is kept in memory.
pub const MAX_FILE: usize = 1024 * 1024;
const TAB: usize = 4;

/// Colors of the text and of the status line (VGA attribute bytes).
const TEXT_ATTR: u8 = 0x07;
const STATUS_ATTR: u8 = 0x70;
const WARNING_ATTR: u8 = 0x4F;

/// What a key asks the editor's owner to do.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Action {
    /// Nothing but drawing again.
    None,
    Save,
    Quit,
}

pub struct Editor {
    /// Always at least one line.
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// The first line and column shown.
    top: usize,
    left: usize,
    modified: bool,
    /// Ctrl+Q was pressed with changes not saved: once more leaves.
    leaving: bool,
    /// Shown in the status line until the next key.
    message: String,
}

impl Editor {
    pub fn new(text: &str) -> Editor {
        let mut lines: Vec<Vec<char>> = text.split('\n').map(|l| l.trim_end_matches('\r').chars().collect()).collect();
        // A file that ends with a line end has no extra empty line after it.
        if lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        Editor { lines, row: 0, col: 0, top: 0, left: 0, modified: false, leaving: false, message: String::new() }
    }

    /// The whole text; every line ends with a line end.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for line in &self.lines {
            text.extend(line.iter());
            text.push('\n');
        }
        text
    }

    pub fn modified(&self) -> bool {
        self.modified
    }

    /// Line and column of the cursor, counted from 1.
    pub fn position(&self) -> (usize, usize) {
        (self.row + 1, self.col + 1)
    }

    /// After a save: `Ok` with the number of bytes, or what went wrong.
    pub fn saved(&mut self, result: Result<usize, &str>) {
        match result {
            Ok(bytes) => {
                self.modified = false;
                self.message = format!("saved, {bytes} bytes");
            }
            Err(why) => self.message = format!("NOT saved: {why}"),
        }
    }

    /// Handle one key. `page`: the number of text lines the window shows.
    pub fn key(&mut self, key: Key, page: usize) -> Action {
        self.message.clear();
        let was_leaving = core::mem::take(&mut self.leaving);
        let page = page.max(1);
        match key {
            Key::Ctrl('s') => return Action::Save,
            Key::Ctrl('q') => {
                if !self.modified || was_leaving {
                    return Action::Quit;
                }
                self.leaving = true;
                self.message = String::from("changes not saved: Ctrl+Q again leaves without saving, Ctrl+S saves");
            }
            Key::Char(c) => self.insert(c),
            Key::Tab => {
                for _ in 0..TAB - self.col % TAB {
                    self.insert(' ');
                }
            }
            Key::Enter => {
                let rest = self.lines[self.row].split_off(self.col);
                // The new line starts under the first letter of this one.
                let indent = self.lines[self.row].iter().take_while(|c| **c == ' ').count().min(self.col);
                let mut line = vec![' '; indent];
                line.extend(rest);
                self.lines.insert(self.row + 1, line);
                self.row += 1;
                self.col = indent;
                self.modified = true;
            }
            Key::Backspace => {
                if self.col > 0 {
                    self.col -= 1;
                    self.lines[self.row].remove(self.col);
                    self.modified = true;
                } else if self.row > 0 {
                    let line = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                    self.lines[self.row].extend(line);
                    self.modified = true;
                }
            }
            Key::Delete => {
                if self.col < self.lines[self.row].len() {
                    self.lines[self.row].remove(self.col);
                    self.modified = true;
                } else if self.row + 1 < self.lines.len() {
                    let line = self.lines.remove(self.row + 1);
                    self.lines[self.row].extend(line);
                    self.modified = true;
                }
            }
            Key::Ctrl('k') => {
                // Cut the rest of the line; at its end, join the next line.
                if self.col < self.lines[self.row].len() {
                    self.lines[self.row].truncate(self.col);
                    self.modified = true;
                } else if self.row + 1 < self.lines.len() {
                    let line = self.lines.remove(self.row + 1);
                    self.lines[self.row].extend(line);
                    self.modified = true;
                }
            }
            Key::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                }
            }
            Key::Right => {
                if self.col < self.lines[self.row].len() {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            Key::Up => self.row = self.row.saturating_sub(1),
            Key::Down => self.row = (self.row + 1).min(self.lines.len() - 1),
            Key::PageUp => self.row = self.row.saturating_sub(page),
            Key::PageDown => self.row = (self.row + page).min(self.lines.len() - 1),
            // A click in the text puts the cursor there.
            Key::Click(col, row) if row >= 1 => {
                self.row = (self.top + row as usize - 1).min(self.lines.len() - 1);
                self.col = self.left + col as usize;
            }
            Key::Home => self.col = 0,
            Key::End => self.col = usize::MAX,
            _ => {}
        }
        self.col = self.col.min(self.lines[self.row].len());
        Action::None
    }

    fn insert(&mut self, c: char) {
        self.lines[self.row].insert(self.col, c);
        self.col += 1;
        self.modified = true;
    }

    /// What a window of `cols` by `rows` characters shows: `rows` lines of
    /// `cols` cells (VGA format: color in the high byte), the status line
    /// first, and the cell (row, column) the cursor is in. The text scrolls
    /// so that the cursor is visible.
    pub fn render(&mut self, name: &str, cols: usize, rows: usize) -> (Vec<Vec<u16>>, (usize, usize)) {
        let (cols, rows) = (cols.max(1), rows.max(2));
        let page = rows - 1;
        if self.row < self.top {
            self.top = self.row;
        } else if self.row >= self.top + page {
            self.top = self.row + 1 - page;
        }
        if self.col < self.left {
            self.left = self.col;
        } else if self.col >= self.left + cols {
            self.left = self.col + 1 - cols;
        }
        let cell = |c: char, attr: u8| (attr as u16) << 8 | crate::vga::cp437(c) as u16;
        let mut out = Vec::with_capacity(rows);
        // Status line: the message if there is one, else name, state and keys.
        let (status, attr) = if self.message.is_empty() {
            let (line, col) = self.position();
            let state = if self.modified { " (changed)" } else { "" };
            (format!(" {name}{state}  line {line} of {}, column {col}  |  Ctrl+S save  Ctrl+Q leave", self.lines.len()), STATUS_ATTR)
        } else {
            (format!(" {}", self.message), if self.leaving || self.message.starts_with("NOT") { WARNING_ATTR } else { STATUS_ATTR })
        };
        let mut line: Vec<u16> = status.chars().take(cols).map(|c| cell(c, attr)).collect();
        line.resize(cols, cell(' ', attr));
        out.push(line);
        for r in self.top..self.top + page {
            let mut line: Vec<u16> = match self.lines.get(r) {
                Some(text) => text.iter().skip(self.left).take(cols).map(|c| cell(*c, TEXT_ATTR)).collect(),
                None => Vec::new(),
            };
            line.resize(cols, cell(' ', TEXT_ATTR));
            out.push(line);
        }
        (out, (self.row - self.top + 1, self.col - self.left))
    }
}

/// Read a file for editing. A file that does not exist yet gives an empty
/// text. Refused: a directory, a file that is too large or is not text.
pub fn load(storage: &mut Storage, path: &str) -> Result<String, &'static str> {
    let mut fs = storage.fs();
    if fs.is_dir(path) {
        return Err("is a directory");
    }
    if !fs.exists(path) {
        return Ok(String::new());
    }
    let data = fs.read_file(path).map_err(|_| "could not be read")?;
    if data.len() > MAX_FILE {
        return Err("too large to edit (more than 1 MiB)");
    }
    String::from_utf8(data).map_err(|_| "not a text file")
}

/// The editor's thread: runs in the session of its window until Ctrl+Q,
/// or until the window is closed.
pub fn run(window: usize, path: String, text: String, mut storage: Storage) {
    let mut editor = Editor::new(&text);
    let name = path.rsplit('/').next().unwrap_or(&path);
    loop {
        let Some((cols, rows)) = console::pane_size(window) else { return };
        let (lines, cursor) = editor.render(name, cols, rows);
        console::pane_show(window, &lines, cursor);
        console::pane_scroll(window, editor.top, rows.max(2) - 1, editor.lines.len());
        let key = input::read_key();
        if key == Key::Ctrl('d') && session::current().is_none_or(session::is_closed) {
            return; // the window was closed
        }
        match editor.key(key, rows.saturating_sub(1)) {
            Action::None => {}
            Action::Quit => return,
            Action::Save => match crate::users::console_may_change(&mut storage, &path) {
                Ok(()) => {
                    let text = editor.text();
                    let result = storage.fs().write_file(&path, text.as_bytes());
                    editor.saved(result.map(|_| text.len()).map_err(|_| "the file could not be written"));
                }
                Err(whose) => editor.saved(Err(&whose)),
            },
        }
    }
}
