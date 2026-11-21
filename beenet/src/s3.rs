//! The S3 protocol, as far as the file server speaks it: checking a request's
//! signature (AWS Signature Version 4), taking a signed or chunked body
//! apart, choosing the keys of a listing, and the XML answers. No sockets
//! and no files here: `kernel/src/fileserver.rs` does those.
//!
//! Everything received is untrusted: a request is only acted on after its
//! signature, made with the shared secret, has been checked in full, and
//! every length in a chunked body is checked before it is used.

use crate::fileserver::Head;
use crate::http::percent_decode;
use crate::sha256::{hmac, sha256};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The access key id every client uses; the secret is the file server's key.
pub const ACCESS_KEY: &str = "beecoders";
/// A request's time may differ this much from ours (seconds).
pub const MAX_SKEW: u64 = 15 * 60;
/// A listing gives this many keys at most per answer.
pub const MAX_KEYS: usize = 1000;

/// Why a request was not accepted. `code` is S3's name for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthError {
    /// No `Authorization` header of the right kind.
    Missing,
    /// The header, or a header it names, is missing or malformed.
    Malformed,
    /// Not our access key.
    UnknownKey,
    /// The request's time is too far from ours.
    Skewed,
    /// The signature is not the one the secret gives.
    BadSignature,
    /// The body is not what was signed, or its chunks are broken.
    BadBody,
}

impl AuthError {
    pub fn code(self) -> &'static str {
        match self {
            AuthError::Missing => "AccessDenied",
            AuthError::Malformed => "AuthorizationHeaderMalformed",
            AuthError::UnknownKey => "InvalidAccessKeyId",
            AuthError::Skewed => "RequestTimeTooSkewed",
            AuthError::BadSignature => "SignatureDoesNotMatch",
            AuthError::BadBody => "XAmzContentSHA256Mismatch",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            AuthError::Missing => "sign the request (AWS Signature Version 4)",
            AuthError::Malformed => "the Authorization header is not a complete AWS4-HMAC-SHA256 signature",
            AuthError::UnknownKey => "unknown access key id",
            AuthError::Skewed => "the request's time differs too much from this machine's clock",
            AuthError::BadSignature => "the signature does not match: check the secret key",
            AuthError::BadBody => "the body is not what was signed",
        }
    }
}

/// How the body was signed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Payload {
    /// `UNSIGNED-PAYLOAD`: the body is taken as it is.
    Unsigned,
    /// The SHA-256 the body must have.
    Hash([u8; 32]),
    /// `aws-chunked` pieces; `signed`: each with a signature of its own.
    Chunked { signed: bool },
}

/// A request whose signature was right.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Auth {
    pub payload: Payload,
    amz_date: String,
    scope: String,
    signature: String,
    signing_key: [u8; 32],
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out += &format!("{b:02x}");
    }
    out
}

fn unhex32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Encode as AWS does: everything but letters, digits and `-._~` becomes
/// `%XX` (capitals); `/` stays when `keep_slash`.
pub fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') || (b == b'/' && keep_slash) {
            out.push(b as char);
        } else {
            out += &format!("%{b:02X}");
        }
    }
    out
}

/// The query as it is signed: every name and value encoded anew, sorted.
fn canonical_query(query: &str) -> Option<String> {
    let mut pairs = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        pairs.push((uri_encode(&percent_decode(name)?, false), uri_encode(&percent_decode(value)?, false)));
    }
    pairs.sort();
    let joined: Vec<String> = pairs.iter().map(|(n, v)| format!("{n}={v}")).collect();
    Some(joined.join("&"))
}

/// A header's value as it is signed: no spaces around it, runs of spaces as one.
fn canonical_value(value: &str) -> String {
    value.split(' ').filter(|w| !w.is_empty()).collect::<Vec<&str>>().join(" ")
}

/// Seconds since 1970 for `YYYYMMDDTHHMMSSZ`.
pub fn parse_amz_date(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' || !b[..8].iter().chain(&b[9..15]).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = |from: usize, to: usize| text[from..to].parse::<u64>().ok();
    let (year, month, day) = (n(0, 4)?, n(4, 6)?, n(6, 8)?);
    let (hour, minute, second) = (n(9, 11)?, n(11, 13)?, n(13, 15)?);
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days since 1970-01-01 (the usual civil-calendar arithmetic, March-based years).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// (year, month, day, hour, minute, second, day of the week with 0 = Sunday).
fn civil(unix: u64) -> (u64, u64, u64, u64, u64, u64, u64) {
    let days = unix / 86_400;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as u64;
    let rest = unix % 86_400;
    (year, month, day, rest / 3600, rest / 60 % 60, rest % 60, (days + 4) % 7)
}

/// `2026-11-21T07:00:00.000Z`, as the XML answers give times.
pub fn iso_time(unix: u64) -> String {
    let (y, mo, d, h, mi, s, _) = civil(unix);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000Z")
}

/// `Sat, 21 Nov 2026 07:00:00 GMT`, as HTTP headers give times.
pub fn http_time(unix: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, mo, d, h, mi, s, wd) = civil(unix);
    format!("{}, {d:02} {} {y:04} {h:02}:{mi:02}:{s:02} GMT", DAYS[wd as usize], MONTHS[mo as usize - 1])
}

