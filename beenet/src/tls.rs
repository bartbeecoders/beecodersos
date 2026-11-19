//! A TLS 1.3 client (RFC 8446), socket-free like the rest of beenet: bytes
//! from the server go in (`receive`), bytes for the server and the data it
//! sent come out (`take_outgoing`, `take_plain`).
//!
//! One cipher suite, TLS_AES_128_GCM_SHA256, with X25519 key exchange
//! (crypto.rs); no session resumption, no early data, no client
//! certificates. The handshake and the key schedule are checked against
//! the trace in RFC 8448 byte for byte.
//!
//! The server's certificate chain is checked against the trusted roots
//! (x509.rs) for the host name and the time given to `Client::new`, and
//! its CertificateVerify signature with the server's key: a certificate
//! that is wrong ends the handshake (`TlsError::Certificate`). Chains that
//! lead to an unknown root, or need another kind of signature than RSA or
//! ECDSA on P-256/P-384, are not checked: `trust` says so, and callers tell
//! the user.

use crate::crypto::{hkdf_expand_label, hkdf_extract, hmac_sha256, x25519, x25519_public, Gcm};
use crate::rsa::Hash;
use crate::ecdsa::{Curve, CurveName};
use crate::x509::{self, Certificate, PublicKey, Trust};
use crate::sha256::sha256;
use alloc::string::String;
use alloc::vec::Vec;

