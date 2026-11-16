//! The link to beehive (the AI host) over the network: how a machine proves
//! it was paired, and how every line is sealed afterwards. The transport is
//! a plain TCP connection carrying text lines; this module only builds and
//! checks the lines. Plan: Docs/beehive-network-plan.html.
//!
//! The machine and beehive share a key (made by `beehive.py pair NAME`, typed
//! into the OS once, never sent):
//!
//!   machine: {"type":"hello","name":NAME,"nonce":A,...}     A, B: 32 hex digits, random
//!   beehive: {"type":"challenge","nonce":B}
//!   machine: {"type":"auth","mac":proof(key, "os", A, B)}
//!   beehive: {"type":"welcome",...,"mac":proof(key, "hive", B, A)}
//!
//! After that every line is "MAC N JSON": N counts the lines of one
//! direction from 1; MAC is made from the direction ("o" from the OS, "h"
//! from beehive), N and the JSON text, with a key for this connection only.
//! A line that was changed, repeated, left out or made up does not pass.
//!
//! Not encrypted: others on the network can read the lines.
//! The same code exists in tools/beehive/beehive.py. Keep the two in step.

use crate::sha256::hmac;
use alloc::format;
use alloc::string::String;

pub const DEFAULT_PORT: u16 = 4560;
/// Longest line either side accepts.
pub const MAX_LINE: usize = 8192;
/// Letters and digits a key is made of: no l, o, 0, 1.
const KEY_ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
pub const KEY_LEN: usize = 20;

pub type Key = [u8; KEY_LEN];

/// The key as typed: four groups of five characters; dashes and capitals do
/// not matter. `None` if it cannot be a key.
pub fn key_bytes(code: &str) -> Option<Key> {
    let mut key = [0u8; KEY_LEN];
    let mut n = 0;
    for c in code.bytes().filter(|c| *c != b'-') {
        let c = c.to_ascii_lowercase();
        if n == KEY_LEN || !KEY_ALPHABET.contains(&c) {
            return None;
        }
        key[n] = c;
        n += 1;
    }
    (n == KEY_LEN).then_some(key)
}

/// The key in the form it is typed: abcde-fghij-kmnpq-rstuv.
pub fn key_text(key: &Key) -> String {
    let mut text = String::new();
    for (i, c) in key.iter().enumerate() {
        if i > 0 && i % 5 == 0 {
            text.push('-');
        }
        text.push(*c as char);
    }
    text
}

/// A machine name: 1 to 16 characters, a-z, 0-9 and -, not starting with -.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=16).contains(&b.len())
        && b[0] != b'-'
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A random number as it travels: 32 lowercase hex digits.
pub fn valid_nonce(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        text.push(DIGITS[(b >> 4) as usize] as char);
        text.push(DIGITS[(b & 15) as usize] as char);
    }
    text
}

/// Compare without stopping at the first difference, so the time taken
/// does not tell how much of a guess was right.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Proof of knowing the key: `who` is "os" or "hive", then the two random
/// numbers, the prover's own second.
pub fn proof(key: &Key, who: &str, first: &str, second: &str) -> String {
    hex(&hmac(key, format!("{who} {first} {second}").as_bytes()))
}

/// The key for the lines of one connection.
pub fn session_key(key: &Key, a: &str, b: &str) -> [u8; 32] {
    hmac(key, format!("session {a} {b}").as_bytes())
}

/// Line number `n` of a direction ("o" from the OS, "h" from beehive).
pub fn seal(session: &[u8; 32], direction: &str, n: u64, text: &str) -> String {
    let mac = hmac(session, format!("{direction} {n} {text}").as_bytes());
    format!("{} {n} {text}", hex(&mac[..16]))
}

