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
//! The same folder speaks the S3 protocol on every other path (path style:
//! /BUCKET/KEY): its folders are the buckets, the files below them the
//! objects. Requests are signed (AWS Signature Version 4) with the access
//! key id `beecoders` and the same key as the secret; an unsigned or wrongly
//! signed request gets 403 and nothing else. Buckets: list, make, remove.
//! Objects: put, get, head, delete, delete many, list (both versions, with
//! prefix, delimiter and pages), uploads in parts (multipart). A link (`fileserver link FILE`, or a
//! client's presigned URL) fetches one file without a key until it expires.
//! Not there: copies, versions, access lists.
//!
//! The key is made up by the OS and kept in the setting file. The link is
//! not encrypted: this is for the local network. The shared folder may not be
//! `/` or lie in /system, so settings and passwords stay out of reach.
//!
//! One thread, one request at a time. Clients are untrusted: the head is
//! bounded in size and time, a body in size (the disk's free room) and in how long it
//! may stall, and a path can never leave the folder (`beenet::fileserver`).
//! Client addresses and file names are never logged.

use crate::ring::Global;
use crate::{net, rand, storage, thread, time};
use beenet::s3;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beefs::{FileKind, FsError, MAX_NAME_LEN};
use beenet::fileserver::{self as proto, Entry, Head, HeadReader};

pub const DEFAULT_PORT: u16 = 9000;
pub const DEFAULT_FOLDER: &str = "/share";
const SETTING_FILE: &str = "/system/fileserver";
/// The largest body that is held in memory (a list of keys to delete).
/// A file goes to the disk as it arrives, and comes from it piece by piece:
/// it may be as large as the disk has room, and as BeeFS holds (about 1 GiB).
pub const MAX_BODY: u64 = 2 * 1024 * 1024;
/// The size of the pieces in which a file is received and sent.
const PIECE: usize = 64 * 1024;
/// The size of the pieces in which parts are put together on the disk:
/// every write ends with the file system's bookkeeping and a flush of the
/// disk's cache, so fewer, larger writes are faster. (Not for uploads:
/// while a write is under way nothing is taken from the network, and with
/// writes this large an upload was slower, 16 s against 10 s for 100 MB.)
const WRITE_PIECE: usize = 1024 * 1024;
/// Uploads are written under this name in the shared folder until they are
/// complete and checked; only then do they get their own name.
const UPLOAD_PREFIX: &str = ".up-";
/// The parts of an upload in parts wait in a folder of this name (and the
/// upload's 16 hex digits) in the shared folder until they are put together.
const PARTS_PREFIX: &str = ".mp-";
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
    clear_uploads(&setting().folder);
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

/// A link that fetches the file at `path` (inside a folder of the shared
/// folder) without a key, for `hours`: "http://ADDRESS:PORT/..." to open
/// in a browser or hand to someone on the network.
pub fn link(path: &str, hours: u64) -> Result<String, String> {
    let s = setting();
    let port = port().ok_or("the file server is off ('fileserver on')")?;
    let ip = net::address().ok_or("no network address yet (see 'net')")?;
    let now = time::now_unix().ok_or("the clock is not set (see 'ntp'): a link could never expire")?;
    if hours == 0 || hours * 3600 > s3::MAX_LINK_SECONDS {
        return Err(format!("a link lasts 1 to {} hours", s3::MAX_LINK_SECONDS / 3600));
    }
    let inside = path.strip_prefix(s.folder.as_str()).filter(|rest| rest.starts_with('/')).ok_or(format!("only files in {} can be linked", s.folder))?;
    let names = proto::safe_path(inside, MAX_NAME_LEN).ok_or("not a usable path")?;
    if names.len() < 2 || !s3::bucket_name_ok(names[0], MAX_NAME_LEN) {
        return Err(format!("the file must be in a folder of {} (a bucket), not in {} itself", s.folder, s.folder));
    }
    let is_file = storage::handle().map(|mut st| st.fs().metadata(path).is_ok_and(|m| m.kind == FileKind::File)).unwrap_or(false);
    if !is_file {
        return Err(format!("{path}: no such file"));
    }
    let host = format!("{ip}:{port}");
    Ok(format!("http://{host}{}", s3::link(&format!("/{}", names.join("/")), &host, &s.key, now, hours * 3600)))
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
    /// For a file: the length to announce (the body stays empty).
    length: Option<u64>,
    /// The file whose content is the body: sent from the disk piece by piece.
    file: Option<String>,
    /// The head went out already (an answer that took long to make): only the body follows.
    headless: bool,
    /// Where in the file the body starts (a `Range` request).
    offset: u64,
}

/// What came with a request: a small body, or an upload already on the disk.
enum Body {
    Bytes(Vec<u8>),
    /// A complete, checked upload under its temporary name, and its size.
    File(String, u64),
}

