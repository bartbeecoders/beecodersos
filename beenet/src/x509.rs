//! X.509 certificates (RFC 5280), as far as checking a TLS server needs:
//! reading DER, the names a certificate is for (subjectAltName), its
//! validity dates, its public key, whether it may sign others, and
//! following the chain to one of the root certificates the OS trusts
//! (`ROOTS`, the Mozilla set).
//!
//! RSA signatures are checked (rsa.rs). ECDSA ones are not yet: a chain
//! that needs one is "not checked" rather than wrong. Everything a
//! certificate holds is untrusted input: every length is checked.

use crate::rsa::{self, Hash};
use alloc::string::String;
use alloc::vec::Vec;

/// The root certificates the OS trusts, one DER certificate after the other.
pub static ROOTS: &[u8] = include_bytes!("../certs/roots.der");

const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_RSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
const OID_RSA_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
const OID_RSA_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d];
const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_ECDSA_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03];
const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];
const OID_BASIC: &[u8] = &[0x55, 0x1d, 0x13];
const OID_CN: &[u8] = &[0x55, 0x04, 0x03];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertError {
    /// Not a certificate this reader understands.
    Malformed,
    /// Made for other names.
    WrongName(String),
    Expired,
    NotYetValid,
    /// A signature in the chain is wrong: forged or damaged.
    BadSignature,
    /// A certificate that is not a CA signed another one.
    NotACa,
}

impl CertError {
    pub fn message(&self) -> String {
        match self {
            CertError::Malformed => "the server's certificate cannot be read".into(),
            CertError::WrongName(names) => alloc::format!("the certificate is for {names}, not this server"),
            CertError::Expired => "a certificate in the chain has expired".into(),
            CertError::NotYetValid => "a certificate in the chain is not valid yet (or the clock is wrong)".into(),
            CertError::BadSignature => "a signature in the certificate chain is wrong".into(),
            CertError::NotACa => "a certificate that may not sign others signed the server's".into(),
        }
    }
}

/// How far a server's certificate could be checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// The chain leads to this root, every signature checked.
    Verified { root: String },
    /// Encrypted, but who is at the other end is not known: why.
    Unchecked(&'static str),
}

// --- DER -------------------------------------------------------------------------------------

/// One DER element: its tag, its contents, and the whole element.
#[derive(Debug, Clone, Copy)]
struct Tlv<'a> {
    tag: u8,
    body: &'a [u8],
    whole: &'a [u8],
}

fn tlv(data: &[u8]) -> Result<(Tlv<'_>, &[u8]), CertError> {
    let bad = CertError::Malformed;
    let tag = *data.first().ok_or(bad.clone())?;
    let first = *data.get(1).ok_or(bad.clone())? as usize;
    let (len, head) = match first {
        0..=0x7F => (first, 2),
        0x81..=0x83 => {
            let n = first - 0x80;
            let bytes = data.get(2..2 + n).ok_or(bad.clone())?;
            (bytes.iter().fold(0usize, |a, b| a << 8 | *b as usize), 2 + n)
        }
        _ => return Err(bad),
    };
    let end = head.checked_add(len).ok_or(bad.clone())?;
    let whole = data.get(..end).ok_or(bad)?;
    Ok((Tlv { tag, body: &whole[head..], whole }, &data[end..]))
}

/// The elements inside a SEQUENCE or SET.
fn items(data: &[u8]) -> Result<Vec<Tlv<'_>>, CertError> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (t, r) = tlv(rest)?;
        out.push(t);
        rest = r;
        if out.len() > 64 {
            return Err(CertError::Malformed);
        }
    }
    Ok(out)
}

/// The certificates of a bundle (one DER certificate after the other).
pub fn split(bundle: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = bundle;
    while let Ok((t, r)) = tlv(rest) {
        out.push(t.whole);
        rest = r;
    }
    out
}

// --- certificates -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKey {
    Rsa(rsa::PublicKey),
    /// An elliptic-curve key: the curve's OID and the point.
    Ec { curve: Vec<u8>, point: Vec<u8> },
    Other,
}

#[derive(Debug, Clone)]
pub struct Certificate<'a> {
    /// The signed part, as it is in the file.
    pub tbs: &'a [u8],
    pub sig_oid: &'a [u8],
    pub signature: &'a [u8],
    pub issuer: &'a [u8],
    pub subject: &'a [u8],
    pub not_before: i64,
    pub not_after: i64,
    pub key: PublicKey,
    pub dns_names: Vec<String>,
    /// basicConstraints cA (None: no such extension).
    pub is_ca: Option<bool>,
}