/// The JSON text of line number `n`, or `None` when the line is not genuine:
/// changed, from the other direction, out of turn, or not sealed at all.
pub fn unseal<'a>(session: &[u8; 32], direction: &str, n: u64, line: &'a str) -> Option<&'a str> {
    if line.len() > MAX_LINE {
        return None;
    }
    let (_, rest) = line.split_once(' ')?;
    let (number, text) = rest.split_once(' ')?;
    if number.parse::<u64>().ok()? != n {
        return None;
    }
    same(seal(session, direction, n, text).as_bytes(), line.as_bytes()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "00112233445566778899aabbccddeeff";
    const B: &str = "ffeeddccbbaa99887766554433221100";

    #[test]
    fn keys_and_names() {
        let key = key_bytes("ABCDE-fghij-kmnpq-rstuv").unwrap();
        assert_eq!(&key, b"abcdefghijkmnpqrstuv");
        assert_eq!(key_bytes("abcdefghijkmnpqrstuv"), Some(key));
        assert_eq!(key_text(&key), "abcde-fghij-kmnpq-rstuv");
        for bad in ["", "abcde", "abcde-fghij-klmno-pqrst", "abcde-fghij-kmnpq-rstu", "abcde-fghij-kmnpq-rstuvw",
                    "abcde fghij kmnpq rstuv", "abcde-fghij-kmnpq-rstu0", "abcde-fghij-kmnpq-rstué"] {
            assert_eq!(key_bytes(bad), None, "{bad}");
        }
        assert!(valid_name("hp") && valid_name("a") && valid_name("nuc-2") && valid_name("0123456789abcdef"));
        for bad in ["", "-hp", "HP", "h p", "0123456789abcdefg", "hé", "a\"b", "a\nb"] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_nonce(A) && valid_nonce(B));
        for bad in ["", "0011", "00112233445566778899AABBCCDDEEFF", "00112233445566778899aabbccddeef ", "00112233445566778899aabbccddeeff0"] {
            assert!(!valid_nonce(bad), "{bad}");
        }
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"abc", b"ab") && same(b"", b""));
    }

    /// The same values are in tests/test_beehive.py: a change on one side fails a test.
    #[test]
    fn agrees_with_beehive() {
        let key = key_bytes("abcde-fghij-kmnpq-rstuv").unwrap();
        assert_eq!(proof(&key, "os", A, B), "c6a12e0fb9bfc1849ceef735bb0e27218e7150f4366bd88dab638b76d0ccb894");
        assert_eq!(proof(&key, "hive", B, A), "d97922ca4948a5f094f7f3b6da1f0061ef6ce2b77e4e1b5ede14b0946df87b85");
        let session = session_key(&key, A, B);
        assert_eq!(hex(&session), "226f0e56f7988da09c521f72d137c85112110a73b3671ed26bdf2f0127f8bb3b");
        assert_eq!(seal(&session, "o", 1, r#"{"type":"ping"}"#), r#"69b8381eba22110a9a2cb3cef051f20d 1 {"type":"ping"}"#);
        let line = seal(&session, "h", 7, r#"{"type":"line","id":3,"text":"héllo"}"#);
        assert_eq!(line, r#"8c430b21a0d327d623ac6ef6d8369495 7 {"type":"line","id":3,"text":"héllo"}"#);
        assert_eq!(unseal(&session, "h", 7, &line), Some(r#"{"type":"line","id":3,"text":"héllo"}"#));
    }

    #[test]
    fn only_genuine_lines_pass() {
        let key = key_bytes("abcde-fghij-kmnpq-rstuv").unwrap();
        let session = session_key(&key, A, B);
        let text = r#"{"type":"signal","kind":"wish","text":"a banner"}"#;
        let line = seal(&session, "o", 5, text);
        assert_eq!(unseal(&session, "o", 5, &line), Some(text));
        // The other direction, another place in the row, another connection, another key.
        assert_eq!(unseal(&session, "h", 5, &line), None);
        assert_eq!(unseal(&session, "o", 4, &line), None);
        assert_eq!(unseal(&session, "o", 6, &line), None);
        assert_eq!(unseal(&session_key(&key, B, A), "o", 5, &line), None);
        assert_eq!(unseal(&session_key(&key_bytes("abcde-fghij-kmnpq-rstuw").unwrap(), A, B), "o", 5, &line), None);
        // Changed text, a changed number, a changed hash.
        assert_eq!(unseal(&session, "o", 5, &line.replace("banner", "bannor")), None);
        assert_eq!(unseal(&session, "o", 6, &line.replace(" 5 ", " 6 ")), None);
        let mut flipped = line.clone().into_bytes();
        flipped[0] = if flipped[0] == b'0' { b'1' } else { b'0' };
        assert_eq!(unseal(&session, "o", 5, core::str::from_utf8(&flipped).unwrap()), None);
        // Cut short anywhere, or not sealed at all.
        for len in 0..line.len() {
            if line.is_char_boundary(len) {
                assert_eq!(unseal(&session, "o", 5, &line[..len]), None, "cut at {len}");
            }
        }
        for junk in ["", " ", "  ", text, "5", "x 5", "x 5 ", "x five y", "x 99999999999999999999999 y", "x -5 y", "x +5 y"] {
            assert_eq!(unseal(&session, "o", 5, junk), None, "{junk}");
        }
        // A number written differently is another line, even if it means the same.
        let padded = line.replace(" 5 ", " 05 ");
        assert_eq!(unseal(&session, "o", 5, &padded), None);
        // Too long.
        let long = seal(&session, "o", 5, &"x".repeat(MAX_LINE));
        assert_eq!(unseal(&session, "o", 5, &long), None);
        // The empty text and text with spaces are fine.
        assert_eq!(unseal(&session, "o", 1, &seal(&session, "o", 1, "")), Some(""));
        assert_eq!(unseal(&session, "o", 1, &seal(&session, "o", 1, "a b  c ")), Some("a b  c "));
    }

    #[test]
    fn the_proof_depends_on_everything() {
        let key = key_bytes("abcde-fghij-kmnpq-rstuv").unwrap();
        let other = key_bytes("abcde-fghij-kmnpq-rstuw").unwrap();
        let p = proof(&key, "os", A, B);
        assert_eq!(p.len(), 64);
        assert_ne!(p, proof(&other, "os", A, B));
        assert_ne!(p, proof(&key, "hive", A, B)); // the answer cannot be the question sent back
        assert_ne!(p, proof(&key, "os", B, A));
        assert_ne!(session_key(&key, A, B), session_key(&key, B, A));
        assert_ne!(session_key(&key, A, B), session_key(&other, A, B));
    }
}