impl Answer {
    fn new(status: u16, reason: &'static str, kind: &'static str, body: Vec<u8>) -> Answer {
        Answer { status, reason, kind, more: String::new(), body, length: None, file: None, headless: false, offset: 0 }
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
    let (reply, authorized) = if head.path == "/api" || head.path.starts_with("/api/") {
        if !proto::bearer(&head).is_some_and(|key| proto::same_secret(key, &setting.key)) {
            let mut a = Answer::error(401, "Unauthorized", "send the key: Authorization: Bearer KEY");
            a.more = String::from("WWW-Authenticate: Bearer\r\n");
            (a, false)
        } else {
            let taken = if head.method == "PUT" && !head.path.ends_with('/') {
                receive_upload(stream, &head, early, s3::BodyReader::plain(), &setting.folder, 0)
            } else {
                body(stream, &head, early).map(Body::Bytes)
            };
            match taken {
                Ok(body) => (acted(body, |body| rest(&setting.folder, &head, body)), true),
                Err(e) => {
                    let (status, reason, _, message) = e.parts();
                    (Answer::error(status, reason, &message), true)
                }
            }
        }
    } else if s3::is_link(&head) {
        // A link (presigned URL): its signature is in the address, so a
        // browser can follow it. It fetches one file, until it expires.
        let one_file = proto::safe_path(&head.path, MAX_NAME_LEN).is_some_and(|names| names.len() >= 2) && !head.path.ends_with('/');
        match s3::verify_link(&head, &setting.key, time::now_unix()) {
            Err(e) => (s3_error(403, "Forbidden", e.code(), if e == s3::AuthError::Skewed { "the link has expired" } else { e.message() }, &head.path), false),
            Ok(()) if !one_file || !matches!(head.method.as_str(), "GET" | "HEAD") => {
                (s3_error(403, "Forbidden", "AccessDenied", "a link only fetches one file", &head.path), false)
            }
            Ok(()) => (s3_act(&setting.folder, &head, Body::Bytes(Vec::new())), true),
        }
    } else {
        // Everything else is S3: nothing happens before the signature is right.
        match s3::verify(&head, &setting.key, time::now_unix()) {
            Err(e) => (s3_error(403, "Forbidden", e.code(), e.message(), &head.path), false),
            Ok(auth) => (s3_request(stream, &setting.folder, &head, early, &auth), true),
        }
    };
    send(stream, &reply, head.method == "HEAD");
    authorized
}

/// More than a file's size that a chunked upload may take on the wire.
const CHUNK_ROOM: u64 = 256 * 1024;

fn s3_error(status: u16, reason: &'static str, code: &str, message: &str, resource: &str) -> Answer {
    Answer::new(status, reason, "application/xml", s3::error_xml(code, message, resource).into_bytes())
}

fn s3_xml(text: String) -> Answer {
    Answer::new(200, "OK", "application/xml", text.into_bytes())
}

/// A file's answer cut down to the part a `Range` header asks for (206), or
/// `None` when the file has no such part (416: each side says that its way).
fn ranged(mut answer: Answer, head: &Head) -> Option<Answer> {
    let (Some(size), true) = (answer.length, answer.file.is_some()) else { return Some(answer) };
    answer.more += "Accept-Ranges: bytes\r\n";
    match head.header("range").map(|range| proto::byte_range(range, size)) {
        None | Some(Ok(None)) => Some(answer),
        Some(Ok(Some((first, last)))) => {
            answer.status = 206;
            answer.reason = "Partial Content";
            answer.offset = first;
            answer.length = Some(last - first + 1);
            answer.more += &format!("Content-Range: bytes {first}-{last}/{size}\r\n");
            Some(answer)
        }
        Some(Err(())) => None,
    }
}

/// Run a request's action; an upload that the action did not take (it gave
/// the file its name) is removed afterwards.
fn acted(body: Body, act: impl FnOnce(Body) -> Answer) -> Answer {
    let temp = match &body {
        Body::File(temp, _) => Some(temp.clone()),
        Body::Bytes(_) => None,
    };
    let answer = act(body);
    if let (Some(temp), Some(mut storage)) = (temp, storage::handle()) {
        let _ = storage.fs().remove(&temp);
    }
    answer
}

/// A signed S3 request: take its body as the client meant it, then act.
fn s3_request(stream: &net::TcpStream, folder: &str, head: &Head, early: Vec<u8>, auth: &s3::Auth) -> Answer {
    let chunked = matches!(auth.payload, s3::Payload::Chunked { .. });
    let decoded = head.header("x-amz-decoded-content-length").and_then(|v| v.parse::<u64>().ok());
    let object = proto::safe_path(&head.path, MAX_NAME_LEN).is_some_and(|names| names.len() >= 2);
    let taken = if head.method == "PUT" && object && !head.path.ends_with('/') && head.header("x-amz-copy-source").is_none() {
        // An object: to the disk as it arrives, checked piece by piece.
        receive_upload(stream, head, early, s3::BodyReader::new(auth, decoded), folder, if chunked { length_room(head) } else { 0 })
    } else {
        body(stream, head, early).and_then(|received| s3::body(auth, received, decoded).map(Body::Bytes).map_err(|_| BodyError::NotAsSigned))
    };
    match taken {
        Ok(Body::Bytes(xml)) if head.method == "POST" && object && head.query_value("uploadId").is_some() => complete_parts(stream, folder, head, &xml),
        Ok(body) => acted(body, |body| s3_act(folder, head, body)),
        Err(e) => {
            let (status, reason, code, message) = e.parts();
            s3_error(status, reason, code, &message, &head.path)
        }
    }
}

/// Where the parts of upload `id` wait, if that is an upload for this key.
fn parts_dir(fs: &mut storage::FsGuard, folder: &str, id: &str, key: &str) -> Option<String> {
    if !s3::upload_id_ok(id) {
        return None;
    }
    let dir = format!("{folder}/{PARTS_PREFIX}{id}");
    let for_key = fs.read_file(&format!("{dir}/key")).ok()?;
    (for_key == key.as_bytes()).then_some(dir)
}

/// Remove a folder of parts and what is in it.
fn remove_parts(fs: &mut storage::FsGuard, dir: &str) {
    for entry in fs.read_dir(dir).unwrap_or_default() {
        let _ = fs.remove(&format!("{dir}/{}", entry.name));
    }
    let _ = fs.remove(dir);
}

/// "The parts are complete": put them together, in the order the client
/// names them, into the object. That can take a while for a large file, so
/// the answer starts at once (200, as S3 does) and spaces keep the
/// connection alive until the result, or the error, can follow.
fn complete_parts(stream: &net::TcpStream, folder: &str, head: &Head, xml: &[u8]) -> Answer {
    let fail = |status: u16, reason: &'static str, code: &str, message: &str| s3_error(status, reason, code, message, &head.path);
    let no_upload = || fail(404, "Not Found", "NoSuchUpload", "no such upload in parts (finished, given up, or never started)");
    let names = match proto::safe_path(&head.path, MAX_NAME_LEN) {
        Some(names) if names.len() >= 2 && s3::bucket_name_ok(names[0], MAX_NAME_LEN) => names,
        _ => return fail(400, "Bad Request", "KeyTooLongError", "not a usable key"),
    };
    let key = names.join("/");
    let id = head.query_value("uploadId").unwrap_or_default();
    let Some(parts) = s3::completed_parts(xml) else {
        return fail(400, "Bad Request", "MalformedXML", "not a list of parts, numbered 1 to 10000 in rising order");
    };
    let Some(mut storage) = storage::handle() else { return fail(503, "Service Unavailable", "ServiceUnavailable", "no file system") };
    let path = format!("{folder}/{key}");
    // Everything that can be refused is, before the answer starts.
    let dir = {
        let mut fs = storage.fs();
        let Some(dir) = parts_dir(&mut fs, folder, &id, &key) else { return no_upload() };
        if !fs.is_dir(&format!("{folder}/{}", names[0])) {
            return fail(404, "Not Found", "NoSuchBucket", "no such bucket");
        }
        if fs.is_dir(&path) {
            return fail(409, "Conflict", "InvalidRequest", "a folder has that name");
        }
        let mut total = 0u64;
        for part in &parts {
            match fs.metadata(&format!("{dir}/{part}")) {
                Ok(m) if m.kind == FileKind::File => total += m.size,
                _ => return fail(400, "Bad Request", "InvalidPart", &format!("part {part} was not sent")),
            }
        }
        if total > beefs::MAX_FILE_SIZE {
            return fail(400, "Bad Request", "EntityTooLarge", "the parts together are larger than a file can be");
        }
        dir
    };
    let _ = stream.send_some(b"HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nConnection: close\r\n\r\n<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let temp = format!("{folder}/{UPLOAD_PREFIX}{:08x}", rand::next_u32());
    let result = (|| -> Result<(u32, u64), FsError> {
        // The first part becomes the file; the others are added to it piece by piece.
        storage.fs().rename(&format!("{dir}/{}", parts[0]), &temp)?;
        let mut piece = alloc::vec![0u8; WRITE_PIECE];
        let mut last_sign = time::uptime_ms();
        for part in &parts[1..] {
            let from = format!("{dir}/{part}");
            let mut offset = 0u64;
            loop {
                let mut fs = storage.fs();
                let n = fs.read_at(&from, offset, &mut piece)?;
                if n > 0 {
                    fs.append(&temp, &piece[..n])?;
                }
                drop(fs);
                if n == 0 {
                    break;
                }
                offset += n as u64;
                if time::uptime_ms() - last_sign > 2_000 {
                    let _ = stream.send_some(b" "); // still at it
                    last_sign = time::uptime_ms();
                }
                thread::yield_now();
            }
        }
        let mut fs = storage.fs();
        place_upload(&mut fs, &temp, &path)?;
        let m = fs.metadata(&path)?;
        Ok((m.inode, m.size))
    })();
    let mut fs = storage.fs();
    remove_parts(&mut fs, &dir);
    let text = match result {
        Ok((inode, size)) => s3::completed_xml(names[0], &names[1..].join("/"), &etag(inode, size)),
        Err(e) => {
            let _ = fs.remove(&temp);
            // The answer has begun: the error goes where the result would (S3 does the same).
            format!("<Error><Code>InternalError</Code><Message>{e}</Message></Error>")
        }
    };
    let mut a = Answer::new(200, "OK", "application/xml", text.into_bytes());
    a.headless = true;
    a
}

/// How much larger than the file a chunked upload may be on the wire: what
/// the client says the wrapping takes, within reason.
fn length_room(head: &Head) -> u64 {
    let decoded = head.header("x-amz-decoded-content-length").and_then(|v| v.parse::<u64>().ok());
    match (head.content_length(), decoded) {
        (Ok(Some(length)), Some(decoded)) if length >= decoded => (length - decoded).min(decoded / 8 + CHUNK_ROOM),
        _ => CHUNK_ROOM,
    }
}

/// The most objects a listing looks at.
const MAX_WALK: usize = 10_000;

/// An object's tag: which file it is and how large. (BeeFS keeps no times
/// and the content is not hashed; the "-1" tells clients it is no MD5.)
fn etag(inode: u32, size: u64) -> String {
    format!("bee{inode:08x}{size:012x}-1")
}

/// Every file below `dir` as an object whose key starts with `key_start`, sorted by key.
fn walk(fs: &mut storage::FsGuard, dir: &str, key_start: &str) -> Result<Vec<s3::Object>, ()> {
    let mut objects = Vec::new();
    let mut todo = alloc::vec![(String::from(dir), String::from(key_start))];
    while let Some((dir, start)) = todo.pop() {
        let Ok(entries) = fs.read_dir(&dir) else { continue };
        if entries.is_empty() && start != key_start {
            // An empty folder shows as a key that ends in "/".
            objects.push(s3::Object { key: start.clone(), size: 0, etag: etag(0, 0) });
        }
        for e in entries {
            if objects.len() + todo.len() >= MAX_WALK {
                return Err(());
            }
            match e.kind {
                FileKind::Directory => todo.push((format!("{dir}/{}", e.name), format!("{start}{}/", e.name))),
                FileKind::File => objects.push(s3::Object { key: format!("{start}{}", e.name), size: e.size, etag: etag(e.inode, e.size) }),
            }
        }
    }
    objects.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(objects)
}

/// Remove the folders above `path` that became empty, up to the bucket.
fn prune(fs: &mut storage::FsGuard, bucket_dir: &str, path: &str) {
    let mut at = path;
    while let Some(cut) = at.rfind('/') {
        at = &at[..cut];
        if at.len() <= bucket_dir.len() || fs.remove(at).is_err() {
            return;
        }
    }
}

/// The S3 interface: /BUCKET/KEY below the shared folder.
fn s3_act(folder: &str, head: &Head, body: Body) -> Answer {
    let resource = head.path.as_str();
    let fail = |status: u16, reason: &'static str, code: &str, message: &str| s3_error(status, reason, code, message, resource);
    let not_done = |what: &str| fail(501, "Not Implemented", "NotImplemented", &format!("{what}: this file server does not do that"));
    let fs_failed = |e: FsError| {
        let text = format!("{e}");
        match e {
            FsError::NoSpace | FsError::NoInodes | FsError::FileTooLarge => fail(507, "Insufficient Storage", "EntityTooLarge", &text),
            FsError::NameTooLong | FsError::InvalidPath => fail(400, "Bad Request", "KeyTooLongError", &text),
            FsError::IsADirectory | FsError::NotADirectory | FsError::AlreadyExists => fail(409, "Conflict", "InvalidRequest", &text),
            _ => fail(500, "Internal Server Error", "InternalError", &text),
        }
    };
    let Some(names) = proto::safe_path(&head.path, MAX_NAME_LEN) else {
        return fail(400, "Bad Request", "KeyTooLongError", &format!("not a usable key (names between the / have {MAX_NAME_LEN} characters at most)"));
    };
    let has = |name: &str| head.query_value(name).is_some();
    let now = time::now_unix().unwrap_or(0);
    let Some(mut storage) = storage::handle() else { return fail(503, "Service Unavailable", "ServiceUnavailable", "no file system") };
    let mut fs = storage.fs();
    let method = head.method.as_str();

    // The service itself: its buckets are the folders of the shared folder.
    let Some(bucket) = names.first().copied() else {
        return match method {
            "GET" | "HEAD" => {
                let mut buckets: Vec<String> = fs
                    .read_dir(folder)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| e.kind == FileKind::Directory && s3::bucket_name_ok(&e.name, MAX_NAME_LEN))
                    .map(|e| e.name)
                    .collect();
                buckets.sort();
                s3_xml(s3::buckets_xml(&buckets, &s3::iso_time(now)))
            }
            _ => fail(405, "Method Not Allowed", "MethodNotAllowed", "only GET here"),
        };
    };
    if !s3::bucket_name_ok(bucket, MAX_NAME_LEN) {
        return fail(400, "Bad Request", "InvalidBucketName", "a bucket's name: letters, digits, . - _ (26 at most), and not 'api'");
    }
    let bucket_dir = format!("{folder}/{bucket}");
    let bucket_there = fs.is_dir(&bucket_dir);
    let no_bucket = || fail(404, "Not Found", "NoSuchBucket", "no such bucket");

    if names.len() == 1 {
        return match method {
            "PUT" if has("acl") || has("versioning") || has("policy") || has("lifecycle") || has("cors") || has("tagging") => not_done("bucket settings"),
            "PUT" => match fs.mkdir_all(&bucket_dir) {
                Ok(()) => {
                    let mut a = Answer::new(200, "OK", "application/xml", Vec::new());
                    a.more = format!("Location: /{bucket}\r\n");
                    a
                }
                Err(e) => fs_failed(e),
            },
            _ if !bucket_there => no_bucket(),
            "HEAD" => Answer::new(200, "OK", "application/xml", Vec::new()),
            "DELETE" => match fs.remove(&bucket_dir) {
                Ok(()) => Answer::new(204, "No Content", "application/xml", Vec::new()),
                Err(FsError::DirectoryNotEmpty) => fail(409, "Conflict", "BucketNotEmpty", "the bucket is not empty"),
                Err(e) => fs_failed(e),
            },
            "GET" if has("location") => s3_xml(s3::location_xml()),
            "GET" if has("versioning") => {
                s3_xml(String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"/>"))
            }
            "GET" if ["uploads", "versions", "acl", "policy", "lifecycle", "cors", "tagging", "object-lock", "encryption"].iter().any(|n| has(n)) => {
                not_done("this bucket question")
            }
            "GET" => {
                let query = s3::ListQuery::from(head);
                // Only the folder the prefix points into needs to be read.
                let (start_dir, key_start) = match query.prefix.rfind('/') {
                    Some(cut) => match proto::safe_path(&query.prefix[..cut], MAX_NAME_LEN) {
                        Some(dirs) if !dirs.is_empty() => (format!("{bucket_dir}/{}", dirs.join("/")), format!("{}/", dirs.join("/"))),
                        _ => return s3_xml(s3::list_xml(bucket, &query, &s3::Page::default(), &s3::iso_time(now))),
                    },
                    None => (bucket_dir.clone(), String::new()),
                };
                match walk(&mut fs, &start_dir, &key_start) {
                    Ok(all) => s3_xml(s3::list_xml(bucket, &query, &s3::select(&all, &query), &s3::iso_time(now))),
                    Err(()) => fail(500, "Internal Server Error", "InternalError", "too many objects to list: ask with a prefix"),
                }
            }
            "POST" if has("delete") => {
                let Body::Bytes(body) = &body else { return fail(400, "Bad Request", "MalformedXML", "not a list of keys") };
                let Some((keys, quiet)) = s3::delete_keys(body) else {
                    return fail(400, "Bad Request", "MalformedXML", "not a list of at most 1000 keys");
                };
                let (mut deleted, mut failed) = (Vec::new(), Vec::new());
                for key in keys {
                    let path = match proto::safe_path(&key, MAX_NAME_LEN) {
                        Some(names) if !names.is_empty() => format!("{bucket_dir}/{}", names.join("/")),
                        _ => {
                            failed.push((key, "not a usable key"));
                            continue;
                        }
                    };
                    match fs.remove(&path) {
                        Ok(()) => {
                            prune(&mut fs, &bucket_dir, &path);
                            deleted.push(key);
                        }
                        Err(FsError::NotFound | FsError::NotADirectory) => deleted.push(key),
                        Err(_) => failed.push((key, "could not be removed")),
                    }
                }
                s3_xml(s3::deleted_xml(&deleted, &failed, quiet))
            }
            "POST" => not_done("this request"),
            _ => fail(405, "Method Not Allowed", "MethodNotAllowed", "GET, HEAD, PUT, DELETE or POST ?delete"),
        };
    }