fn time(t: &Tlv) -> Result<i64, CertError> {
    let s = core::str::from_utf8(t.body).map_err(|_| CertError::Malformed)?;
    let digits = |r: core::ops::Range<usize>| s.get(r).and_then(|d| d.parse::<i64>().ok()).ok_or(CertError::Malformed);
    let (year, rest) = match (t.tag, s.len()) {
        (0x17, 13) => {
            let y = digits(0..2)?;
            (if y < 50 { 2000 + y } else { 1900 + y }, 2)
        }
        (0x18, 15) => (digits(0..4)?, 4),
        _ => return Err(CertError::Malformed),
    };
    let field = |i: usize| digits(rest + i * 2..rest + i * 2 + 2);
    let (month, day, hour, min, sec) = (field(0)?, field(1)?, field(2)?, field(3)?, field(4)?);
    if !s.ends_with('Z') || !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return Err(CertError::Malformed);
    }
    Ok(days_from_civil(year, month, day) * 86400 + hour * 3600 + min * 60 + sec)
}

/// Days since 1970-01-01 of a date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The value of an INTEGER without its sign byte.
fn unsigned(body: &[u8]) -> Vec<u8> {
    let start = body.iter().position(|b| *b != 0).unwrap_or(body.len());
    body[start..].to_vec()
}

/// A BIT STRING's bits (whole bytes only).
fn bits<'a>(t: &Tlv<'a>) -> Result<&'a [u8], CertError> {
    match t.body.split_first() {
        Some((0, rest)) if t.tag == 0x03 => Ok(rest),
        _ => Err(CertError::Malformed),
    }
}

impl<'a> Certificate<'a> {
    pub fn parse(der: &'a [u8]) -> Result<Certificate<'a>, CertError> {
        let bad = CertError::Malformed;
        let (cert, _) = tlv(der)?;
        let parts = items(cert.body)?;
        let [tbs, alg, sig] = parts[..] else { return Err(bad) };
        let sig_oid = items(alg.body)?.first().ok_or(bad.clone())?.body;
        let signature = bits(&sig)?;
        let mut fields = items(tbs.body)?.into_iter().peekable();
        if fields.peek().is_some_and(|f| f.tag == 0xA0) {
            fields.next(); // version
        }
        let _serial = fields.next().ok_or(bad.clone())?;
        let _alg = fields.next().ok_or(bad.clone())?;
        let issuer = fields.next().ok_or(bad.clone())?.whole;
        let validity = items(fields.next().ok_or(bad.clone())?.body)?;
        let (not_before, not_after) = match &validity[..] {
            [a, b] => (time(a)?, time(b)?),
            _ => return Err(bad),
        };
        let subject = fields.next().ok_or(bad.clone())?.whole;
        let spki = items(fields.next().ok_or(bad.clone())?.body)?;
        let key = match &spki[..] {
            [alg, key] => {
                let alg = items(alg.body)?;
                let oid = alg.first().ok_or(bad.clone())?.body;
                let key = bits(key)?;
                if oid == OID_RSA {
                    let (seq, _) = tlv(key)?;
                    match &items(seq.body)?[..] {
                        [n, e] => PublicKey::Rsa(rsa::PublicKey { n: unsigned(n.body), e: unsigned(e.body) }),
                        _ => return Err(bad),
                    }
                } else if oid == OID_EC {
                    PublicKey::Ec { curve: alg.get(1).map_or(Vec::new(), |c| c.body.to_vec()), point: key.to_vec() }
                } else {
                    PublicKey::Other
                }
            }
            _ => return Err(bad),
        };
        let (mut dns_names, mut is_ca) = (Vec::new(), None);
        for f in fields {
            if f.tag != 0xA3 {
                continue; // unique identifiers
            }
            let (seq, _) = tlv(f.body)?;
            for ext in items(seq.body)? {
                let parts = items(ext.body)?;
                let (Some(oid), Some(value)) = (parts.first(), parts.last()) else { continue };
                if oid.body == OID_SAN {
                    let (names, _) = tlv(value.body)?;
                    for name in items(names.body)? {
                        if name.tag == 0x82 {
                            dns_names.push(String::from_utf8_lossy(name.body).to_ascii_lowercase());
                        }
                    }
                } else if oid.body == OID_BASIC {
                    let (seq, _) = tlv(value.body)?;
                    is_ca = Some(items(seq.body)?.first().is_some_and(|b| b.tag == 0x01 && b.body.first().is_some_and(|v| *v != 0)));
                }
            }
        }
        Ok(Certificate { tbs: tbs.whole, sig_oid, signature, issuer, subject, not_before, not_after, key, dns_names, is_ca })
    }

    /// The common name of the subject, for showing.
    pub fn common_name(&self) -> String {
        name_cn(self.subject).unwrap_or_else(|| String::from("(no name)"))
    }

    /// Is the certificate made for `host`? (Its DNS names; a `*.` matches one label.)
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.dns_names.iter().any(|name| match name.strip_prefix("*.") {
            Some(rest) => host.split_once('.').is_some_and(|(first, tail)| !first.is_empty() && tail == rest && rest.contains('.')),
            None => *name == host,
        })
    }
}

