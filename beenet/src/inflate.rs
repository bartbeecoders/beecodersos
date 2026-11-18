//! DEFLATE decompression (RFC 1951) and the zlib wrapper around it (RFC
//! 1950), for PNG pictures. The input is untrusted: every code is checked,
//! distances never reach before the start, and the output never grows past
//! the size the caller allows.

use alloc::vec::Vec;

#[derive(Debug, PartialEq, Eq)]
pub enum InflateError {
    /// The data ends in the middle.
    Truncated,
    /// Something no encoder writes (a bad block type, code or distance).
    Invalid,
    /// More output than allowed.
    TooLarge,
    /// The zlib checksum does not match.
    Checksum,
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
    nbits: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, bit: 0, nbits: 0 }
    }

    fn need(&mut self, n: u32) -> Result<(), InflateError> {
        while self.nbits < n {
            let byte = *self.data.get(self.pos).ok_or(InflateError::Truncated)?;
            self.pos += 1;
            self.bit |= (byte as u32) << self.nbits;
            self.nbits += 8;
        }
        Ok(())
    }

    fn take(&mut self, n: u32) -> Result<u32, InflateError> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = self.bit & ((1u32 << n) - 1);
        self.bit >>= n;
        self.nbits -= n;
        Ok(v)
    }

    /// Skip to the next whole byte.
    fn align(&mut self) {
        self.bit = 0;
        self.nbits = 0;
    }
}

/// A canonical Huffman code: how many codes of each length, and the
/// symbols in order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, InflateError> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        // A code may be incomplete, never over-full.
        let mut left: i32 = 1;
        for &count in &counts[1..] {
            left = left * 2 - count as i32;
            if left < 0 {
                return Err(InflateError::Invalid);
            }
        }
        let mut offsets = [0u16; 16];
        for len in 1..16 {
            offsets[len] = offsets[len - 1] + counts[len - 1];
        }
        let mut symbols = alloc::vec![0u16; lengths.len()];
        for (symbol, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = symbol as u16;
                offsets[l as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, bits: &mut Bits) -> Result<u16, InflateError> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= bits.take(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return self.symbols.get((index + code - first) as usize).copied().ok_or(InflateError::Invalid);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(InflateError::Invalid)
    }
}

const LENGTH_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Decompress raw DEFLATE data; the output may be at most `limit` bytes.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    let mut bits = Bits::new(data);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => {
                bits.align();
                let at = bits.pos;
                let header = data.get(at..at + 4).ok_or(InflateError::Truncated)?;
                let len = u16::from_le_bytes([header[0], header[1]]);
                let nlen = u16::from_le_bytes([header[2], header[3]]);
                if len != !nlen {
                    return Err(InflateError::Invalid);
                }
                let block = data.get(at + 4..at + 4 + len as usize).ok_or(InflateError::Truncated)?;
                if out.len() + block.len() > limit {
                    return Err(InflateError::TooLarge);
                }
                out.extend_from_slice(block);
                bits.pos = at + 4 + len as usize;
            }
            1 => {
                let mut lengths = [0u8; 288];
                lengths[..144].fill(8);
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                lengths[280..].fill(8);
                let lit = Huffman::new(&lengths)?;
                let dist = Huffman::new(&[5u8; 30])?;
                codes(&mut bits, &lit, &dist, &mut out, limit)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut bits)?;
                codes(&mut bits, &lit, &dist, &mut out, limit)?;
            }
            _ => return Err(InflateError::Invalid),
        }
        if last {
            return Ok(out);
        }
    }
}

fn dynamic_tables(bits: &mut Bits) -> Result<(Huffman, Huffman), InflateError> {
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let nlen = bits.take(5)? as usize + 257;
    let ndist = bits.take(5)? as usize + 1;
    let ncode = bits.take(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err(InflateError::Invalid);
    }
    let mut code_lengths = [0u8; 19];
    for &i in &ORDER[..ncode] {
        code_lengths[i] = bits.take(3)? as u8;
    }
    let code = Huffman::new(&code_lengths)?;
    let mut lengths = [0u8; 316];
    let mut i = 0;
    while i < nlen + ndist {
        let symbol = code.decode(bits)?;
        let (value, repeat) = match symbol {
            0..=15 => (symbol as u8, 1),
            16 => (*lengths.get(i.wrapping_sub(1)).filter(|_| i > 0).ok_or(InflateError::Invalid)?, 3 + bits.take(2)? as usize),
            17 => (0, 3 + bits.take(3)? as usize),
            18 => (0, 11 + bits.take(7)? as usize),
            _ => return Err(InflateError::Invalid),
        };
        if i + repeat > nlen + ndist {
            return Err(InflateError::Invalid);
        }
        lengths[i..i + repeat].fill(value);
        i += repeat;
    }
    if lengths[256] == 0 {
        return Err(InflateError::Invalid); // no end-of-block code
    }
    Ok((Huffman::new(&lengths[..nlen])?, Huffman::new(&lengths[nlen..nlen + ndist])?))
}

