//! The file manager (`files [FOLDER]`): a window on the desktop that lists
//! a folder. Arrows choose, Enter opens (a folder here, a picture in the
//! viewer, a WAV file on the sound card, anything else in the text editor;
//! `e` edits any file), Backspace goes up; folders can be made and things renamed or
//! deleted.
//!
//! `Browser` is the list, the choice and the questions it asks, and knows
//! nothing about windows or disks: keys go in, what to do comes out, and
//! `render` gives the characters to show. That part is covered by the
//! self-tests. `run` is the thread of the window: it does what was asked on
//! the file system and shows the folder again.

use crate::input::{self, Key};
use crate::storage::Storage;
use crate::{console, desk, session};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use beefs::FileKind;

const TEXT_ATTR: u8 = 0x07;
const FOLDER_ATTR: u8 = 0x09;
const CHOSEN_ATTR: u8 = 0x1F;
const STATUS_ATTR: u8 = 0x70;
const WARNING_ATTR: u8 = 0x4F;
const HELP_ATTR: u8 = 0x08;

/// Longest name that can be typed for a new folder or a rename.
const MAX_NAME: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub folder: bool,
    pub size: u64,
}

/// What a key asks the browser's owner to do. Paths are absolute.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Action {
    /// Nothing but drawing again.
    None,
    Quit,
    /// Show this folder.
    Go(String),
    /// Open this file with what fits it (`opener`).
    Open(String),
    /// Open this file in the text editor, whatever it holds.
    Edit(String),
    Delete(String),
    MakeFolder(String),
    Rename(String, String),
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum Mode {
    List,
    /// "Delete NAME? y/n"
    ConfirmDelete,
    /// Typing a name: for a new folder, or (`rename`) for the chosen entry.
    Name { rename: bool, text: String },
}

pub struct Browser {
    folder: String,
    entries: Vec<Entry>,
    chosen: usize,
    top: usize,
    mode: Mode,
    /// Shown in the status line until the next key.
    message: String,
    warning: bool,
}

/// `folder` + "/" + `name`, also right for the root folder.
pub fn join(folder: &str, name: &str) -> String {
    if folder.ends_with('/') {
        format!("{folder}{name}")
    } else {
        format!("{folder}/{name}")
    }
}

/// The folder a path lies in ("/" for the root and for what lies in it).
pub fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => String::from("/"),
        Some(at) => String::from(&path[..at]),
    }
}

/// May a new folder or a renamed entry be called this?
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && name.chars().count() <= MAX_NAME
}

impl Browser {
    pub fn new() -> Browser {
        Browser { folder: String::from("/"), entries: Vec::new(), chosen: 0, top: 0, mode: Mode::List, message: String::new(), warning: false }
    }

    pub fn folder(&self) -> &str {
        &self.folder
    }

    /// The entries in the order shown.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Which entry the bar is on.
    pub fn chosen_index(&self) -> usize {
        self.chosen
    }

    /// Put the bar on entry `at` (kept in the list).
    pub fn set_chosen(&mut self, at: usize) {
        self.chosen = at.min(self.entries.len().saturating_sub(1));
    }

    /// Is it waiting for an ordinary key (not a name or a yes/no answer)?
    pub fn is_listing(&self) -> bool {
        self.mode == Mode::List
    }

    /// The status line as `render` shows it, and whether it is a warning or question.
    pub fn status(&self) -> (String, bool) {
        let (text, attr, _) = self.status_line();
        (text, attr != STATUS_ATTR)
    }

    /// The status line: its text, color, and where the cursor goes in it
    /// when a name is being typed.
    fn status_line(&self) -> (String, u8, Option<usize>) {
        let chosen_name = self.chosen().map(|e| e.name.as_str()).unwrap_or("");
        match &self.mode {
            Mode::ConfirmDelete => (format!(" Delete {chosen_name}? y: yes, any other key: no"), WARNING_ATTR, None),
            Mode::Name { rename, text } => {
                let ask = if *rename { format!(" New name for {chosen_name}: ") } else { String::from(" Name of the new folder: ") };
                let at = ask.chars().count() + text.chars().count();
                (format!("{ask}{text}"), STATUS_ATTR, Some(at))
            }
            Mode::List if !self.message.is_empty() => (format!(" {}", self.message), if self.warning { WARNING_ATTR } else { STATUS_ATTR }, None),
            Mode::List => {
                let count = match self.entries.len() {
                    0 => String::from("empty"),
                    n => format!("{} of {n}", self.chosen + 1),
                };
                (format!(" {}  ({count})", self.folder), STATUS_ATTR, None)
            }
        }
    }