fn name_cn(name: &[u8]) -> Option<String> {
    let (seq, _) = tlv(name).ok()?;
    for set in items(seq.body).ok()? {
        for attr in items(set.body).ok()? {
            let parts = items(attr.body).ok()?;
            if parts.first()?.body == OID_CN {
                return Some(String::from_utf8_lossy(parts.get(1)?.body).into_owned());
            }
        }
    }
    None
}

#[derive(Debug, PartialEq, Eq)]
pub enum Checked {
    Good,
    Bad,
    /// A kind of signature this OS cannot check yet.
    Unsupported,
}

/// Is `cert` signed by `key`?
pub fn signed_by(cert: &Certificate, key: &PublicKey) -> Checked {
    let hash = match cert.sig_oid {
        o if o == OID_RSA_SHA256 => Hash::Sha256,
        o if o == OID_RSA_SHA384 => Hash::Sha384,
        o if o == OID_RSA_SHA512 => Hash::Sha512,
        o if o.starts_with(OID_ECDSA_PREFIX) => return Checked::Unsupported,
        _ => return Checked::Unsupported,
    };
    match key {
        PublicKey::Rsa(k) if k.verify_pkcs1(hash, &hash.digest(cert.tbs), cert.signature) => Checked::Good,
        PublicKey::Rsa(_) => Checked::Bad,
        _ => Checked::Unsupported,
    }
}

