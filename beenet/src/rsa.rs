//! RSA signature checks (RFC 8017): PKCS#1 v1.5 (what certificates are
//! signed with) and PSS (what TLS 1.3 servers sign the handshake with),
//! over SHA-256, SHA-384 and SHA-512. Only public-key operations: no
//! secrets here, so nothing needs to run in constant time.
//!
//! The numbers are 64-bit limbs, least significant first; `s^e mod n` uses
//! schoolbook products reduced bit by bit, which is slow in theory and
//! fast enough for the few signatures of a handshake (e is 65537).

use crate::sha256::sha256;
use crate::sha512::{sha384, sha512};
use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha256 => sha256(data).to_vec(),
            Hash::Sha384 => sha384(data),
            Hash::Sha512 => sha512(data),
        }
    }

    pub fn len(self) -> usize {
        match self {
            Hash::Sha256 => 32,
            Hash::Sha384 => 48,
            Hash::Sha512 => 64,
        }
    }

    /// The DER DigestInfo that PKCS#1 v1.5 puts before the hash.
    fn digest_info(self) -> &'static [u8] {
        match self {
            Hash::Sha256 => &[0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20],
            Hash::Sha384 => &[0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30],
            Hash::Sha512 => &[0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40],
        }
    }
}

/// A big number: limbs, least significant first, no trailing zero limbs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Big(Vec<u64>);

impl Big {
    fn from_be(bytes: &[u8]) -> Big {
        let mut limbs = Vec::with_capacity(bytes.len() / 8 + 1);
        for chunk in bytes.rchunks(8) {
            let mut b = [0u8; 8];
            b[8 - chunk.len()..].copy_from_slice(chunk);
            limbs.push(u64::from_be_bytes(b));
        }
        Big(limbs).trimmed()
    }

    fn trimmed(mut self) -> Big {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
        self
    }

    fn bits(&self) -> usize {
        self.0.last().map_or(0, |top| self.0.len() * 64 - top.leading_zeros() as usize)
    }

    fn bit(&self, i: usize) -> bool {
        self.0.get(i / 64).is_some_and(|l| l >> (i % 64) & 1 == 1)
    }

    /// Big-endian, left-padded to `len` bytes (None if it does not fit).
    fn to_be(&self, len: usize) -> Option<Vec<u8>> {
        if self.bits().div_ceil(8) > len {
            return None;
        }
        let mut out = vec![0u8; len];
        for (i, limb) in self.0.iter().enumerate() {
            for (k, b) in limb.to_le_bytes().iter().enumerate() {
                let at = i * 8 + k;
                if at < len {
                    out[len - 1 - at] = *b;
                }
            }
        }
        Some(out)
    }

    fn mul(&self, other: &Big) -> Big {
        let mut r = vec![0u64; self.0.len() + other.0.len()];
        for (i, &a) in self.0.iter().enumerate() {
            let mut carry = 0u128;
            for (j, &b) in other.0.iter().enumerate() {
                let t = a as u128 * b as u128 + r[i + j] as u128 + carry;
                r[i + j] = t as u64;
                carry = t >> 64;
            }
            r[i + other.0.len()] = carry as u64;
        }
        Big(r).trimmed()
    }

    /// self mod n, by shifting the bits of self in one at a time.
    fn rem(&self, n: &Big) -> Big {
        let len = n.0.len() + 1;
        let mut r = vec![0u64; len];
        for i in (0..self.bits()).rev() {
            // r = r * 2 + bit
            let mut carry = self.bit(i) as u64;
            for limb in r.iter_mut() {
                let next = *limb >> 63;
                *limb = *limb << 1 | carry;
                carry = next;
            }
            if !less(&r, &n.0) {
                sub_in_place(&mut r, &n.0);
            }
        }
        Big(r).trimmed()
    }

    fn pow_mod(&self, e: &Big, n: &Big) -> Big {
        let base = self.rem(n);
        let mut r = Big(vec![1]);
        for i in (0..e.bits()).rev() {
            r = r.mul(&r).rem(n);
            if e.bit(i) {
                r = r.mul(&base).rem(n);
            }
        }
        r
    }
}

