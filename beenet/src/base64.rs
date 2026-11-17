//! Base64 (RFC 4648, with padding): binary data inside the text lines of the
//! beehive link (a kernel being downloaded, a screenshot being sent).

use alloc::string::String;
use alloc::vec::Vec;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

fn value(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    } as u32)
}

/// `None` for anything that is not well-formed: a wrong length, a character
/// outside the alphabet, padding in the wrong place.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, group) in bytes.chunks(4).enumerate() {
        let last = i + 1 == bytes.len() / 4;
        let padding = group.iter().rev().take_while(|c| **c == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &group[..4 - padding] {
            n = n << 6 | value(c)?;
        }
        n <<= 6 * padding as u32;
        out.push((n >> 16) as u8);
        if padding < 2 {
            out.push((n >> 8) as u8);
        }
        if padding < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn published_values() {
        for (plain, coded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded), Some(plain.as_bytes().to_vec()));
        }
        assert_eq!(encode(&[0xFB, 0xFF, 0xFE]), "+//+");
        assert_eq!(decode("+//+"), Some(vec![0xFB, 0xFF, 0xFE]));
    }

    #[test]
    fn every_length_goes_there_and_back() {
        let data: Vec<u8> = (0..700u32).map(|i| (i * 37 + 11) as u8).collect();
        for len in 0..data.len() {
            assert_eq!(decode(&encode(&data[..len])).as_deref(), Some(&data[..len]));
        }
    }

    #[test]
    fn nonsense_is_refused() {
        for bad in ["Z", "Zg", "Zg=", "Zm9", "Zm9v=", "=Zm9", "Zg==Zm9v", "Zm9v Zm9v", "Zm9!", "Z===", "====", "Zm\n9v", "Zmé="] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }
}
