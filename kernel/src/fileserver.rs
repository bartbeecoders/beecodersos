//! The file server: one folder of this machine, read and written over the
//! network by programs (`curl`, scripts, backup tools).
//!
//! OFF unless switched on with `fileserver on` (saved in /system/fileserver).
//! When on, a client that sends the key (`Authorization: Bearer KEY`) may,
//! below the shared folder and nowhere else:
//!   GET    /api/files/PATH    a file's bytes, or a folder's listing as JSON
//!   HEAD   /api/files/PATH    the same, without the bytes
//!   PUT    /api/files/PATH    write the file (folders above it are made);
//!                             PATH ending in `/`: make that folder
//!   DELETE /api/files/PATH    remove the file, or the empty folder
//! Without the right key every request gets 401 and nothing else.
//!
//! The key is made up by the OS and kept in the setting file. The link is
//! not encrypted: this is for the local network. The shared folder may not be
//! `/` or lie in /system, so settings and passwords stay out of reach.
//!
//! One thread, one request at a time. Clients are untrusted: the head is
//! bounded in size and time, a body in size (`MAX_BODY`) and in how long it
//! may stall, and a path can never leave the folder (`beenet::fileserver`).
//! Client addresses and file names are never logged.

use crate::ring::Global;
use crate::{net, rand, storage, thread, time};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beefs::{FileKind, FsError, MAX_NAME_LEN};
use beenet::fileserver::{self as proto, Entry, Head, HeadReader};

pub const DEFAULT_PORT: u16 = 9000;
pub const DEFAULT_FOLDER: &str = "/share";
const SETTING_FILE: &str = "/system/fileserver";
/// The largest file that can be sent or fetched: what BeeFS can hold (about
/// 8 MiB), and never more than 16 MiB, as it is held in memory whole.
pub const MAX_BODY: u64 = if beefs::MAX_FILE_SIZE < 16 * 1024 * 1024 { beefs::MAX_FILE_SIZE } else { 16 * 1024 * 1024 };
/// A client gets this long to send its request's head...
const HEAD_TIMEOUT_MS: u64 = 5_000;
/// ...and may stall this long while a body is received or sent.
const STALL_MS: u64 = 10_000;
const KEY_CHARS: usize = 40;

/// What is saved: the port when on, the folder, the key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Setting {
    pub port: Option<u16>,
    pub folder: String,
    pub key: String,
}

impl Setting {
    pub fn new() -> Setting {
        Setting { port: None, folder: String::from(DEFAULT_FOLDER), key: String::new() }
    }

    /// From the setting file's text. Lines: `on PORT` or `off`, `folder DIR`,
    /// `key KEY`; anything else is skipped, and a missing line keeps its default.
    pub fn parse(text: &str) -> Setting {
        let mut setting = Setting::new();
        for line in text.lines() {
            let line = line.trim();
            let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
            match (word, rest.trim()) {
                ("on", "") => setting.port = Some(DEFAULT_PORT),
                ("on", port) => setting.port = port.parse().ok().filter(|p| *p != 0),
                ("off", _) => setting.port = None,
                ("folder", dir) if folder_problem(dir).is_none() => setting.folder = String::from(dir),
                ("key", key) if key.len() >= 16 && key.bytes().all(|b| b.is_ascii_alphanumeric()) => setting.key = String::from(key),
                _ => {}
            }
        }
        setting
    }

    pub fn text(&self) -> String {
        let state = match self.port {
            Some(port) => format!("on {port}"),
            None => String::from("off"),
        };
        format!("{state}\nfolder {}\nkey {}\n", self.folder, self.key)
    }
}

/// Why this folder may not be shared (None: it may).
pub fn folder_problem(dir: &str) -> Option<&'static str> {
    let clean = dir.trim_end_matches('/');
    if !dir.starts_with('/') || dir.split('/').any(|n| n == "." || n == "..") {
        Some("give the whole path of a folder, from /")
    } else if clean.is_empty() {
        Some("not the whole disk: / holds the settings and passwords")
    } else if clean == "/system" || clean.starts_with("/system/") {
        Some("not /system: it holds the settings and passwords")
    } else {
        None
    }
}

struct State {
    /// Should be running, and on which port.
    wanted: Option<u16>,
    thread: Option<thread::ThreadId>,
    answered: u64,
    refused: u64,
}

static STATE: Global<State> = Global::new(State { wanted: None, thread: None, answered: 0, refused: 0 });

