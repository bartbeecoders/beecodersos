//! The web page viewer (`browse URL|FILE`): a window on the desktop that
//! shows a page from the web (http:// or https://) or an HTML or text file
//! from the disk, as text (beenet::html lays it out). Arrows and Page
//! Up/Down scroll, Tab picks the next link, Enter or a click follows it,
//! Backspace goes back, `g` asks for an address or words to search for,
//! `f` reads the page through FrogFind (simpler text), `r` loads again,
//! `q` leaves.
//!
//! `Viewer` is the page, the scrolling and the choice of link, and knows
//! nothing about windows, disks or the network: keys go in, what to do
//! comes out, and `render` gives the characters to show (the self-tests
//! cover it). `run` is the thread of the window: it fetches what was asked.

use crate::input::{self, Key};
use crate::storage::Storage;
use crate::{console, net, session};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beenet::html::{self, Page, Style};

/// The start page the desktop's Web button opens, written to the home
/// folder the first time (and left alone after that: it is the user's).
pub const START_NAME: &str = "start.html";
pub const START_PAGE: &str = "<title>Start</title>\n\
<h1>Start</h1>\n\
<p>This page is <b>start.html</b> in your home folder: change it with <code>edit</code>.\n\
Tab picks a link, Enter or a click follows it, Backspace goes back, g asks for an address.\n\
<h2>On the web</h2>\n<ul>\n\
<li><a href=\"http://info.cern.ch/hypertext/WWW/TheProject.html\">The first web site</a> (CERN, 1991)\n\
<li><a href=\"http://frogfind.com/\">FrogFind</a>: a search engine for simple browsers\n\
<li><a href=\"http://68k.news/\">68k.news</a>: the news, for simple browsers\n\
<li><a href=\"https://en.wikipedia.org/wiki/Honey_bee\">Honey bees</a> on Wikipedia\n<li><a href=\"http://neverssl.com/\">NeverSSL</a>\n</ul>\n\
<h2>On this machine</h2>\n<ul>\n<li><a href=\"notes.txt\">notes.txt</a>\n</ul>\n\
<p>https:// pages are encrypted (TLS 1.3); their certificates are not checked yet.\n";

/// Searching, and reading heavy pages as simpler text (`f`), goes through
/// FrogFind: a search engine for simple browsers that also hands out other
/// sites as plain HTML over http. The user chooses it (words typed, or `f`): it sees what
/// is searched for and which pages are read through it.
const SEARCH: &str = "http://frogfind.com/?q=";
const READER: &str = "http://frogfind.com/read?a=";

/// The address that searches for these words.
pub fn search_address(words: &str) -> String {
    format!("{SEARCH}{}", beenet::http::percent_encode(words.trim()))
}

/// The address that shows a page through the reader.
pub fn reader_address(url: &str) -> String {
    format!("{READER}{}", beenet::http::percent_encode(url.trim()))
}

/// What was typed (after `g`, or with `browse`): a path on the disk, a web
/// address (`http://` may be left out), or words to search for (with a
/// space, or without a dot).
pub fn typed_address(typed: &str) -> Result<String, String> {
    let typed = typed.trim();
    let lower = typed.to_ascii_lowercase();
    if typed.starts_with('/') {
        return resolve("/", typed);
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return resolve("/", typed);
    }
    let host = typed.split(['/', '?']).next().unwrap_or("");
    if typed.contains(char::is_whitespace) || !(host.contains('.') || host.contains(':') || host.eq_ignore_ascii_case("localhost")) {
        return Ok(search_address(typed));
    }
    resolve("/", &format!("http://{typed}"))
}

/// Pages larger than this are not shown.
pub const MAX_PAGE: u64 = 1024 * 1024;
/// Back steps remembered.
const MAX_HISTORY: usize = 50;

const TEXT_ATTR: u8 = 0x07;
const HEADING_ATTR: u8 = 0x0E;
const BOLD_ATTR: u8 = 0x0F;
const CODE_ATTR: u8 = 0x0A;
const LINK_ATTR: u8 = 0x0B;
const CHOSEN_ATTR: u8 = 0x1F;
const STATUS_ATTR: u8 = 0x70;
const WARNING_ATTR: u8 = 0x4F;
const HELP_ATTR: u8 = 0x08;

