//! The file server's protocol side: what a client sent, taken apart, and the
//! answers put together. No sockets and no files here: `kernel/src/fileserver.rs`
//! does those. Everything received is untrusted: lengths are checked, the
//! head and the number of headers are bounded, and a path can never leave
//! the shared folder (`safe_path`).

use crate::http::percent_decode;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A request's line and headers together may not be larger than this.
pub const MAX_HEAD: usize = 8 * 1024;
/// More headers than this is a bad request.
pub const MAX_HEADERS: usize = 64;
/// The longest path (decoded) and the deepest folder that are accepted.
pub const MAX_PATH: usize = 512;
pub const MAX_DEPTH: usize = 16;

/// A request's head: its first line and its headers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Head {
    /// "GET", "PUT", ...
    pub method: String,
    /// The path as it was sent (still percent-encoded), without the query.
    pub raw_path: String,
    /// The path, decoded, starting with `/`.
    pub path: String,
    /// What came after `?` (still encoded), or nothing.
    pub query: String,
    /// Names in lower case, values without the spaces around them.
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The body's length; `Err` when the header is there but no number.
    pub fn content_length(&self) -> Result<Option<u64>, ()> {
        match self.header("content-length") {
            None => Ok(None),
            Some(v) if !v.is_empty() && v.len() <= 18 && v.bytes().all(|b| b.is_ascii_digit()) => Ok(v.parse().ok()),
            Some(_) => Err(()),
        }
    }

    /// The value of a query parameter (decoded); `Some("")` for a bare name.
    pub fn query_value(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then(|| percent_decode(&value.replace('+', " ")).unwrap_or_default())
        })
    }
}

/// Collects a request's head as its bytes arrive.
#[derive(Default)]
pub struct HeadReader {
    buf: Vec<u8>,
}

impl HeadReader {
    pub fn new() -> HeadReader {
        HeadReader::default()
    }

    /// Take the next received bytes. Once the head is complete: the head, and
    /// the bytes that came after it (the start of the body).
    pub fn feed(&mut self, data: &[u8]) -> Result<Option<(Head, Vec<u8>)>, ()> {
        let from = self.buf.len().saturating_sub(3);
        self.buf.extend_from_slice(data);
        let end = (from..self.buf.len()).find_map(|i| {
            if self.buf[i..].starts_with(b"\r\n\r\n") {
                Some((i, 4))
            } else if self.buf[i..].starts_with(b"\n\n") {
                Some((i, 2))
            } else {
                None
            }
        });
        match end {
            Some((i, _)) if i > MAX_HEAD => Err(()),
            Some((i, marker)) => {
                let head = parse_head(&self.buf[..i])?;
                Ok(Some((head, self.buf.split_off(i + marker))))
            }
            None if self.buf.len() > MAX_HEAD => Err(()),
            None => Ok(None),
        }
    }
}

/// Parse a request's head (without the empty line that ends it).
pub fn parse_head(head: &[u8]) -> Result<Head, ()> {
    let text = core::str::from_utf8(head).map_err(|_| ())?;
    let mut lines = text.split('\n').map(|l| l.trim_end_matches('\r'));
    let mut parts = lines.next().unwrap_or("").split(' ');
    let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let method_ok = !method.is_empty() && method.len() <= 10 && method.bytes().all(|b| b.is_ascii_uppercase());
    if !method_ok || parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return Err(());
    }
    let target = target.split('#').next().unwrap_or("");
    let (raw_path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = percent_decode(raw_path).ok_or(())?;
    if !path.starts_with('/') || path.len() > MAX_PATH || path.chars().any(|c| c.is_control()) {
        return Err(());
    }
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(())?;
        if name.is_empty() || name.bytes().any(|b| b <= b' ' || b >= 0x7F) || headers.len() >= MAX_HEADERS {
            return Err(());
        }
        headers.push((name.to_ascii_lowercase(), String::from(value.trim())));
    }
    Ok(Head { method: String::from(method), raw_path: String::from(raw_path), path, query: String::from(query), headers })
}

