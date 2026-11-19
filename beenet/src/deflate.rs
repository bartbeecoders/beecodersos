//! DEFLATE compression (RFC 1951), for the PNG files the OS writes.
//!
//! LZ77 with a hash of the next three bytes and short chains of earlier
//! places (a 32 KiB window), coded with the fixed Huffman codes: simple,
//! fast, and plenty for screens with large flat areas. A piece of data
//! can be compressed on its own and the pieces put one after the other:
//! each ends on a whole byte (with an empty stored block, a "sync flush"),
//! and none refers back into another, so the pieces of one picture can be
//! compressed on different processors.

use alloc::vec;
use alloc::vec::Vec;

const WINDOW: usize = 32 * 1024;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
/// How many earlier places with the same three bytes are tried.
const CHAIN: usize = 24;
const HASH_BITS: u32 = 15;

struct BitWriter {
    out: Vec<u8>,
    bits: u64,
    nbits: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: u32) {
        self.bits |= (value as u64) << self.nbits;
        self.nbits += n;
        while self.nbits >= 8 {
            self.out.push(self.bits as u8);
            self.bits >>= 8;
            self.nbits -= 8;
        }
    }

    /// A Huffman code: written most significant bit first.
    fn code(&mut self, code: u32, n: u32) {
        self.put(code.reverse_bits() >> (32 - n), n);
    }

    fn align(&mut self) {
        if self.nbits > 0 {
            self.put(0, 8 - self.nbits);
        }
    }
}

/// The fixed code of a literal or length symbol (0-287).
fn fixed_literal(w: &mut BitWriter, symbol: u32) {
    match symbol {
        0..=143 => w.code(0x30 + symbol, 8),
        144..=255 => w.code(0x190 + symbol - 144, 9),
        256..=279 => w.code(symbol - 256, 7),
        _ => w.code(0xC0 + symbol - 280, 8),
    }
}

const LENGTH_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

fn put_match(w: &mut BitWriter, len: usize, dist: usize) {
    let l = LENGTH_BASE.iter().rposition(|&b| b as usize <= len).unwrap_or(0);
    fixed_literal(w, 257 + l as u32);
    w.put((len - LENGTH_BASE[l] as usize) as u32, LENGTH_EXTRA[l] as u32);
    let d = DIST_BASE.iter().rposition(|&b| b as usize <= dist).unwrap_or(0);
    w.code(d as u32, 5);
    w.put((dist - DIST_BASE[d] as usize) as u32, DIST_EXTRA[d] as u32);
}

fn hash(data: &[u8], i: usize) -> usize {
    let v = (data[i] as u32) << 16 | (data[i + 1] as u32) << 8 | data[i + 2] as u32;
    (v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

/// Compress `data` as one piece: fixed-Huffman blocks, then either the
/// final mark (`last`) or a sync flush; it always ends on a whole byte.
pub fn deflate_piece(data: &[u8], last: bool) -> Vec<u8> {
    let mut w = BitWriter { out: Vec::with_capacity(data.len() / 4 + 64), bits: 0, nbits: 0 };
    // One block for the whole piece (a block may be any length).
    w.put(last as u32, 1);
    w.put(1, 2); // fixed Huffman codes
    let mut head = vec![u32::MAX; 1 << HASH_BITS];
    let mut prev = vec![u32::MAX; WINDOW];
    let mut i = 0;
    let insert = |head: &mut Vec<u32>, prev: &mut Vec<u32>, at: usize| {
        if at + MIN_MATCH <= data.len() {
            let h = hash(data, at);
            prev[at % WINDOW] = head[h];
            head[h] = at as u32;
        }
    };
    while i < data.len() {
        let mut best = (0usize, 0usize);
        if i + MIN_MATCH <= data.len() {
            let mut candidate = head[hash(data, i)];
            let limit = (data.len() - i).min(MAX_MATCH);
            for _ in 0..CHAIN {
                if candidate == u32::MAX {
                    break;
                }
                let c = candidate as usize;
                if i - c > WINDOW - 1 || c >= i {
                    break;
                }
                if data[c + best.0.min(limit - 1)] == data[i + best.0.min(limit - 1)] {
                    let len = data[c..c + limit].iter().zip(&data[i..i + limit]).take_while(|(a, b)| a == b).count();
                    if len > best.0 {
                        best = (len, i - c);
                        if len == limit {
                            break;
                        }
                    }
                }
                let next = prev[c % WINDOW];
                if next == u32::MAX || next as usize >= c {
                    break;
                }
                candidate = next;
            }
        }
        if best.0 >= MIN_MATCH {
            put_match(&mut w, best.0, best.1);
            for k in i..i + best.0 {
                insert(&mut head, &mut prev, k);
            }
            i += best.0;
        } else {
            fixed_literal(&mut w, data[i] as u32);
            insert(&mut head, &mut prev, i);
            i += 1;
        }
    }
    fixed_literal(&mut w, 256); // end of block
    if !last {
        // Sync flush: an empty stored block brings the stream to a whole byte.
        w.put(0, 1);
        w.put(0, 2);
        w.align();
        w.out.extend_from_slice(&[0, 0, 0xFF, 0xFF]);
    }
    w.align();
    w.out
}

/// Compress `data` (raw DEFLATE, one piece).
pub fn deflate(data: &[u8]) -> Vec<u8> {
    deflate_piece(data, true)
}

/// A zlib stream of data compressed in pieces (`pieces` in order, the
/// last one made with `last`), with the checksum of the whole.
pub fn zlib_from_pieces(pieces: &[Vec<u8>], adler: u32) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for p in pieces {
        out.extend_from_slice(p);
    }
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflate::{adler32, inflate, zlib};

    fn samples() -> Vec<Vec<u8>> {
        let mut seed = 7u32;
        let mut random = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        vec![
            vec![],
            b"a".to_vec(),
            b"abcabcabcabcabcabc".to_vec(),
            vec![0; 100_000],
            (0..100_000).map(|_| random()).collect(),
            (0..100_000).map(|_| random() & 7).collect(),
            (0..200_000u32).map(|i| (i % 251 ^ i / 777) as u8).collect(),
        ]
    }

    #[test]
    fn round_trips() {
        for data in samples() {
            let packed = deflate(&data);
            assert_eq!(inflate(&packed, data.len()).unwrap(), data);
        }
        // Flat data shrinks a lot.
        assert!(deflate(&[0u8; 100_000]).len() < 1000);
        // A picture-like run of lines: about as well as zlib's level 1 would.
        let lines: Vec<u8> = (0..300u32).flat_map(|y| (0..900u32).map(move |x| if (x / 30 + y / 20) % 2 == 0 { 0 } else { (x % 256) as u8 })).collect();
        assert!(deflate(&lines).len() < lines.len() / 10);
    }

    #[test]
    fn pieces_join() {
        for data in samples() {
            for n in [1, 2, 3, 7] {
                let size = data.len().div_ceil(n).max(1);
                let chunks: Vec<&[u8]> = if data.is_empty() { vec![&data[..]] } else { data.chunks(size).collect() };
                let last = chunks.len() - 1;
                let pieces: Vec<Vec<u8>> = chunks.iter().enumerate().map(|(i, c)| deflate_piece(c, i == last)).collect();
                let stream = zlib_from_pieces(&pieces, adler32(&data));
                assert_eq!(zlib(&stream, data.len()).unwrap(), data, "{} bytes in {n} pieces", data.len());
            }
        }
    }
}
