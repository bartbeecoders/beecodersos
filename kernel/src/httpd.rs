//! The status page: a small web server for the local network (phase N6).
//!
//! OFF unless switched on with `httpd on` (saved in /system/httpd). When on,
//! anyone who can reach the machine can read, without a password:
//!   /             status page: version, uptime, A/B slots, threads, recent events
//!   /status.json  the same facts for programs
//!   /files/NAME   the files in /public (only that directory, never below or above it)
//! Nothing can be changed through it: GET and HEAD only.
//!
//! It runs in a thread of its own and answers one request at a time. Clients
//! are untrusted: the request is bounded in size and in time, and everything
//! put into a page is escaped. Client addresses are never logged.

use crate::ring::Global;
use crate::{allocator, bootslots, eventlog, json, net, storage, thread, time};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beenet::http::{escape_html, percent_encode, response_head, RequestReader};

pub const DEFAULT_PORT: u16 = 80;
/// The only directory whose files are served.
pub const PUBLIC_DIR: &str = "/public";
const SETTING_FILE: &str = "/system/httpd";
/// A client gets this long to send its request...
const REQUEST_TIMEOUT_MS: u64 = 3_000;
/// ...and may stall this long while the answer is sent.
const SEND_STALL_MS: u64 = 10_000;
const RECENT_EVENTS: usize = 15;

struct State {
    /// Should be running, and on which port.
    wanted: Option<u16>,
    thread: Option<thread::ThreadId>,
    answered: u64,
}

static STATE: Global<State> = Global::new(State { wanted: None, thread: None, answered: 0 });

/// The port the server listens on, if it is on.
pub fn port() -> Option<u16> {
    STATE.get().wanted
}

pub fn answered() -> u64 {
    STATE.get().answered
}

/// Switch the server on (None: off). The choice is saved when `save` is set.
pub fn set(port: Option<u16>, save: bool) -> Result<(), String> {
    // Let a running server stop first: it frees the port.
    STATE.get().wanted = None;
    if let Some(id) = STATE.get().thread.take() {
        thread::join(id);
    }
    if let Some(port) = port {
        if !net::present() {
            return Err(String::from("no network card"));
        }
        net::tcp_listen(port).map_err(String::from)?;
        STATE.get().wanted = Some(port);
        let id = thread::spawn("httpd", None, move || serve(port));
        if id.is_none() {
            net::tcp_unlisten(port);
            STATE.get().wanted = None;
            return Err(String::from("too many threads"));
        }
        STATE.get().thread = id;
    }
    if save {
        if let Some(mut storage) = storage::handle() {
            let text = match port {
                Some(p) => format!("on {p}\n"),
                None => String::from("off\n"),
            };
            let mut fs = storage.fs();
            let _ = fs.mkdir_all("/system");
            fs.write_file(SETTING_FILE, text.as_bytes()).map_err(|e| format!("can't save the setting: {e}"))?;
        }
    }
    Ok(())
}

/// At boot: start the server if it was left switched on. Returns its port.
pub fn load() -> Option<u16> {
    let data = storage::handle()?.fs().read_file(SETTING_FILE).ok()?;
    let port = parse_setting(&String::from_utf8_lossy(&data))?;
    set(Some(port), false).ok()?;
    Some(port)
}

/// "on PORT" (or "on"): the port. Anything else means off.
pub fn parse_setting(text: &str) -> Option<u16> {
    let mut words = text.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("on"), None, _) => Some(DEFAULT_PORT),
        (Some("on"), Some(port), None) => port.parse().ok().filter(|p| *p != 0),
        _ => None,
    }
}

fn serve(port: u16) {
    while STATE.get().wanted == Some(port) {
        match net::tcp_accept(port) {
            Some(stream) => {
                answer(&stream);
                STATE.get().answered += 1;
            }
            None => {
                thread::pause(|| false); // until the next timer tick
            }
        }
    }
    net::tcp_unlisten(port);
}