    /// The name the bar is on, if the folder is not empty.
    pub fn chosen(&self) -> Option<&Entry> {
        self.entries.get(self.chosen)
    }

    /// Show a folder: its entries, folders first, each group by name. The
    /// bar goes to `choose` if that name is there (the folder one came out
    /// of, the thing just made), else stays where it was as far as it can.
    pub fn show(&mut self, folder: &str, mut entries: Vec<Entry>, choose: Option<&str>) {
        entries.sort_by(|a, b| b.folder.cmp(&a.folder).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.name.cmp(&b.name)));
        if self.folder != folder {
            self.chosen = 0;
            self.top = 0;
        }
        self.folder = String::from(folder);
        self.entries = entries;
        if let Some(at) = choose.and_then(|name| self.entries.iter().position(|e| e.name == name)) {
            self.chosen = at;
        }
        self.chosen = self.chosen.min(self.entries.len().saturating_sub(1));
    }

    /// Say something in the status line (`warning`: in red).
    pub fn say(&mut self, message: &str, warning: bool) {
        self.message = String::from(message);
        self.warning = warning;
    }

    /// Handle one key. `page`: the number of entries the window shows.
    pub fn key(&mut self, key: Key, page: usize) -> Action {
        self.message.clear();
        self.warning = false;
        let last = self.entries.len().saturating_sub(1);
        match core::mem::replace(&mut self.mode, Mode::List) {
            Mode::ConfirmDelete => {
                return match (key, self.chosen()) {
                    (Key::Char('y' | 'Y'), Some(entry)) => Action::Delete(join(&self.folder, &entry.name)),
                    _ => {
                        self.say("nothing was deleted", false);
                        Action::None
                    }
                };
            }
            Mode::Name { rename, mut text } => {
                match key {
                    Key::Enter if text.is_empty() => {}
                    Key::Enter if !valid_name(&text) => self.say("not a usable name (no '/', not '.' or '..')", true),
                    Key::Enter => {
                        let to = join(&self.folder, &text);
                        return match (rename, self.chosen()) {
                            (true, Some(entry)) => Action::Rename(join(&self.folder, &entry.name), to),
                            (true, None) => Action::None,
                            (false, _) => Action::MakeFolder(to),
                        };
                    }
                    Key::Escape | Key::Ctrl('c') => {}
                    Key::Backspace => {
                        text.pop();
                        self.mode = Mode::Name { rename, text };
                    }
                    Key::Char(c) if c != '/' && text.chars().count() < MAX_NAME => {
                        text.push(c);
                        self.mode = Mode::Name { rename, text };
                    }
                    _ => self.mode = Mode::Name { rename, text },
                }
                return Action::None;
            }
            Mode::List => {}
        }
        match key {
            Key::Up => self.chosen = self.chosen.saturating_sub(1),
            Key::Down => self.chosen = (self.chosen + 1).min(last),
            Key::PageUp => self.chosen = self.chosen.saturating_sub(page.max(1)),
            Key::PageDown => self.chosen = (self.chosen + page.max(1)).min(last),
            Key::Home => self.chosen = 0,
            Key::End => self.chosen = last,
            // A click on an entry puts the bar there; on the bar itself it opens.
            Key::Click(_, row) if row >= 1 && self.top + row as usize - 1 < self.entries.len() && row as usize <= page => {
                let at = self.top + row as usize - 1;
                if at != self.chosen {
                    self.chosen = at;
                } else if let Some(entry) = self.chosen() {
                    let path = join(&self.folder, &entry.name);
                    return if entry.folder { Action::Go(path) } else { Action::Open(path) };
                }
            }
            Key::Enter | Key::Right => {
                if let Some(entry) = self.chosen() {
                    let path = join(&self.folder, &entry.name);
                    return if entry.folder { Action::Go(path) } else { Action::Open(path) };
                }
            }
            Key::Backspace | Key::Left if self.folder != "/" => return Action::Go(parent(&self.folder)),
            Key::Char('e') if self.chosen().is_some_and(|e| !e.folder) => {
                return Action::Edit(join(&self.folder, &self.chosen().map(|e| e.name.clone()).unwrap_or_default()));
            }
            Key::Char('n') => self.mode = Mode::Name { rename: false, text: String::new() },
            Key::Char('r') if self.chosen().is_some() => {
                let text = self.chosen().map(|e| e.name.clone()).unwrap_or_default();
                self.mode = Mode::Name { rename: true, text };
            }
            Key::Char('d') | Key::Delete if self.chosen().is_some() => self.mode = Mode::ConfirmDelete,
            Key::Char('u') => return Action::Go(self.folder.clone()),
            Key::Char('q') | Key::Ctrl('q') | Key::Escape => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    /// For the scroll bar: the first entry shown, how many fit, how many there are.
    pub fn scroll(&self, rows: usize) -> (usize, usize, usize) {
        (self.top, rows.max(3) - 2, self.entries.len())
    }

    /// What a window of `cols` by `rows` characters shows: `rows` lines of
    /// `cols` cells (VGA format), and the cell for the cursor. First the
    /// status line (the folder, or a question), then the entries with the
    /// chosen one marked, and the keys on the last line.
    pub fn render(&mut self, cols: usize, rows: usize) -> (Vec<Vec<u16>>, (usize, usize)) {
        let (cols, rows) = (cols.max(1), rows.max(3));
        let page = rows - 2;
        if self.chosen < self.top {
            self.top = self.chosen;
        } else if self.chosen >= self.top + page {
            self.top = self.chosen + 1 - page;
        }
        let line = |text: &str, attr: u8| {
            let mut cells: Vec<u16> = text.chars().take(cols).map(|c| (attr as u16) << 8 | crate::vga::cp437(c) as u16).collect();
            cells.resize(cols, (attr as u16) << 8 | b' ' as u16);
            cells
        };
        let mut out = Vec::with_capacity(rows);
        let mut cursor = (0, cols - 1);
        let (status, attr, at) = self.status_line();
        if let Some(at) = at {
            cursor = (0, at.min(cols - 1));
        }
        out.push(line(&status, attr));
        for at in self.top..self.top + page {
            out.push(match self.entries.get(at) {
                Some(entry) => {
                    let size = if entry.folder { String::from("<folder>") } else { entry.size.to_string() };
                    let slash = if entry.folder { "/" } else { "" };
                    let attr = if at == self.chosen { CHOSEN_ATTR } else if entry.folder { FOLDER_ATTR } else { TEXT_ATTR };
                    line(&format!(" {size:>10}  {}{slash}", entry.name), attr)
                }
                None => line("", TEXT_ATTR),
            });
        }
        out.push(line(" Enter open  e edit  Backspace up  v icons  n new folder  r rename  d delete  u again  q leave", HELP_ATTR));
        (out, cursor)
    }
}

fn listing(storage: &mut Storage, folder: &str) -> Result<Vec<Entry>, beefs::FsError> {
    let entries = storage.fs().read_dir(folder)?;
    Ok(entries.into_iter().map(|e| Entry { name: e.name, folder: e.kind == FileKind::Directory, size: e.size }).collect())
}

/// The thread of a file manager window: runs in the window's session until
/// `q`, or until the window is closed.
pub fn run(window: usize, start: String, mut storage: Storage) {
    let mut browser = Browser::new();
    match listing(&mut storage, &start) {
        Ok(entries) => browser.show(&start, entries, None),
        Err(_) => return,
    }
    // The icon view (`v`), its thumbnails and the first entry it shows.
    let (mut icons, mut thumbs, mut top) = (false, crate::icons::Thumbs::new(), 0);
    loop {
        let Some((cols, rows)) = console::pane_size(window) else { return };
        // The window in pixels (characters are 8 x 16).
        let grid = crate::icons::Grid::new(cols * 8, rows * 16);
        if icons {
            thumbs.update(&mut storage, &browser);
            top = grid.top_for(browser.chosen_index(), top);
            console::pane_draw(window, |canvas| crate::icons::draw(canvas, &browser, &thumbs, top));
        } else {
            let (lines, cursor) = browser.render(cols, rows);
            console::pane_show(window, &lines, cursor);
        }
        let (first, shown, total) = if icons { (0, 1, 1) } else { browser.scroll(rows) };
        console::pane_scroll(window, first, shown, total);
        let mut key = input::read_key();
        if key == Key::Ctrl('d') && session::current().is_none_or(session::is_closed) {
            return; // the window was closed
        }
        if browser.is_listing() {
            if key == Key::Char('v') {
                icons = !icons;
                top = 0;
                continue;
            }
            if icons {
                let count = browser.entries().len();
                if let Some(at) = crate::icons::step(browser.chosen_index(), key, grid.cols, grid.rows, count) {
                    browser.set_chosen(at);
                    continue;
                }
                if let Key::Click(col, row) = key {
                    // A click on an icon chooses it; on the chosen one it opens.
                    match grid.cell_at(col as usize * 8 + 4, row as usize * 16 + 8, top, count) {
                        Some(at) if at == browser.chosen_index() => key = Key::Enter,
                        Some(at) => {
                            browser.set_chosen(at);
                            continue;
                        }
                        None => continue,
                    }
                }
            }
        }
        let folder = String::from(browser.folder());
        // What to show afterwards: (folder, the name to put the bar on).
        let (mut next, mut choose) = (folder.clone(), None);
        let mut said: Option<(String, bool)> = None;
        match browser.key(key, rows.saturating_sub(2)) {
            Action::None => continue,
            Action::Quit => return,
            Action::Go(to) => {
                // Coming out of a folder, the bar is on that folder.
                if parent(&folder) == to && folder != to {
                    choose = folder.rsplit('/').next().map(String::from);
                }
                next = to;
            }
            Action::Open(path) if opener(&path) == Opener::Picture => {
                if let Err(why) = desk::open_picture(&path) {
                    said = Some((format!("{}: {why}", path.rsplit('/').next().unwrap_or("")), true));
                }
            }
            Action::Open(path) if opener(&path) == Opener::Sound => said = Some(play_in_background(&mut storage, &path)),
            Action::Open(path) if opener(&path) == Opener::Video => {
                if let Err(why) = desk::open_video(&path) {
                    said = Some((format!("{}: {why}", path.rsplit('/').next().unwrap_or("")), true));
                }
            }
            Action::Open(path) | Action::Edit(path) => {
                if let Err(why) = desk::open_editor(&path) {
                    said = Some((format!("{}: {why}", path.rsplit('/').next().unwrap_or("")), true));
                }
            }
            Action::Delete(path) => {
                let result = crate::users::console_may_change(&mut storage, &path)
                    .map_err(|whose| alloc::format!("it {whose}"))
                    .and_then(|()| storage.fs().remove(&path).map_err(|e| alloc::format!("{e}")));
                said = Some(match result {
                    Ok(()) => (String::from("deleted"), false),
                    Err(e) => (format!("not deleted: {e}"), true),
                });
            }
            Action::MakeFolder(path) => {
                let result = match crate::users::console_may_change(&mut storage, &path) {
                    Err(whose) => Err(format!("this folder {whose}")),
                    Ok(()) => {
                        let mut fs = storage.fs();
                        let made = if fs.exists(&path) { Err(beefs::FsError::AlreadyExists) } else { fs.mkdir_all(&path) };
                        made.map_err(|e| format!("{e}"))
                    }
                };
                match result {
                    Ok(()) => choose = path.rsplit('/').next().map(String::from),
                    Err(e) => said = Some((format!("no folder made: {e}"), true)),
                }
            }
            Action::Rename(from, to) => {
                let result = match crate::users::console_may_change(&mut storage, &from) {
                    Err(whose) => Err(format!("it {whose}")),
                    Ok(()) => {
                        let mut fs = storage.fs();
                        let renamed = if fs.exists(&to) { Err(beefs::FsError::AlreadyExists) } else { fs.rename(&from, &to) };
                        renamed.map_err(|e| format!("{e}"))
                    }
                };
                match result {
                    Ok(()) => choose = to.rsplit('/').next().map(String::from),
                    Err(e) => said = Some((format!("not renamed: {e}"), true)),
                }
            }
        }
        match listing(&mut storage, &next) {
            Ok(entries) => browser.show(&next, entries, choose.as_deref()),
            Err(e) => {
                // The folder cannot be shown (it is gone, or is a file): stay.
                said = Some((format!("{next}: {e}"), true));
                if let Ok(entries) = listing(&mut storage, &folder) {
                    browser.show(&folder, entries, None);
                }
            }
        }
        if let Some((text, warning)) = said {
            browser.say(&text, warning);
        }
    }
}

/// What opens a file: the picture viewer, the sound card, or the text editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opener {
    Picture,
    Sound,
    Video,
    Text,
}

/// By the name's ending (any case).
pub fn opener(name: &str) -> Opener {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ppm" => Opener::Picture,
        "wav" => Opener::Sound,
        "avi" => Opener::Video,
        _ => Opener::Text,
    }
}

/// Play a WAV file on a thread of its own, so the window goes on; what
/// to say in the window.
fn play_in_background(storage: &mut crate::storage::Storage, path: &str) -> (String, bool) {
    let name = path.rsplit('/').next().unwrap_or(path);
    let wav = storage.fs().read_file(path).map_err(|e| format!("{e}")).and_then(|d| crate::sound::parse_wav(&d).map_err(String::from));
    let wav = match wav {
        Ok(w) => w,
        Err(why) => return (format!("{name}: {why}"), true),
    };
    let seconds = wav.duration_text();
    let frames = wav.to_48k();
    let started = crate::thread::spawn("sound", None, move || {
        let _ = crate::hda::play(&frames, || false);
    });
    match started {
        Some(_) => (format!("playing {name} ({seconds})"), false),
        None => (format!("{name}: no room for another thread"), true),
    }
}