    // An object: a file below the bucket's folder. A key ending in "/" is a folder.
    let path = format!("{bucket_dir}/{}", names[1..].join("/"));
    let is_folder_key = head.path.ends_with('/');
    let no_key = || fail(404, "Not Found", "NoSuchKey", "no such key");
    match method {
        // An upload in parts: started, part by part, given up. (Putting the
        // parts together takes long and is done in `complete_parts`.)
        "POST" if has("uploads") => {
            if !bucket_there {
                return no_bucket();
            }
            let id = format!("{:08x}{:08x}", rand::next_u32(), rand::next_u32());
            let dir = format!("{folder}/{PARTS_PREFIX}{id}");
            let key = names.join("/");
            match fs.mkdir_all(&dir).and_then(|()| fs.write_file(&format!("{dir}/key"), key.as_bytes())) {
                Ok(()) => s3_xml(s3::initiated_xml(bucket, &names[1..].join("/"), &id)),
                Err(e) => {
                    remove_parts(&mut fs, &dir);
                    fs_failed(e)
                }
            }
        }
        "PUT" if has("uploadId") || has("partNumber") => {
            let id = head.query_value("uploadId").unwrap_or_default();
            let number = head.query_value("partNumber").and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=s3::MAX_PARTS).contains(n));
            let Some(dir) = parts_dir(&mut fs, folder, &id, &names.join("/")) else {
                return fail(404, "Not Found", "NoSuchUpload", "no such upload in parts (finished, given up, or never started)");
            };
            let (Some(number), Body::File(temp, _)) = (number, &body) else {
                return fail(400, "Bad Request", "InvalidArgument", "a part needs a number from 1 to 10000 and its data");
            };
            let part = format!("{dir}/{number}");
            match place_upload(&mut fs, temp, &part).and_then(|_| fs.metadata(&part)) {
                Ok(m) => {
                    let mut a = Answer::new(200, "OK", "application/xml", Vec::new());
                    a.more = format!("ETag: \"{}\"\r\n", etag(m.inode, m.size));
                    a
                }
                Err(e) => fs_failed(e),
            }
        }
        "DELETE" if has("uploadId") => {
            let id = head.query_value("uploadId").unwrap_or_default();
            match parts_dir(&mut fs, folder, &id, &names.join("/")) {
                Some(dir) => {
                    remove_parts(&mut fs, &dir);
                    Answer::new(204, "No Content", "application/xml", Vec::new())
                }
                None => fail(404, "Not Found", "NoSuchUpload", "no such upload in parts (finished, given up, or never started)"),
            }
        }
        "POST" if has("uploadId") => not_done("this request"),
        "PUT" if head.header("x-amz-copy-source").is_some() => not_done("copies"),
        "PUT" if has("acl") || has("tagging") || has("retention") || has("legal-hold") => not_done("object settings"),
        "PUT" if !bucket_there => no_bucket(),
        "PUT" if is_folder_key => match fs.mkdir_all(&path) {
            Ok(()) => {
                let mut a = Answer::new(200, "OK", "application/xml", Vec::new());
                a.more = format!("ETag: \"{}\"\r\n", etag(0, 0));
                a
            }
            Err(e) => fs_failed(e),
        },
        "PUT" => {
            let Body::File(temp, _) = &body else { return fail(400, "Bad Request", "InvalidRequest", "no object came with the request") };
            match place_upload(&mut fs, temp, &path).and_then(|_| fs.metadata(&path)) {
                Ok(m) => {
                    let mut a = Answer::new(200, "OK", "application/xml", Vec::new());
                    a.more = format!("ETag: \"{}\"\r\n", etag(m.inode, m.size));
                    a
                }
                Err(e) => {
                    prune(&mut fs, &bucket_dir, &path);
                    fs_failed(e)
                }
            }
        }
        "GET" | "HEAD" if has("acl") || has("tagging") || has("uploadId") || has("retention") || has("legal-hold") || has("attributes") => not_done("this object question"),
        "GET" | "HEAD" => match fs.metadata(&path) {
            _ if !bucket_there => no_bucket(),
            Ok(m) if (m.kind == FileKind::Directory) != is_folder_key => no_key(),
            Ok(m) => {
                let folder_key = m.kind == FileKind::Directory;
                let (size, tag) = if folder_key { (0, etag(0, 0)) } else { (m.size, etag(m.inode, m.size)) };
                let mut a = Answer::new(200, "OK", content_type(&path), Vec::new());
                a.length = Some(size);
                a.file = (!folder_key).then(|| path.clone());
                a.more = format!("ETag: \"{tag}\"\r\nLast-Modified: {}\r\n", s3::http_time(now));
                match ranged(a, head) {
                    Some(a) => a,
                    None => {
                        let mut a = fail(416, "Range Not Satisfiable", "InvalidRange", "the object has no such bytes");
                        a.more = format!("Content-Range: bytes */{size}\r\n");
                        a
                    }
                }
            }
            Err(FsError::NotFound | FsError::NotADirectory) => no_key(),
            Err(e) => fs_failed(e),
        },
        "DELETE" => {
            // Gone is gone: a key that is not there is no error in S3.
            match fs.remove(&path) {
                Ok(()) => prune(&mut fs, &bucket_dir, &path),
                Err(FsError::NotFound | FsError::NotADirectory | FsError::DirectoryNotEmpty) => {}
                Err(e) => return fs_failed(e),
            }
            Answer::new(204, "No Content", "application/xml", Vec::new())
        }
        "POST" => not_done("this request"),
        _ => fail(405, "Method Not Allowed", "MethodNotAllowed", "GET, HEAD, PUT or DELETE"),
    }
}