/// What a key asks the viewer's owner to do.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Action {
    None,
    Quit,
    /// Load this address (absolute: a path on the disk or an http:// URL).
    Open(String),
    /// Go back to this address (taken off the history).
    Back(String),
    /// Load this address again, keeping the place on the page.
    Reload(String),
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum Mode {
    View,
    /// Typing an address after `g`.
    Address(String),
}

/// What was loaded: kept to lay it out again when the window's width changes.
#[derive(Clone)]
enum Source {
    Html(String),
    Plain(String),
}

pub struct Viewer {
    address: String,
    source: Source,
    page: Page,
    /// The width the page was laid out for.
    width: usize,
    top: usize,
    chosen: Option<usize>,
    history: Vec<String>,
    mode: Mode,
    message: String,
    warning: bool,
    /// Pictures loaded for this page: the address as the page wrote it,
    /// and the picture made to fit the window.
    images: Vec<(String, crate::image::Image)>,
    /// Pictures that could not be loaded (not tried again).
    failed: Vec<String>,
}

/// At most this many pictures are loaded for a page.
pub const MAX_PICTURES: usize = 16;
/// Pictures are drawn at most this high (pixels).
pub const PICTURE_HEIGHT: usize = 320;

/// Does this look like HTML (rather than plain text)?
pub fn looks_like_html(address: &str, body: &str) -> bool {
    let lower = address.to_ascii_lowercase();
    let path = lower.split(['?', '#']).next().unwrap_or("");
    if path.ends_with(".html") || path.ends_with(".htm") {
        return true;
    }
    if path.ends_with(".txt") {
        return false;
    }
    let start: String = body.chars().take(1024).collect::<String>().to_ascii_lowercase();
    start.trim_start().starts_with('<') && (start.contains("<html") || start.contains("<!doctype html") || start.contains("<p") || start.contains("<a ") || start.contains("<h1"))
}

/// Remove `.` and `..` steps from an absolute path (`..` above the root stays at the root).
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    if path.ends_with('/') && out.len() > 1 {
        out.push('/');
    }
    out
}

/// Where a link on the page at `base` leads: an absolute path on the disk
/// or an http(s):// URL, or why it cannot be followed.
pub fn resolve(base: &str, href: &str) -> Result<String, String> {
    let href = href.trim();
    let lower = href.to_ascii_lowercase();
    let scheme = href.find(':').map(|i| &lower[..i]).filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()));
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let url = beenet::http::Url::parse(href).map_err(|e| String::from(e.message()))?;
        return Ok(format!("{url}"));
    }
    if let Some(rest) = href.strip_prefix("//") {
        // The same scheme as the page.
        let secure = base.to_ascii_lowercase().starts_with("https://");
        let url = beenet::http::Url::parse(&format!("{}{rest}", if secure { "https://" } else { "http://" })).map_err(|e| String::from(e.message()))?;
        return Ok(format!("{url}"));
    }
    if let Some(scheme) = scheme.filter(|s| *s != "http" && *s != "https") {
        return Err(format!("{scheme}: links cannot be followed"));
    }
    let href = href.split('#').next().unwrap_or("");
    if base.starts_with('/') {
        // A page from the disk: its links are files on the disk.
        let href = href.split('?').next().unwrap_or("");
        if href.is_empty() {
            return Ok(String::from(base));
        }
        let path = if href.starts_with('/') { String::from(href) } else { format!("{}/{href}", crate::filemgr::parent(base)) };
        return Ok(normalize(&path));
    }
    let url = beenet::http::Url::parse(base).map_err(|e| String::from(e.message()))?;
    let joined = url.join(if href.is_empty() { "./" } else { href }).map_err(|e| String::from(e.message()))?;
    let (path, query) = match joined.path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (joined.path.as_str(), None),
    };
    let mut path = normalize(path);
    if let Some(q) = query {
        path.push('?');
        path.push_str(q);
    }
    Ok(format!("{}{}{path}", joined.scheme(), joined.authority()))
}