/// A path from a request as the names below the shared folder: no `..`, no
/// `.`, no empty or over-long names, no `\`, and not too deep. `None` when it
/// is not safe; an empty list is the shared folder itself. A `/` at the end
/// is allowed (it says "folder"; see `names_folder`).
pub fn safe_path(path: &str, max_name: usize) -> Option<Vec<&str>> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);
    if path.is_empty() {
        return Some(Vec::new());
    }
    let names: Vec<&str> = path.split('/').collect();
    let good = |n: &&str| !n.is_empty() && *n != "." && *n != ".." && n.len() <= max_name && !n.chars().any(|c| c == '\\' || c.is_control());
    (names.len() <= MAX_DEPTH && names.iter().all(good)).then_some(names)
}

/// Is `given` the secret? Compares every byte, whatever differs first.
pub fn same_secret(given: &str, secret: &str) -> bool {
    !secret.is_empty() && given.len() == secret.len() && given.bytes().zip(secret.bytes()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}

/// The key in `Authorization: Bearer KEY`.
pub fn bearer(head: &Head) -> Option<&str> {
    let value = head.header("authorization")?;
    let (scheme, key) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(key.trim())
}

/// The part of a file of `size` bytes that a `Range` header asks for, as
/// its first and last byte: `bytes=A-B`, `bytes=A-` (from A on) or
/// `bytes=-N` (the last N). `Ok(None)`: not a range this server serves
/// (another unit, several ranges): the whole file is the answer.
/// `Err`: a range that the file does not have.
pub fn byte_range(header: &str, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.trim().strip_prefix("bytes=") else { return Ok(None) };
    let Some((from, to)) = spec.split_once('-') else { return Ok(None) };
    let number = |text: &str| -> Option<u64> { (!text.is_empty() && text.len() <= 18 && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().ok()).flatten() };
    match (from.trim(), to.trim()) {
        ("", last) => match number(last) {
            Some(0) => Err(()),
            Some(n) if size > 0 => Ok(Some((size.saturating_sub(n), size - 1))),
            Some(_) => Err(()),
            None => Ok(None),
        },
        (first, "") => match number(first) {
            Some(a) if a < size => Ok(Some((a, size - 1))),
            Some(_) => Err(()),
            None => Ok(None),
        },
        (first, last) => match (number(first), number(last)) {
            (Some(a), Some(b)) if a <= b && a < size => Ok(Some((a, b.min(size - 1)))),
            (Some(a), Some(b)) if a <= b => Err(()),
            _ => Ok(None),
        },
    }
}

/// Text inside a JSON string.
pub fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            c if (c as u32) < 0x20 => out += &format!("\\u{:04x}", c as u32),
            c => out.push(c),
        }
    }
    out
}

/// One entry of a folder, for a listing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub name: String,
    pub folder: bool,
    pub size: u64,
}

/// A folder's listing as JSON: `{"path":"/a","entries":[{"name":..,"type":"file","size":3}]}`.
pub fn listing_json(path: &str, entries: &[Entry]) -> String {
    let mut out = format!("{{\"path\":\"{}\",\"entries\":[", json_escape(path));
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let kind = if e.folder { "folder" } else { "file" };
        out += &format!("{{\"name\":\"{}\",\"type\":\"{kind}\",\"size\":{}}}", json_escape(&e.name), e.size);
    }
    out += "]}\n";
    out
}

/// `{"error":"..."}` for an answer that is not a success.
pub fn error_json(message: &str) -> String {
    format!("{{\"error\":\"{}\"}}\n", json_escape(message))
}