/// Does this request carry an S3 signature at all?
pub fn is_signed(head: &Head) -> bool {
    head.header("authorization").is_some_and(|v| v.starts_with("AWS4-HMAC-SHA256"))
}

/// Check the request's signature against `secret`. `now`: our clock (seconds
/// since 1970), when it is known; the request's time is then checked too.
pub fn verify(head: &Head, secret: &str, now: Option<u64>) -> Result<Auth, AuthError> {
    let value = head.header("authorization").ok_or(AuthError::Missing)?;
    let rest = value.strip_prefix("AWS4-HMAC-SHA256").ok_or(AuthError::Missing)?;
    let (mut credential, mut signed, mut signature) = (None, None, None);
    for part in rest.split(',') {
        match part.trim().split_once('=') {
            Some(("Credential", v)) => credential = Some(v),
            Some(("SignedHeaders", v)) => signed = Some(v),
            Some(("Signature", v)) => signature = Some(v),
            _ => return Err(AuthError::Malformed),
        }
    }
    let (Some(credential), Some(signed), Some(signature)) = (credential, signed, signature) else { return Err(AuthError::Malformed) };
    let (access_key, scope) = credential.split_once('/').ok_or(AuthError::Malformed)?;
    let parts: Vec<&str> = scope.split('/').collect();
    let [date, region, service, "aws4_request"] = parts.as_slice() else { return Err(AuthError::Malformed) };
    if date.len() != 8 || region.is_empty() || region.len() > 32 || *service != "s3" || unhex32(signature).is_none() {
        return Err(AuthError::Malformed);
    }
    if !same(access_key, ACCESS_KEY) {
        return Err(AuthError::UnknownKey);
    }
    let amz_date = head.header("x-amz-date").ok_or(AuthError::Malformed)?;
    let when = parse_amz_date(amz_date).ok_or(AuthError::Malformed)?;
    if !amz_date.starts_with(date) {
        return Err(AuthError::Malformed);
    }
    let content = head.header("x-amz-content-sha256").ok_or(AuthError::Malformed)?;
    let payload = match content {
        "UNSIGNED-PAYLOAD" => Payload::Unsigned,
        "STREAMING-AWS4-HMAC-SHA256-PAYLOAD" | "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER" => Payload::Chunked { signed: true },
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER" => Payload::Chunked { signed: false },
        hash => Payload::Hash(unhex32(hash).ok_or(AuthError::Malformed)?),
    };

    // The canonical request: what both sides hash and sign.
    let names: Vec<&str> = signed.split(';').collect();
    if !names.contains(&"host") || names.iter().any(|n| n.is_empty() || n.bytes().any(|b| b.is_ascii_uppercase())) {
        return Err(AuthError::Malformed);
    }
    let mut canonical = format!("{}\n{}\n", head.method, uri_encode(&head.path, true));
    canonical += &canonical_query(&head.query).ok_or(AuthError::Malformed)?;
    canonical.push('\n');
    for name in &names {
        let values: Vec<String> = head.headers.iter().filter(|(n, _)| n == name).map(|(_, v)| canonical_value(v)).collect();
        if values.is_empty() {
            return Err(AuthError::Malformed);
        }
        canonical += &format!("{name}:{}\n", values.join(","));
    }
    canonical += &format!("\n{signed}\n{content}");

    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&sha256(canonical.as_bytes())));
    let mut key = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    for part in [region.as_bytes(), service.as_bytes(), b"aws4_request".as_slice()] {
        key = hmac(&key, part);
    }
    if secret.is_empty() || !same(&hex(&hmac(&key, to_sign.as_bytes())), signature) {
        return Err(AuthError::BadSignature);
    }
    // Only now the time: a wrong secret must not learn about our clock.
    if let Some(now) = now {
        if now.abs_diff(when) > MAX_SKEW {
            return Err(AuthError::Skewed);
        }
    }
    Ok(Auth { payload, amz_date: String::from(amz_date), scope: String::from(scope), signature: String::from(signature), signing_key: key })
}

/// The body as the client meant it: checked against its hash, or put
/// together from its `aws-chunked` pieces (each one's signature checked).
/// `decoded_length`: the `x-amz-decoded-content-length` header, if sent.
pub fn body(auth: &Auth, received: Vec<u8>, decoded_length: Option<u64>) -> Result<Vec<u8>, AuthError> {
    match auth.payload {
        Payload::Unsigned => Ok(received),
        Payload::Hash(hash) => (sha256(&received) == hash).then_some(received).ok_or(AuthError::BadBody),
        Payload::Chunked { signed } => {
            let data = unchunk(auth, &received, signed)?;
            match decoded_length {
                Some(n) if n != data.len() as u64 => Err(AuthError::BadBody),
                _ => Ok(data),
            }
        }
    }
}

