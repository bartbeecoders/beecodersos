//! ECDSA signature checks (FIPS 186-4) on the curves P-256 and P-384, which
//! nearly every ECDSA certificate uses. Only public-key work: the values
//! are public, so nothing here needs to run in constant time.
//!
//! Numbers mod p (and mod the group order n) are 64-bit limbs in Montgomery
//! form; points are Jacobian (X, Y, Z) with the curves' a = -3. Checking
//! adds u1*G and u2*Q together in one pass over the bits (Shamir's trick).

use alloc::vec;
use alloc::vec::Vec;

type Limbs = Vec<u64>;

/// Arithmetic mod an odd number `m` of `m.len()` limbs, in Montgomery form.
struct Field {
    m: Limbs,
    /// -m^-1 mod 2^64
    m_inv: u64,
    /// R^2 mod m (R = 2^(64 * limbs))
    r2: Limbs,
}

fn from_be(bytes: &[u8], limbs: usize) -> Option<Limbs> {
    if bytes.len() > limbs * 8 {
        return None;
    }
    let mut out = vec![0u64; limbs];
    for (i, b) in bytes.iter().rev().enumerate() {
        out[i / 8] |= (*b as u64) << (8 * (i % 8));
    }
    Some(out)
}

fn hex(s: &str, limbs: usize) -> Limbs {
    let bytes: Vec<u8> = (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0)).collect();
    from_be(&bytes, limbs).unwrap_or_default()
}

fn less(a: &[u64], b: &[u64]) -> bool {
    for i in (0..a.len().max(b.len())).rev() {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

fn is_zero(a: &[u64]) -> bool {
    a.iter().all(|l| *l == 0)
}

/// a -= b over a.len() limbs; the borrow out.
fn sub_in_place(a: &mut [u64], b: &[u64]) -> bool {
    let mut borrow = false;
    for i in 0..a.len() {
        let (d1, o1) = a[i].overflowing_sub(b.get(i).copied().unwrap_or(0));
        let (d2, o2) = d1.overflowing_sub(borrow as u64);
        a[i] = d2;
        borrow = o1 || o2;
    }
    borrow
}

/// a += b; the carry out.
fn add_in_place(a: &mut [u64], b: &[u64]) -> bool {
    let mut carry = false;
    for i in 0..a.len() {
        let (s1, o1) = a[i].overflowing_add(b.get(i).copied().unwrap_or(0));
        let (s2, o2) = s1.overflowing_add(carry as u64);
        a[i] = s2;
        carry = o1 || o2;
    }
    carry
}

impl Field {
    fn new(m: Limbs) -> Field {
        let mut inv = 1u64;
        for _ in 0..6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(m[0].wrapping_mul(inv)));
        }
        // R^2 mod m: 1 doubled 2 * 64 * limbs times, mod m.
        let mut x = vec![0u64; m.len()];
        x[0] = 1;
        let mut f = Field { m_inv: inv.wrapping_neg(), r2: Vec::new(), m };
        for _ in 0..128 * f.m.len() {
            x = f.add(&x, &x);
        }
        f.r2 = x;
        f
    }

    fn add(&self, a: &[u64], b: &[u64]) -> Limbs {
        let mut s = a.to_vec();
        let carry = add_in_place(&mut s, b);
        if carry || !less(&s, &self.m) {
            sub_in_place(&mut s, &self.m);
        }
        s
    }

    fn sub(&self, a: &[u64], b: &[u64]) -> Limbs {
        let mut d = a.to_vec();
        if sub_in_place(&mut d, b) {
            add_in_place(&mut d, &self.m);
        }
        d
    }

    /// a * b / R mod m.
    fn mul(&self, a: &[u64], b: &[u64]) -> Limbs {
        let l = self.m.len();
        let mut t = vec![0u64; l + 2];
        for i in 0..l {
            let mut c = 0u128;
            for j in 0..l {
                let s = t[j] as u128 + a[i] as u128 * b[j] as u128 + c;
                t[j] = s as u64;
                c = s >> 64;
            }
            let s = t[l] as u128 + c;
            t[l] = s as u64;
            t[l + 1] = (s >> 64) as u64;
            let u = t[0].wrapping_mul(self.m_inv);
            let mut c = (t[0] as u128 + u as u128 * self.m[0] as u128) >> 64;
            for j in 1..l {
                let s = t[j] as u128 + u as u128 * self.m[j] as u128 + c;
                t[j - 1] = s as u64;
                c = s >> 64;
            }
            let s = t[l] as u128 + c;
            t[l - 1] = s as u64;
            let s2 = t[l + 1] as u128 + (s >> 64);
            t[l] = s2 as u64;
            t[l + 1] = 0;
        }
        let mut r = t[..l + 1].to_vec();
        if !less(&r, &self.m) {
            sub_in_place(&mut r, &self.m);
        }
        r.truncate(l);
        r
    }

    fn to_mont(&self, a: &[u64]) -> Limbs {
        self.mul(a, &self.r2)
    }

    fn from_mont(&self, a: &[u64]) -> Limbs {
        let mut one = vec![0u64; self.m.len()];
        one[0] = 1;
        self.mul(a, &one)
    }

    fn one(&self) -> Limbs {
        let mut one = vec![0u64; self.m.len()];
        one[0] = 1;
        self.to_mont(&one)
    }

    /// a^(m-2): the inverse of a (m prime), in Montgomery form.
    fn inv(&self, a: &[u64]) -> Limbs {
        let mut e = self.m.clone();
        sub_in_place(&mut e, &[2]);
        let mut r = self.one();
        for i in (0..64 * e.len()).rev() {
            r = self.mul(&r, &r);
            if e[i / 64] >> (i % 64) & 1 == 1 {
                r = self.mul(&r, a);
            }
        }
        r
    }
}

