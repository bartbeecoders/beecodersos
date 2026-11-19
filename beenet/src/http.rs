//! HTTP/1.1 pieces. Client (network phase N5): URLs, building a request and
//! decoding a response as its bytes arrive. Server (phase N6): reading a
//! request, building a response head, escaping text for a page. No sockets here: the caller moves
//! the bytes over a TCP connection.
//!
//! Plain HTTP only. `https://` is recognised so the caller can say so clearly.
//! Bodies: `Content-Length`, `Transfer-Encoding: chunked`, or "until the
//! server closes". The response is untrusted: the header block and every
//! chunk-size line are bounded, and the caller bounds the body.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub const DEFAULT_PORT: u16 = 80;
pub const HTTPS_PORT: u16 = 443;
/// The status line and headers together may not be larger than this.
pub const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 100;
const MAX_CHUNK_LINE: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HttpError {
    /// The address is `https://`: TLS is not supported.
    Https,
    BadUrl,
    /// Not an HTTP response, or headers that contradict each other.
    BadResponse,
    HeadTooLarge,
    BadChunk,
    /// The connection ended before the whole response arrived.
    Truncated,
}

impl HttpError {
    pub fn message(self) -> &'static str {
        match self {
            HttpError::Https => "HTTPS (encrypted HTTP) is not supported yet: only http:// addresses work",
            HttpError::BadUrl => "not a valid address (expected http://HOST[:PORT]/PATH)",
            HttpError::BadResponse => "the server's answer is not valid HTTP",
            HttpError::HeadTooLarge => "the server's answer has too many headers",
            HttpError::BadChunk => "the server's answer is damaged (bad chunk)",
            HttpError::Truncated => "the connection ended before the whole answer arrived",
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// An `http://` or `https://` address.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Url {
    /// `https://`: the connection goes through TLS (tls.rs), port 443 by default.
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`.
    pub path: String,
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

impl Url {
    /// Parse `http[s]://HOST[:PORT][/PATH]`; `http://` may be left out.
    pub fn parse(text: &str) -> Result<Url, HttpError> {
        let text = text.trim();
        let (secure, rest) = match strip_prefix_ignore_case(text, "https://") {
            Some(rest) => (true, rest),
            None => (false, strip_prefix_ignore_case(text, "http://").unwrap_or(text)),
        };
        let default_port = if secure { HTTPS_PORT } else { DEFAULT_PORT };
        if rest.contains("://") {
            return Err(HttpError::BadUrl);
        }
        let rest = rest.split('#').next().unwrap_or(""); // a fragment is never sent
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(split);
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().ok().filter(|p| *p != 0).ok_or(HttpError::BadUrl)?),
            None => (authority, default_port),
        };
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_');
        // No spaces or control characters: they could smuggle extra header lines.
        let path_ok = path.len() <= 2000 && path.bytes().all(|b| b > 0x20 && b != 0x7F);
        if !host_ok || !path_ok {
            return Err(HttpError::BadUrl);
        }
        let path = match path.as_bytes().first() {
            None => String::from("/"),
            Some(b'?') => format!("/{path}"),
            _ => String::from(path),
        };
        Ok(Url { secure, host: host.to_ascii_lowercase(), port, path })
    }

    /// The address a redirect's `Location` header points to, seen from here.
    pub fn join(&self, location: &str) -> Result<Url, HttpError> {
        let location = location.trim();
        if location.contains("://") {
            return Url::parse(location);
        }
        if let Some(rest) = location.strip_prefix("//") {
            return Url::parse(&format!("{}{rest}", self.scheme()));
        }
        let path = if location.starts_with('/') {
            String::from(location)
        } else {
            let without_query = self.path.split('?').next().unwrap_or("/");
            let dir = &without_query[..without_query.rfind('/').map_or(0, |i| i + 1)];
            format!("{dir}{location}")
        };
        Url::parse(&format!("{}{}{path}", self.scheme(), self.authority()))
    }

    pub fn scheme(&self) -> &'static str {
        if self.secure { "https://" } else { "http://" }
    }

    /// The value of the `Host` header.
    pub fn authority(&self) -> String {
        if self.port == if self.secure { HTTPS_PORT } else { DEFAULT_PORT } {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{}{}", self.scheme(), self.authority(), self.path)
    }
}