/// Why a request's body could not be taken.
#[derive(Clone, Copy)]
enum BodyError {
    BadLength,
    NoLength,
    /// Larger than this many bytes.
    TooLarge(u64),
    EndedEarly,
    /// Not what the client signed.
    NotAsSigned,
    Disk(FsError),
}

impl BodyError {
    /// Status, reason, S3's name for it, and a sentence.
    fn parts(self) -> (u16, &'static str, &'static str, String) {
        match self {
            BodyError::BadLength => (400, "Bad Request", "InvalidArgument", String::from("Content-Length is not a number")),
            BodyError::NoLength => (411, "Length Required", "MissingContentLength", String::from("PUT needs a Content-Length")),
            BodyError::TooLarge(most) => (413, "Content Too Large", "EntityTooLarge", format!("too large: {} KiB at most here", most >> 10)),
            BodyError::EndedEarly => (400, "Bad Request", "IncompleteBody", String::from("the body ended early")),
            BodyError::NotAsSigned => (400, "Bad Request", s3::AuthError::BadBody.code(), String::from(s3::AuthError::BadBody.message())),
            BodyError::Disk(FsError::NoSpace | FsError::NoInodes | FsError::FileTooLarge) => {
                (507, "Insufficient Storage", "EntityTooLarge", String::from("the disk is full"))
            }
            BodyError::Disk(e) => (500, "Internal Server Error", "InternalError", format!("{e}")),
        }
    }
}