/// Read one request and answer it. Whatever goes wrong, the connection is
/// simply dropped: there is nobody to report it to.
fn answer(stream: &net::TcpStream) {
    let deadline = time::uptime_ms() + REQUEST_TIMEOUT_MS;
    let mut reader = RequestReader::new();
    let mut buf = [0u8; 1024];
    let request = loop {
        match stream.receive(&mut buf) {
            net::Received::Data(n) => match reader.feed(&buf[..n]) {
                Ok(Some(request)) => break Ok(request),
                Ok(None) => {}
                Err(_) => break Err(()),
            },
            net::Received::Nothing if time::uptime_ms() < deadline => {
                thread::pause(|| false);
            }
            _ => return, // closed, failed or too slow
        }
    };
    let (status, reason, kind, body) = match &request {
        Err(()) => text(400, "Bad Request", "bad request\n"),
        Ok(r) if r.method != "GET" && r.method != "HEAD" => text(405, "Method Not Allowed", "only GET is supported\n"),
        Ok(r) => page(&r.path),
    };
    let mut out = response_head(status, reason, kind, body.len()).into_bytes();
    if !matches!(&request, Ok(r) if r.method == "HEAD") {
        out.extend_from_slice(&body);
    }
    let mut rest = out.as_slice();
    let mut last_progress = time::uptime_ms();
    while !rest.is_empty() {
        match stream.send_some(rest) {
            Ok(0) if time::uptime_ms() - last_progress > SEND_STALL_MS => return,
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

type Page = (u16, &'static str, &'static str, Vec<u8>);

fn text(status: u16, reason: &'static str, body: &str) -> Page {
    (status, reason, "text/plain; charset=utf-8", Vec::from(body.as_bytes()))
}

fn page(path: &str) -> Page {
    match path {
        "/" => (200, "OK", "text/html; charset=utf-8", status_html().into_bytes()),
        "/status.json" => (200, "OK", "application/json", status_json().into_bytes()),
        _ => match path.strip_prefix("/files/") {
            Some(name) => file(name),
            None => text(404, "Not Found", "no such page\n"),
        },
    }
}

/// A file from /public. The name must be a plain file name: no directories.
fn file(name: &str) -> Page {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return text(404, "Not Found", "no such file\n");
    }
    let Some(mut storage) = storage::handle() else { return text(404, "Not Found", "no such file\n") };
    let path = format!("{PUBLIC_DIR}/{name}");
    let mut fs = storage.fs();
    if fs.is_dir(&path) {
        return text(404, "Not Found", "no such file\n");
    }
    match fs.read_file(&path) {
        Ok(data) => (200, "OK", content_type(name), data),
        Err(_) => text(404, "Not Found", "no such file\n"),
    }
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("txt" | "md" | "log" | "jsonl" | "scn" | "rs" | "py" | "sh" | "conf") => "text/plain; charset=utf-8",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("css") => "text/css",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn status_json() -> String {
    let slot = bootslots::booted_slot().map(|s| String::from(s as char)).unwrap_or_default();
    json::Obj::new()
        .str("os", "BeeCodersOs")
        .str("version", crate::OS_VERSION)
        .str("build", crate::OS_BUILD)
        .num("uptime_ms", time::uptime_ms() as i64)
        .str("time", &time::timestamp())
        .str("slot", &slot)
        .bool("on_trial", bootslots::on_trial())
        .num("threads", thread::list().len() as i64)
        .num("heap_used_kib", (allocator::used() / 1024) as i64)
        .finish()
}

/// The newest events, as (time, what): names and outcomes only, as recorded.
fn recent_events() -> Vec<(String, String)> {
    let Some(mut storage) = storage::handle() else { return Vec::new() };
    let Ok(data) = storage.fs().read_file(eventlog::LOG) else { return Vec::new() };
    let log = String::from_utf8_lossy(&data);
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(RECENT_EVENTS)..]
        .iter()
        .rev()
        .filter_map(|line| {
            let event = json::parse(line)?;
            let what = match (event.str("ev")?, event.str("cmd"), event.str("status")) {
                ("cmd", Some(cmd), Some(status)) => format!("command {cmd}: {status}"),
                (name, _, _) => String::from(name),
            };
            Some((String::from(event.str("t").unwrap_or("")), what))
        })
        .collect()
}

fn public_files() -> Vec<(String, u64)> {
    let Some(mut storage) = storage::handle() else { return Vec::new() };
    let entries = storage.fs().read_dir(PUBLIC_DIR).unwrap_or_default();
    entries.into_iter().filter(|e| e.kind == beefs::FileKind::File).map(|e| (e.name, e.size)).collect()
}

fn status_html() -> String {
    let row = |name: &str, value: &str| format!("<tr><th>{}</th><td>{}</td></tr>\n", escape_html(name), escape_html(value));
    let mut html = String::from(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>BeeCodersOs status</title>\n<style>\n\
         body{font:16px/1.5 system-ui,sans-serif;max-width:46rem;margin:2rem auto;padding:0 1rem;color:#222;background:#fdfcf7}\n\
         h1{font-size:1.5rem}h2{font-size:1.1rem;margin-top:2rem}\n\
         table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:.25rem .75rem .25rem 0;vertical-align:top}\n\
         th{font-weight:600;white-space:nowrap;width:1%}td,pre{font-family:ui-monospace,monospace;font-size:.9rem}\n\
         tr{border-bottom:1px solid #e6e2d3}p.note{color:#666;font-size:.9rem}\n\
         @media(prefers-color-scheme:dark){body{color:#e8e6df;background:#1b1a17}tr{border-color:#3a382f}p.note{color:#aaa}a{color:#f0c040}}\n\
         </style></head><body>\n<h1>BeeCodersOs</h1>\n<table>\n",
    );
    html += &row("version", &format!("{} b{}", crate::OS_VERSION, crate::OS_BUILD));
    html += &row("time", &time::timestamp());
    html += &row("uptime", &time::format_duration(time::uptime_ms()));
    html += &row("memory", &format!("{} of {} KiB of the kernel heap in use", allocator::used() / 1024, allocator::heap_size() / 1024));
    if let Some(mut storage) = storage::handle() {
        let s = storage.fs().stats();
        let kib = |blocks: u32| blocks as u64 * s.block_size as u64 / 1024;
        let kind = if storage.persistent { "disk" } else { "RAM disk (lost at reboot)" };
        html += &row("storage", &format!("{kind}: {} of {} KiB used", kib(s.total_blocks - s.free_blocks), kib(s.total_blocks)));
    }
    html += &row("status page", &format!("{} requests answered since it was switched on", answered()));
    html += "</table>\n<h2>Kernel slots</h2>\n";
    html += &match bootslots::describe() {
        Some(slots) => format!("<pre>{}</pre>\n", escape_html(&slots)),
        None => String::from("<p class=\"note\">Not booted from a boot disk with A/B slots.</p>\n"),
    };
    html += "<h2>Threads</h2>\n<table>\n";
    for t in thread::list() {
        html += &row(&t.name, &format!("{}, ran {} times", t.state, t.runs));
    }
    html += "</table>\n<h2>Recent events</h2>\n";
    let events = recent_events();
    if events.is_empty() {
        html += "<p class=\"note\">Nothing recorded yet.</p>\n";
    } else {
        html += "<table>\n";
        for (when, what) in events {
            html += &row(&when, &what);
        }
        html += "</table>\n";
    }
    html += "<p class=\"note\">Commands are recorded by name only, never with what was typed after them.</p>\n<h2>Files</h2>\n";
    let files = public_files();
    if files.is_empty() {
        html += &format!("<p class=\"note\">No files are shared. Files put in {PUBLIC_DIR} appear here.</p>\n");
    } else {
        html += "<table>\n";
        for (name, size) in files {
            html += &format!(
                "<tr><th><a href=\"/files/{}\">{}</a></th><td>{size} bytes</td></tr>\n",
                percent_encode(&name),
                escape_html(&name)
            );
        }
        html += "</table>\n";
    }
    html += "</body></html>\n";
    html
}