/// The head of an answer; the connection is closed after the body. `more`
/// holds whole header lines, each ending with "\r\n".
pub fn response_head(status: u16, reason: &str, content_type: &str, length: u64, more: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\n{more}\
         Connection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUT: &[u8] = b"PUT /api/files/notes/a%20b.txt?x=1&name=a+b%21 HTTP/1.1\r\nHost: hp:9000\r\nContent-Length: 5\r\n\
        Authorization:  Bearer s3cr3t \r\n\r\nhello";

    #[test]
    fn head_with_headers_query_and_body() {
        let (head, body) = HeadReader::new().feed(PUT).unwrap().unwrap();
        assert_eq!(head.method, "PUT");
        assert_eq!(head.raw_path, "/api/files/notes/a%20b.txt");
        assert_eq!(head.path, "/api/files/notes/a b.txt");
        assert_eq!(head.query, "x=1&name=a+b%21");
        assert_eq!(head.query_value("name").as_deref(), Some("a b!"));
        assert_eq!(head.query_value("x").as_deref(), Some("1"));
        assert_eq!(head.query_value("y"), None);
        assert_eq!(head.header("host"), Some("hp:9000"));
        assert_eq!(head.content_length(), Ok(Some(5)));
        assert_eq!(bearer(&head), Some("s3cr3t"));
        assert_eq!(body, b"hello");
    }

    #[test]
    fn head_in_pieces_of_any_size() {
        for size in 1..PUT.len() {
            let mut reader = HeadReader::new();
            let mut got = None;
            let mut body = Vec::new();
            for piece in PUT.chunks(size) {
                match &got {
                    None => {
                        if let Some((head, rest)) = reader.feed(piece).unwrap() {
                            got = Some(head);
                            body = rest;
                        }
                    }
                    Some(_) => body.extend_from_slice(piece),
                }
            }
            assert_eq!(got.unwrap().path, "/api/files/notes/a b.txt", "pieces of {size}");
            assert_eq!(body, b"hello", "pieces of {size}");
        }
    }

    #[test]
    fn truncated_and_broken_heads() {
        // Every cut of a good head is either "not yet" or refused, never a panic.
        let whole = &PUT[..PUT.len() - 5];
        for cut in 0..whole.len() - 1 {
            assert!(!matches!(HeadReader::new().feed(&whole[..cut]), Ok(Some(_))), "cut at {cut}");
            let _ = parse_head(&whole[..cut]);
        }
        assert!(parse_head(b"GET").is_err());
        assert!(parse_head(b"GET /a").is_err());
        assert!(parse_head(b"get /a HTTP/1.1").is_err());
        assert!(parse_head(b"GET a HTTP/1.1").is_err());
        assert!(parse_head(b"GET /a HTTP/2").is_err());
        assert!(parse_head(b"GET /%zz HTTP/1.1").is_err());
        assert!(parse_head(b"GET /a%00b HTTP/1.1").is_err());
        assert!(parse_head(b"GET /a HTTP/1.1\r\nno colon here").is_err());
        assert!(parse_head(b"GET /a HTTP/1.1\r\nbad name: x").is_err());
        assert!(parse_head(b"GET /\xff HTTP/1.1").is_err());
        let long = format!("GET /{} HTTP/1.1", "a".repeat(MAX_PATH));
        assert!(parse_head(long.as_bytes()).is_err());
        let many = format!("GET / HTTP/1.1{}", "\r\nx: y".repeat(MAX_HEADERS + 1));
        assert!(parse_head(many.as_bytes()).is_err());
        // A head that never ends is refused once it is too large.
        let mut reader = HeadReader::new();
        let mut refused = false;
        for _ in 0..(MAX_HEAD / 100 + 2) {
            refused |= reader.feed(&[b'a'; 100]).is_err();
        }
        assert!(refused);
        // A bad length is an error, not "no body".
        let head = parse_head(b"PUT /a HTTP/1.1\r\nContent-Length: -1").unwrap();
        assert_eq!(head.content_length(), Err(()));
        let head = parse_head(b"PUT /a HTTP/1.1\r\nContent-Length: 99999999999999999999").unwrap();
        assert_eq!(head.content_length(), Err(()));
    }

    #[test]
    fn paths_stay_inside_the_folder() {
        assert_eq!(safe_path("/", 27), Some(vec![]));
        assert_eq!(safe_path("", 27), Some(vec![]));
        assert_eq!(safe_path("/a/b.txt", 27), Some(vec!["a", "b.txt"]));
        assert_eq!(safe_path("/a/b/", 27), Some(vec!["a", "b"]));
        for bad in ["/..", "/a/../b", "/a/./b", "/a//b", "///", "/a\\b", "/a/..", "/.", "/a\tb"] {
            assert_eq!(safe_path(bad, 27), None, "{bad}");
        }
        assert_eq!(safe_path(&format!("/{}", "n".repeat(28)), 27), None);
        assert!(safe_path(&"/d".repeat(MAX_DEPTH), 27).is_some());
        assert_eq!(safe_path(&"/d".repeat(MAX_DEPTH + 1), 27), None);
    }

    #[test]
    fn secrets_and_bearer() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abd", "abc"));
        assert!(!same_secret("ab", "abc"));
        assert!(!same_secret("", ""));
        let head = parse_head(b"GET / HTTP/1.1\r\nAuthorization: Basic abc").unwrap();
        assert_eq!(bearer(&head), None);
        let head = parse_head(b"GET / HTTP/1.1\r\nAuthorization: bearer").unwrap();
        assert_eq!(bearer(&head), None);
        let head = parse_head(b"GET / HTTP/1.1").unwrap();
        assert_eq!(bearer(&head), None);
    }

    #[test]
    fn byte_ranges() {
        assert_eq!(byte_range("bytes=0-9", 100), Ok(Some((0, 9))));
        assert_eq!(byte_range("bytes=10-", 100), Ok(Some((10, 99))));
        assert_eq!(byte_range("bytes=-10", 100), Ok(Some((90, 99))));
        assert_eq!(byte_range("bytes=-1000", 100), Ok(Some((0, 99))));
        assert_eq!(byte_range("bytes=90-1000", 100), Ok(Some((90, 99))));
        assert_eq!(byte_range(" bytes=5-5 ", 100), Ok(Some((5, 5))));
        assert_eq!(byte_range("bytes=99-99", 100), Ok(Some((99, 99))));
        // Not in the file: refused.
        for gone in ["bytes=100-", "bytes=100-200", "bytes=-0", "bytes=500-600"] {
            assert_eq!(byte_range(gone, 100), Err(()), "{gone}");
        }
        assert_eq!(byte_range("bytes=0-", 0), Err(()));
        assert_eq!(byte_range("bytes=-5", 0), Err(()));
        // Not a range we serve: the whole file.
        for whole in ["", "bytes", "bytes=", "bytes=-", "items=0-9", "bytes=0-9,20-29", "bytes=9-0", "bytes=a-b", "bytes=1-b", "bytes=--5", "bytes=99999999999999999999-", "bytes=0x10-"] {
            assert_eq!(byte_range(whole, 100), Ok(None), "{whole}");
        }
    }

    #[test]
    fn json_answers() {
        let entries = [
            Entry { name: String::from("a \"b\".txt"), folder: false, size: 12 },
            Entry { name: String::from("sub"), folder: true, size: 0 },
        ];
        assert_eq!(
            listing_json("/x\\y", &entries),
            "{\"path\":\"/x\\\\y\",\"entries\":[{\"name\":\"a \\\"b\\\".txt\",\"type\":\"file\",\"size\":12},{\"name\":\"sub\",\"type\":\"folder\",\"size\":0}]}\n"
        );
        assert_eq!(listing_json("/", &[]), "{\"path\":\"/\",\"entries\":[]}\n");
        assert_eq!(error_json("no\nsuch"), "{\"error\":\"no\\u000asuch\"}\n");
    }
}