/// `SIZE[;chunk-signature=SIG]\r\n DATA \r\n` ... ending with a chunk of size 0.
fn unchunk(auth: &Auth, mut rest: &[u8], signed: bool) -> Result<Vec<u8>, AuthError> {
    let empty_hash = hex(&sha256(b""));
    let mut previous = auth.signature.clone();
    let mut out = Vec::new();
    loop {
        let line_end = rest.iter().take(256).position(|b| *b == b'\n').ok_or(AuthError::BadBody)?;
        let line = core::str::from_utf8(&rest[..line_end]).map_err(|_| AuthError::BadBody)?.trim_end_matches('\r');
        rest = &rest[line_end + 1..];
        let (size, extension) = line.split_once(';').unwrap_or((line, ""));
        if size.is_empty() || size.len() > 8 {
            return Err(AuthError::BadBody);
        }
        let size = usize::from_str_radix(size, 16).map_err(|_| AuthError::BadBody)?;
        if size > rest.len() {
            return Err(AuthError::BadBody);
        }
        let data = &rest[..size];
        rest = &rest[size..];
        if signed {
            let given = extension.strip_prefix("chunk-signature=").ok_or(AuthError::BadBody)?;
            let to_sign = format!(
                "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}\n{previous}\n{empty_hash}\n{}",
                auth.amz_date,
                auth.scope,
                hex(&sha256(data))
            );
            let right = hex(&hmac(&auth.signing_key, to_sign.as_bytes()));
            if !same(&right, given) {
                return Err(AuthError::BadBody);
            }
            previous = right;
        }
        if size == 0 {
            // Trailers (checksums, their signature) may follow; they are not used.
            return Ok(out);
        }
        out.extend_from_slice(data);
        rest = rest.strip_prefix(b"\r\n").or_else(|| rest.strip_prefix(b"\n")).ok_or(AuthError::BadBody)?;
    }
}

// --- listings ---------------------------------------------------------------------------

/// One object of a bucket.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Object {
    pub key: String,
    pub size: u64,
    pub etag: String,
}

/// What a listing asks for.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ListQuery {
    /// `list-type=2`.
    pub v2: bool,
    pub prefix: String,
    pub delimiter: String,
    /// Only keys after this one (marker, start-after or continuation token).
    pub after: String,
    /// The continuation token as it was sent (v2 answers repeat it).
    pub token: Option<String>,
    pub max_keys: usize,
    /// `encoding-type=url`: keys are sent percent-encoded.
    pub url_encoded: bool,
}

impl ListQuery {
    pub fn from(head: &Head) -> ListQuery {
        let value = |name: &str| head.query_value(name).unwrap_or_default();
        let v2 = value("list-type") == "2";
        let token = head.query_value("continuation-token");
        let after = match (&token, v2) {
            (Some(token), true) => token.clone(),
            (None, true) => value("start-after"),
            (_, false) => value("marker"),
        };
        ListQuery {
            v2,
            prefix: value("prefix"),
            delimiter: value("delimiter"),
            after,
            token,
            max_keys: head.query_value("max-keys").and_then(|v| v.parse::<usize>().ok()).unwrap_or(MAX_KEYS).min(MAX_KEYS),
            url_encoded: value("encoding-type") == "url",
        }
    }
}

/// One answer's worth of a listing.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Page {
    pub objects: Vec<Object>,
    /// Key beginnings that stand for everything below them ("folders").
    pub prefixes: Vec<String>,
    /// More follows: ask again with this as marker / continuation token.
    pub next: Option<String>,
}

/// Choose what a listing shows from all objects of the bucket, sorted by key.
pub fn select(all: &[Object], q: &ListQuery) -> Page {
    let mut page = Page::default();
    let mut last = String::new();
    // After a "folder" was given, everything in it has been given.
    let skip = (!q.delimiter.is_empty() && q.after.ends_with(&q.delimiter)).then_some(q.after.as_str());
    for object in all {
        let key = object.key.as_str();
        if !key.starts_with(&q.prefix) || key <= q.after.as_str() || skip.is_some_and(|s| key.starts_with(s)) {
            continue;
        }
        let rest = &key[q.prefix.len()..];
        let folder = match rest.find(&q.delimiter) {
            Some(at) if !q.delimiter.is_empty() => Some(&key[..q.prefix.len() + at + q.delimiter.len()]),
            _ => None,
        };
        if folder.is_some_and(|f| page.prefixes.last().is_some_and(|p| p == f)) {
            continue;
        }
        if page.objects.len() + page.prefixes.len() >= q.max_keys {
            page.next = Some(last);
            return page;
        }
        match folder {
            Some(f) => {
                page.prefixes.push(String::from(f));
                last = String::from(f);
            }
            None => {
                page.objects.push(object.clone());
                last = String::from(key);
            }
        }
    }
    page
}