const HANDSHAKE: u8 = 22;
const APPLICATION: u8 = 23;
const ALERT: u8 = 21;
const CHANGE_CIPHER_SPEC: u8 = 20;
const MAX_RECORD: usize = 16384 + 256;
/// The ServerHello random that means "HelloRetryRequest".
const RETRY_RANDOM: [u8; 32] = [
    0xCF, 0x21, 0xAD, 0x74, 0xE5, 0x9A, 0x61, 0x11, 0xBE, 0x1D, 0x8C, 0x02, 0x1E, 0x65, 0xB8, 0x91, 0xC2, 0xA2, 0x11, 0x16, 0x7A, 0xBB, 0x8C, 0x5E, 0x07, 0x9E, 0x09, 0xE2,
    0xC8, 0xA8, 0x33, 0x9C,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsError {
    /// The server sent this alert (RFC 8446 section 6).
    Alert(u8),
    /// The server did something TLS 1.3 does not allow.
    Protocol(&'static str),
    /// Something this client does not do (another cipher, a retry, ...).
    Unsupported(&'static str),
    /// A record that does not decrypt: changed on the way, or wrong keys.
    BadRecord,
    /// The server's certificate is wrong (x509.rs says why).
    Certificate(x509::CertError),
    /// The server's handshake signature does not match its certificate.
    BadServerSignature,
}

impl TlsError {
    pub fn message(&self) -> alloc::string::String {
        match self {
            TlsError::Alert(40) => "the server refused the handshake (alert 40)".into(),
            TlsError::Alert(70) => "the server does not speak TLS 1.3 (alert 70)".into(),
            TlsError::Alert(n) => alloc::format!("the server sent alert {n}"),
            TlsError::Protocol(why) => alloc::format!("TLS error: {why}"),
            TlsError::Unsupported(what) => alloc::format!("not supported yet: {what}"),
            TlsError::BadRecord => "a record did not decrypt (changed on the way?)".into(),
            TlsError::Certificate(e) => alloc::format!("refused: {}", e.message()),
            TlsError::BadServerSignature => "refused: the server's signature does not match its certificate".into(),
        }
    }
}

struct Keys {
    gcm: Gcm,
    iv: [u8; 12],
    seq: u64,
}

impl Keys {
    fn new(secret: &[u8]) -> Keys {
        let key: [u8; 16] = hkdf_expand_label(secret, "key", &[], 16).try_into().unwrap_or([0; 16]);
        let iv: [u8; 12] = hkdf_expand_label(secret, "iv", &[], 12).try_into().unwrap_or([0; 12]);
        Keys { gcm: Gcm::new(&key), iv, seq: 0 }
    }

    fn nonce(&mut self) -> [u8; 12] {
        let mut n = self.iv;
        for (b, s) in n[4..].iter_mut().zip(self.seq.to_be_bytes()) {
            *b ^= s;
        }
        self.seq += 1;
        n
    }

    fn seal(&mut self, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut inner = data.to_vec();
        inner.push(kind);
        let header = [APPLICATION, 3, 3, ((inner.len() + 16) >> 8) as u8, (inner.len() + 16) as u8];
        let nonce = self.nonce();
        let mut record = header.to_vec();
        record.extend_from_slice(&self.gcm.seal(&nonce, &header, &inner));
        record
    }

    /// The content type and the data of a sealed record.
    fn open(&mut self, header: &[u8], body: &[u8]) -> Result<(u8, Vec<u8>), TlsError> {
        let nonce = self.nonce();
        let mut inner = self.gcm.open(&nonce, header, body).ok_or(TlsError::BadRecord)?;
        // The content type is the last byte that is not zero padding.
        let at = inner.iter().rposition(|b| *b != 0).ok_or(TlsError::Protocol("a record without a content type"))?;
        let kind = inner[at];
        inner.truncate(at);
        Ok((kind, inner))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    ServerHello,
    EncryptedExtensions,
    Certificate,
    CertificateVerify,
    Finished,
    Connected,
}

pub struct Client {
    state: State,
    private: [u8; 32],
    /// Every handshake message so far, for the transcript hash.
    transcript: Vec<u8>,
    inbox: Vec<u8>,
    handshake: Vec<u8>,
    read: Option<Keys>,
    write: Option<Keys>,
    handshake_secret: [u8; 32],
    client_hs: Vec<u8>,
    server_hs: Vec<u8>,
    outgoing: Vec<u8>,
    plain: Vec<u8>,
    closed: bool,
    /// The server's certificates, its own first (DER), as it sent them.
    pub certificates: Vec<Vec<u8>>,
    /// The CertificateVerify signature: (scheme, signature, transcript hash it signs).
    pub signature: Option<(u16, Vec<u8>, [u8; 32])>,
    /// What to check the certificate against: the host name and the time
    /// (Unix seconds); None: not checked (`trust` says why).
    check: Option<(String, i64)>,
    /// How far the server's identity was checked (after the handshake).
    pub trust: Option<Trust>,
}

fn u16_at(d: &[u8], at: usize) -> Result<usize, TlsError> {
    Ok(u16::from_be_bytes(d.get(at..at + 2).ok_or(TlsError::Protocol("a message is cut short"))?.try_into().unwrap_or([0; 2])) as usize)
}

fn u24_at(d: &[u8], at: usize) -> Result<usize, TlsError> {
    let b = d.get(at..at + 3).ok_or(TlsError::Protocol("a message is cut short"))?;
    Ok((b[0] as usize) << 16 | (b[1] as usize) << 8 | b[2] as usize)
}

fn bytes<'a>(d: &'a [u8], at: usize, len: usize) -> Result<&'a [u8], TlsError> {
    d.get(at..at.checked_add(len).ok_or(TlsError::Protocol("a length is too large"))?).ok_or(TlsError::Protocol("a message is cut short"))
}

/// A ClientHello for `host` (server name), with this random and key share.
/// `ecdsa`: also accept ECDSA certificates (which are not checked yet); a
/// server with both kinds then may choose either.
pub fn client_hello(host: &str, random: &[u8; 32], public: &[u8; 32], ecdsa: bool) -> Vec<u8> {
    let mut body = alloc::vec![3, 3];
    body.extend_from_slice(random);
    body.push(32); // a session id, as middleboxes expect
    body.extend_from_slice(random);
    body.extend_from_slice(&[0, 2, 0x13, 0x01]); // TLS_AES_128_GCM_SHA256
    body.extend_from_slice(&[1, 0]); // no compression
    let mut ext = Vec::new();
    let mut add = |kind: u16, data: &[u8]| {
        ext.extend_from_slice(&kind.to_be_bytes());
        ext.extend_from_slice(&(data.len() as u16).to_be_bytes());
        ext.extend_from_slice(data);
    };
    if !host.is_empty() && host.len() < 256 && host.parse::<core::net::Ipv4Addr>().is_err() {
        let name = host.as_bytes();
        let mut sni = ((name.len() + 3) as u16).to_be_bytes().to_vec();
        sni.push(0);
        sni.extend_from_slice(&(name.len() as u16).to_be_bytes());
        sni.extend_from_slice(name);
        add(0, &sni);
    }
    add(10, &[0, 2, 0, 0x1D]); // groups: x25519
    // Signatures: RSA-PSS and PKCS#1 with SHA-256/384/512 (and ECDSA P-256/384).
    if ecdsa {
        add(13, &[0, 16, 8, 4, 8, 5, 8, 6, 4, 1, 5, 1, 6, 1, 4, 3, 5, 3]);
    } else {
        add(13, &[0, 12, 8, 4, 8, 5, 8, 6, 4, 1, 5, 1, 6, 1]);
    }
    let mut share = alloc::vec![0, 36, 0, 0x1D, 0, 32];
    share.extend_from_slice(public);
    add(51, &share);
    add(43, &[2, 3, 4]); // supported versions: TLS 1.3
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);
    let mut msg = alloc::vec![1, (body.len() >> 16) as u8, (body.len() >> 8) as u8, body.len() as u8];
    msg.extend_from_slice(&body);
    msg
}

impl Client {
    /// Start a handshake with `host`: `random` and `private` must be fresh
    /// random bytes; `now` (Unix seconds) is the time the certificate must
    /// be valid at (None: the clock is not known, so it is not checked).
    /// The ClientHello is waiting in `take_outgoing`.
    pub fn new(host: &str, random: [u8; 32], private: [u8; 32], now: Option<i64>, ecdsa: bool) -> Client {
        let hello = client_hello(host, &random, &x25519_public(&private), ecdsa);
        let mut client = Client::with_hello(&hello, private);
        client.check = now.map(|t| (String::from(host), t));
        client
    }

    /// Start with a ready-made ClientHello message (tests use RFC 8448's).
    pub fn with_hello(hello: &[u8], private: [u8; 32]) -> Client {
        let mut outgoing = alloc::vec![HANDSHAKE, 3, 1, (hello.len() >> 8) as u8, hello.len() as u8];
        outgoing.extend_from_slice(hello);
        Client {
            state: State::ServerHello,
            private,
            transcript: hello.to_vec(),
            inbox: Vec::new(),
            handshake: Vec::new(),
            read: None,
            write: None,
            handshake_secret: [0; 32],
            client_hs: Vec::new(),
            server_hs: Vec::new(),
            outgoing,
            plain: Vec::new(),
            closed: false,
            certificates: Vec::new(),
            signature: None,
            check: None,
            trust: None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    /// The server ended the connection (close_notify).
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn take_outgoing(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outgoing)
    }

    pub fn take_plain(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.plain)
    }

    /// Data taken with `take_plain` but not used: it comes first next time.
    pub fn put_back(&mut self, data: &[u8]) {
        let mut again = data.to_vec();
        again.extend_from_slice(&self.plain);
        self.plain = again;
    }

    /// Send application data (after the handshake).
    pub fn send(&mut self, data: &[u8]) {
        if let (State::Connected, Some(keys)) = (self.state, self.write.as_mut()) {
            for chunk in data.chunks(16384) {
                let record = keys.seal(APPLICATION, chunk);
                self.outgoing.extend_from_slice(&record);
            }
        }
    }

    /// Say goodbye (close_notify).
    pub fn close(&mut self) {
        if let (State::Connected, Some(keys)) = (self.state, self.write.as_mut()) {
            let record = keys.seal(ALERT, &[1, 0]);
            self.outgoing.extend_from_slice(&record);
        }
    }

    /// Bytes from the server: whole records are handled, the rest waits.
    pub fn receive(&mut self, data: &[u8]) -> Result<(), TlsError> {
        self.inbox.extend_from_slice(data);
        loop {
            if self.inbox.len() < 5 {
                return Ok(());
            }
            let len = u16_at(&self.inbox, 3)?;
            if len > MAX_RECORD {
                return Err(TlsError::Protocol("a record is too long"));
            }
            if self.inbox.len() < 5 + len {
                return Ok(());
            }
            let record: Vec<u8> = self.inbox.drain(..5 + len).collect();
            self.record(&record[..5], &record[5..])?;
        }
    }

    fn record(&mut self, header: &[u8], body: &[u8]) -> Result<(), TlsError> {
        match header[0] {
            CHANGE_CIPHER_SPEC => Ok(()), // sent for middleboxes; means nothing in TLS 1.3
            ALERT if self.read.is_none() => Err(TlsError::Alert(*body.get(1).unwrap_or(&0))),
            HANDSHAKE if self.state == State::ServerHello => {
                self.handshake.extend_from_slice(body);
                self.messages()
            }
            APPLICATION => {
                let keys = self.read.as_mut().ok_or(TlsError::Protocol("encrypted data before the keys"))?;
                let (kind, data) = keys.open(header, body)?;
                match kind {
                    HANDSHAKE => {
                        self.handshake.extend_from_slice(&data);
                        self.messages()
                    }
                    APPLICATION if self.state == State::Connected => {
                        self.plain.extend_from_slice(&data);
                        Ok(())
                    }
                    ALERT if data.get(1) == Some(&0) => {
                        self.closed = true;
                        Ok(())
                    }
                    ALERT => Err(TlsError::Alert(*data.get(1).unwrap_or(&0))),
                    _ => Err(TlsError::Protocol("unexpected content")),
                }
            }
            _ => Err(TlsError::Protocol("unexpected record")),
        }
    }

    /// Handle every whole handshake message waiting.
    fn messages(&mut self) -> Result<(), TlsError> {
        loop {
            if self.handshake.len() < 4 {
                return Ok(());
            }
            let len = u24_at(&self.handshake, 1)?;
            if len > 1 << 20 {
                return Err(TlsError::Protocol("a handshake message is too long"));
            }
            if self.handshake.len() < 4 + len {
                return Ok(());
            }
            let msg: Vec<u8> = self.handshake.drain(..4 + len).collect();
            self.message(&msg)?;
        }
    }

    fn transcript_hash(&self) -> [u8; 32] {
        sha256(&self.transcript)
    }

    fn message(&mut self, msg: &[u8]) -> Result<(), TlsError> {
        let (kind, body) = (msg[0], &msg[4..]);
        match (self.state, kind) {
            (State::ServerHello, 2) => {
                self.server_hello(body)?;
                self.transcript.extend_from_slice(msg);
                self.handshake_keys();
                self.state = State::EncryptedExtensions;
            }
            (State::EncryptedExtensions, 8) => {
                self.transcript.extend_from_slice(msg);
                self.state = State::Certificate;
            }
            (State::Certificate, 13) => return Err(TlsError::Unsupported("the server asks for a client certificate")),
            (State::Certificate, 11) => {
                self.certificate(body)?;
                self.transcript.extend_from_slice(msg);
                self.state = State::CertificateVerify;
            }
            (State::CertificateVerify, 15) => {
                let scheme = u16_at(body, 0)? as u16;
                let len = u16_at(body, 2)?;
                let signature = bytes(body, 4, len)?.to_vec();
                let hash = self.transcript_hash();
                self.check_server(scheme, &signature, &hash)?;
                self.signature = Some((scheme, signature, hash));
                self.transcript.extend_from_slice(msg);
                self.state = State::Finished;
            }
            (State::Finished, 20) => {
                let key = hkdf_expand_label(&self.server_hs, "finished", &[], 32);
                let expected = hmac_sha256(&key, &self.transcript_hash());
                if body.len() != 32 || body.iter().zip(expected).fold(0u8, |d, (a, b)| d | (a ^ b)) != 0 {
                    return Err(TlsError::Protocol("the server's Finished does not match (wrong keys?)"));
                }
                self.transcript.extend_from_slice(msg);
                self.finish();
            }
            // After the handshake: tickets and key updates are not used.
            (State::Connected, 4) => {}
            (State::Connected, 24) => return Err(TlsError::Unsupported("key updates")),
            _ => return Err(TlsError::Protocol("a handshake message out of order")),
        }
        Ok(())
    }

    /// The certificate chain, then the server's signature over the
    /// handshake so far (RFC 8446 section 4.4.3) with its key.
    fn check_server(&mut self, scheme: u16, signature: &[u8], hash: &[u8; 32]) -> Result<(), TlsError> {
        let mut trust = match &self.check {
            Some((host, now)) => x509::verify(&self.certificates, host, *now, x509::ROOTS).map_err(TlsError::Certificate)?,
            None => Trust::Unchecked("the clock is not set"),
        };
        let leaf = Certificate::parse(&self.certificates[0]).map_err(TlsError::Certificate)?;
        let mut signed = alloc::vec![0x20u8; 64];
        signed.extend_from_slice(b"TLS 1.3, server CertificateVerify\0");
        signed.extend_from_slice(hash);
        // RSA-PSS with SHA-256/384/512, or ECDSA with the curve's own hash.
        let good = match (scheme, &leaf.key) {
            (0x0804..=0x0806, PublicKey::Rsa(key)) => {
                let h = [Hash::Sha256, Hash::Sha384, Hash::Sha512][(scheme - 0x0804) as usize];
                Some(key.verify_pss(h, &h.digest(&signed), signature))
            }
            (0x0403 | 0x0503, PublicKey::Ec { curve, point }) => {
                let (name, h) = if scheme == 0x0403 { (CurveName::P256, Hash::Sha256) } else { (CurveName::P384, Hash::Sha384) };
                Some(x509::curve_of(curve) == Some(name) && Curve::new(name).verify(point, &h.digest(&signed), signature))
            }
            _ => None,
        };
        match good {
            Some(true) => {}
            Some(false) => return Err(TlsError::BadServerSignature),
            None => trust = Trust::Unchecked("a kind of signature this OS cannot check yet"),
        }
        self.trust = Some(trust);
        Ok(())
    }

    fn server_hello(&mut self, body: &[u8]) -> Result<(), TlsError> {
        if bytes(body, 2, 32)? == RETRY_RANDOM {
            return Err(TlsError::Unsupported("the server wants another key exchange (HelloRetryRequest)"));
        }
        let session = *body.get(34).ok_or(TlsError::Protocol("a message is cut short"))? as usize;
        let at = 35 + session;
        if u16_at(body, at)? != 0x1301 {
            return Err(TlsError::Unsupported("the server chose another cipher"));
        }
        let ext_len = u16_at(body, at + 3)?;
        let ext = bytes(body, at + 5, ext_len)?;
        let (mut version, mut key) = (false, None);
        let mut i = 0;
        while i + 4 <= ext.len() {
            let (kind, len) = (u16_at(ext, i)?, u16_at(ext, i + 2)?);
            let data = bytes(ext, i + 4, len)?;
            match kind {
                43 => version = data == [3, 4],
                51 => {
                    if u16_at(data, 0)? != 0x1D || u16_at(data, 2)? != 32 {
                        return Err(TlsError::Unsupported("the server chose another key exchange"));
                    }
                    key = Some(<[u8; 32]>::try_from(bytes(data, 4, 32)?).unwrap_or([0; 32]));
                }
                _ => {}
            }
            i += 4 + len;
        }
        if !version {
            return Err(TlsError::Unsupported("the server does not speak TLS 1.3"));
        }
        let server_public = key.ok_or(TlsError::Protocol("no key share from the server"))?;
        let shared = x25519(&self.private, &server_public);
        if shared == [0; 32] {
            return Err(TlsError::Protocol("a bad key share"));
        }
        let early = hkdf_extract(&[0; 32], &[0; 32]);
        let derived = hkdf_expand_label(&early, "derived", &sha256(&[]), 32);
        self.handshake_secret = hkdf_extract(&derived, &shared);
        Ok(())
    }

    fn handshake_keys(&mut self) {
        let hash = self.transcript_hash();
        self.client_hs = hkdf_expand_label(&self.handshake_secret, "c hs traffic", &hash, 32);
        self.server_hs = hkdf_expand_label(&self.handshake_secret, "s hs traffic", &hash, 32);
        self.read = Some(Keys::new(&self.server_hs));
        self.write = Some(Keys::new(&self.client_hs));
    }

    fn certificate(&mut self, body: &[u8]) -> Result<(), TlsError> {
        let context = *body.first().ok_or(TlsError::Protocol("a message is cut short"))? as usize;
        let list_len = u24_at(body, 1 + context)?;
        let list = bytes(body, 4 + context, list_len)?;
        let mut i = 0;
        while i < list.len() {
            let len = u24_at(list, i)?;
            self.certificates.push(bytes(list, i + 3, len)?.to_vec());
            let ext = u16_at(list, i + 3 + len)?;
            i += 3 + len + 2 + ext;
            if self.certificates.len() > 16 {
                return Err(TlsError::Protocol("too many certificates"));
            }
        }
        if self.certificates.is_empty() {
            return Err(TlsError::Protocol("the server sent no certificate"));
        }
        Ok(())
    }

    /// After the server's Finished: our Finished, then the application keys.
    fn finish(&mut self) {
        let hash = self.transcript_hash(); // up to the server's Finished
        let key = hkdf_expand_label(&self.client_hs, "finished", &[], 32);
        let verify = hmac_sha256(&key, &hash);
        let mut msg = alloc::vec![20, 0, 0, 32];
        msg.extend_from_slice(&verify);
        if let Some(keys) = self.write.as_mut() {
            let record = keys.seal(HANDSHAKE, &msg);
            self.outgoing.extend_from_slice(&record);
        }
        let derived = hkdf_expand_label(&self.handshake_secret, "derived", &sha256(&[]), 32);
        let master = hkdf_extract(&derived, &[0; 32]);
        let client_ap = hkdf_expand_label(&master, "c ap traffic", &hash, 32);
        let server_ap = hkdf_expand_label(&master, "s ap traffic", &hash, 32);
        self.transcript.extend_from_slice(&msg);
        self.read = Some(Keys::new(&server_ap));
        self.write = Some(Keys::new(&client_ap));
        self.state = State::Connected;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(name: &str) -> Vec<u8> {
        let text = include_str!("../testdata/rfc8448-1rtt.txt");
        let hex = text.lines().find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(' '))).unwrap();
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    fn client() -> Client {
        let private: [u8; 32] = trace("client_private").try_into().unwrap();
        Client::with_hello(&trace("client_hello"), private)
    }

    #[test]
    fn rfc8448_simple_handshake() {
        let mut c = client();
        assert_eq!(c.take_outgoing(), trace("client_hello_record"));
        c.receive(&trace("server_hello_record")).unwrap();
        assert_eq!(c.handshake_secret.to_vec(), trace("handshake_secret"));
        assert_eq!(c.client_hs, trace("client_hs_traffic"));
        assert_eq!(c.server_hs, trace("server_hs_traffic"));
        c.receive(&trace("server_flight_record")).unwrap();
        assert!(c.is_connected());
        assert_eq!(c.certificates.len(), 1);
        assert_eq!(c.signature.as_ref().map(|s| s.0), Some(0x0804)); // RSA-PSS with SHA-256, checked
        assert_eq!(c.trust, Some(Trust::Unchecked("the clock is not set")));
        assert_eq!(c.take_outgoing(), trace("client_finished_record"));
        // The ticket is read and set aside; then data both ways.
        c.receive(&trace("ticket_record")).unwrap();
        c.send(&trace("client_data"));
        assert_eq!(c.take_outgoing(), trace("client_data_record"));
        c.receive(&trace("server_data_record")).unwrap();
        assert_eq!(c.take_plain(), trace("server_data"));
    }

    #[test]
    fn records_in_any_pieces() {
        let mut c = client();
        c.take_outgoing();
        let mut all = trace("server_hello_record");
        all.extend(trace("server_flight_record"));
        all.extend(trace("ticket_record"));
        all.extend(trace("server_data_record"));
        for b in all {
            c.receive(&[b]).unwrap();
        }
        assert!(c.is_connected());
        assert_eq!(c.take_outgoing(), trace("client_finished_record"));
        assert_eq!(c.take_plain(), trace("server_data"));
    }

    #[test]
    fn changes_are_refused() {
        let flight = trace("server_flight_record");
        for i in [5, 100, 400, flight.len() - 1] {
            let mut c = client();
            c.receive(&trace("server_hello_record")).unwrap();
            let mut bad = flight.clone();
            bad[i] ^= 1;
            assert_eq!(c.receive(&bad), Err(TlsError::BadRecord));
            assert!(!c.is_connected());
        }
        // Cut or damaged server hellos: errors, never a panic.
        let hello = trace("server_hello_record");
        for end in 0..hello.len() {
            let mut c = client();
            let _ = c.receive(&hello[..end]);
        }
        for i in 5..hello.len() {
            let mut c = client();
            let mut bad = hello.clone();
            bad[i] ^= 0x41;
            let _ = c.receive(&bad);
        }
        // The server's handshake signature, changed: refused. (Its last byte
        // sits just before the 16-byte tag and the Finished message.)
        let mut c = client();
        c.receive(&trace("server_hello_record")).unwrap();
        let keys = c.read.as_mut().unwrap();
        let (header, body) = flight.split_at(5);
        let (_, mut plain) = keys.open(header, body).unwrap();
        let cv = plain.windows(4).position(|w| w == [15, 0, 0, 0x84]).unwrap();
        plain[cv + 8] ^= 1;
        let mut c = client();
        c.receive(&trace("server_hello_record")).unwrap();
        c.handshake.extend_from_slice(&plain);
        assert_eq!(c.messages(), Err(TlsError::BadServerSignature));
        // A plain alert before the keys.
        let mut c = client();
        assert_eq!(c.receive(&[21, 3, 3, 0, 2, 2, 40]), Err(TlsError::Alert(40)));
    }

    #[test]
    fn hello_has_what_servers_need() {
        let hello = client_hello("example.com", &[7; 32], &[9; 32], true);
        assert_eq!(hello[0], 1);
        assert_eq!(u24_at(&hello, 1).unwrap(), hello.len() - 4);
        let has = |needle: &[u8]| hello.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"example.com"));
        assert!(has(&[0, 43, 0, 3, 2, 3, 4])); // TLS 1.3
        assert!(has(&[0, 51, 0, 38, 0, 36, 0, 0x1D, 0, 32, 9, 9])); // the key share
        // No server name for an address.
        assert!(!client_hello("10.0.2.2", &[7; 32], &[9; 32], true).windows(8).any(|w| w == b"10.0.2.2"));
        // Without ECDSA: RSA schemes only.
        let rsa_only = client_hello("example.com", &[7; 32], &[9; 32], false);
        assert!(!rsa_only.windows(2).any(|w| w == [4, 3]) && rsa_only.windows(2).any(|w| w == [8, 4]));
    }
}