/// A curve y^2 = x^3 - 3x + b over the field mod p, with base point G of order n.
pub struct Curve {
    p: Field,
    n: Field,
    b: Limbs,
    g: (Limbs, Limbs),
    bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurveName {
    P256,
    P384,
}

impl Curve {
    pub fn new(name: CurveName) -> Curve {
        let (l, p, n, b, gx, gy) = match name {
            CurveName::P256 => (
                4,
                "ffffffff00000001000000000000000000000000ffffffffffffffffffffffff",
                "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551",
                "5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b",
                "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
                "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
            ),
            CurveName::P384 => (
                6,
                "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000ffffffff",
                "ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52973",
                "b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875ac656398d8a2ed19d2a85c8edd3ec2aef",
                "aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab7",
                "3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f",
            ),
        };
        let pf = Field::new(hex(p, l));
        let b = pf.to_mont(&hex(b, l));
        let g = (pf.to_mont(&hex(gx, l)), pf.to_mont(&hex(gy, l)));
        Curve { n: Field::new(hex(n, l)), p: pf, b, g, bytes: l * 8 }
    }

    /// A public key point `04 || X || Y`, checked to lie on the curve.
    fn point(&self, encoded: &[u8]) -> Option<(Limbs, Limbs)> {
        let (&tag, xy) = encoded.split_first()?;
        if tag != 4 || xy.len() != 2 * self.bytes {
            return None; // compressed points are not used by certificates
        }
        let l = self.bytes / 8;
        let (x, y) = (from_be(&xy[..self.bytes], l)?, from_be(&xy[self.bytes..], l)?);
        if !less(&x, &self.p.m) || !less(&y, &self.p.m) {
            return None;
        }
        let (x, y) = (self.p.to_mont(&x), self.p.to_mont(&y));
        // y^2 = x^3 - 3x + b
        let f = &self.p;
        let x3 = f.mul(&f.mul(&x, &x), &x);
        let three_x = f.add(&f.add(&x, &x), &x);
        let right = f.add(&f.sub(&x3, &three_x), &self.b);
        (f.mul(&y, &y) == right).then_some((x, y))
    }

