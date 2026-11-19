//! The cryptography TLS 1.3 needs for TLS_AES_128_GCM_SHA256 with X25519:
//! key exchange (X25519, RFC 7748), HMAC-SHA256 and HKDF (RFC 2104, RFC
//! 5869) and AES-128-GCM (FIPS-197, NIST SP 800-38D). Each is checked
//! against the published test vectors.
//!
//! Written for clarity first. X25519 runs the same steps whatever the key
//! (a constant-time ladder); AES uses a table S-box and GHASH a bitwise
//! multiply, which are not hardened against timing attacks by someone on
//! the same machine.

use crate::sha256::{sha256, Sha256};
use alloc::vec::Vec;

// --- X25519 ---------------------------------------------------------------------------

/// A number mod 2^255 - 19 in five 51-bit limbs.
type Fe = [u64; 5];
const MASK: u64 = (1 << 51) - 1;

fn fe_load(b: &[u8; 32]) -> Fe {
    let r = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]));
    [r(0) & MASK, (r(6) >> 3) & MASK, (r(12) >> 6) & MASK, (r(19) >> 1) & MASK, (r(24) >> 12) & MASK]
}

fn fe_carry(t: [u128; 5]) -> Fe {
    let mut r = [0u64; 5];
    let t1 = t[1] + (t[0] >> 51);
    r[0] = t[0] as u64 & MASK;
    let t2 = t[2] + (t1 >> 51);
    r[1] = t1 as u64 & MASK;
    let t3 = t[3] + (t2 >> 51);
    r[2] = t2 as u64 & MASK;
    let t4 = t[4] + (t3 >> 51);
    r[3] = t3 as u64 & MASK;
    r[4] = t4 as u64 & MASK;
    // 2^255 = 19 (mod p).
    r[0] += (t4 >> 51) as u64 * 19;
    r[1] += r[0] >> 51;
    r[0] &= MASK;
    r
}

fn fe_add(a: &Fe, b: &Fe) -> Fe {
    fe_carry(core::array::from_fn(|i| a[i] as u128 + b[i] as u128))
}

fn fe_sub(a: &Fe, b: &Fe) -> Fe {
    // a + 4p - b: every limb stays positive (limbs are below 2^52).
    const FOUR_P: [u64; 5] = [0x1F_FFFF_FFFF_FFB4, 0x1F_FFFF_FFFF_FFFC, 0x1F_FFFF_FFFF_FFFC, 0x1F_FFFF_FFFF_FFFC, 0x1F_FFFF_FFFF_FFFC];
    fe_carry(core::array::from_fn(|i| (a[i] + FOUR_P[i] - b[i]) as u128))
}

fn fe_mul(a: &Fe, b: &Fe) -> Fe {
    let m = |x: u64, y: u64| x as u128 * y as u128;
    let (b1, b2, b3, b4) = (b[1] * 19, b[2] * 19, b[3] * 19, b[4] * 19);
    fe_carry([
        m(a[0], b[0]) + m(a[4], b1) + m(a[3], b2) + m(a[2], b3) + m(a[1], b4),
        m(a[1], b[0]) + m(a[0], b[1]) + m(a[4], b2) + m(a[3], b3) + m(a[2], b4),
        m(a[2], b[0]) + m(a[1], b[1]) + m(a[0], b[2]) + m(a[4], b3) + m(a[3], b4),
        m(a[3], b[0]) + m(a[2], b[1]) + m(a[1], b[2]) + m(a[0], b[3]) + m(a[4], b4),
        m(a[4], b[0]) + m(a[3], b[1]) + m(a[2], b[2]) + m(a[1], b[3]) + m(a[0], b[4]),
    ])
}

fn fe_small(a: &Fe, k: u64) -> Fe {
    fe_carry(core::array::from_fn(|i| a[i] as u128 * k as u128))
}

/// a^(p-2) = 1/a.
fn fe_invert(a: &Fe) -> Fe {
    // p - 2 = 2^255 - 21: bits from the top.
    let mut r: Fe = [1, 0, 0, 0, 0];
    for bit in (0..255).rev() {
        r = fe_mul(&r, &r);
        let set = !(bit == 2 || bit == 4); // 2^255 - 21 = ...11101011 in its lowest bits
        if set {
            r = fe_mul(&r, a);
        }
    }
    r
}

