//! Line editor: cursor movement, insert/delete and command history.

use crate::console;
use crate::input::{read_key, Key};
use alloc::string::String;
use alloc::vec::Vec;

const MAX_LINE: usize = 512;
const MAX_HISTORY: usize = 64;

pub struct Editor {
    history: Vec<String>,
}

pub enum Line {
    Text(String),
    /// Ctrl+C: discard the line.
    Cancelled,
    /// Ctrl+L: the caller should clear the screen and prompt again.
    ClearScreen,
    /// The power button was pressed.
    PowerButton,
}

impl Editor {
    pub const fn new() -> Self {
        Editor { history: Vec::new() }
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Read one line. The prompt must already be printed.
    pub fn read_line(&mut self) -> Line {
        self.read_line_with(&mut |_, _| Vec::new())
    }

    /// `read_line` with Tab completion: `complete(word, first)` gives what
    /// the word under the cursor could become (`first`: it is the command).
    pub fn read_line_with(&mut self, complete: &mut dyn FnMut(&str, bool) -> Vec<String>) -> Line {
        let mark = console::mark();
        let mut buf: Vec<char> = Vec::new();
        let mut cur = 0;
        let mut hist_pos = self.history.len();
        let mut draft: Vec<char> = Vec::new();

        loop {
            let (old_len, old_cur) = (buf.len(), cur);
            let key = read_key();
            let tail_edit = matches!(key, Key::Char(_) | Key::Backspace);
            match key {
                Key::Enter => {
                    console::end_line(buf.len(), cur);
                    let line: String = buf.iter().collect();
                    if !line.trim().is_empty() && self.history.last() != Some(&line) {
                        if self.history.len() == MAX_HISTORY {
                            self.history.remove(0);
                        }
                        self.history.push(line.clone());
                    }
                    return Line::Text(line);
                }
                Key::Ctrl('c') => {
                    console::end_line(buf.len(), cur);
                    return Line::Cancelled;
                }
                Key::Ctrl('l') => return Line::ClearScreen,
                Key::PowerButton => {
                    console::end_line(buf.len(), cur);
                    return Line::PowerButton;
                }
                Key::Char(c) if buf.len() < MAX_LINE => {
                    buf.insert(cur, c);
                    cur += 1;
                }
                Key::Tab => {
                    let before: String = buf[..cur].iter().collect();
                    let start = before.rfind(' ').map_or(0, |i| i + 1);
                    let first = before[..start].trim().is_empty();
                    let word = &before[start..];
                    let Some(more) = completion(word, &complete(word, first)) else { continue };
                    for c in more.chars() {
                        if buf.len() < MAX_LINE {
                            buf.insert(cur, c);
                            cur += 1;
                        }
                    }
                }
                Key::Backspace if cur > 0 => {
                    cur -= 1;
                    buf.remove(cur);
                }
                Key::Delete | Key::Ctrl('d') if cur < buf.len() => {
                    buf.remove(cur);
                }
                Key::Left | Key::Ctrl('b') => cur = cur.saturating_sub(1),
                Key::Right | Key::Ctrl('f') => cur = (cur + 1).min(buf.len()),
                Key::Home | Key::Ctrl('a') => cur = 0,
                Key::End | Key::Ctrl('e') => cur = buf.len(),
                Key::Ctrl('u') => {
                    buf.drain(..cur);
                    cur = 0;
                }
                Key::Ctrl('k') => buf.truncate(cur),
                Key::Escape => {
                    buf.clear();
                    cur = 0;
                }
                Key::Up if hist_pos > 0 => {
                    if hist_pos == self.history.len() {
                        draft = buf.clone();
                    }
                    // With something typed, only the lines that start with it.
                    let prefix: String = draft.iter().collect();
                    match history_step(&self.history, hist_pos, &prefix, true) {
                        Some(at) => {
                            hist_pos = at;
                            buf = self.history[hist_pos].chars().collect();
                            cur = buf.len();
                        }
                        None => continue,
                    }
                }
                Key::Down if hist_pos < self.history.len() => {
                    let prefix: String = draft.iter().collect();
                    hist_pos = history_step(&self.history, hist_pos, &prefix, false).unwrap_or(self.history.len());
                    buf = match self.history.get(hist_pos) {
                        Some(h) => h.chars().collect(),
                        None => draft.clone(),
                    };
                    cur = buf.len();
                }
                _ => continue,
            }
            console::redraw_line(&mark, &buf, old_len, cur, old_cur, tail_edit);
        }
    }
}

/// What Tab adds to `word`, given what it could become: the rest of the
/// one choice (then a space, or nothing after a folder's '/'), or as much
/// as all the choices share. `None`: nothing to add.
pub fn completion(word: &str, choices: &[String]) -> Option<String> {
    let fits: Vec<&String> = choices.iter().filter(|c| c.starts_with(word)).collect();
    let first = fits.first()?;
    if fits.len() == 1 {
        let mut more = String::from(&first[word.len()..]);
        if !more.ends_with('/') && !first.ends_with('/') {
            more.push(' ');
        }
        return Some(more);
    }
    // The longest beginning they all share.
    let mut shared = first.len();
    for c in fits.iter().skip(1) {
        shared = shared.min(first.chars().zip(c.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a.len_utf8()).sum());
    }
    (shared > word.len()).then(|| String::from(&first[word.len()..shared]))
}

/// The next history line from `at` going back (`up`) or forward that
/// starts with `prefix` (empty: any line); `None` when there is none.
pub fn history_step(history: &[String], at: usize, prefix: &str, up: bool) -> Option<usize> {
    if up {
        (0..at.min(history.len())).rev().find(|&i| history[i].starts_with(prefix))
    } else {
        (at + 1..history.len()).find(|&i| history[i].starts_with(prefix))
    }
}
