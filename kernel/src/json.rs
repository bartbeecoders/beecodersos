//! Minimal JSON for one-line messages: build flat objects and parse them back.
//! Used by the event log, crash reports and the AI bridge protocol.
//! Nested objects and arrays are not supported (the parser rejects them).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Num(i64),
    Bool(bool),
    Null,
}

/// Builder for a flat JSON object: `Obj::new().str("a", "x").num("n", 1).finish()`.
pub struct Obj(String);

impl Obj {
    pub fn new() -> Self {
        Obj(String::from("{"))
    }

    fn key(&mut self, k: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        escape_into(&mut self.0, k);
        self.0.push(':');
    }

    pub fn str(mut self, k: &str, v: &str) -> Self {
        self.key(k);
        escape_into(&mut self.0, v);
        self
    }

    pub fn num(mut self, k: &str, v: i64) -> Self {
        self.key(k);
        let _ = write!(self.0, "{v}");
        self
    }

    pub fn bool(mut self, k: &str, v: bool) -> Self {
        self.key(k);
        self.0.push_str(if v { "true" } else { "false" });
        self
    }

    pub fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

/// Append `s` as a quoted JSON string.
pub fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A parsed flat object: (key, value) pairs in order.
pub struct Object(pub Vec<(String, Value)>);

impl Object {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn num(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
}

/// Parse one flat JSON object. Returns None for anything else.
pub fn parse(text: &str) -> Option<Object> {
    let mut p = Parser { s: text.as_bytes(), i: 0 };
    p.ws();
    p.eat(b'{')?;
    let mut out = Vec::new();
    p.ws();
    if p.peek() == Some(b'}') {
        p.i += 1;
    } else {
        loop {
            p.ws();
            let k = p.string()?;
            p.ws();
            p.eat(b':')?;
            p.ws();
            let v = p.value()?;
            out.push((k, v));
            p.ws();
            match p.next()? {
                b',' => continue,
                b'}' => break,
                _ => return None,
            }
        }
    }
    p.ws();
    (p.i == p.s.len()).then_some(Object(out))
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.i += 1;
        Some(b)
    }

    fn eat(&mut self, b: u8) -> Option<()> {
        (self.next()? == b).then_some(())
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.i += 1;
        }
    }

    fn lit(&mut self, word: &[u8], v: Value) -> Option<Value> {
        let end = self.i + word.len();
        (self.s.get(self.i..end)? == word).then(|| {
            self.i = end;
            v
        })
    }

    fn value(&mut self) -> Option<Value> {
        match self.peek()? {
            b'"' => self.string().map(Value::Str),
            b't' => self.lit(b"true", Value::Bool(true)),
            b'f' => self.lit(b"false", Value::Bool(false)),
            b'n' => self.lit(b"null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    /// Integers; a fraction or exponent is accepted and dropped.
    fn number(&mut self) -> Option<Value> {
        let neg = self.peek() == Some(b'-');
        if neg {
            self.i += 1;
        }
        let mut n: i64 = 0;
        let start = self.i;
        while let Some(d @ b'0'..=b'9') = self.peek() {
            n = n.saturating_mul(10).saturating_add((d - b'0') as i64);
            self.i += 1;
        }
        if self.i == start {
            return None;
        }
        while matches!(self.peek(), Some(b'.' | b'e' | b'E' | b'+' | b'-' | b'0'..=b'9')) {
            self.i += 1;
        }
        Some(Value::Num(if neg { -n } else { n }))
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = core::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(h, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            // Copy a run of plain bytes (keeps multi-byte UTF-8 intact).
            let start = self.i;
            while !matches!(self.peek()?, b'"' | b'\\') {
                self.i += 1;
            }
            out.push_str(core::str::from_utf8(&self.s[start..self.i]).ok()?);
            if self.next()? == b'"' {
                return Some(out);
            }
            let c = match self.next()? {
                b'"' => '"',
                b'\\' => '\\',
                b'/' => '/',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'u' => {
                    let hi = self.hex4()?;
                    let code = if (0xD800..0xDC00).contains(&hi) {
                        self.eat(b'\\')?;
                        self.eat(b'u')?;
                        let lo = self.hex4()?;
                        0x10000 + ((hi - 0xD800) << 10) + (lo.checked_sub(0xDC00)? & 0x3FF)
                    } else {
                        hi
                    };
                    char::from_u32(code).unwrap_or('\u{FFFD}')
                }
                _ => return None,
            };
            out.push(c);
        }
    }
}