fn fe_store(a: &Fe) -> [u8; 32] {
    let mut h = fe_carry(core::array::from_fn(|i| a[i] as u128));
    h = fe_carry(core::array::from_fn(|i| h[i] as u128));
    // Subtract p once if h >= p.
    let mut q = (h[0] + 19) >> 51;
    for limb in &h[1..] {
        q = (limb + q) >> 51;
    }
    h[0] += 19 * q;
    for i in 0..4 {
        h[i + 1] += h[i] >> 51;
        h[i] &= MASK;
    }
    h[4] &= MASK;
    let mut out = [0u8; 32];
    let (mut acc, mut bits, mut at) = (0u64, 0u32, 0usize);
    for limb in h {
        acc |= limb << bits;
        bits += 51;
        while bits >= 8 && at < 32 {
            out[at] = acc as u8;
            acc >>= 8;
            bits -= 8;
            at += 1;
        }
    }
    if at < 32 {
        out[at] = acc as u8;
    }
    out
}

fn fe_cswap(swap: u64, a: &mut Fe, b: &mut Fe) {
    let mask = 0u64.wrapping_sub(swap);
    for i in 0..5 {
        let t = mask & (a[i] ^ b[i]);
        a[i] ^= t;
        b[i] ^= t;
    }
}

/// X25519: `scalar` times the point with u-coordinate `u` (RFC 7748).
pub fn x25519(scalar: &[u8; 32], u: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    let x1 = fe_load(u);
    let (mut x2, mut z2, mut x3, mut z3): (Fe, Fe, Fe, Fe) = ([1, 0, 0, 0, 0], [0; 5], x1, [1, 0, 0, 0, 0]);
    let mut swap = 0u64;
    for t in (0..255).rev() {
        let bit = (k[t / 8] >> (t % 8)) as u64 & 1;
        swap ^= bit;
        fe_cswap(swap, &mut x2, &mut x3);
        fe_cswap(swap, &mut z2, &mut z3);
        swap = bit;
        let a = fe_add(&x2, &z2);
        let aa = fe_mul(&a, &a);
        let b = fe_sub(&x2, &z2);
        let bb = fe_mul(&b, &b);
        let e = fe_sub(&aa, &bb);
        let c = fe_add(&x3, &z3);
        let d = fe_sub(&x3, &z3);
        let da = fe_mul(&d, &a);
        let cb = fe_mul(&c, &b);
        let sum = fe_add(&da, &cb);
        x3 = fe_mul(&sum, &sum);
        let diff = fe_sub(&da, &cb);
        z3 = fe_mul(&x1, &fe_mul(&diff, &diff));
        x2 = fe_mul(&aa, &bb);
        z2 = fe_mul(&e, &fe_add(&aa, &fe_small(&e, 121_665)));
    }
    fe_cswap(swap, &mut x2, &mut x3);
    fe_cswap(swap, &mut z2, &mut z3);
    fe_store(&fe_mul(&x2, &fe_invert(&z2)))
}

/// The public key of a private one: the scalar times the base point 9.
pub fn x25519_public(private: &[u8; 32]) -> [u8; 32] {
    let mut nine = [0u8; 32];
    nine[0] = 9;
    x25519(private, &nine)
}

// --- HMAC and HKDF -----------------------------------------------------------------------

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(&k.map(|b| b ^ 0x36));
    inner.update(data);
    let inner = inner.finish();
    let mut outer = Sha256::new();
    outer.update(&k.map(|b| b ^ 0x5C));
    outer.update(&inner);
    outer.finish()
}

pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    hmac_sha256(salt, ikm)
}

pub fn hkdf_expand(prk: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut block: Vec<u8> = Vec::new();
    let mut counter = 1u8;
    while out.len() < len {
        let mut input = block.clone();
        input.extend_from_slice(info);
        input.push(counter);
        block = hmac_sha256(prk, &input).to_vec();
        out.extend_from_slice(&block);
        counter = counter.wrapping_add(1);
    }
    out.truncate(len);
    out
}

/// TLS 1.3's HKDF-Expand-Label (RFC 8446 section 7.1).
pub fn hkdf_expand_label(secret: &[u8], label: &str, context: &[u8], len: usize) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(&(len as u16).to_be_bytes());
    info.push(6 + label.len() as u8);
    info.extend_from_slice(b"tls13 ");
    info.extend_from_slice(label.as_bytes());
    info.push(context.len() as u8);
    info.extend_from_slice(context);
    hkdf_expand(secret, &info, len)
}

// --- AES-128 and GCM ---------------------------------------------------------------------