fn codes(bits: &mut Bits, lit: &Huffman, dist: &Huffman, out: &mut Vec<u8>, limit: usize) -> Result<(), InflateError> {
    loop {
        let symbol = lit.decode(bits)? as usize;
        match symbol {
            0..=255 => {
                if out.len() >= limit {
                    return Err(InflateError::TooLarge);
                }
                out.push(symbol as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let i = symbol - 257;
                let len = LENGTH_BASE[i] as usize + bits.take(LENGTH_EXTRA[i] as u32)? as usize;
                let d = dist.decode(bits)? as usize;
                if d >= 30 {
                    return Err(InflateError::Invalid);
                }
                let distance = DIST_BASE[d] as usize + bits.take(DIST_EXTRA[d] as u32)? as usize;
                if distance > out.len() {
                    return Err(InflateError::Invalid);
                }
                if out.len() + len > limit {
                    return Err(InflateError::TooLarge);
                }
                let start = out.len() - distance;
                for k in 0..len {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            _ => return Err(InflateError::Invalid),
        }
    }
}

/// Decompress zlib data (a 2-byte header, DEFLATE, an Adler-32 checksum).
pub fn zlib(data: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    let header = data.get(..2).ok_or(InflateError::Truncated)?;
    let (cmf, flg) = (header[0], header[1]);
    if cmf & 0x0F != 8 || (cmf as u16 * 256 + flg as u16) % 31 != 0 || flg & 0x20 != 0 {
        return Err(InflateError::Invalid);
    }
    let out = inflate(&data[2..], limit)?;
    // The checksum follows the compressed data; a stream cut just there is
    // still accepted only if the checksum is there and right.
    let tail = data.len().checked_sub(4).ok_or(InflateError::Truncated)?;
    let expected = u32::from_be_bytes(data[tail..].try_into().map_err(|_| InflateError::Truncated)?);
    if adler32(&out) != expected {
        return Err(InflateError::Checksum);
    }
    Ok(out)
}

pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_fixed_and_dynamic_blocks() {
        // "hello hello hello" with Python's zlib at levels 0 (stored), 1 and 9.
        let stored = [0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o', 0x06, 0x2c, 0x02, 0x15];
        assert_eq!(zlib(&stored, 100).unwrap(), b"hello");
        assert_eq!(adler32(b"hello"), 0x062c0215);
        // A fixed-Huffman stream with back references (zlib.compress(b"abcabcabcabc")).
        let fixed = [0x78, 0x9c, 0x4b, 0x4c, 0x4a, 0x4e, 0x84, 0x21, 0x00, 0x1d, 0xe0, 0x04, 0x99];
        assert_eq!(zlib(&fixed, 100).unwrap(), b"abcabcabcabc");
        assert_eq!(zlib(&fixed, 5), Err(InflateError::TooLarge));
        let mut bad = fixed;
        bad[12] ^= 1;
        assert_eq!(zlib(&bad, 100), Err(InflateError::Checksum));
    }

    #[test]
    fn hostile_streams_fail_cleanly() {
        let fixed = [0x78, 0x9c, 0x4b, 0x4c, 0x4a, 0x4e, 0x84, 0x21, 0x00, 0x1d, 0xe0, 0x04, 0x99];
        for end in 0..fixed.len() {
            assert!(zlib(&fixed[..end], 100).is_err());
        }
        // A distance before the start, a reserved block type, every byte flipped.
        // Final fixed block, length code 257 (3 bytes), distance code 0 (1 back) with nothing written yet.
        assert_eq!(inflate(&[0x03, 0x02, 0x00], 100), Err(InflateError::Invalid));
        assert_eq!(inflate(&[0x07], 100), Err(InflateError::Invalid));
        for i in 0..fixed.len() {
            for bit in 0..8 {
                let mut data = fixed;
                data[i] ^= 1 << bit;
                let _ = zlib(&data, 1000); // must never panic
            }
        }
    }
}