impl Viewer {
    pub fn new() -> Viewer {
        Viewer {
            address: String::new(),
            source: Source::Plain(String::new()),
            page: Page::default(),
            width: 0,
            top: 0,
            chosen: None,
            history: Vec::new(),
            mode: Mode::View,
            message: String::new(),
            warning: false,
            images: Vec::new(),
            failed: Vec::new(),
        }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn title(&self) -> &str {
        &self.page.title
    }

    /// Show what was loaded from `address`. `back`: it is the page we came
    /// from, so it is not remembered again. `again`: the same page loaded
    /// again, which keeps its place.
    pub fn show(&mut self, address: &str, body: &str, back: bool, again: bool) {
        if !back && !again && !self.address.is_empty() && self.address != address {
            if self.history.len() == MAX_HISTORY {
                self.history.remove(0);
            }
            self.history.push(core::mem::take(&mut self.address));
        }
        self.address = String::from(address);
        self.source = if looks_like_html(address, body) { Source::Html(String::from(body)) } else { Source::Plain(String::from(body)) };
        self.width = 0;
        self.images.clear();
        self.failed.clear();
        if !again {
            self.top = 0;
            self.chosen = None;
        }
        self.mode = Mode::View;
    }

    /// Pictures on the page not loaded (or tried) yet, as the page wrote
    /// their addresses; at most `MAX_PICTURES` in all.
    pub fn missing_images(&mut self, width: usize) -> Vec<String> {
        self.lay_out(width);
        let mut out: Vec<String> = Vec::new();
        for i in &self.page.images {
            let seen = out.contains(&i.src) || self.images.iter().any(|(s, _)| *s == i.src) || self.failed.contains(&i.src);
            if i.rows == 0 && !seen && out.len() + self.images.len() + self.failed.len() < MAX_PICTURES {
                out.push(i.src.clone());
            }
        }
        out
    }

    /// A picture of the page, loaded: the page is laid out again around it.
    pub fn add_image(&mut self, src: &str, picture: crate::image::Image) {
        self.images.push((String::from(src), picture));
        self.width = 0;
    }

    pub fn image_failed(&mut self, src: &str) {
        self.failed.push(String::from(src));
    }

    /// How many lines a picture of `height` pixels takes (and one more as a gap).
    fn rows_for(height: usize) -> usize {
        height.div_ceil(16) + 1
    }

    /// The pictures to draw in a window of `rows` lines: each with the
    /// pixel line its top is at (above the window if negative).
    pub fn image_spots(&self, rows: usize) -> Vec<(&crate::image::Image, isize)> {
        let page_rows = rows.saturating_sub(2);
        let mut out = Vec::new();
        for spot in self.page.images.iter().filter(|i| i.rows > 0) {
            if spot.line + spot.rows <= self.top || spot.line >= self.top + page_rows {
                continue;
            }
            if let Some((_, picture)) = self.images.iter().find(|(s, _)| *s == spot.src) {
                out.push((picture, (spot.line as isize - self.top as isize + 1) * 16));
            }
        }
        out
    }

    /// A message in the status line until the next key.
    pub fn say(&mut self, text: &str, warning: bool) {
        self.message = String::from(text);
        self.warning = warning;
    }

    /// Lay the page out for this width, if it was not already.
    fn lay_out(&mut self, width: usize) {
        if self.width == width {
            return;
        }
        let known: Vec<(String, usize)> = self.images.iter().map(|(s, p)| (s.clone(), Self::rows_for(p.height))).collect();
        self.page = match &self.source {
            Source::Html(text) => html::render_with(text, width, &known),
            Source::Plain(text) => html::plain(text, width),
        };
        self.width = width;
        self.chosen = self.chosen.filter(|c| *c < self.page.links.len());
        self.top = self.top.min(self.page.lines.len().saturating_sub(1));
    }

    /// The page's lines as text (what the host sees of the window, and tests).
    pub fn lines(&mut self, width: usize) -> Vec<String> {
        self.lay_out(width);
        (0..self.page.lines.len()).map(|i| self.page.text(i)).collect()
    }

    /// Where a link leads, by its number on the page.
    pub fn link(&self, n: usize) -> Option<&str> {
        self.page.links.get(n).map(String::as_str)
    }

    fn follow(&mut self, n: usize) -> Action {
        let Some(href) = self.page.links.get(n) else { return Action::None };
        match resolve(&self.address, href) {
            Ok(to) => Action::Open(to),
            Err(why) => {
                self.say(&why, true);
                Action::None
            }
        }
    }

    /// Bring the chosen link into view.
    fn reveal(&mut self, page_rows: usize) {
        let Some(line) = self.chosen.and_then(|c| self.page.line_of_link(c)) else { return };
        if line < self.top || line >= self.top + page_rows {
            self.top = line.saturating_sub(page_rows / 3);
        }
    }

    /// The link that starts on or after the top line, after `from`.
    fn next_link(&self, from: Option<usize>) -> Option<usize> {
        let count = self.page.links.len();
        if count == 0 {
            return None;
        }
        Some(match from {
            Some(c) => (c + 1) % count,
            None => (0..count).find(|l| self.page.line_of_link(*l).is_some_and(|line| line >= self.top)).unwrap_or(0),
        })
    }

    /// Handle a key; `width` and `rows` are the window's size in characters.
    pub fn key(&mut self, key: Key, width: usize, rows: usize) -> Action {
        self.lay_out(width);
        self.message.clear();
        let page_rows = rows.saturating_sub(2).max(1);
        let last_top = self.page.lines.len().saturating_sub(page_rows);
        if let Mode::Address(text) = &mut self.mode {
            match key {
                Key::Char(c) if text.len() < 2000 && !c.is_control() => text.push(c),
                Key::Backspace => {
                    text.pop();
                }
                Key::Enter => {
                    let typed = String::from(text.trim());
                    self.mode = Mode::View;
                    if typed.is_empty() {
                        return Action::None;
                    }
                    return match typed_address(&typed) {
                        Ok(to) => Action::Open(to),
                        Err(why) => {
                            self.say(&why, true);
                            Action::None
                        }
                    };
                }
                Key::Escape | Key::Ctrl('c') => self.mode = Mode::View,
                _ => {}
            }
            return Action::None;
        }
        match key {
            Key::Char('q') | Key::Ctrl('q') => return Action::Quit,
            Key::Up => self.top = self.top.saturating_sub(1),
            Key::Down => self.top = (self.top + 1).min(last_top),
            Key::PageUp => self.top = self.top.saturating_sub(page_rows),
            Key::PageDown | Key::Char(' ') => self.top = (self.top + page_rows).min(last_top),
            Key::Home => self.top = 0,
            Key::End => self.top = last_top,
            Key::Tab => {
                self.chosen = self.next_link(self.chosen);
                if self.chosen.is_none() {
                    self.say("no links on this page", false);
                }
                self.reveal(page_rows);
            }
            Key::Enter => {
                if let Some(n) = self.chosen {
                    return self.follow(n);
                }
                self.say("Tab picks a link first", false);
            }
            Key::Click(col, row) => {
                let row = row as usize;
                if row >= 1 && row <= page_rows {
                    let link = self.page.lines.get(self.top + row - 1).and_then(|l| l.get(col as usize)).and_then(|c| c.link);
                    if let Some(n) = link {
                        self.chosen = Some(n as usize);
                        return self.follow(n as usize);
                    }
                }
            }
            Key::Backspace => match self.history.pop() {
                Some(to) => return Action::Back(to),
                None => self.say("this is the first page", false),
            },
            Key::Char('g') => self.mode = Mode::Address(String::new()),
            Key::Char('r') => return Action::Reload(self.address.clone()),
            // The page through FrogFind's reader: simpler text for heavy pages.
            Key::Char('f') if self.address.starts_with("http") && !self.address.starts_with(READER) => {
                return Action::Open(reader_address(&self.address));
            }
            Key::Char('f') => self.say("f reads a page from the web through FrogFind", false),
            _ => {}
        }
        Action::None
    }

    /// The characters to show in a window of `cols` x `rows` (cells in the
    /// VGA format) and where the cursor goes (row, column).
    pub fn render(&mut self, cols: usize, rows: usize) -> (Vec<Vec<u16>>, (usize, usize)) {
        let (cols, rows) = (cols.max(10), rows.max(3));
        self.lay_out(cols);
        let page_rows = rows - 2;
        let line = |text: &str, attr: u8| {
            let mut cells: Vec<u16> = text.chars().take(cols).map(|c| (attr as u16) << 8 | crate::vga::cp437(c) as u16).collect();
            cells.resize(cols, (attr as u16) << 8 | b' ' as u16);
            cells
        };
        let mut out = Vec::with_capacity(rows);
        let mut cursor = (0, cols - 1);
        let (status, attr) = match &self.mode {
            Mode::Address(text) => {
                let ask = " Go to: ";
                cursor = (0, (ask.len() + text.chars().count()).min(cols - 1));
                (format!("{ask}{text}"), STATUS_ATTR)
            }
            Mode::View if !self.message.is_empty() => (format!(" {}", self.message), if self.warning { WARNING_ATTR } else { STATUS_ATTR }),
            Mode::View => {
                let total = self.page.lines.len().max(1);
                let shown = ((self.top + page_rows).min(total) * 100) / total;
                let title = if self.page.title.is_empty() { String::new() } else { format!("{} - ", self.page.title) };
                (format!(" {title}{}  ({shown}%)", self.address), STATUS_ATTR)
            }
        };
        out.push(line(&status, attr));
        for at in self.top..self.top + page_rows {
            let cells = match self.page.lines.get(at) {
                Some(cells) => {
                    let mut shown: Vec<u16> = cells
                        .iter()
                        .take(cols)
                        .map(|cell| {
                            let attr = match (cell.link, cell.style) {
                                (Some(n), _) if Some(n as usize) == self.chosen => CHOSEN_ATTR,
                                (Some(_), _) | (_, Style::Link) => LINK_ATTR,
                                (_, Style::Heading) => HEADING_ATTR,
                                (_, Style::Bold) => BOLD_ATTR,
                                (_, Style::Code) => CODE_ATTR,
                                (_, Style::Text) => TEXT_ATTR,
                            };
                            (attr as u16) << 8 | crate::vga::cp437(cell.ch) as u16
                        })
                        .collect();
                    shown.resize(cols, (TEXT_ATTR as u16) << 8 | b' ' as u16);
                    shown
                }
                None => line("", TEXT_ATTR),
            };
            out.push(cells);
        }
        let more = if self.page.cut { "  (page cut off)" } else { "" };
        out.push(line(&format!(" Tab link  Enter/click follow  Backspace back  g go to  r reload  q leave{more}"), HELP_ATTR));
        (out, cursor)
    }
}

/// Fetch a page: a file from the disk (`/...`) or an http:// URL. Returns
/// the text, the address it really came from (after redirects), and for
/// HTTPS how far the certificate was checked.
pub fn fetch(storage: &mut Storage, address: &str) -> Result<(String, String, Option<String>), String> {
    if address.starts_with('/') {
        let data = storage.fs().read_file(address).map_err(|e| format!("{e}"))?;
        if data.len() as u64 > MAX_PAGE {
            return Err(format!("too large to show ({} KiB at most)", MAX_PAGE / 1024));
        }
        return Ok((String::from_utf8_lossy(&data).into_owned(), String::from(address), None));
    }
    let mut body = Vec::new();
    let result = net::http_get(address, MAX_PAGE, &mut |data| {
        body.extend_from_slice(data);
        Ok(())
    });
    match result {
        Ok(r) => {
            let note = r.trust_note();
            let at = r.redirected_to.map_or_else(|| String::from(address), |to| resolve(address, &to).unwrap_or(to));
            Ok((String::from_utf8_lossy(&body).into_owned(), at, note))
        }
        Err(e) if e == "too large" => Err(format!("too large to show ({} KiB at most)", MAX_PAGE / 1024)),
        Err(e) => Err(e),
    }
}

/// The thread of a viewer window: runs in the window's session until `q`,
/// or until the window is closed.
pub fn run(window: usize, start: String, mut storage: Storage) {
    let mut viewer = Viewer::new();
    let mut next = Some((start, false, false));
    loop {
        if let Some((address, back, again)) = next.take() {
            if let Some((cols, rows)) = console::pane_size(window) {
                viewer.say(&format!("loading {address} ..."), false);
                let (lines, cursor) = viewer.render(cols, rows);
                console::pane_show(window, &lines, cursor);
            }
            match fetch(&mut storage, &address) {
                Ok((body, at, note)) => {
                    viewer.show(&at, &body, back, again);
                    let note = String::from(note.as_deref().unwrap_or(""));
                    load_pictures(window, &mut viewer, &mut storage, &note);
                    viewer.say(&note, false);
                }
                Err(why) => {
                    if back {
                        // Could not go back: the page stays, and so does the way back.
                        viewer.history.push(address.clone());
                    }
                    viewer.say(&format!("{address}: {why}"), true);
                }
            }
        }
        let Some((cols, rows)) = console::pane_size(window) else { return };
        let (lines, cursor) = viewer.render(cols, rows);
        console::pane_show(window, &lines, cursor);
        draw_pictures(window, &viewer, rows);
        let key = input::read_key();
        if key == Key::Ctrl('d') && session::current().is_none_or(session::is_closed) {
            return; // the window was closed
        }
        match viewer.key(key, cols.max(10), rows.max(3)) {
            Action::None => {}
            Action::Quit => return,
            Action::Open(to) => next = Some((to, false, false)),
            Action::Back(to) => next = Some((to, true, false)),
            Action::Reload(to) => next = Some((to, false, true)),
        }
    }
}

/// Load the pictures of the page just shown (from the web or the disk),
/// each made to fit the window; the page is laid out again around them.
fn load_pictures(window: usize, viewer: &mut Viewer, storage: &mut Storage, note: &str) {
    let Some((cols, rows)) = console::pane_size(window) else { return };
    let wanted = viewer.missing_images(cols);
    let base = String::from(viewer.address());
    for (i, src) in wanted.iter().enumerate() {
        viewer.say(&format!("loading pictures ({} of {}) ... {note}", i + 1, wanted.len()), false);
        let (lines, cursor) = viewer.render(cols, rows);
        console::pane_show(window, &lines, cursor);
        draw_pictures(window, viewer, rows);
        match load_picture(storage, &base, src, (cols * 8).saturating_sub(16), PICTURE_HEIGHT) {
            Some(picture) => viewer.add_image(src, picture),
            None => viewer.image_failed(src),
        }
    }
}

/// One picture: its address seen from the page, read, decoded, made to fit.
pub fn load_picture(storage: &mut Storage, base: &str, src: &str, max_w: usize, max_h: usize) -> Option<crate::image::Image> {
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    let address = resolve(base, src).ok()?;
    let data = if address.starts_with('/') {
        storage.fs().read_file(&address).ok().filter(|d| d.len() as u64 <= MAX_BYTES)?
    } else {
        let mut body = Vec::new();
        net::http_get(&address, MAX_BYTES, &mut |d| {
            body.extend_from_slice(d);
            Ok(())
        })
        .ok()?;
        body
    };
    // Only formats the OS reads, and not too many pixels (SVG and WebP are skipped).
    let (w, h) = crate::image::dimensions(&data)?;
    if w * h > 16 * 1024 * 1024 {
        return None;
    }
    let picture = crate::image::decode(&data).ok()?;
    Some(crate::image::shrunk(&picture, max_w, max_h))
}

/// Draw the page's pictures where their lines are, between the status
/// line and the help line.
fn draw_pictures(window: usize, viewer: &Viewer, rows: usize) {
    let spots = viewer.image_spots(rows);
    if spots.is_empty() {
        return;
    }
    let (top, bottom) = (16isize, (rows.saturating_sub(1) * 16) as isize);
    console::pane_draw(window, |canvas| {
        for (picture, y) in &spots {
            for row in 0..picture.height {
                let py = y + row as isize;
                if py >= top && py < bottom {
                    canvas.put_row(8, py as usize, &picture.pixels[row * picture.width..(row + 1) * picture.width]);
                }
            }
        }
    });
}