const SBOX: [u8; 256] = {
    // Built at compile time: the multiplicative inverse in GF(2^8), then the affine map.
    let mut sbox = [0u8; 256];
    let mut p: u8 = 1;
    let mut q: u8 = 1;
    loop {
        // p times 3, q divided by 3.
        p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1B } else { 0 };
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 {
            q ^= 0x09;
        }
        let x = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4);
        sbox[p as usize] = x ^ 0x63;
        if p == 1 {
            break;
        }
    }
    sbox[0] = 0x63;
    sbox
};

fn xtime(b: u8) -> u8 {
    (b << 1) ^ if b & 0x80 != 0 { 0x1B } else { 0 }
}

/// AES-128 with its 11 round keys made ready.
pub struct Aes128 {
    keys: [[u8; 16]; 11],
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Aes128 {
        let mut keys = [[0u8; 16]; 11];
        keys[0] = *key;
        let mut rcon = 1u8;
        for r in 1..11 {
            let prev = keys[r - 1];
            let mut t = [prev[13], prev[14], prev[15], prev[12]];
            for b in &mut t {
                *b = SBOX[*b as usize];
            }
            t[0] ^= rcon;
            rcon = xtime(rcon);
            for i in 0..16 {
                let before = if i < 4 { t[i] } else { keys[r][i - 4] };
                keys[r][i] = prev[i] ^ before;
            }
        }
        Aes128 { keys }
    }

    pub fn encrypt(&self, block: &[u8; 16]) -> [u8; 16] {
        let mut s = *block;
        let add = |s: &mut [u8; 16], k: &[u8; 16]| s.iter_mut().zip(k).for_each(|(a, b)| *a ^= b);
        add(&mut s, &self.keys[0]);
        for round in 1..11 {
            for b in &mut s {
                *b = SBOX[*b as usize];
            }
            // Shift rows (the state is column by column).
            let t = s;
            for c in 0..4 {
                for r in 0..4 {
                    s[c * 4 + r] = t[((c + r) % 4) * 4 + r];
                }
            }
            if round < 10 {
                for c in 0..4 {
                    let col = [s[c * 4], s[c * 4 + 1], s[c * 4 + 2], s[c * 4 + 3]];
                    let all = col[0] ^ col[1] ^ col[2] ^ col[3];
                    for r in 0..4 {
                        s[c * 4 + r] = col[r] ^ all ^ xtime(col[r] ^ col[(r + 1) % 4]);
                    }
                }
            }
            add(&mut s, &self.keys[round]);
        }
        s
    }
}

/// Multiply in GF(2^128) as GCM defines it.
fn gf_mul(x: u128, y: u128) -> u128 {
    let (mut z, mut v) = (0u128, y);
    for i in 0..128 {
        if x >> (127 - i) & 1 == 1 {
            z ^= v;
        }
        v = if v & 1 == 1 { (v >> 1) ^ (0xE1 << 120) } else { v >> 1 };
    }
    z
}

fn ghash(h: u128, aad: &[u8], text: &[u8]) -> u128 {
    let mut y = 0u128;
    for part in [aad, text] {
        for chunk in part.chunks(16) {
            let mut block = [0u8; 16];
            block[..chunk.len()].copy_from_slice(chunk);
            y = gf_mul(y ^ u128::from_be_bytes(block), h);
        }
    }
    let lengths = ((aad.len() as u128 * 8) << 64) | (text.len() as u128 * 8);
    gf_mul(y ^ lengths, h)
}

/// AES-128-GCM with a 12-byte nonce and a 16-byte tag.
pub struct Gcm {
    aes: Aes128,
    h: u128,
}

impl Gcm {
    pub fn new(key: &[u8; 16]) -> Gcm {
        let aes = Aes128::new(key);
        let h = u128::from_be_bytes(aes.encrypt(&[0; 16]));
        Gcm { aes, h }
    }