/// The setting as the file has it now (so a change works at once).
pub fn setting() -> Setting {
    let read = storage::handle().and_then(|mut s| s.fs().read_file(SETTING_FILE).ok());
    read.map(|d| Setting::parse(&String::from_utf8_lossy(&d))).unwrap_or_else(Setting::new)
}

/// The port the server listens on, if it is on.
pub fn port() -> Option<u16> {
    STATE.get().wanted
}

/// Requests answered since boot, and how many of them came without the key.
pub fn counters() -> (u64, u64) {
    let s = STATE.get();
    (s.answered, s.refused)
}

fn save(setting: &Setting) -> Result<(), String> {
    let mut storage = storage::handle().ok_or("no file system")?;
    let mut fs = storage.fs();
    let _ = fs.mkdir_all("/system");
    fs.write_file(SETTING_FILE, setting.text().as_bytes()).map_err(|e| format!("can't save the setting: {e}"))
}

fn new_key() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut key = String::new();
    while key.len() < KEY_CHARS {
        for byte in rand::secret32() {
            // Bytes past a whole number of alphabets are left out: every letter is as likely.
            if (byte as usize) < 256 - 256 % ALPHABET.len() && key.len() < KEY_CHARS {
                key.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
            }
        }
    }
    key
}

/// Make a new key and save it (a running server uses it from the next request).
pub fn change_key() -> Result<String, String> {
    let mut s = setting();
    s.key = new_key();
    save(&s)?;
    Ok(s.key)
}

/// Share another folder (it must exist) and save the choice.
pub fn set_folder(dir: &str) -> Result<(), String> {
    if let Some(why) = folder_problem(dir) {
        return Err(String::from(why));
    }
    let dir = if dir.len() > 1 { dir.trim_end_matches('/') } else { dir };
    let is_dir = storage::handle().map(|mut s| s.fs().is_dir(dir)).unwrap_or(false);
    if !is_dir {
        return Err(format!("{dir}: no such folder"));
    }
    let mut s = setting();
    s.folder = String::from(dir);
    save(&s)
}

/// Switch the server on (None: off) and save the choice. Switching on makes
/// the key if there is none yet, and the shared folder if it is missing.
pub fn set(port: Option<u16>) -> Result<(), String> {
    stop();
    let mut s = setting();
    if let Some(port) = port {
        if s.key.is_empty() {
            s.key = new_key();
        }
        if let Some(mut storage) = storage::handle() {
            storage.fs().mkdir_all(&s.folder).map_err(|e| format!("{}: {e}", s.folder))?;
        }
        start(port)?;
    }
    s.port = port;
    save(&s)
}

fn stop() {
    // Let a running server stop first: it frees the port.
    STATE.get().wanted = None;
    if let Some(id) = STATE.get().thread.take() {
        thread::join(id);
    }
}

fn start(port: u16) -> Result<(), String> {
    if !net::present() {
        return Err(String::from("no network card"));
    }
    net::tcp_listen(port).map_err(String::from)?;
    STATE.get().wanted = Some(port);
    let id = thread::spawn("fileserver", None, move || serve(port));
    if id.is_none() {
        net::tcp_unlisten(port);
        STATE.get().wanted = None;
        return Err(String::from("too many threads"));
    }
    STATE.get().thread = id;
    Ok(())
}

/// At boot: start the server if it was left switched on. Returns its port.
pub fn load() -> Option<u16> {
    let s = setting();
    let port = s.port?;
    if s.key.is_empty() {
        return None;
    }
    start(port).ok()?;
    Some(port)
}

fn serve(port: u16) {
    while STATE.get().wanted == Some(port) {
        match net::tcp_accept(port) {
            Some(stream) => {
                let authorized = answer(&stream);
                let s = STATE.get();
                s.answered += 1;
                s.refused += !authorized as u64;
            }
            None => {
                thread::pause(|| false); // until the next timer tick
            }
        }
    }
    net::tcp_unlisten(port);
}

/// An answer: status, reason, content type, extra header lines, body.
struct Answer {
    status: u16,
    reason: &'static str,
    kind: &'static str,
    more: String,
    body: Vec<u8>,
    /// For HEAD of a file: the length to announce (the body stays empty).
    length: Option<u64>,
}

impl Answer {
    fn new(status: u16, reason: &'static str, kind: &'static str, body: Vec<u8>) -> Answer {
        Answer { status, reason, kind, more: String::new(), body, length: None }
    }