/// Check a server's chain (`chain[0]` its own certificate) for `host` at
/// `now` (Unix seconds) against the trusted `roots`. An error means the
/// certificate is wrong; `Unchecked` that it could not be checked.
pub fn verify(chain: &[Vec<u8>], host: &str, now: i64, roots: &[u8]) -> Result<Trust, CertError> {
    let certs: Vec<Certificate> = chain.iter().map(|c| Certificate::parse(c)).collect::<Result<_, _>>()?;
    let leaf = certs.first().ok_or(CertError::Malformed)?;
    if !leaf.matches(host) {
        let mut names = leaf.dns_names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        if names.is_empty() {
            names = leaf.common_name();
        }
        return Err(CertError::WrongName(names));
    }
    let roots: Vec<Certificate> = split(roots).into_iter().filter_map(|r| Certificate::parse(r).ok()).collect();
    let mut current = leaf;
    for depth in 0..8 {
        if now > current.not_after {
            return Err(CertError::Expired);
        }
        if now < current.not_before {
            return Err(CertError::NotYetValid);
        }
        if depth > 0 && current.is_ca == Some(false) {
            return Err(CertError::NotACa);
        }
        // A certificate the OS trusts as it is (a root, or a root's twin).
        if depth > 0 && roots.iter().any(|r| r.subject == current.subject && r.key == current.key) {
            return Ok(Trust::Verified { root: current.common_name() });
        }
        // Signed by a root?
        let mut unsupported = false;
        for root in roots.iter().filter(|r| r.subject == current.issuer) {
            match signed_by(current, &root.key) {
                Checked::Good => return Ok(Trust::Verified { root: root.common_name() }),
                Checked::Unsupported => unsupported = true,
                Checked::Bad => {}
            }
        }
        if unsupported {
            return Ok(Trust::Unchecked("ECDSA signatures are not checked yet"));
        }
        // Signed by a certificate the server sent?
        let Some(next) = certs.iter().skip(1).find(|c| c.subject == current.issuer && !core::ptr::eq(*c, current)) else {
            return Ok(Trust::Unchecked("issued by a certificate authority this OS does not know"));
        };
        match signed_by(current, &next.key) {
            Checked::Good => current = next,
            Checked::Unsupported => return Ok(Trust::Unchecked("ECDSA signatures are not checked yet")),
            Checked::Bad => return Err(CertError::BadSignature),
        }
    }
    Ok(Trust::Unchecked("the certificate chain is too long"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(name: &str) -> Vec<Vec<u8>> {
        let data: &[u8] = match name {
            "example.com" => include_bytes!("../testdata/certs/example.com.der"),
            "github.com" => include_bytes!("../testdata/certs/github.com.der"),
            _ => include_bytes!("../testdata/certs/www.google.com.der"),
        };
        split(data).into_iter().map(|c| c.to_vec()).collect()
    }

    /// When the chains were saved.
    fn saved_at() -> i64 {
        include_str!("../testdata/certs/saved-at.txt").trim().parse().unwrap()
    }

    #[test]
    fn the_root_set_reads() {
        let roots = split(ROOTS);
        assert!(roots.len() > 100);
        let parsed = roots.iter().filter(|r| Certificate::parse(r).is_ok()).count();
        assert!(parsed * 100 / roots.len() >= 95, "{parsed} of {}", roots.len());
    }

    #[test]
    fn real_chains_check_out() {
        for host in ["example.com", "github.com", "www.google.com"] {
            let trust = verify(&chain(host), host, saved_at(), ROOTS);
            assert!(matches!(trust, Ok(Trust::Verified { .. })), "{host}: {trust:?}");
        }
        let leaf = chain("github.com");
        let cert = Certificate::parse(&leaf[0]).unwrap();
        assert!(cert.matches("github.com") && cert.matches("GitHub.com.") && !cert.matches("evil.com") && !cert.matches("a.github.com"));
    }

    #[test]
    fn wrong_certificates_are_refused() {
        let at = saved_at();
        // Another name.
        assert!(matches!(verify(&chain("github.com"), "gitlab.com", at, ROOTS), Err(CertError::WrongName(_))));
        // Ten years later, and twenty years before.
        assert_eq!(verify(&chain("github.com"), "github.com", at + 10 * 365 * 86400, ROOTS), Err(CertError::Expired));
        assert_eq!(verify(&chain("github.com"), "github.com", at - 20 * 365 * 86400, ROOTS), Err(CertError::NotYetValid));
        // A changed byte in the signed part of the server's certificate.
        let mut forged = chain("github.com");
        let at_name = forged[0].windows(10).position(|w| w == b"github.com").unwrap();
        forged[0][at_name] = b'G';
        assert_eq!(verify(&forged, "Github.com", at, ROOTS).map(|_| ()), Err(CertError::BadSignature));
        // Without the root set: not an error, but not checked.
        assert_eq!(verify(&chain("github.com"), "github.com", at, &[]), Ok(Trust::Unchecked("issued by a certificate authority this OS does not know")));
        // Damaged certificates: errors, never a panic.
        let good = chain("example.com");
        for i in (0..good[0].len()).step_by(7) {
            let mut c = good.clone();
            c[0][i] ^= 0x5A;
            let _ = verify(&c, "example.com", at, ROOTS);
        }
        for end in 0..good[0].len() {
            let _ = Certificate::parse(&good[0][..end]);
        }
    }

    #[test]
    fn wildcards_and_times() {
        let cert = |names: &[&str]| Certificate {
            tbs: &[],
            sig_oid: &[],
            signature: &[],
            issuer: &[],
            subject: &[],
            not_before: 0,
            not_after: 0,
            key: PublicKey::Other,
            dns_names: names.iter().map(|n| String::from(*n)).collect(),
            is_ca: None,
        };
        let c = cert(&["*.example.org"]);
        assert!(c.matches("www.example.org") && !c.matches("example.org") && !c.matches("a.b.example.org"));
        assert!(!cert(&["*.org"]).matches("example.org"));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        let t = Tlv { tag: 0x17, body: b"491231235959Z", whole: &[] };
        assert_eq!(time(&t), Ok(days_from_civil(2049, 12, 31) * 86400 + 86399));
        let g = Tlv { tag: 0x18, body: b"20500101000000Z", whole: &[] };
        assert_eq!(time(&g), Ok(days_from_civil(2050, 1, 1) * 86400));
    }
}