    fn counter_block(nonce: &[u8; 12], counter: u32) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..12].copy_from_slice(nonce);
        b[12..].copy_from_slice(&counter.to_be_bytes());
        b
    }

    fn ctr(&self, nonce: &[u8; 12], data: &mut [u8]) {
        for (i, chunk) in data.chunks_mut(16).enumerate() {
            let key = self.aes.encrypt(&Self::counter_block(nonce, 2 + i as u32));
            chunk.iter_mut().zip(key).for_each(|(a, b)| *a ^= b);
        }
    }

    fn tag(&self, nonce: &[u8; 12], aad: &[u8], text: &[u8]) -> [u8; 16] {
        let s = ghash(self.h, aad, text) ^ u128::from_be_bytes(self.aes.encrypt(&Self::counter_block(nonce, 1)));
        s.to_be_bytes()
    }

    /// The text encrypted, followed by the tag.
    pub fn seal(&self, nonce: &[u8; 12], aad: &[u8], plain: &[u8]) -> Vec<u8> {
        let mut out = plain.to_vec();
        self.ctr(nonce, &mut out);
        let tag = self.tag(nonce, aad, &out);
        out.extend_from_slice(&tag);
        out
    }

    /// The text, if the tag is right (compared in full whatever differs).
    pub fn open(&self, nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        let split = sealed.len().checked_sub(16)?;
        let (text, tag) = sealed.split_at(split);
        let right = self.tag(nonce, aad, text);
        if right.iter().zip(tag).fold(0u8, |d, (a, b)| d | (a ^ b)) != 0 {
            return None;
        }
        let mut out = text.to_vec();
        self.ctr(nonce, &mut out);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn arr32(s: &str) -> [u8; 32] {
        hex(s).try_into().unwrap()
    }

    #[test]
    fn x25519_rfc7748() {
        let k = arr32("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
        let u = arr32("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
        assert_eq!(x25519(&k, &u).to_vec(), hex("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552"));
        let alice = arr32("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let bob = arr32("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        let (alice_pub, bob_pub) = (x25519_public(&alice), x25519_public(&bob));
        assert_eq!(alice_pub.to_vec(), hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"));
        assert_eq!(bob_pub.to_vec(), hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"));
        let shared = hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
        assert_eq!(x25519(&alice, &bob_pub).to_vec(), shared);
        assert_eq!(x25519(&bob, &alice_pub).to_vec(), shared);
        // The iterated test: 1 and 1000 rounds.
        let (mut k, mut u) = ([0u8; 32], [0u8; 32]);
        k[0] = 9;
        u[0] = 9;
        for round in 1..=1000 {
            let next = x25519(&k, &u);
            u = k;
            k = next;
            if round == 1 {
                assert_eq!(k.to_vec(), hex("422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079"));
            }
        }
        assert_eq!(k.to_vec(), hex("684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51"));
    }

    #[test]
    fn hmac_and_hkdf() {
        assert_eq!(hmac_sha256(&[0x0b; 20], b"Hi There").to_vec(), hex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"));
        assert_eq!(hmac_sha256(b"Jefe", b"what do ya want for nothing?").to_vec(), hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"));
        // A key longer than a block is hashed first (RFC 4231 test 6).
        assert_eq!(
            hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First").to_vec(),
            hex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
        let prk = hkdf_extract(&hex("000102030405060708090a0b0c"), &[0x0b; 22]);
        assert_eq!(prk.to_vec(), hex("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5"));
        assert_eq!(
            hkdf_expand(&prk, &hex("f0f1f2f3f4f5f6f7f8f9"), 42),
            hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865")
        );
    }

    #[test]
    fn aes_and_gcm() {
        let aes = Aes128::new(&hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap());
        assert_eq!(aes.encrypt(&hex("00112233445566778899aabbccddeeff").try_into().unwrap()).to_vec(), hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
        assert_eq!(SBOX[0x53], 0xED);
        // GCM test case 2: zero key, zero block.
        let gcm = Gcm::new(&[0; 16]);
        let sealed = gcm.seal(&[0; 12], &[], &[0; 16]);
        assert_eq!(sealed, hex("0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf"));
        // Test case 4: with associated data and a partial last block.
        let gcm = Gcm::new(&hex("feffe9928665731c6d6a8f9467308308").try_into().unwrap());
        let nonce: [u8; 12] = hex("cafebabefacedbaddecaf888").try_into().unwrap();
        let plain = hex("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39");
        let aad = hex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let sealed = gcm.seal(&nonce, &aad, &plain);
        assert_eq!(
            sealed,
            hex("42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e0915bc94fbc3221a5db94fae95ae7121a47")
        );
        assert_eq!(gcm.open(&nonce, &aad, &sealed), Some(plain.clone()));
        // Any change is refused.
        for i in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[i] ^= 1;
            assert_eq!(gcm.open(&nonce, &aad, &bad), None);
        }
        assert_eq!(gcm.open(&nonce, b"other", &sealed), None);
        assert_eq!(gcm.open(&nonce, &aad, &sealed[..10]), None);
    }
}