    fn error(status: u16, reason: &'static str, message: &str) -> Answer {
        Answer::new(status, reason, "application/json", proto::error_json(message).into_bytes())
    }

    fn json(status: u16, reason: &'static str, text: String) -> Answer {
        Answer::new(status, reason, "application/json", text.into_bytes())
    }
}

/// Read one request and answer it. False: it came without the right key.
/// Whatever goes wrong with the connection, it is simply dropped.
fn answer(stream: &net::TcpStream) -> bool {
    let deadline = time::uptime_ms() + HEAD_TIMEOUT_MS;
    let mut reader = HeadReader::new();
    let mut buf = [0u8; 2048];
    let request = loop {
        match stream.receive(&mut buf) {
            net::Received::Data(n) => match reader.feed(&buf[..n]) {
                Ok(Some(request)) => break Ok(request),
                Ok(None) => {}
                Err(()) => break Err(()),
            },
            net::Received::Nothing if time::uptime_ms() < deadline => {
                thread::pause(|| false);
            }
            _ => return true, // closed, failed or too slow
        }
    };
    let Ok((head, early)) = request else {
        send(stream, &Answer::error(400, "Bad Request", "bad request"), false);
        return true;
    };
    let setting = setting();
    let authorized = proto::bearer(&head).is_some_and(|key| proto::same_secret(key, &setting.key));
    let reply = if !authorized {
        let mut a = Answer::error(401, "Unauthorized", "send the key: Authorization: Bearer KEY");
        a.more = String::from("WWW-Authenticate: Bearer\r\n");
        a
    } else {
        match body(stream, &head, early) {
            Ok(body) => rest(&setting.folder, &head, body),
            Err(a) => a,
        }
    };
    send(stream, &reply, head.method == "HEAD");
    authorized
}