/// The bytes of a request. `body`: (content type, data), for POST.
/// The connection is closed after the answer (`Connection: close`).
pub fn request(method: &str, url: &Url, agent: &str, body: Option<(&str, &[u8])>) -> Vec<u8> {
    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {agent}\r\nAccept: */*\r\nConnection: close\r\n",
        url.path,
        url.authority()
    );
    if let Some((kind, data)) = body {
        head += &format!("Content-Type: {kind}\r\nContent-Length: {}\r\n", data.len());
    }
    head += "\r\n";
    let mut out = head.into_bytes();
    if let Some((_, data)) = body {
        out.extend_from_slice(data);
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Head,
    /// A body of known length: bytes still to come.
    Length(u64),
    ChunkSize,
    ChunkData(u64),
    /// The line end after a chunk's data.
    ChunkEnd,
    Trailers,
    UntilClose,
    Done,
}

/// Decodes a response as its bytes arrive.
pub struct Response {
    state: State,
    /// The head, or one chunk-size / trailer line, while incomplete.
    buf: Vec<u8>,
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
}

impl Default for Response {
    fn default() -> Self {
        Self::new()
    }
}

impl Response {
    pub fn new() -> Response {
        Response { state: State::Head, buf: Vec::new(), status: 0, reason: String::new(), headers: Vec::new() }
    }

    /// The status code (200, 404, ...); 0 until the headers have arrived.
    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn headers_done(&self) -> bool {
        self.state != State::Head
    }

    /// The whole response has arrived.
    pub fn complete(&self) -> bool {
        self.state == State::Done
    }

    /// A header's value; `name` in lower case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The body's length, when the server announced it.
    pub fn content_length(&self) -> Option<u64> {
        if self.is_chunked() {
            return None;
        }
        self.header("content-length")?.parse().ok()
    }

    /// 301, 302, 303, 307, 308 with a `Location`: where to go instead.
    pub fn redirect(&self) -> Option<&str> {
        matches!(self.status, 301 | 302 | 303 | 307 | 308).then(|| self.header("location")).flatten()
    }

    fn is_chunked(&self) -> bool {
        self.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    }

    /// Take the next received bytes; decoded body bytes are added to `body`.
    pub fn feed(&mut self, mut data: &[u8], body: &mut Vec<u8>) -> Result<(), HttpError> {
        while !data.is_empty() {
            match self.state {
                State::Done => return Ok(()), // anything after the response is ignored
                State::Head => {
                    // Look for the empty line, also across the pieces received so far.
                    let old = self.buf.len();
                    let take = data.len().min(MAX_HEAD + 4 - old.min(MAX_HEAD + 4));
                    self.buf.extend_from_slice(&data[..take]);
                    let from = old.saturating_sub(3);
                    let end = find_head_end(&self.buf[from..]).map(|(i, len)| (from + i, len));
                    let Some((end, marker)) = end else {
                        if self.buf.len() > MAX_HEAD {
                            return Err(HttpError::HeadTooLarge);
                        }
                        data = &data[take..];
                        continue;
                    };
                    let used = end + marker - old; // bytes of `data` that belong to the head
                    data = &data[used..];
                    let head = core::mem::take(&mut self.buf);
                    self.parse_head(&head[..end])?;
                }
                State::Length(left) | State::ChunkData(left) => {
                    let n = (data.len() as u64).min(left) as usize;
                    body.extend_from_slice(&data[..n]);
                    data = &data[n..];
                    let left = left - n as u64;
                    self.state = match (self.state, left) {
                        (State::Length(_), 0) => State::Done,
                        (State::Length(_), _) => State::Length(left),
                        (_, 0) => State::ChunkEnd,
                        _ => State::ChunkData(left),
                    };
                }
                State::UntilClose => {
                    body.extend_from_slice(data);
                    return Ok(());
                }
                State::ChunkSize | State::ChunkEnd | State::Trailers => {
                    let Some(line) = self.take_line(&mut data)? else { continue };
                    self.state = match self.state {
                        State::ChunkSize => {
                            let digits = line.split(|b| *b == b';').next().unwrap_or(&[]);
                            let text = core::str::from_utf8(digits).map_err(|_| HttpError::BadChunk)?.trim();
                            if text.is_empty() || text.len() > 15 {
                                return Err(HttpError::BadChunk);
                            }
                            match u64::from_str_radix(text, 16).map_err(|_| HttpError::BadChunk)? {
                                0 => State::Trailers,
                                size => State::ChunkData(size),
                            }
                        }
                        State::ChunkEnd if line.is_empty() => State::ChunkSize,
                        State::ChunkEnd => return Err(HttpError::BadChunk),
                        _ if line.is_empty() => State::Done, // end of the trailers
                        _ => State::Trailers,
                    };
                }
            }
        }
        Ok(())
    }

    /// The connection ended. Fine if the response was complete (or its end is
    /// marked by the server closing); otherwise the response was cut short.
    pub fn end_of_stream(&mut self) -> Result<(), HttpError> {
        match self.state {
            State::Done => Ok(()),
            State::UntilClose => {
                self.state = State::Done;
                Ok(())
            }
            _ => Err(HttpError::Truncated),
        }
    }

    /// Collect one line (without its line end) from `data` into `self.buf`.
    fn take_line(&mut self, data: &mut &[u8]) -> Result<Option<Vec<u8>>, HttpError> {
        while let Some((&b, rest)) = data.split_first() {
            *data = rest;
            if b == b'\n' {
                let mut line = core::mem::take(&mut self.buf);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            if self.buf.len() >= MAX_CHUNK_LINE {
                return Err(HttpError::BadChunk);
            }
            self.buf.push(b);
        }
        Ok(None)
    }

    fn parse_head(&mut self, head: &[u8]) -> Result<(), HttpError> {
        let text = core::str::from_utf8(head).map_err(|_| HttpError::BadResponse)?;
        let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
        let status_line = lines.next().unwrap_or("");
        let rest = status_line.strip_prefix("HTTP/1.").ok_or(HttpError::BadResponse)?;
        let mut parts = rest.splitn(3, ' ');
        let (_minor, code) = (parts.next(), parts.next().unwrap_or(""));
        if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(HttpError::BadResponse);
        }
        let status: u16 = code.parse().map_err(|_| HttpError::BadResponse)?;
        let mut headers = Vec::new();
        for line in lines {
            let (name, value) = line.split_once(':').ok_or(HttpError::BadResponse)?;
            if name.is_empty() || name.contains(' ') || headers.len() >= MAX_HEADERS {
                return Err(HttpError::BadResponse);
            }
            headers.push((name.to_ascii_lowercase(), String::from(value.trim())));
        }
        if (100..200).contains(&status) && status != 101 {
            return Ok(()); // "100 Continue" and friends: the real response follows
        }
        self.status = status;
        self.reason = String::from(parts.next().unwrap_or("").trim());
        self.headers = headers;
        let mut lengths = self.headers.iter().filter(|(n, _)| n == "content-length").map(|(_, v)| v.parse::<u64>());
        let length = match lengths.next() {
            None => None,
            Some(first) => {
                let first = first.map_err(|_| HttpError::BadResponse)?;
                if lengths.any(|l| l != Ok(first)) {
                    return Err(HttpError::BadResponse); // two lengths that disagree
                }
                Some(first)
            }
        };
        self.state = if status == 204 || status == 304 {
            State::Done
        } else if self.is_chunked() {
            State::ChunkSize
        } else {
            match length {
                Some(0) => State::Done,
                Some(n) => State::Length(n),
                None => State::UntilClose,
            }
        };
        Ok(())
    }
}

// --- the server side (phase N6) ---------------------------------------------------------

/// A request's line and headers together may not be larger than this.
pub const MAX_REQUEST_HEAD: usize = 8 * 1024;
const MAX_TARGET: usize = 1024;

/// What a client asked for. Headers are not kept: the server does not use them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    /// "GET", "HEAD", ...
    pub method: String,
    /// The path, decoded, starting with `/`, without the query.
    pub path: String,
}

/// Collects a request as its bytes arrive.
#[derive(Default)]
pub struct RequestReader {
    buf: Vec<u8>,
}

impl RequestReader {
    pub fn new() -> RequestReader {
        RequestReader::default()
    }

    /// Take the next received bytes; the request, once its head is complete.
    pub fn feed(&mut self, data: &[u8]) -> Result<Option<Request>, HttpError> {
        let from = self.buf.len().saturating_sub(3);
        let room = (MAX_REQUEST_HEAD + 4).saturating_sub(self.buf.len());
        self.buf.extend_from_slice(&data[..data.len().min(room)]);
        match find_head_end(&self.buf[from..]) {
            Some((i, _)) => parse_request(&self.buf[..from + i]).map(Some),
            None if self.buf.len() > MAX_REQUEST_HEAD => Err(HttpError::HeadTooLarge),
            None => Ok(None),
        }
    }
}

/// Parse a request's head (without the empty line that ends it).
pub fn parse_request(head: &[u8]) -> Result<Request, HttpError> {
    let text = core::str::from_utf8(head).map_err(|_| HttpError::BadResponse)?;
    let line = text.split('\n').next().unwrap_or("").trim_end_matches('\r');
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let method_ok = !method.is_empty() && method.len() <= 10 && method.bytes().all(|b| b.is_ascii_uppercase());
    if !method_ok || parts.next().is_some() || !version.starts_with("HTTP/1.") || target.len() > MAX_TARGET {
        return Err(HttpError::BadResponse);
    }
    let raw_path = target.split(['?', '#']).next().unwrap_or("");
    let path = percent_decode(raw_path).ok_or(HttpError::BadResponse)?;
    if !path.starts_with('/') || path.chars().any(|c| c.is_control()) {
        return Err(HttpError::BadResponse);
    }
    Ok(Request { method: String::from(method), path })
}

/// Undo `%20`-style encoding. None: a broken escape, or not UTF-8 afterwards.
pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = core::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Encode a file name for use in a link.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out += &format!("%{b:02X}");
        }
    }
    out
}

/// Text made safe to put inside an HTML page.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out += "&amp;",
            '<' => out += "&lt;",
            '>' => out += "&gt;",
            '"' => out += "&quot;",
            '\'' => out += "&#39;",
            c => out.push(c),
        }
    }
    out
}

/// The head of a response; the connection is closed after the body.
pub fn response_head(status: u16, reason: &str, content_type: &str, length: usize) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\n\
         Connection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n"
    )
}

/// Where the empty line that ends the head starts, and how long that marker is.
fn find_head_end(buf: &[u8]) -> Option<(usize, usize)> {
    (0..buf.len()).find_map(|i| {
        if buf[i..].starts_with(b"\r\n\r\n") {
            Some((i, 4))
        } else if buf[i..].starts_with(b"\n\n") {
            Some((i, 2))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    /// Feed `data` in pieces of `step` bytes; (response, body).
    fn decode(data: &[u8], step: usize) -> Result<(Response, Vec<u8>), HttpError> {
        let (mut r, mut body) = (Response::new(), Vec::new());
        for piece in data.chunks(step) {
            r.feed(piece, &mut body)?;
        }
        r.end_of_stream()?;
        Ok((r, body))
    }

    #[test]
    fn urls() {
        assert_eq!(url("http://example.com"), Url { secure: false, host: "example.com".into(), port: 80, path: "/".into() });
        assert_eq!(url("HTTPS://example.com/x"), Url { secure: true, host: "example.com".into(), port: 443, path: "/x".into() });
        assert_eq!(format!("{}", url("https://example.com:8443/x")), "https://example.com:8443/x");
        assert_eq!(format!("{}", url("https://example.com:443/x")), "https://example.com/x");
        assert_eq!(url("https://a/b/c").join("d").unwrap(), url("https://a/b/d"));
        assert_eq!(url("https://a/b").join("//c/x").unwrap(), url("https://c/x"));
        assert_eq!(url("http://a/b").join("https://c/").unwrap(), url("https://c/"));
        assert_eq!(url("Example.COM/a/b?x=1#frag").path, "/a/b?x=1");
        assert_eq!(url("HTTP://10.0.2.2:8080/x").port, 8080);
        assert_eq!(url("http://h?q=1").path, "/?q=1");
        assert_eq!(url("http://h:8080/x").authority(), "h:8080");
        assert_eq!(format!("{}", url("h/x")), "http://h/x");
        for bad in ["", "http://", "ftp://h/x", "http://h:0/", "http://h:99999/", "http://h:x/", "http://a b/",
            "http://h/a b", "http://h/a\r\nEvil: 1", "http://user@h/", "http://[::1]/"]
        {
            assert_eq!(Url::parse(bad), Err(HttpError::BadUrl), "{bad:?}");
        }
    }

    #[test]
    fn redirect_targets() {
        let base = url("http://h:8080/dir/page?x=1");
        assert_eq!(base.join("/other").unwrap(), url("http://h:8080/other"));
        assert_eq!(base.join("next").unwrap(), url("http://h:8080/dir/next"));
        assert_eq!(base.join("http://else/where").unwrap(), url("http://else/where"));
        assert_eq!(base.join("//else/where").unwrap(), url("http://else/where"));
        assert_eq!(url("http://h").join("x").unwrap(), url("http://h/x"));
    }

    #[test]
    fn request_text() {
        let get = request("GET", &url("http://h:81/p?q"), "Bee/1", None);
        assert_eq!(
            core::str::from_utf8(&get).unwrap(),
            "GET /p?q HTTP/1.1\r\nHost: h:81\r\nUser-Agent: Bee/1\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        let post = request("POST", &url("http://h/"), "Bee/1", Some(("text/plain", b"hi")));
        assert!(post.ends_with(b"Content-Type: text/plain\r\nContent-Length: 2\r\n\r\nhi"));
    }

    #[test]
    fn body_with_a_length_in_any_piece_size() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 11\r\n\r\nhello worldEXTRA";
        for step in [1, 2, 3, 7, 50, 1000] {
            let (r, body) = decode(data, step).unwrap();
            assert_eq!((r.status(), r.reason(), body.as_slice()), (200, "OK", &b"hello world"[..]), "step {step}");
            assert_eq!(r.header("content-type"), Some("text/plain"));
            assert_eq!(r.content_length(), Some(11));
            assert!(r.complete());
        }
    }

    #[test]
    fn chunked_body() {
        let data = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
            5\r\nhello\r\n1;ext=1\r\n \r\nA\r\nworld12345\r\n0\r\nTrailer: x\r\n\r\n";
        for step in [1, 2, 5, 1000] {
            let (r, body) = decode(data, step).unwrap();
            assert_eq!(body, b"hello world12345", "step {step}");
            assert!(r.complete());
            assert_eq!(r.content_length(), None);
        }
    }

    #[test]
    fn body_until_the_server_closes() {
        let (r, body) = decode(b"HTTP/1.0 200 OK\n\nline one\nline two\n", 4).unwrap();
        assert_eq!((r.status(), body.as_slice()), (200, &b"line one\nline two\n"[..]));
    }

    #[test]
    fn responses_without_a_body() {
        let (r, body) = decode(b"HTTP/1.1 204 No Content\r\n\r\n", 5).unwrap();
        assert_eq!((r.status(), body.len()), (204, 0));
        let (r, body) = decode(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n", 5).unwrap();
        assert_eq!((r.status(), r.reason(), body.len()), (404, "Not Found", 0));
    }

    #[test]
    fn redirects_and_interim_responses() {
        let (r, _) = decode(b"HTTP/1.1 302 Found\r\nLocation: /new\r\nContent-Length: 0\r\n\r\n", 9).unwrap();
        assert_eq!(r.redirect(), Some("/new"));
        let (r, _) = decode(b"HTTP/1.1 200 OK\r\nLocation: /new\r\nContent-Length: 0\r\n\r\n", 9).unwrap();
        assert_eq!(r.redirect(), None);
        let data = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let (r, body) = decode(data, 6).unwrap();
        assert_eq!((r.status(), body.as_slice()), (200, &b"ok"[..]));
    }

    #[test]
    fn cut_short_or_damaged_responses_are_errors() {
        let full = b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world";
        for cut in 0..full.len() {
            assert_eq!(decode(&full[..cut], 4).err(), Some(HttpError::Truncated), "cut at {cut}");
        }
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        for cut in 0..chunked.len() {
            assert_eq!(decode(&chunked[..cut], 4).err(), Some(HttpError::Truncated), "cut at {cut}");
        }
        let bad: [&[u8]; 8] = [
            b"SSH-2.0-OpenSSH\r\n\r\n",
            b"HTTP/1.1 20 OK\r\n\r\n",
            b"HTTP/1.1 abc OK\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello!",
            b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n\xff\xfe: x\r\n\r\n",
            b"HTTP/2 200\r\n\r\n",
        ];
        for data in bad {
            assert_eq!(decode(data, 1000).err(), Some(HttpError::BadResponse), "{:?}", String::from_utf8_lossy(data));
        }
        let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let bad_chunks: [&[u8]; 5] =
            [b"zz\r\n", b"\r\n", b"5\r\nhelloXX\r\n", b"11111111111111111\r\n", &[b'1'; MAX_CHUNK_LINE + 1]];
        for tail in bad_chunks {
            let data = [head, tail].concat();
            assert_eq!(decode(&data, 3).err(), Some(HttpError::BadChunk), "{:?}", String::from_utf8_lossy(tail));
        }
    }

    #[test]
    fn server_reads_requests() {
        let text = b"GET /files/my%20notes.txt?x=1 HTTP/1.1\r\nHost: bee\r\nAccept: */*\r\n\r\nignored";
        for step in [1, 3, 7, 1000] {
            let mut reader = RequestReader::new();
            let mut got = None;
            for piece in text.chunks(step) {
                if let Some(r) = reader.feed(piece).unwrap() {
                    got = Some(r);
                    break;
                }
            }
            let r = got.expect("a request");
            assert_eq!((r.method.as_str(), r.path.as_str()), ("GET", "/files/my notes.txt"), "step {step}");
        }
        assert_eq!(parse_request(b"HEAD / HTTP/1.0").unwrap(), Request { method: "HEAD".into(), path: "/".into() });
        // Cut off anywhere: not complete yet, never a wrong request.
        for cut in 0..text.len() - 11 {
            assert_eq!(RequestReader::new().feed(&text[..cut]), Ok(None), "cut at {cut}");
        }
    }

    #[test]
    fn server_refuses_bad_requests() {
        let bad: [&[u8]; 12] = [
            b"",
            b"GET",
            b"GET /",
            b"get / HTTP/1.1",
            b"GET / HTTP/2",
            b"GET / HTTP/1.1 extra",
            b"GET relative HTTP/1.1",
            b"GET http://other/ HTTP/1.1",
            b"GET /a%zz HTTP/1.1",
            b"GET /a%2 HTTP/1.1",
            b"GET /a%00b HTTP/1.1",
            b"\xff\xfe / HTTP/1.1",
        ];
        for head in bad {
            assert_eq!(parse_request(head), Err(HttpError::BadResponse), "{:?}", String::from_utf8_lossy(head));
        }
        assert_eq!(parse_request(b"GET /%ff HTTP/1.1"), Err(HttpError::BadResponse)); // not UTF-8
        let long = format!("GET /{} HTTP/1.1", "a".repeat(MAX_TARGET));
        assert_eq!(parse_request(long.as_bytes()), Err(HttpError::BadResponse));
        // A client that never finishes its headers is cut off.
        let mut reader = RequestReader::new();
        let mut result = reader.feed(b"GET / HTTP/1.1\r\n");
        for _ in 0..1000 {
            if result.is_err() {
                break;
            }
            result = reader.feed(b"X-Padding: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        assert_eq!(result, Err(HttpError::HeadTooLarge));
    }

    #[test]
    fn server_text_helpers() {
        assert_eq!(escape_html("<a href=\"x\">Tom & 'Jerry'</a>"), "&lt;a href=&quot;x&quot;&gt;Tom &amp; &#39;Jerry&#39;&lt;/a&gt;");
        assert_eq!(percent_encode("my notes (1).txt"), "my%20notes%20%281%29.txt");
        assert_eq!(percent_decode(&percent_encode("caf\u{e9} 100%/x")).as_deref(), Some("caf\u{e9} 100%/x"));
        assert_eq!(percent_decode("%"), None);
        let head = response_head(404, "Not Found", "text/plain", 12);
        assert!(head.starts_with("HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 12\r\n"));
        assert!(head.ends_with("\r\n\r\n"));
    }

    #[test]
    fn an_endless_head_is_refused() {
        let (mut r, mut body) = (Response::new(), Vec::new());
        r.feed(b"HTTP/1.1 200 OK\r\n", &mut body).unwrap();
        let mut result = Ok(());
        for _ in 0..1000 {
            result = r.feed(b"X-Padding: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n", &mut body);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result, Err(HttpError::HeadTooLarge));
        let many = [&b"HTTP/1.1 200 OK\r\n"[..], &b"a: b\r\n".repeat(MAX_HEADERS + 1), b"\r\n"].concat();
        assert_eq!(decode(&many, 1000).err(), Some(HttpError::BadResponse));
    }
}