// --- XML answers ------------------------------------------------------------------------

pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out += "&amp;",
            '<' => out += "&lt;",
            '>' => out += "&gt;",
            '"' => out += "&quot;",
            '\'' => out += "&apos;",
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' => out += &format!("&#{};", c as u32),
            c => out.push(c),
        }
    }
    out
}

const XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";
const NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

pub fn error_xml(code: &str, message: &str, resource: &str) -> String {
    format!(
        "{XML}<Error><Code>{}</Code><Message>{}</Message><Resource>{}</Resource><RequestId>0</RequestId></Error>",
        xml_escape(code),
        xml_escape(message),
        xml_escape(resource)
    )
}

pub fn buckets_xml(names: &[String], time: &str) -> String {
    let mut out = format!("{XML}<ListAllMyBucketsResult xmlns=\"{NS}\"><Owner><ID>{ACCESS_KEY}</ID><DisplayName>{ACCESS_KEY}</DisplayName></Owner><Buckets>");
    for name in names {
        out += &format!("<Bucket><Name>{}</Name><CreationDate>{time}</CreationDate></Bucket>", xml_escape(name));
    }
    out + "</Buckets></ListAllMyBucketsResult>"
}

pub fn location_xml() -> String {
    format!("{XML}<LocationConstraint xmlns=\"{NS}\"></LocationConstraint>")
}

pub fn list_xml(bucket: &str, q: &ListQuery, page: &Page, time: &str) -> String {
    let show = |text: &str| if q.url_encoded { uri_encode(text, true) } else { xml_escape(text) };
    let mut out = format!("{XML}<ListBucketResult xmlns=\"{NS}\"><Name>{}</Name><Prefix>{}</Prefix>", xml_escape(bucket), show(&q.prefix));
    if !q.delimiter.is_empty() {
        out += &format!("<Delimiter>{}</Delimiter>", show(&q.delimiter));
    }
    out += &format!("<MaxKeys>{}</MaxKeys><IsTruncated>{}</IsTruncated>", q.max_keys, page.next.is_some());
    if q.url_encoded {
        out += "<EncodingType>url</EncodingType>";
    }
    if q.v2 {
        out += &format!("<KeyCount>{}</KeyCount>", page.objects.len() + page.prefixes.len());
        if let Some(token) = &q.token {
            out += &format!("<ContinuationToken>{}</ContinuationToken>", xml_escape(token));
        }
        if let Some(next) = &page.next {
            out += &format!("<NextContinuationToken>{}</NextContinuationToken>", xml_escape(next));
        }
    } else {
        out += &format!("<Marker>{}</Marker>", show(&q.after));
        if let Some(next) = &page.next {
            out += &format!("<NextMarker>{}</NextMarker>", show(next));
        }
    }
    for o in &page.objects {
        out += &format!(
            "<Contents><Key>{}</Key><LastModified>{time}</LastModified><ETag>&quot;{}&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
            show(&o.key),
            xml_escape(&o.etag),
            o.size
        );
    }
    for p in &page.prefixes {
        out += &format!("<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", show(p));
    }
    out + "</ListBucketResult>"
}

/// The keys in a "delete these objects" request (`<Key>..</Key>`), and
/// whether it asks for a quiet answer. `None`: more than 1000, or broken.
pub fn delete_keys(xml: &[u8]) -> Option<(Vec<String>, bool)> {
    let text = core::str::from_utf8(xml).ok()?;
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("<Key>") {
        let after = &rest[at + 5..];
        let end = after.find("</Key>")?;
        if keys.len() >= 1000 {
            return None;
        }
        keys.push(xml_unescape(&after[..end])?);
        rest = &after[end + 6..];
    }
    Some((keys, text.contains("<Quiet>true</Quiet>")))
}

fn xml_unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out += &rest[..at];
        let end = rest[at..].find(';')? + at;
        match &rest[at + 1..end] {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            number => {
                let code = match number.strip_prefix("#x") {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => number.strip_prefix('#')?.parse().ok()?,
                };
                out.push(char::from_u32(code)?);
            }
        }
        rest = &rest[end + 1..];
    }
    out += rest;
    Some(out)
}

/// The answer to a "delete these objects" request: what went, what did not.
pub fn deleted_xml(deleted: &[String], failed: &[(String, &str)], quiet: bool) -> String {
    let mut out = format!("{XML}<DeleteResult xmlns=\"{NS}\">");
    if !quiet {
        for key in deleted {
            out += &format!("<Deleted><Key>{}</Key></Deleted>", xml_escape(key));
        }
    }
    for (key, why) in failed {
        out += &format!("<Error><Key>{}</Key><Code>AccessDenied</Code><Message>{}</Message></Error>", xml_escape(key), xml_escape(why));
    }
    out + "</DeleteResult>"
}