/// Wait for the network during a transfer. While data is moving (the last
/// piece came or went less than 50 ms ago) the other threads get a turn
/// and the card is looked at again at once: sleeping until the next timer
/// tick would cost up to 10 ms for every buffer of 16 KiB. Otherwise sleep.
fn wait_for_network(last_progress_ms: u64) {
    if time::uptime_ms() - last_progress_ms < 50 {
        thread::yield_now();
    } else {
        thread::pause(|| false);
    }
}

/// A small body, held in memory: the list of keys of a "delete these" request.
fn body(stream: &net::TcpStream, head: &Head, early: Vec<u8>) -> Result<Vec<u8>, BodyError> {
    let length = match head.content_length() {
        Err(()) => return Err(BodyError::BadLength),
        Ok(Some(n)) => n,
        Ok(None) => 0,
    };
    if length > MAX_BODY {
        return Err(BodyError::TooLarge(MAX_BODY));
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
                wait_for_network(last_progress);
            }
            _ => return Err(BodyError::EndedEarly),
        }
    }
    Ok(data)
}

/// How large a file in the shared folder can become: what its disk has
/// free, and no more than a file holds.
fn room(folder: &str) -> u64 {
    let Some(mut storage) = storage::handle() else { return 0 };
    let fs = storage.fs();
    let on_data = storage::route(folder, fs.has_data()).0;
    let stats = if on_data { fs.data_stats().unwrap_or(fs.stats()) } else { fs.stats() };
    (stats.free_blocks as u64 * stats.block_size as u64).min(beefs::MAX_FILE_SIZE)
}