/// The request's body, for the methods that have one.
fn body(stream: &net::TcpStream, head: &Head, early: Vec<u8>) -> Result<Vec<u8>, Answer> {
    let length = match head.content_length() {
        Err(()) => return Err(Answer::error(400, "Bad Request", "Content-Length is not a number")),
        Ok(Some(n)) => n,
        Ok(None) if head.method == "PUT" && !head.path.ends_with('/') => {
            return Err(Answer::error(411, "Length Required", "PUT needs a Content-Length"));
        }
        Ok(None) => 0,
    };
    if length > MAX_BODY {
        return Err(Answer::error(413, "Content Too Large", &format!("a file may have {} KiB at most", MAX_BODY >> 10)));
    }
    let length = length as usize;
    let mut data = early;
    data.truncate(length);
    if data.len() < length && head.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        let _ = stream.send_some(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let mut buf = alloc::vec![0u8; 16 * 1024];
    let mut last_progress = time::uptime_ms();
    while data.len() < length {
        let want = (length - data.len()).min(buf.len());
        match stream.receive(&mut buf[..want]) {
            net::Received::Data(n) => {
                data.extend_from_slice(&buf[..n]);
                last_progress = time::uptime_ms();
            }
            net::Received::Nothing if time::uptime_ms() - last_progress < STALL_MS => {
                thread::pause(|| false);
            }
            _ => return Err(Answer::error(400, "Bad Request", "the body ended early")),
        }
    }
    Ok(data)
}

fn send(stream: &net::TcpStream, answer: &Answer, head_only: bool) {
    let length = answer.length.unwrap_or(answer.body.len() as u64);
    let mut out = proto::response_head(answer.status, answer.reason, answer.kind, length, &answer.more).into_bytes();
    if !head_only {
        out.extend_from_slice(&answer.body);
    }
    let mut rest = out.as_slice();
    let mut last_progress = time::uptime_ms();
    while !rest.is_empty() {
        match stream.send_some(rest) {
            Ok(0) if time::uptime_ms() - last_progress > STALL_MS => return,
            Ok(0) => {
                thread::pause(|| false);
            }
            Ok(n) => {
                rest = &rest[n..];
                last_progress = time::uptime_ms();
            }
            Err(_) => return,
        }
    }
    stream.finish();
}

/// The REST interface: /api/files/PATH below the shared folder.
fn rest(folder: &str, head: &Head, body: Vec<u8>) -> Answer {
    let not_found = || Answer::error(404, "Not Found", "no such file or folder");
    let relative = match head.path.strip_prefix("/api/files") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => return Answer::error(404, "Not Found", "the files are at /api/files/"),
    };
    let Some(names) = proto::safe_path(relative, MAX_NAME_LEN) else {
        return Answer::error(400, "Bad Request", &format!("not a usable path (names have {MAX_NAME_LEN} characters at most)"));
    };
    let shown = format!("/{}", names.join("/"));
    let path = if names.is_empty() { String::from(folder) } else { format!("{folder}/{}", names.join("/")) };
    let Some(mut storage) = storage::handle() else { return Answer::error(503, "Service Unavailable", "no file system") };
    let mut fs = storage.fs();
    match head.method.as_str() {
        "GET" | "HEAD" => match fs.metadata(&path) {
            Ok(m) if m.kind == FileKind::Directory => match fs.read_dir(&path) {
                Ok(list) => {
                    let mut entries: Vec<Entry> = list
                        .into_iter()
                        .map(|e| Entry { name: e.name, folder: e.kind == FileKind::Directory, size: e.size })
                        .collect();
                    for e in entries.iter_mut().filter(|e| e.folder) {
                        e.size = 0;
                    }
                    entries.sort_by(|a, b| a.name.cmp(&b.name));
                    Answer::json(200, "OK", proto::listing_json(&shown, &entries))
                }
                Err(e) => failed(e),
            },
            Ok(m) if m.size > MAX_BODY => Answer::error(413, "Content Too Large", "the file is too large to send"),
            Ok(m) if head.method == "HEAD" => {
                let mut a = Answer::new(200, "OK", content_type(&shown), Vec::new());
                a.length = Some(m.size);
                a
            }
            Ok(_) => match fs.read_file(&path) {
                Ok(data) => Answer::new(200, "OK", content_type(&shown), data),
                Err(e) => failed(e),
            },
            Err(FsError::NotFound | FsError::NotADirectory) => not_found(),
            Err(e) => failed(e),
        },
        "PUT" if names.is_empty() => Answer::error(400, "Bad Request", "name a file or a folder"),
        "PUT" if relative.ends_with('/') => match fs.mkdir_all(&path) {
            Ok(()) => Answer::json(201, "Created", format!("{{\"path\":\"{}\",\"type\":\"folder\"}}\n", proto::json_escape(&shown))),
            Err(e) => failed(e),
        },
        "PUT" => {
            let existed = fs.exists(&path);
            let parent = &path[..path.rfind('/').unwrap_or(0)];
            let written = fs.mkdir_all(parent).and_then(|()| fs.write_file(&path, &body));
            match written {
                Ok(()) => {
                    let text = format!("{{\"path\":\"{}\",\"type\":\"file\",\"size\":{}}}\n", proto::json_escape(&shown), body.len());
                    if existed {
                        Answer::json(200, "OK", text)
                    } else {
                        Answer::json(201, "Created", text)
                    }
                }
                Err(e) => {
                    // Nothing half-written stays behind under a new name.
                    if !existed {
                        let _ = fs.remove(&path);
                    }
                    failed(e)
                }
            }
        }
        "DELETE" if names.is_empty() => Answer::error(403, "Forbidden", "the shared folder itself stays"),
        "DELETE" => match fs.remove(&path) {
            Ok(()) => Answer::new(204, "No Content", "application/json", Vec::new()),
            Err(FsError::NotFound | FsError::NotADirectory) => not_found(),
            Err(e) => failed(e),
        },
        _ => {
            let mut a = Answer::error(405, "Method Not Allowed", "GET, HEAD, PUT or DELETE");
            a.more = String::from("Allow: GET, HEAD, PUT, DELETE\r\n");
            a
        }
    }
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("txt" | "md" | "log" | "jsonl" | "scn" | "rs" | "py" | "sh" | "conf" | "bee" | "csv") => "text/plain; charset=utf-8",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("wav") => "audio/wav",
        Some("avi") => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

/// What a file system error is for the client.
fn failed(e: FsError) -> Answer {
    let text = format!("{e}");
    match e {
        FsError::NotFound => Answer::error(404, "Not Found", &text),
        FsError::IsADirectory | FsError::NotADirectory | FsError::DirectoryNotEmpty | FsError::AlreadyExists => Answer::error(409, "Conflict", &text),
        FsError::NameTooLong | FsError::InvalidPath => Answer::error(400, "Bad Request", &text),
        FsError::NoSpace | FsError::NoInodes | FsError::FileTooLarge => Answer::error(507, "Insufficient Storage", &text),
        _ => Answer::error(500, "Internal Server Error", &text),
    }
}