/// May a folder of this name be a bucket? (Letters, digits, `.`, `-`, `_`.)
pub fn bucket_name_ok(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name != "api"
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fileserver::parse_head;

    // The examples of the AWS documentation ("Signature Calculations for the
    // Authorization Header" in the Amazon S3 API reference).
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const NOW: u64 = 1_369_353_600; // 2013-05-24T00:00:00Z

    fn request(line: &str, headers: &[(&str, &str)], signed: &str, signature: &str) -> Head {
        let mut text = format!("{line} HTTP/1.1\r\nHost: examplebucket.s3.amazonaws.com\r\nx-amz-date: 20130524T000000Z");
        for (name, value) in headers {
            text += &format!("\r\n{name}: {value}");
        }
        // Our access key id in place of the documentation's: it is not part of what is signed.
        text += &format!(
            "\r\nAuthorization: AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/20130524/us-east-1/s3/aws4_request,SignedHeaders={signed},Signature={signature}"
        );
        parse_head(text.as_bytes()).unwrap()
    }

    #[test]
    fn aws_example_get_object() {
        let head = request(
            "GET /test.txt",
            &[("Range", "bytes=0-9"), ("x-amz-content-sha256", EMPTY)],
            "host;range;x-amz-content-sha256;x-amz-date",
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
        );
        let auth = verify(&head, SECRET, Some(NOW)).unwrap();
        assert_eq!(body(&auth, Vec::new(), None), Ok(Vec::new()));
        assert_eq!(body(&auth, b"x".to_vec(), None), Err(AuthError::BadBody));
        // Another secret, another time, another path: refused.
        assert_eq!(verify(&head, "wrong", Some(NOW)), Err(AuthError::BadSignature));
        assert_eq!(verify(&head, "", Some(NOW)), Err(AuthError::BadSignature));
        assert_eq!(verify(&head, SECRET, Some(NOW + MAX_SKEW + 1)), Err(AuthError::Skewed));
        assert_eq!(verify(&head, SECRET, Some(NOW - MAX_SKEW - 1)), Err(AuthError::Skewed));
        assert!(verify(&head, SECRET, Some(NOW + MAX_SKEW)).is_ok());
        assert!(verify(&head, SECRET, None).is_ok());
        let mut other = head.clone();
        other.path = String::from("/other.txt");
        assert_eq!(verify(&other, SECRET, Some(NOW)), Err(AuthError::BadSignature));
        let mut other = head.clone();
        other.method = String::from("DELETE");
        assert_eq!(verify(&other, SECRET, Some(NOW)), Err(AuthError::BadSignature));
        let mut other = head.clone();
        other.headers.iter_mut().find(|(n, _)| n == "range").unwrap().1 = String::from("bytes=0-99");
        assert_eq!(verify(&other, SECRET, Some(NOW)), Err(AuthError::BadSignature));
    }

    #[test]
    fn aws_example_put_object() {
        let head = request(
            "PUT /test%24file.text",
            &[
                ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                ("x-amz-content-sha256", "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"),
            ],
            "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class",
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd",
        );
        let auth = verify(&head, SECRET, Some(NOW)).unwrap();
        assert_eq!(body(&auth, b"Welcome to Amazon S3.".to_vec(), None), Ok(b"Welcome to Amazon S3.".to_vec()));
        assert_eq!(body(&auth, b"Welcome to Amazon S4.".to_vec(), None), Err(AuthError::BadBody));
    }

    #[test]
    fn aws_examples_with_a_query() {
        let head = request(
            "GET /?lifecycle",
            &[("x-amz-content-sha256", EMPTY)],
            "host;x-amz-content-sha256;x-amz-date",
            "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543",
        );
        assert!(verify(&head, SECRET, Some(NOW)).is_ok());
        let head = request(
            "GET /?max-keys=2&prefix=J",
            &[("x-amz-content-sha256", EMPTY)],
            "host;x-amz-content-sha256;x-amz-date",
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7",
        );
        assert!(verify(&head, SECRET, Some(NOW)).is_ok());
        // The order in which the client sent them does not matter; their values do.
        let mut other = head.clone();
        other.query = String::from("prefix=J&max-keys=2");
        assert!(verify(&other, SECRET, Some(NOW)).is_ok());
        other.query = String::from("prefix=K&max-keys=2");
        assert_eq!(verify(&other, SECRET, Some(NOW)), Err(AuthError::BadSignature));
    }

    fn chunked_example() -> (Head, Vec<u8>) {
        let head = request(
            "PUT /examplebucket/chunkObject.txt",
            &[
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
                ("Content-Encoding", "aws-chunked"),
                ("x-amz-decoded-content-length", "66560"),
                ("Content-Length", "66824"),
            ],
            "content-encoding;content-length;host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length;x-amz-storage-class",
            "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9",
        );
        let mut sent = Vec::new();
        sent.extend_from_slice(b"10000;chunk-signature=ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648\r\n");
        sent.extend_from_slice(&[b'a'; 65536]);
        sent.extend_from_slice(b"\r\n400;chunk-signature=0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497\r\n");
        sent.extend_from_slice(&[b'a'; 1024]);
        sent.extend_from_slice(b"\r\n0;chunk-signature=b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9\r\n\r\n");
        (head, sent)
    }

    #[test]
    fn aws_example_chunked_upload() {
        let (mut head, sent) = chunked_example();
        // The documentation's host for this example is s3.amazonaws.com.
        head.headers.iter_mut().find(|(n, _)| n == "host").unwrap().1 = String::from("s3.amazonaws.com");
        assert_eq!(sent.len(), 66824);
        let auth = verify(&head, SECRET, Some(NOW)).unwrap();
        assert_eq!(auth.payload, Payload::Chunked { signed: true });
        let data = body(&auth, sent.clone(), Some(66560)).unwrap();
        assert_eq!(data, vec![b'a'; 66560]);
        // A changed byte, a wrong announced length: refused.
        let mut changed = sent.clone();
        changed[200] = b'b';
        assert_eq!(body(&auth, changed, Some(66560)), Err(AuthError::BadBody));
        assert_eq!(body(&auth, sent.clone(), Some(66561)), Err(AuthError::BadBody));
        // Cut anywhere, it is refused and never read past its end.
        for cut in (0..sent.len() - 2).step_by(997).chain(sent.len() - 120..sent.len() - 2) {
            assert_eq!(body(&auth, sent[..cut].to_vec(), None), Err(AuthError::BadBody), "cut at {cut}");
        }
    }

    #[test]
    fn unsigned_chunks_with_a_trailer() {
        let (head, _) = chunked_example();
        let mut auth = verify(&{
            let mut h = head.clone();
            h.headers.iter_mut().find(|(n, _)| n == "host").unwrap().1 = String::from("s3.amazonaws.com");
            h
        }, SECRET, None)
        .unwrap();
        auth.payload = Payload::Chunked { signed: false };
        let sent = b"5\r\nhello\r\n6\r\n, bees\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n".to_vec();
        assert_eq!(body(&auth, sent, Some(11)), Ok(b"hello, bees".to_vec()));
        for broken in [&b"5\r\nhel"[..], b"zz\r\nhello\r\n0\r\n", b"5\r\nhelloXX0\r\n", b"fffffffff\r\n", b"5", b"", b"\r\n", b"-1\r\n"] {
            assert_eq!(body(&auth, broken.to_vec(), None), Err(AuthError::BadBody), "{broken:?}");
        }
    }

    #[test]
    fn headers_that_are_not_a_signature() {
        let good = request("GET /", &[("x-amz-content-sha256", EMPTY)], "host;x-amz-content-sha256;x-amz-date", &"0".repeat(64));
        assert_eq!(verify(&good, SECRET, None), Err(AuthError::BadSignature));
        let with = |authorization: &str| {
            let mut head = good.clone();
            head.headers.iter_mut().find(|(n, _)| n == "authorization").unwrap().1 = String::from(authorization);
            verify(&head, SECRET, None)
        };
        let sig = "0".repeat(64);
        assert_eq!(with("Bearer abc"), Err(AuthError::Missing));
        assert_eq!(with("AWS4-HMAC-SHA256"), Err(AuthError::Malformed));
        assert_eq!(with("AWS4-HMAC-SHA256 Credential=beecoders/20130524/us-east-1/s3/aws4_request"), Err(AuthError::Malformed));
        assert_eq!(
            with(&format!("AWS4-HMAC-SHA256 Credential=someone/20130524/us-east-1/s3/aws4_request,SignedHeaders=host,Signature={sig}")),
            Err(AuthError::UnknownKey)
        );
        for broken in [
            format!("AWS4-HMAC-SHA256 Credential=beecoders,SignedHeaders=host,Signature={sig}"),
            format!("AWS4-HMAC-SHA256 Credential=beecoders/20130524/us-east-1/ec2/aws4_request,SignedHeaders=host,Signature={sig}"),
            format!("AWS4-HMAC-SHA256 Credential=beecoders/20130524/us-east-1/s3,SignedHeaders=host,Signature={sig}"),
            format!("AWS4-HMAC-SHA256 Credential=beecoders/20130525/us-east-1/s3/aws4_request,SignedHeaders=host,Signature={sig}"),
            format!("AWS4-HMAC-SHA256 Credential=beecoders/20130524/us-east-1/s3/aws4_request,SignedHeaders=x-amz-date,Signature={sig}"),
            format!("AWS4-HMAC-SHA256 Credential=beecoders/20130524/us-east-1/s3/aws4_request,SignedHeaders=host;x-missing,Signature={sig}"),
            format!("AWS4-HMAC-SHA256 Credential=beecoders/20130524/us-east-1/s3/aws4_request,SignedHeaders=host,Signature=abc"),
            format!("AWS4-HMAC-SHA256 Credential=beecoders/20130524/us-east-1/s3/aws4_request,SignedHeaders=host,Signature={sig},More=1"),
        ] {
            assert_eq!(with(&broken), Err(AuthError::Malformed), "{broken}");
        }
        // Without the date or the body's hash there is nothing to check.
        for missing in ["x-amz-date", "x-amz-content-sha256"] {
            let mut head = good.clone();
            head.headers.retain(|(n, _)| n != missing);
            assert_eq!(verify(&head, SECRET, None), Err(AuthError::Malformed));
        }
        let mut head = good.clone();
        head.headers.retain(|(n, _)| n != "authorization");
        assert_eq!(verify(&head, SECRET, None), Err(AuthError::Missing));
        assert!(!is_signed(&head));
        assert!(is_signed(&good));
    }

    #[test]
    fn times() {
        assert_eq!(parse_amz_date("20130524T000000Z"), Some(NOW));
        assert_eq!(parse_amz_date("19700101T000000Z"), Some(0));
        assert_eq!(parse_amz_date("20240229T235959Z"), Some(1_709_251_199));
        for bad in ["", "20130524", "20130524T000000", "20130524 000000Z", "2013052aT000000Z", "20131324T000000Z", "20130524T250000Z", "20130500T000000Z"] {
            assert_eq!(parse_amz_date(bad), None, "{bad}");
        }
        assert_eq!(iso_time(NOW), "2013-05-24T00:00:00.000Z");
        assert_eq!(http_time(NOW), "Fri, 24 May 2013 00:00:00 GMT");
        assert_eq!(iso_time(1_709_251_199), "2024-02-29T23:59:59.000Z");
        assert_eq!(http_time(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_time(1_795_244_400), "Sat, 21 Nov 2026 07:00:00 GMT");
    }

    fn objects(keys: &[&str]) -> Vec<Object> {
        keys.iter().map(|k| Object { key: String::from(*k), size: k.len() as u64, etag: String::from("e") }).collect()
    }

    fn keys(page: &Page) -> Vec<&str> {
        page.objects.iter().map(|o| o.key.as_str()).collect()
    }

    #[test]
    fn listings() {
        let all = objects(&["a.txt", "docs/one.txt", "docs/sub/two.txt", "docs/three.txt", "pics/cat.jpg", "z.txt"]);
        let mut all_sorted = all.clone();
        all_sorted.sort_by(|a, b| a.key.cmp(&b.key));
        let q = |prefix: &str, delimiter: &str, after: &str, max: usize| ListQuery {
            prefix: String::from(prefix),
            delimiter: String::from(delimiter),
            after: String::from(after),
            max_keys: max,
            ..ListQuery::default()
        };
        // Everything.
        let page = select(&all_sorted, &q("", "", "", 1000));
        assert_eq!(keys(&page), ["a.txt", "docs/one.txt", "docs/sub/two.txt", "docs/three.txt", "pics/cat.jpg", "z.txt"]);
        assert_eq!(page.next, None);
        // As folders.
        let page = select(&all_sorted, &q("", "/", "", 1000));
        assert_eq!(keys(&page), ["a.txt", "z.txt"]);
        assert_eq!(page.prefixes, ["docs/", "pics/"]);
        let page = select(&all_sorted, &q("docs/", "/", "", 1000));
        assert_eq!(keys(&page), ["docs/one.txt", "docs/three.txt"]);
        assert_eq!(page.prefixes, ["docs/sub/"]);
        let page = select(&all_sorted, &q("docs/t", "/", "", 1000));
        assert_eq!(keys(&page), ["docs/three.txt"]);
        assert!(select(&all_sorted, &q("nothing", "/", "", 1000)).objects.is_empty());
        // In pages: every key and folder exactly once.
        for max in 1..=6 {
            for delimiter in ["", "/"] {
                let (mut seen, mut folders, mut after, mut rounds) = (Vec::new(), Vec::new(), String::new(), 0);
                loop {
                    let page = select(&all_sorted, &q("", delimiter, &after, max));
                    assert!(page.objects.len() + page.prefixes.len() <= max);
                    seen.extend(page.objects.iter().map(|o| o.key.clone()));
                    folders.extend(page.prefixes.iter().cloned());
                    rounds += 1;
                    match page.next {
                        Some(next) => after = next,
                        None => break,
                    }
                    assert!(rounds < 10);
                }
                let whole = select(&all_sorted, &q("", delimiter, "", 1000));
                assert_eq!(seen, keys(&whole), "max {max} delimiter {delimiter:?}");
                assert_eq!(folders, whole.prefixes, "max {max} delimiter {delimiter:?}");
            }
        }
        assert_eq!(select(&all_sorted, &q("", "", "", 0)).next, Some(String::new()));
    }

    #[test]
    fn list_queries_and_xml() {
        let head = parse_head(b"GET /b?list-type=2&prefix=docs%2F&delimiter=%2F&max-keys=5000&continuation-token=docs%2Fa&encoding-type=url HTTP/1.1").unwrap();
        let q = ListQuery::from(&head);
        assert!(q.v2 && q.url_encoded);
        assert_eq!((q.prefix.as_str(), q.delimiter.as_str(), q.after.as_str(), q.max_keys), ("docs/", "/", "docs/a", MAX_KEYS));
        let head = parse_head(b"GET /b?marker=m&max-keys=x HTTP/1.1").unwrap();
        let q = ListQuery::from(&head);
        assert!(!q.v2 && !q.url_encoded);
        assert_eq!((q.after.as_str(), q.max_keys), ("m", MAX_KEYS));

        let page = Page { objects: objects(&["a <b>&.txt"]), prefixes: vec![String::from("p q/")], next: Some(String::from("p q/")) };
        let q = ListQuery { v2: true, max_keys: 2, ..ListQuery::default() };
        let xml = list_xml("bucket", &q, &page, "2026-11-21T07:00:00.000Z");
        assert!(xml.contains("<Key>a &lt;b&gt;&amp;.txt</Key>"), "{xml}");
        assert!(xml.contains("<IsTruncated>true</IsTruncated><KeyCount>2</KeyCount><NextContinuationToken>p q/</NextContinuationToken>"), "{xml}");
        assert!(xml.contains("<CommonPrefixes><Prefix>p q/</Prefix></CommonPrefixes>"), "{xml}");
        assert!(xml.contains("<ETag>&quot;e&quot;</ETag><Size>10</Size>"), "{xml}");
        let q = ListQuery { url_encoded: true, max_keys: 2, ..ListQuery::default() };
        let xml = list_xml("bucket", &q, &page, "t");
        assert!(xml.contains("<Key>a%20%3Cb%3E%26.txt</Key>") && xml.contains("<EncodingType>url</EncodingType>"), "{xml}");
        assert!(xml.contains("<NextMarker>p%20q/</NextMarker>"), "{xml}");

        assert_eq!(
            error_xml("NoSuchKey", "no <such> key", "/b/k"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>NoSuchKey</Code><Message>no &lt;such&gt; key</Message><Resource>/b/k</Resource><RequestId>0</RequestId></Error>"
        );
        assert!(buckets_xml(&[String::from("one")], "T").contains("<Bucket><Name>one</Name><CreationDate>T</CreationDate></Bucket>"));
    }

    #[test]
    fn delete_requests() {
        let xml = b"<Delete><Object><Key>a.txt</Key></Object><Object><Key>d/b &amp; c&#33;&#x21;.txt</Key></Object><Quiet>true</Quiet></Delete>";
        assert_eq!(delete_keys(xml), Some((vec![String::from("a.txt"), String::from("d/b & c!!.txt")], true)));
        assert_eq!(delete_keys(b"<Delete></Delete>"), Some((vec![], false)));
        // Truncated or broken: refused, never a panic.
        for cut in 0..xml.len() {
            let _ = delete_keys(&xml[..cut]);
        }
        assert_eq!(delete_keys(b"<Delete><Object><Key>a.txt"), None);
        assert_eq!(delete_keys(b"<Key>a &broken</Key>"), None);
        assert_eq!(delete_keys(b"<Key>a &#xZZ;</Key>"), None);
        assert_eq!(delete_keys(b"<Key>\xff</Key>"), None);
        let many = "<Key>k</Key>".repeat(1001);
        assert_eq!(delete_keys(many.as_bytes()), None);
        let out = deleted_xml(&[String::from("a<")], &[(String::from("b"), "no")], false);
        assert!(out.contains("<Deleted><Key>a&lt;</Key></Deleted><Error><Key>b</Key><Code>AccessDenied</Code><Message>no</Message></Error>"));
        assert!(!deleted_xml(&[String::from("a")], &[], true).contains("Deleted>"));
    }

    #[test]
    fn bucket_names() {
        for good in ["photos", "my-bucket_1", "a", "Backups.2026"] {
            assert!(bucket_name_ok(good, 26), "{good}");
        }
        for bad in ["", "api", ".", "..", "a/b", "a b", "ümlaut", "a-name-that-is-longer-than-26"] {
            assert!(!bucket_name_ok(bad, 26), "{bad}");
        }
    }
}
