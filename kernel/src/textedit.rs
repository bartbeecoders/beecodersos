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
    /// Ctrl+F: the text being typed to look for (the status line asks).
    search: Option<String>,
    /// What was looked for last (Ctrl+G looks again).
    last_search: String,
    /// Ctrl+Z: earlier texts and cursors, the latest last.
    undo: Vec<(Vec<Vec<char>>, usize, usize, bool)>,
    /// The last change was typing letters: more letters join that step.
    typing: bool,
    /// The last key was Ctrl+K: another one adds to the clipboard.
    cutting: bool,
}

/// What Ctrl+K cut, for Ctrl+U: shared by every editor window.
static CLIPBOARD: crate::ring::Global<String> = crate::ring::Global::new(String::new());

/// Steps kept for Ctrl+Z (fewer for large texts: each keeps a whole copy).
const UNDO_STEPS: usize = 50;
const UNDO_BYTES: usize = 4 * 1024 * 1024;

impl Editor {
    pub fn new(text: &str) -> Editor {
        let mut lines: Vec<Vec<char>> = text.split('\n').map(|l| l.trim_end_matches('\r').chars().collect()).collect();
        // A file that ends with a line end has no extra empty line after it.
        if lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        Editor {
            lines,
            row: 0,
            col: 0,
            top: 0,
            left: 0,
            modified: false,
            leaving: false,
            message: String::new(),
            search: None,
            last_search: String::new(),
            undo: Vec::new(),
            typing: false,
            cutting: false,
        }
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

    /// Look for `what` (any case) after the cursor, round past the end;
    /// the cursor goes to the start of what was found.
    fn find(&mut self, what: &str) {
        let needle: Vec<char> = what.chars().flat_map(|c| c.to_lowercase()).collect();
        if needle.is_empty() {
            return;
        }
        let lower = |line: &[char]| -> Vec<char> { line.iter().flat_map(|c| c.to_lowercase()).collect() };
        let at = |line: &[char], from: usize| -> Option<usize> {
            let line = lower(line);
            (from..line.len().saturating_sub(needle.len() - 1)).find(|&i| line[i..i + needle.len()] == needle[..])
        };
        let n = self.lines.len();
        for step in 0..=n {
            let row = (self.row + step) % n;
            let from = if step == 0 { self.col + 1 } else { 0 };
            let found = if step == n { at(&self.lines[row][..self.col.min(self.lines[row].len())], 0) } else { at(&self.lines[row], from) };
            if let Some(col) = found {
                let wrapped = row < self.row || (row == self.row && col <= self.col);
                self.row = row;
                self.col = col;
                if wrapped {
                    self.message = format!("\"{what}\" found (from the top again)");
                }
                return;
            }
        }
        self.message = format!("\"{what}\" not found");
    }

    /// Remember the text before a change (`typing`: letters join one step).
    fn remember(&mut self, typing: bool) {
        if typing && self.typing {
            return;
        }
        self.typing = typing;
        let size: usize = self.lines.iter().map(|l| l.len() + 1).sum();
        let most = (UNDO_BYTES / size.max(1)).clamp(1, UNDO_STEPS);
        while self.undo.len() >= most {
            self.undo.remove(0);
        }
        self.undo.push((self.lines.clone(), self.row, self.col, self.modified));
    }

    /// Handle one key. `page`: the number of text lines the window shows.
    pub fn key(&mut self, key: Key, page: usize) -> Action {
        // Before a key that changes the text: remember it for Ctrl+Z.
        match key {
            Key::Char(_) if self.search.is_none() => self.remember(true),
            Key::Tab | Key::Enter | Key::Backspace | Key::Delete | Key::Ctrl('k') | Key::Ctrl('u') if self.search.is_none() => self.remember(false),
            Key::Ctrl('z') => {}
            _ => self.typing = false,
        }
        if key == Key::Resized {
            return Action::None; // drawn again for the new size; a question stays asked
        }
        if let Some(text) = self.search.as_mut() {
            match key {
                Key::Char(c) if text.chars().count() < 60 => text.push(c),
                Key::Backspace => {
                    text.pop();
                }
                Key::Enter => {
                    let what = self.search.take().unwrap_or_default();
                    if !what.is_empty() {
                        self.last_search = what.clone();
                        self.find(&what);
                    }
                }
                Key::Escape | Key::Ctrl('f') | Key::Ctrl('q') => self.search = None,
                _ => {}
            }
            return Action::None;
        }
        self.message.clear();
        // Ctrl+K in a row gathers; any other key starts the next cut afresh.
        if key != Key::Ctrl('k') {
            self.cutting = false;
        }
        let was_leaving = core::mem::take(&mut self.leaving);
        let page = page.max(1);
        match key {
            Key::Ctrl('s') => return Action::Save,
            Key::Ctrl('f') => self.search = Some(String::new()),
            Key::Ctrl('z') => match self.undo.pop() {
                Some((lines, row, col, modified)) => {
                    self.lines = lines;
                    self.row = row;
                    self.col = col;
                    self.modified = modified;
                    self.typing = false;
                }
                None => self.message = String::from("nothing to undo"),
            },
            Key::Ctrl('g') if !self.last_search.is_empty() => {
                let what = self.last_search.clone();
                self.find(&what);
            }
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
                // Cut the rest of the line; at its end, join the next line
                // (the line end is cut). Cuts in a row gather on the clipboard.
                let clip = CLIPBOARD.get();
                if !self.cutting {
                    clip.clear();
                }
                if self.col < self.lines[self.row].len() {
                    clip.extend(self.lines[self.row].drain(self.col..));
                    self.modified = true;
                } else if self.row + 1 < self.lines.len() {
                    let line = self.lines.remove(self.row + 1);
                    self.lines[self.row].extend(line);
                    clip.push('\n');
                    self.modified = true;
                }
                if clip.len() > MAX_FILE {
                    clip.truncate(MAX_FILE);
                }
                self.cutting = true;
            }
            Key::Ctrl('u') => {
                // Paste what was cut (in any editor window) at the cursor.
                let clip = CLIPBOARD.get().clone();
                if clip.is_empty() {
                    self.message = String::from("nothing to paste: Ctrl+K cuts");
                }
                for c in clip.chars() {
                    if c == '\n' {
                        let rest = self.lines[self.row].split_off(self.col);
                        self.lines.insert(self.row + 1, rest);
                        self.row += 1;
                        self.col = 0;
                        self.modified = true;
                    } else {
                        self.insert(c);
                    }
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
        let (status, attr) = if let Some(text) = &self.search {
            (format!(" Find: {text}_   (Enter finds, Esc stops)"), STATUS_ATTR)
        } else if self.message.is_empty() {
            let (line, col) = self.position();
            let state = if self.modified { " (changed)" } else { "" };
            (format!(" {name}{state}  line {line} of {}, column {col}  |  Ctrl+S save  Ctrl+Z undo  Ctrl+F find  Ctrl+Q leave", self.lines.len()), STATUS_ATTR)
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