/// a < b (a may have more limbs, which are then compared too).
fn less(a: &[u64], b: &[u64]) -> bool {
    for i in (0..a.len().max(b.len())).rev() {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

fn sub_in_place(a: &mut [u64], b: &[u64]) {
    let mut borrow = 0u64;
    for i in 0..a.len() {
        let y = b.get(i).copied().unwrap_or(0);
        let (d1, o1) = a[i].overflowing_sub(y);
        let (d2, o2) = d1.overflowing_sub(borrow);
        a[i] = d2;
        borrow = (o1 || o2) as u64;
    }
}

/// An RSA public key: modulus and exponent, big-endian bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub n: Vec<u8>,
    pub e: Vec<u8>,
}

impl PublicKey {
    /// Size of the modulus in bits.
    pub fn bits(&self) -> usize {
        Big::from_be(&self.n).bits()
    }

    /// signature^e mod n as `len` bytes; None if the signature is out of range.
    fn open(&self, signature: &[u8], len: usize) -> Option<Vec<u8>> {
        let n = Big::from_be(&self.n);
        let e = Big::from_be(&self.e);
        let s = Big::from_be(signature);
        // Keys from 1024 to 8192 bits; e odd and small enough to be quick.
        if !(1024..=8192).contains(&n.bits()) || e.bits() > 33 || e.bits() < 2 || !less(&s.0, &n.0) {
            return None;
        }
        s.pow_mod(&e, &n).to_be(len)
    }

    /// PKCS#1 v1.5: the signature of a message whose hash is `hash`.
    pub fn verify_pkcs1(&self, hash: Hash, digest: &[u8], signature: &[u8]) -> bool {
        let k = self.bits().div_ceil(8);
        if signature.len() != k || digest.len() != hash.len() {
            return false;
        }
        let Some(em) = self.open(signature, k) else { return false };
        let info = hash.digest_info();
        let pad = k.saturating_sub(3 + info.len() + digest.len());
        if pad < 8 {
            return false;
        }
        let mut expected = vec![0x00, 0x01];
        expected.resize(2 + pad, 0xFF);
        expected.push(0x00);
        expected.extend_from_slice(info);
        expected.extend_from_slice(digest);
        expected == em
    }

    /// PSS with MGF1 of the same hash; any salt length (TLS uses the hash's).
    pub fn verify_pss(&self, hash: Hash, digest: &[u8], signature: &[u8]) -> bool {
        let mod_bits = self.bits();
        let k = mod_bits.div_ceil(8);
        let em_bits = mod_bits - 1;
        let em_len = em_bits.div_ceil(8);
        let h_len = hash.len();
        if signature.len() != k || digest.len() != h_len || em_len < h_len + 2 {
            return false;
        }
        let Some(full) = self.open(signature, k) else { return false };
        // When em_len < k the extra first byte must be zero.
        if full[..k - em_len].iter().any(|b| *b != 0) {
            return false;
        }
        let em = &full[k - em_len..];
        if em[em_len - 1] != 0xBC {
            return false;
        }
        let (masked, h) = (&em[..em_len - h_len - 1], &em[em_len - h_len - 1..em_len - 1]);
        let top_bits = 8 * em_len - em_bits;
        if masked[0] >> (8 - top_bits) != 0 {
            return false;
        }
        let mask = mgf1(hash, h, masked.len());
        let mut db: Vec<u8> = masked.iter().zip(&mask).map(|(a, b)| a ^ b).collect();
        db[0] &= 0xFF >> top_bits;
        // DB = zeros || 0x01 || salt
        let Some(one) = db.iter().position(|b| *b != 0) else { return false };
        if db[one] != 0x01 {
            return false;
        }
        let salt = &db[one + 1..];
        let mut m = vec![0u8; 8];
        m.extend_from_slice(digest);
        m.extend_from_slice(salt);
        hash.digest(&m) == h
    }
}

fn mgf1(hash: Hash, seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + hash.len());
    let mut counter = 0u32;
    while out.len() < len {
        let mut input = seed.to_vec();
        input.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&hash.digest(&input));
        counter += 1;
    }
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let text = include_str!("../testdata/rsa-openssl.txt");
        let hex = text.lines().find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(' '))).unwrap();
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn numbers() {
        let a = Big::from_be(&[0x01, 0, 0, 0, 0, 0, 0, 0, 0x05]);
        assert_eq!(a.0, vec![5, 1]);
        assert_eq!(a.to_be(9).unwrap(), vec![1, 0, 0, 0, 0, 0, 0, 0, 5]);
        assert_eq!(a.to_be(8), None);
        // 3^200 mod 1000003, checked with Python's pow.
        let r = Big(vec![3]).pow_mod(&Big(vec![200]), &Big(vec![1_000_003]));
        assert_eq!(r.0, vec![333_986]);
        let big = Big::from_be(&[0xFF; 40]);
        let n = Big::from_be(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33]);
        assert!(less(&big.rem(&n).0, &n.0));
    }

    #[test]
    fn openssl_signatures() {
        let key = PublicKey { n: fixture("n"), e: fixture("e") };
        assert_eq!(key.bits(), 2048);
        let msg = fixture("message");
        let d256 = Hash::Sha256.digest(&msg);
        let d384 = Hash::Sha384.digest(&msg);
        let d512 = Hash::Sha512.digest(&msg);
        assert!(key.verify_pkcs1(Hash::Sha256, &d256, &fixture("p1-256")));
        assert!(key.verify_pkcs1(Hash::Sha384, &d384, &fixture("p1-384")));
        assert!(key.verify_pss(Hash::Sha256, &d256, &fixture("pss-256")));
        assert!(key.verify_pss(Hash::Sha512, &d512, &fixture("pss-512")));
        assert!(key.verify_pss(Hash::Sha256, &d256, &fixture("pss-256-s20")));
        // The wrong hash, the wrong scheme, a changed message or signature: refused.
        assert!(!key.verify_pkcs1(Hash::Sha384, &d384, &fixture("p1-256")));
        assert!(!key.verify_pss(Hash::Sha256, &d256, &fixture("p1-256")));
        assert!(!key.verify_pkcs1(Hash::Sha256, &d256, &fixture("pss-256")));
        let other = Hash::Sha256.digest(b"The wasps are in the hive.");
        assert!(!key.verify_pkcs1(Hash::Sha256, &other, &fixture("p1-256")));
        assert!(!key.verify_pss(Hash::Sha256, &other, &fixture("pss-256")));
        for i in [0, 100, 255] {
            let mut bad = fixture("pss-256");
            bad[i] ^= 1;
            assert!(!key.verify_pss(Hash::Sha256, &d256, &bad));
            let mut bad = fixture("p1-256");
            bad[i] ^= 1;
            assert!(!key.verify_pkcs1(Hash::Sha256, &d256, &bad));
        }
        // A signature as large as the modulus, or of the wrong length.
        assert!(!key.verify_pkcs1(Hash::Sha256, &d256, &key.n));
        assert!(!key.verify_pkcs1(Hash::Sha256, &d256, &fixture("p1-256")[1..]));
    }
}