    fn double(&self, (x, y, z): &Jac) -> Jac {
        let f = &self.p;
        if is_zero(z) || is_zero(y) {
            return (f.one(), f.one(), vec![0; x.len()]);
        }
        let delta = f.mul(z, z);
        let gamma = f.mul(y, y);
        let beta = f.mul(x, &gamma);
        let t = f.mul(&f.sub(x, &delta), &f.add(x, &delta));
        let alpha = f.add(&f.add(&t, &t), &t);
        let beta4 = f.add(&f.add(&beta, &beta), &f.add(&beta, &beta));
        let x3 = f.sub(&f.mul(&alpha, &alpha), &f.add(&beta4, &beta4));
        let yz = f.add(y, z);
        let z3 = f.sub(&f.sub(&f.mul(&yz, &yz), &gamma), &delta);
        let g2 = f.mul(&gamma, &gamma);
        let g8 = f.add(&f.add(&f.add(&g2, &g2), &f.add(&g2, &g2)), &f.add(&f.add(&g2, &g2), &f.add(&g2, &g2)));
        let y3 = f.sub(&f.mul(&alpha, &f.sub(&beta4, &x3)), &g8);
        (x3, y3, z3)
    }

    fn add(&self, a: &Jac, b: &Jac) -> Jac {
        let f = &self.p;
        let ((x1, y1, z1), (x2, y2, z2)) = (a, b);
        if is_zero(z1) {
            return b.clone();
        }
        if is_zero(z2) {
            return a.clone();
        }
        let z1z1 = f.mul(z1, z1);
        let z2z2 = f.mul(z2, z2);
        let u1 = f.mul(x1, &z2z2);
        let u2 = f.mul(x2, &z1z1);
        let s1 = f.mul(&f.mul(y1, z2), &z2z2);
        let s2 = f.mul(&f.mul(y2, z1), &z1z1);
        let h = f.sub(&u2, &u1);
        let r = f.sub(&s2, &s1);
        if is_zero(&h) {
            return if is_zero(&r) { self.double(a) } else { (f.one(), f.one(), vec![0; x1.len()]) };
        }
        let h2 = f.add(&h, &h);
        let i = f.mul(&h2, &h2);
        let j = f.mul(&h, &i);
        let r = f.add(&r, &r);
        let v = f.mul(&u1, &i);
        let x3 = f.sub(&f.sub(&f.mul(&r, &r), &j), &f.add(&v, &v));
        let s1j = f.mul(&s1, &j);
        let y3 = f.sub(&f.mul(&r, &f.sub(&v, &x3)), &f.add(&s1j, &s1j));
        let zz = f.add(z1, z2);
        let z3 = f.mul(&f.sub(&f.sub(&f.mul(&zz, &zz), &z1z1), &z2z2), &h);
        (x3, y3, z3)
    }

    /// Is `signature` (DER: r and s) by the key `public` (04 || X || Y) of
    /// a message whose hash is `digest`?
    pub fn verify(&self, public: &[u8], digest: &[u8], signature: &[u8]) -> bool {
        self.check(public, digest, signature).unwrap_or(false)
    }