/// Remove what interrupted uploads left behind (when the server starts).
fn clear_uploads(folder: &str) {
    let Some(mut storage) = storage::handle() else { return };
    let mut fs = storage.fs();
    for entry in fs.read_dir(folder).unwrap_or_default() {
        if entry.kind == FileKind::File && entry.name.starts_with(UPLOAD_PREFIX) {
            let _ = fs.remove(&format!("{folder}/{}", entry.name));
        }
        if entry.kind == FileKind::Directory && entry.name.starts_with(PARTS_PREFIX) {
            remove_parts(&mut fs, &format!("{folder}/{}", entry.name));
        }
    }
}

/// Take a file as it arrives: through `reader` (which checks it against
/// what the client signed) to a temporary file in the shared folder, piece
/// by piece, so that it never has to fit in memory. Only a complete and
/// checked upload is returned; anything else is removed again. `wrapping`:
/// how much larger than the file the body may be (a chunked upload).
fn receive_upload(stream: &net::TcpStream, head: &Head, early: Vec<u8>, mut reader: s3::BodyReader, folder: &str, wrapping: u64) -> Result<Body, BodyError> {
    let length = match head.content_length() {
        Err(()) => return Err(BodyError::BadLength),
        Ok(None) => return Err(BodyError::NoLength),
        Ok(Some(n)) => n,
    };
    let most = room(folder);
    if length > most + wrapping {
        return Err(BodyError::TooLarge(most));
    }
    let temp = format!("{folder}/{UPLOAD_PREFIX}{:08x}", rand::next_u32());
    let on_disk = |act: &mut dyn FnMut(&mut storage::FsGuard) -> Result<(), FsError>| -> Result<(), BodyError> {
        let mut storage = storage::handle().ok_or(BodyError::Disk(FsError::Io))?;
        let mut fs = storage.fs();
        let done = act(&mut fs);
        drop(fs);
        done.map_err(BodyError::Disk)
    };
    on_disk(&mut |fs| fs.write_file(&temp, &[]))?;
    let result = (|| {
        let mut left = length;
        let mut written = 0u64;
        let mut piece = Vec::with_capacity(2 * PIECE);
        let mut take = |received: &[u8], last: bool, reader: &mut s3::BodyReader| -> Result<(), BodyError> {
            reader.feed(received, &mut piece).map_err(|_| BodyError::NotAsSigned)?;
            if piece.len() >= PIECE || (last && !piece.is_empty()) {
                written += piece.len() as u64;
                if written > most {
                    return Err(BodyError::TooLarge(most));
                }
                on_disk(&mut |fs| fs.append(&temp, &piece))?;
                piece.clear();
            }
            Ok(())
        };
        let first = (early.len() as u64).min(left) as usize;
        left -= first as u64;
        take(&early[..first], left == 0, &mut reader)?;
        if left > 0 && head.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
            let _ = stream.send_some(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        let mut buf = alloc::vec![0u8; PIECE];
        let mut last_progress = time::uptime_ms();
        while left > 0 {
            let want = (left as usize).min(buf.len());
            match stream.receive(&mut buf[..want]) {
                net::Received::Data(n) => {
                    left -= n as u64;
                    take(&buf[..n], left == 0, &mut reader)?;
                    last_progress = time::uptime_ms();
                    // A long upload must not keep the processor from the other threads.
                    thread::yield_now();
                }
                net::Received::Nothing if time::uptime_ms() - last_progress < STALL_MS => {
                    wait_for_network(last_progress);
                }
                _ => return Err(BodyError::EndedEarly),
            }
        }
        reader.finish().map_err(|_| BodyError::NotAsSigned)
    })();
    match result {
        Ok(size) => Ok(Body::File(temp, size)),
        Err(e) => {
            let _ = on_disk(&mut |fs| fs.remove(&temp));
            Err(e)
        }
    }
}

/// Give a complete upload its name: `path` (a file that is there is
/// replaced; the folders above it are made). True: there was such a file.
fn place_upload(fs: &mut storage::FsGuard, temp: &str, path: &str) -> Result<bool, FsError> {
    let existed = match fs.metadata(path) {
        Ok(m) if m.kind == FileKind::Directory => return Err(FsError::IsADirectory),
        Ok(_) => true,
        Err(_) => false,
    };
    fs.mkdir_all(&path[..path.rfind('/').unwrap_or(0)])?;
    if existed {
        fs.remove(path)?;
    }
    fs.rename(temp, path)?;
    Ok(existed)
}

fn send(stream: &net::TcpStream, answer: &Answer, head_only: bool) {
    let length = answer.length.unwrap_or(answer.body.len() as u64);
    let mut out = if answer.headless { Vec::new() } else { proto::response_head(answer.status, answer.reason, answer.kind, length, &answer.more).into_bytes() };
    if !head_only {
        out.extend_from_slice(&answer.body);
    }
    let mut rest = out.as_slice();
    let mut last_progress = time::uptime_ms();
    while !rest.is_empty() {
        match stream.send_some(rest) {
            Ok(0) if time::uptime_ms() - last_progress > STALL_MS => return,
            Ok(0) => {
                wait_for_network(last_progress);
            }
            Ok(n) => {
                rest = &rest[n..];
                last_progress = time::uptime_ms();
            }
            Err(_) => return,
        }
    }
    if let (Some(path), false) = (&answer.file, head_only) {
        // The file, from the disk piece by piece: it need not fit in memory.
        let mut buf = alloc::vec![0u8; PIECE];
        let (mut offset, end) = (answer.offset, answer.offset + length);
        while offset < end {
            let want = ((end - offset) as usize).min(buf.len());
            let read = storage::handle().map(|mut storage| storage.fs().read_at(path, offset, &mut buf[..want]));
            let Some(Ok(n)) = read else { return };
            if n == 0 {
                return; // shorter than announced (changed meanwhile): the client sees it cut off
            }
            offset += n as u64;
            let mut rest = &buf[..n];
            let mut last_progress = time::uptime_ms();
            while !rest.is_empty() {
                match stream.send_some(rest) {
                    Ok(0) if time::uptime_ms() - last_progress > STALL_MS => return,
                    Ok(0) => {
                        wait_for_network(last_progress);
                    }
                    Ok(n) => {
                        rest = &rest[n..];
                        last_progress = time::uptime_ms();
                    }
                    Err(_) => return,
                }
            }
            // A long download must not keep the processor from the other threads.
            thread::yield_now();
        }
    }
    stream.finish();
}

/// The REST interface: /api/files/PATH below the shared folder.
fn rest(folder: &str, head: &Head, body: Body) -> Answer {
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
                        .filter(|e| !(names.is_empty() && (e.name.starts_with(UPLOAD_PREFIX) || e.name.starts_with(PARTS_PREFIX)))) // uploads still arriving
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
            Ok(m) => {
                let mut a = Answer::new(200, "OK", content_type(&shown), Vec::new());
                a.length = Some(m.size);
                a.file = Some(path.clone());
                match ranged(a, head) {
                    Some(a) => a,
                    None => {
                        let mut a = Answer::error(416, "Range Not Satisfiable", "the file has no such bytes");
                        a.more = format!("Content-Range: bytes */{}\r\n", m.size);
                        a
                    }
                }
            }
            Err(FsError::NotFound | FsError::NotADirectory) => not_found(),
            Err(e) => failed(e),
        },
        "PUT" if names.is_empty() => Answer::error(400, "Bad Request", "name a file or a folder"),
        "PUT" if relative.ends_with('/') => match fs.mkdir_all(&path) {
            Ok(()) => Answer::json(201, "Created", format!("{{\"path\":\"{}\",\"type\":\"folder\"}}\n", proto::json_escape(&shown))),
            Err(e) => failed(e),
        },
        "PUT" => {
            let Body::File(temp, size) = &body else { return Answer::error(400, "Bad Request", "no file came with the request") };
            match place_upload(&mut fs, temp, &path) {
                Ok(existed) => {
                    let text = format!("{{\"path\":\"{}\",\"type\":\"file\",\"size\":{size}}}\n", proto::json_escape(&shown));
                    if existed {
                        Answer::json(200, "OK", text)
                    } else {
                        Answer::json(201, "Created", text)
                    }
                }
                Err(e) => failed(e),
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
