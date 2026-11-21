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
//! prefix, delimiter and pages). Not there: multipart uploads, copies,
//! versions, access lists, presigned links.
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
use beenet::s3;
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
    let (reply, authorized) = if head.path == "/api" || head.path.starts_with("/api/") {
        if !proto::bearer(&head).is_some_and(|key| proto::same_secret(key, &setting.key)) {
            let mut a = Answer::error(401, "Unauthorized", "send the key: Authorization: Bearer KEY");
            a.more = String::from("WWW-Authenticate: Bearer\r\n");
            (a, false)
        } else {
            match body(stream, &head, early, 0) {
                Ok(body) => (rest(&setting.folder, &head, body), true),
                Err(e) => {
                    let (status, reason, _, message) = e.parts();
                    (Answer::error(status, reason, &message), true)
                }
            }
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

/// A signed S3 request: take its body as the client meant it, then act.
fn s3_request(stream: &net::TcpStream, folder: &str, head: &Head, early: Vec<u8>, auth: &s3::Auth) -> Answer {
    let chunked = matches!(auth.payload, s3::Payload::Chunked { .. });
    let received = match body(stream, head, early, if chunked { CHUNK_ROOM } else { 0 }) {
        Ok(data) => data,
        Err(e) => {
            let (status, reason, code, message) = e.parts();
            return s3_error(status, reason, code, &message, &head.path);
        }
    };
    let decoded = head.header("x-amz-decoded-content-length").and_then(|v| v.parse::<u64>().ok());
    match s3::body(auth, received, decoded) {
        Ok(data) if data.len() as u64 > MAX_BODY => {
            let (status, reason, code, message) = BodyError::TooLarge.parts();
            s3_error(status, reason, code, &message, &head.path)
        }
        Ok(data) => s3_act(folder, head, data),
        Err(e) => s3_error(400, "Bad Request", e.code(), e.message(), &head.path),
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
fn s3_act(folder: &str, head: &Head, body: Vec<u8>) -> Answer {
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
                let Some((keys, quiet)) = s3::delete_keys(&body) else {
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
        "PUT" | "POST" if has("uploads") || has("uploadId") || has("partNumber") => not_done("multipart uploads (send the file in one piece: 8 MiB at most)"),
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
            let existed = fs.exists(&path);
            let parent = &path[..path.rfind('/').unwrap_or(0)];
            let written = fs.mkdir_all(parent).and_then(|()| fs.write_file(&path, &body));
            match written.and_then(|()| fs.metadata(&path)) {
                Ok(m) => {
                    let mut a = Answer::new(200, "OK", "application/xml", Vec::new());
                    a.more = format!("ETag: \"{}\"\r\n", etag(m.inode, m.size));
                    a
                }
                Err(e) => {
                    // Nothing half-written stays behind under a new name.
                    if !existed {
                        let _ = fs.remove(&path);
                        prune(&mut fs, &bucket_dir, &path);
                    }
                    fs_failed(e)
                }
            }
        }
        "GET" | "HEAD" if has("acl") || has("tagging") || has("uploadId") || has("retention") || has("legal-hold") || has("attributes") => not_done("this object question"),
        "GET" | "HEAD" => match fs.metadata(&path) {
            _ if !bucket_there => no_bucket(),
            Ok(m) if (m.kind == FileKind::Directory) != is_folder_key => no_key(),
            Ok(m) if m.size > MAX_BODY => fail(413, "Content Too Large", "EntityTooLarge", "the file is too large to send"),
            Ok(m) => {
                let folder_key = m.kind == FileKind::Directory;
                let (size, tag) = if folder_key { (0, etag(0, 0)) } else { (m.size, etag(m.inode, m.size)) };
                let data = if method == "HEAD" || folder_key {
                    Vec::new()
                } else {
                    match fs.read_file(&path) {
                        Ok(data) => data,
                        Err(e) => return fs_failed(e),
                    }
                };
                let mut a = Answer::new(200, "OK", content_type(&path), data);
                a.length = Some(size);
                a.more = format!("ETag: \"{tag}\"\r\nLast-Modified: {}\r\n", s3::http_time(now));
                a
            }
            Err(FsError::NotFound | FsError::NotADirectory) => no_key(),
            Err(e) => fs_failed(e),
        },
        "DELETE" if has("uploadId") => Answer::new(204, "No Content", "application/xml", Vec::new()),
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
    TooLarge,
    EndedEarly,
}

impl BodyError {
    /// Status, reason, S3's name for it, and a sentence.
    fn parts(self) -> (u16, &'static str, &'static str, String) {
        match self {
            BodyError::BadLength => (400, "Bad Request", "InvalidArgument", String::from("Content-Length is not a number")),
            BodyError::NoLength => (411, "Length Required", "MissingContentLength", String::from("PUT needs a Content-Length")),
            BodyError::TooLarge => (413, "Content Too Large", "EntityTooLarge", format!("a file may have {} KiB at most", MAX_BODY >> 10)),
            BodyError::EndedEarly => (400, "Bad Request", "IncompleteBody", String::from("the body ended early")),
        }
    }
}

/// The request's body, for the methods that have one. `room`: how much
/// larger than a file it may be (the wrapping of a chunked upload).
fn body(stream: &net::TcpStream, head: &Head, early: Vec<u8>, room: u64) -> Result<Vec<u8>, BodyError> {
    let length = match head.content_length() {
        Err(()) => return Err(BodyError::BadLength),
        Ok(Some(n)) => n,
        Ok(None) if head.method == "PUT" && !head.path.ends_with('/') => return Err(BodyError::NoLength),
        Ok(None) => 0,
    };
    if length > MAX_BODY + room {
        return Err(BodyError::TooLarge);
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
            _ => return Err(BodyError::EndedEarly),
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