    fn check(&self, public: &[u8], digest: &[u8], signature: &[u8]) -> Option<bool> {
        let l = self.bytes / 8;
        let q = self.point(public)?;
        let (r, s) = der_signature(signature)?;
        let (r, s) = (from_be(r, l)?, from_be(s, l)?);
        let n = &self.n;
        if is_zero(&r) || is_zero(&s) || !less(&r, &n.m) || !less(&s, &n.m) {
            return Some(false);
        }
        // e: the leftmost bits of the hash, as many as n has (n's bits are whole bytes here).
        let mut e = from_be(&digest[..digest.len().min(self.bytes)], l)?;
        if !less(&e, &n.m) {
            sub_in_place(&mut e, &n.m);
        }
        let w = n.inv(&n.to_mont(&s));
        let u1 = n.from_mont(&n.mul(&n.to_mont(&e), &w));
        let u2 = n.from_mont(&n.mul(&n.to_mont(&r), &w));
        let one = self.p.one();
        let g: Jac = (self.g.0.clone(), self.g.1.clone(), one.clone());
        let q: Jac = (q.0, q.1, one);
        let gq = self.add(&g, &q);
        let mut acc: Jac = (self.p.one(), self.p.one(), vec![0; l]);
        for i in (0..64 * l).rev() {
            acc = self.double(&acc);
            let (a, b) = (u1[i / 64] >> (i % 64) & 1 == 1, u2[i / 64] >> (i % 64) & 1 == 1);
            acc = match (a, b) {
                (true, true) => self.add(&acc, &gq),
                (true, false) => self.add(&acc, &g),
                (false, true) => self.add(&acc, &q),
                (false, false) => acc,
            };
        }
        if is_zero(&acc.2) {
            return Some(false);
        }
        let zi = self.p.inv(&acc.2);
        let x = self.p.from_mont(&self.p.mul(&acc.0, &self.p.mul(&zi, &zi)));
        let mut v = x;
        if !less(&v, &n.m) {
            sub_in_place(&mut v, &n.m);
        }
        Some(v == r)
    }
}

type Jac = (Limbs, Limbs, Limbs);

/// r and s from `SEQUENCE { INTEGER r, INTEGER s }` (without leading zeros).
fn der_signature(sig: &[u8]) -> Option<(&[u8], &[u8])> {
    let short = |d: &[u8], at: usize| -> Option<(usize, usize)> {
        // (header length, content length) of the element at `at`
        let first = *d.get(at + 1)? as usize;
        match first {
            0..=0x7F => Some((2, first)),
            0x81 => Some((3, *d.get(at + 2)? as usize)),
            _ => None,
        }
    };
    if *sig.first()? != 0x30 {
        return None;
    }
    let (h, len) = short(sig, 0)?;
    if h + len != sig.len() {
        return None;
    }
    let mut at = h;
    let mut ints = Vec::new();
    for _ in 0..2 {
        if *sig.get(at)? != 0x02 {
            return None;
        }
        let (h, len) = short(sig, at)?;
        let body = sig.get(at + h..at + h + len)?;
        let start = body.iter().position(|b| *b != 0).unwrap_or(body.len());
        ints.push(&body[start..]);
        at += h + len;
    }
    (at == sig.len()).then(|| (ints[0], ints[1]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rsa::Hash;

    fn fixture(name: &str) -> Vec<u8> {
        let text = include_str!("../testdata/ecdsa-openssl.txt");
        let hex = text.lines().find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(' '))).unwrap();
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn field_arithmetic() {
        let c = Curve::new(CurveName::P256);
        let f = &c.p;
        let a = f.to_mont(&hex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", 4));
        assert_eq!(f.from_mont(&f.mul(&a, &f.inv(&a))), {
            let mut one = vec![0u64; 4];
            one[0] = 1;
            one
        });
        // The base point is on the curve and has order n.
        let mut g = vec![4u8];
        g.extend(f.from_mont(&c.g.0).iter().rev().flat_map(|l| l.to_be_bytes()));
        g.extend(f.from_mont(&c.g.1).iter().rev().flat_map(|l| l.to_be_bytes()));
        assert!(c.point(&g).is_some());
        let mut bad = g.clone();
        bad[40] ^= 1;
        assert!(c.point(&bad).is_none());
    }

    #[test]
    fn openssl_signatures() {
        let msg = fixture("message");
        let (p256, p384) = (Curve::new(CurveName::P256), Curve::new(CurveName::P384));
        let (k256, k384) = (fixture("p256"), fixture("p384"));
        let (d256, d384) = (Hash::Sha256.digest(&msg), Hash::Sha384.digest(&msg));
        assert!(p256.verify(&k256, &d256, &fixture("p256-sha256")));
        assert!(p256.verify(&k256, &d384, &fixture("p256-sha384"))); // the hash is cut to 256 bits
        assert!(p384.verify(&k384, &d384, &fixture("p384-sha384")));
        assert!(p384.verify(&k384, &d256, &fixture("p384-sha256")));
        // The wrong key, hash or curve; changed bytes; nonsense.
        assert!(!p256.verify(&k256, &d384, &fixture("p256-sha256")));
        assert!(!p384.verify(&k384, &d256, &fixture("p384-sha384")));
        assert!(!p256.verify(&k384, &d256, &fixture("p256-sha256")));
        let other = Hash::Sha256.digest(b"The wasps are in the hive.");
        assert!(!p256.verify(&k256, &other, &fixture("p256-sha256")));
        let sig = fixture("p384-sha384");
        for i in 0..sig.len() {
            let mut bad = sig.clone();
            bad[i] ^= 0x10;
            assert!(!p384.verify(&k384, &d384, &bad));
        }
        for end in 0..sig.len() {
            assert!(!p384.verify(&k384, &d384, &sig[..end]));
        }
        assert!(!p256.verify(&k256, &d256, &[0x30, 0x06, 0x02, 0x01, 0x00, 0x02, 0x01, 0x01])); // r = 0
    }
}
