//! TFTP client (RFC 1350, network phase N5): fetch or store one file over
//! UDP in 512-byte blocks, binary ("octet") mode, no options.
//!
//! No sockets here. The caller opens a UDP port, then repeats: give received
//! datagrams to [`Transfer::receive`], call [`Transfer::poll`] for timeouts,
//! send what [`Transfer::transmit`] returns to the given server port.
//!
//! The first packet goes to the server's well-known port; the server answers
//! from a port of its own choosing, and the rest of the transfer uses that
//! one. Datagrams from any other port are ignored.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

pub const PORT: u16 = 69;
pub const BLOCK: usize = 512;
/// Resend the last packet after this long without an answer...
pub const RETRY_MS: u64 = 1000;
/// ...this many times, then give up.
pub const MAX_RETRIES: u32 = 5;

const OP_RRQ: u16 = 1;
const OP_WRQ: u16 = 2;
const OP_DATA: u16 = 3;
const OP_ACK: u16 = 4;
const OP_ERROR: u16 = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TftpError {
    /// The name can't be sent (empty, too long, or contains a zero byte).
    BadName,
    TimedOut,
    /// The server refused; its error code (1 = file not found, 2 = access
    /// denied, 3 = disk full, 6 = file exists, ...).
    Server(u16),
    /// The file is larger than the caller's limit.
    TooLarge,
    /// The server sent something that makes no sense here.
    Protocol,
}

impl TftpError {
    pub fn message(self) -> &'static str {
        match self {
            TftpError::BadName => "not a usable file name",
            TftpError::TimedOut => "no answer from the server (timed out)",
            TftpError::Server(1) => "the server says: file not found",
            TftpError::Server(2) => "the server says: access denied",
            TftpError::Server(3) => "the server says: disk full",
            TftpError::Server(6) => "the server says: the file already exists",
            TftpError::Server(_) => "the server refused the transfer",
            TftpError::TooLarge => "the file is too large",
            TftpError::Protocol => "the server's answer makes no sense",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Done,
    Failed(TftpError),
}

enum Kind {
    /// Fetching: received bytes not yet taken by the caller, and the limit.
    Get { received: Vec<u8>, limit: u64 },
    /// Storing: the whole file.
    Put { data: Vec<u8> },
}

pub struct Transfer {
    kind: Kind,
    status: Status,
    /// The server's port for this transfer, once it has answered.
    server_port: Option<u16>,
    first_port: u16,
    /// Get: the block we expect next. Put: the block we sent last (0 = the request).
    block: u16,
    /// The last packet sent, for retransmission.
    last: Vec<u8>,
    deadline_ms: u64,
    retries: u32,
    out: VecDeque<(u16, Vec<u8>)>,
    /// File bytes received or acknowledged so far.
    bytes: u64,
}

fn request_packet(op: u16, name: &str) -> Result<Vec<u8>, TftpError> {
    if name.is_empty() || name.len() > 255 || name.bytes().any(|b| b == 0) {
        return Err(TftpError::BadName);
    }
    let mut p = Vec::from(op.to_be_bytes());
    p.extend_from_slice(name.as_bytes());
    p.push(0);
    p.extend_from_slice(b"octet\0");
    Ok(p)
}

fn ack_packet(block: u16) -> Vec<u8> {
    [OP_ACK.to_be_bytes(), block.to_be_bytes()].concat()
}

fn error_packet(code: u16, text: &str) -> Vec<u8> {
    let mut p = [OP_ERROR.to_be_bytes(), code.to_be_bytes()].concat();
    p.extend_from_slice(text.as_bytes());
    p.push(0);
    p
}

impl Transfer {
    fn start(kind: Kind, request: Vec<u8>, server_port: u16, now_ms: u64) -> Transfer {
        let mut t = Transfer {
            kind,
            status: Status::Running,
            server_port: None,
            first_port: server_port,
            block: 0,
            last: Vec::new(),
            deadline_ms: 0,
            retries: 0,
            out: VecDeque::new(),
            bytes: 0,
        };
        t.send(request, now_ms);
        t
    }

    /// Fetch `name` from the server listening on `server_port`. A file larger
    /// than `limit` bytes fails with `TooLarge`.
    pub fn get(name: &str, limit: u64, server_port: u16, now_ms: u64) -> Result<Transfer, TftpError> {
        let request = request_packet(OP_RRQ, name)?;
        let mut t = Transfer::start(Kind::Get { received: Vec::new(), limit }, request, server_port, now_ms);
        t.block = 1;
        Ok(t)
    }

    /// Store `data` on the server as `name`.
    pub fn put(name: &str, data: Vec<u8>, server_port: u16, now_ms: u64) -> Result<Transfer, TftpError> {
        if data.len() / BLOCK + 1 > u16::MAX as usize {
            return Err(TftpError::TooLarge); // block numbers are 16 bits
        }
        let request = request_packet(OP_WRQ, name)?;
        Ok(Transfer::start(Kind::Put { data }, request, server_port, now_ms))
    }

    pub fn status(&self) -> Status {
        self.status
    }

    /// File bytes transferred so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Get: the file bytes received since the last call, in order.
    pub fn take_received(&mut self) -> Vec<u8> {
        match &mut self.kind {
            Kind::Get { received, .. } => core::mem::take(received),
            Kind::Put { .. } => Vec::new(),
        }
    }

    /// The next datagram to send: (server port, data).
    pub fn transmit(&mut self) -> Option<(u16, Vec<u8>)> {
        self.out.pop_front()
    }

    fn port(&self) -> u16 {
        self.server_port.unwrap_or(self.first_port)
    }

    /// Send a packet that expects an answer (it is resent on a timeout).
    fn send(&mut self, packet: Vec<u8>, now_ms: u64) {
        self.out.push_back((self.port(), packet.clone()));
        self.last = packet;
        self.deadline_ms = now_ms + RETRY_MS;
        self.retries = 0;
    }

    fn fail(&mut self, error: TftpError) {
        self.status = Status::Failed(error);
    }

    /// Give up (Ctrl+C): tells the server, so it does not keep waiting.
    pub fn cancel(&mut self) {
        if self.status == Status::Running {
            if self.server_port.is_some() {
                self.out.push_back((self.port(), error_packet(0, "cancelled")));
            }
            self.fail(TftpError::TimedOut);
        }
    }

    /// Timeouts: resend the last packet, or give up.
    pub fn poll(&mut self, now_ms: u64) {
        if self.status != Status::Running || now_ms < self.deadline_ms {
            return;
        }
        if self.retries >= MAX_RETRIES {
            return self.fail(TftpError::TimedOut);
        }
        self.retries += 1;
        self.deadline_ms = now_ms + RETRY_MS;
        self.out.push_back((self.port(), self.last.clone()));
    }

    /// A datagram from the server's address arrived from `src_port`.
    pub fn receive(&mut self, src_port: u16, packet: &[u8], now_ms: u64) {
        if self.status != Status::Running || self.server_port.is_some_and(|p| p != src_port) || packet.len() < 4 {
            return;
        }
        let op = u16::from_be_bytes([packet[0], packet[1]]);
        let number = u16::from_be_bytes([packet[2], packet[3]]);
        let payload = &packet[4..];
        if op == OP_ERROR {
            return self.fail(TftpError::Server(number));
        }
        let expected_op = if matches!(self.kind, Kind::Get { .. }) { OP_DATA } else { OP_ACK };
        if op != expected_op || payload.len() > BLOCK {
            self.server_port.get_or_insert(src_port);
            self.out.push_back((self.port(), error_packet(4, "illegal operation")));
            return self.fail(TftpError::Protocol);
        }
        match &mut self.kind {
            Kind::Get { received, limit } => {
                if number != self.block {
                    // An old block again (our ACK was lost or slow). It is not
                    // acknowledged again: the timeout resends our last ACK.
                    return;
                }
                self.server_port = Some(src_port);
                if self.bytes + payload.len() as u64 > *limit {
                    self.out.push_back((src_port, error_packet(3, "file too large")));
                    return self.fail(TftpError::TooLarge);
                }
                received.extend_from_slice(payload);
                self.bytes += payload.len() as u64;
                if payload.len() < BLOCK {
                    // The last block: acknowledge it and finish. (If this ACK is
                    // lost the server retries for a while and gives up; the file is here.)
                    self.out.push_back((src_port, ack_packet(number)));
                    self.status = Status::Done;
                } else if number == u16::MAX {
                    self.out.push_back((src_port, error_packet(3, "file too large")));
                    self.fail(TftpError::TooLarge);
                } else {
                    self.block = number + 1;
                    self.send(ack_packet(number), now_ms);
                }
            }
            Kind::Put { data } => {
                if number != self.block {
                    return; // an old ACK again: never answer it with data (it would double every block)
                }
                self.server_port = Some(src_port);
                let sent = (self.block as usize * BLOCK).min(data.len());
                self.bytes = sent as u64;
                // The transfer ends with a block shorter than 512 bytes (possibly empty).
                if self.block > 0 && data.len() < self.block as usize * BLOCK {
                    self.status = Status::Done;
                    return;
                }
                self.block += 1;
                let chunk = &data[sent..(sent + BLOCK).min(data.len())];
                let mut packet = [OP_DATA.to_be_bytes(), self.block.to_be_bytes()].concat();
                packet.extend_from_slice(chunk);
                self.send(packet, now_ms);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SERVER_TID: u16 = 40000;

    /// A small TFTP server: files in memory, answers from its own port.
    #[derive(Default)]
    struct Server {
        files: HashMap<String, Vec<u8>>,
        /// (name, data so far, blocks stored) of the file being written.
        writing: Option<(String, Vec<u8>, u16)>,
        reading: Option<Vec<u8>>,
        /// Datagrams received, for inspection.
        seen: Vec<Vec<u8>>,
    }

    impl Server {
        fn data(file: &[u8], block: u16) -> Vec<u8> {
            let start = (block as usize - 1) * BLOCK;
            let mut p = [OP_DATA.to_be_bytes(), block.to_be_bytes()].concat();
            p.extend_from_slice(&file[start.min(file.len())..(start + BLOCK).min(file.len())]);
            p
        }

        fn handle(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
            self.seen.push(packet.to_vec());
            let op = u16::from_be_bytes([packet[0], packet[1]]);
            let number = u16::from_be_bytes([packet[2], packet[3]]);
            match op {
                OP_RRQ | OP_WRQ => {
                    let mut fields = packet[2..].split(|b| *b == 0);
                    let name = String::from_utf8(fields.next().unwrap().to_vec()).unwrap();
                    assert_eq!(fields.next().unwrap(), b"octet");
                    if op == OP_WRQ {
                        self.writing = Some((name, Vec::new(), 0));
                        return Some(ack_packet(0));
                    }
                    match self.files.get(&name) {
                        Some(file) => {
                            self.reading = Some(file.clone());
                            Some(Self::data(file, 1))
                        }
                        None => Some(error_packet(1, "File not found")),
                    }
                }
                OP_ACK => {
                    let file = self.reading.as_ref()?;
                    (number as usize * BLOCK <= file.len()).then(|| Self::data(file, number + 1))
                }
                OP_DATA => {
                    let (name, data, blocks) = self.writing.as_mut()?;
                    if number == *blocks + 1 {
                        *blocks = number;
                        data.extend_from_slice(&packet[4..]);
                        if packet.len() - 4 < BLOCK {
                            let (name, data) = (name.clone(), data.clone());
                            self.files.insert(name, data);
                        }
                    }
                    Some(ack_packet(number))
                }
                _ => None,
            }
        }
    }

    /// Run a transfer against the server; `lose(n)` says whether the n-th
    /// datagram (either direction) is lost. Returns the bytes fetched.
    fn run(t: &mut Transfer, server: &mut Server, lose: impl Fn(u32) -> bool) -> Vec<u8> {
        let (mut now, mut n, mut got) = (0u64, 0u32, Vec::new());
        for _ in 0..200_000 {
            let mut busy = false;
            while let Some((port, packet)) = t.transmit() {
                busy = true;
                let first = matches!(u16::from_be_bytes([packet[0], packet[1]]), OP_RRQ | OP_WRQ);
                assert_eq!(port, if first { PORT } else { SERVER_TID });
                n += 1;
                if lose(n) {
                    continue;
                }
                if let Some(answer) = server.handle(&packet) {
                    n += 1;
                    if !lose(n) {
                        t.receive(SERVER_TID, &answer, now);
                    }
                }
            }
            got.extend(t.take_received());
            if t.status() != Status::Running {
                break;
            }
            if !busy {
                now += 100;
                t.poll(now);
            }
        }
        got
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 512) as u8).collect()
    }

    #[test]
    fn get_files_of_every_awkward_size() {
        for len in [0, 1, 511, 512, 513, 1024, 5000] {
            let mut server = Server::default();
            server.files.insert("f.bin".into(), pattern(len));
            let mut t = Transfer::get("f.bin", 1 << 20, PORT, 0).unwrap();
            let got = run(&mut t, &mut server, |_| false);
            assert_eq!(t.status(), Status::Done, "{len} bytes");
            assert_eq!(got, pattern(len), "{len} bytes");
            assert_eq!(t.bytes(), len as u64);
            assert_eq!(server.seen[0], b"\x00\x01f.bin\x00octet\x00");
            assert_eq!(server.seen.len(), len / BLOCK + 2, "one request and one ACK per block, {len} bytes");
        }
    }

    #[test]
    fn put_files_of_every_awkward_size() {
        for len in [0, 1, 511, 512, 513, 1024, 5000] {
            let mut server = Server::default();
            let mut t = Transfer::put("up.bin", pattern(len), PORT, 0).unwrap();
            run(&mut t, &mut server, |_| false);
            assert_eq!(t.status(), Status::Done, "{len} bytes");
            assert_eq!(server.files.get("up.bin"), Some(&pattern(len)), "{len} bytes");
            assert_eq!(t.bytes(), len as u64);
            assert_eq!(server.seen[0], b"\x00\x02up.bin\x00octet\x00");
            assert_eq!(server.seen.len(), len / BLOCK + 2, "{len} bytes");
        }
    }

    #[test]
    fn lost_datagrams_are_resent() {
        for (every, len) in [(3, 20_000), (4, 20_000), (5, 3000), (7, 512)] {
            let mut server = Server::default();
            server.files.insert("f".into(), pattern(len));
            let mut t = Transfer::get("f", 1 << 20, PORT, 0).unwrap();
            let got = run(&mut t, &mut server, |n| n % every == 0);
            assert_eq!((t.status(), got == pattern(len)), (Status::Done, true), "get, every {every}th lost");

            let mut t = Transfer::put("up", pattern(len), PORT, 0).unwrap();
            run(&mut t, &mut server, |n| n % every == 1);
            assert_eq!(t.status(), Status::Done, "put, every {every}th lost");
            assert_eq!(server.files.get("up"), Some(&pattern(len)));
        }
    }

    #[test]
    fn a_silent_server_times_out() {
        let mut t = Transfer::get("f", 1000, PORT, 0).unwrap();
        let mut sent = 0;
        let mut now = 0;
        while t.status() == Status::Running {
            while let Some((port, _)) = t.transmit() {
                assert_eq!(port, PORT);
                sent += 1;
            }
            now += 100;
            t.poll(now);
        }
        assert_eq!(t.status(), Status::Failed(TftpError::TimedOut));
        assert_eq!(sent, 1 + MAX_RETRIES);
        assert_eq!(now, (MAX_RETRIES as u64 + 1) * RETRY_MS);
    }

    #[test]
    fn server_errors_and_limits() {
        let mut server = Server::default();
        let mut t = Transfer::get("missing", 1000, PORT, 0).unwrap();
        run(&mut t, &mut server, |_| false);
        assert_eq!(t.status(), Status::Failed(TftpError::Server(1)));

        server.files.insert("big".into(), pattern(2000));
        let mut t = Transfer::get("big", 1999, PORT, 0).unwrap();
        let got = run(&mut t, &mut server, |_| false);
        assert_eq!(t.status(), Status::Failed(TftpError::TooLarge));
        assert!(got.len() <= 1999);
        assert_eq!(&server.seen.last().unwrap()[..4], &[0, 5, 0, 3], "the server is told");

        assert_eq!(Transfer::get("", 1, PORT, 0).err(), Some(TftpError::BadName));
        assert_eq!(Transfer::get("a\0b", 1, PORT, 0).err(), Some(TftpError::BadName));
        assert_eq!(Transfer::put(&"n".repeat(256), Vec::new(), PORT, 0).err(), Some(TftpError::BadName));
        assert_eq!(Transfer::put("f", vec![0; BLOCK * 65535], PORT, 0).err(), Some(TftpError::TooLarge));
    }

    #[test]
    fn strangers_duplicates_and_nonsense() {
        let file = pattern(1500);
        let mut t = Transfer::get("f", 1 << 20, PORT, 0).unwrap();
        t.transmit().unwrap();
        t.receive(SERVER_TID, &[0, 3, 0], 0); // too short
        t.receive(SERVER_TID, &Server::data(&file, 2), 0); // not the block we wait for
        assert!(t.transmit().is_none());
        t.receive(SERVER_TID, &Server::data(&file, 1), 0);
        assert_eq!(t.transmit(), Some((SERVER_TID, ack_packet(1))));
        // Another port is not our server, whatever it sends.
        t.receive(SERVER_TID + 1, &Server::data(&file, 2), 0);
        t.receive(SERVER_TID + 1, &error_packet(1, "no"), 0);
        // A repeated block is not acknowledged again and not stored twice.
        t.receive(SERVER_TID, &Server::data(&file, 1), 0);
        assert!(t.transmit().is_none());
        assert_eq!(t.status(), Status::Running);
        t.receive(SERVER_TID, &Server::data(&file, 2), 0);
        t.receive(SERVER_TID, &Server::data(&file, 3), 0);
        assert_eq!(t.status(), Status::Done);
        assert_eq!(t.take_received(), file);
        t.receive(SERVER_TID, &Server::data(&file, 3), 0); // after the end: ignored
        assert!(t.take_received().is_empty());

        // An ACK where data is expected, and an oversized block.
        let mut t = Transfer::get("f", 1 << 20, PORT, 0).unwrap();
        t.receive(SERVER_TID, &ack_packet(1), 0);
        assert_eq!(t.status(), Status::Failed(TftpError::Protocol));
        let mut t = Transfer::get("f", 1 << 20, PORT, 0).unwrap();
        t.receive(SERVER_TID, &[&[0, 3, 0, 1][..], &[0u8; BLOCK + 1]].concat(), 0);
        assert_eq!(t.status(), Status::Failed(TftpError::Protocol));

        // Put: a repeated ACK does not send the next block twice.
        let mut t = Transfer::put("f", pattern(2000), PORT, 0).unwrap();
        t.transmit().unwrap();
        t.receive(SERVER_TID, &ack_packet(0), 0);
        t.receive(SERVER_TID, &ack_packet(0), 0);
        assert_eq!(t.transmit().map(|(_, p)| p[..4].to_vec()), Some(vec![0, 3, 0, 1]));
        assert!(t.transmit().is_none());
    }

    #[test]
    fn cancel_tells_the_server() {
        let mut t = Transfer::get("f", 1 << 20, PORT, 0).unwrap();
        t.transmit().unwrap();
        t.receive(SERVER_TID, &Server::data(&pattern(600), 1), 0);
        t.transmit().unwrap();
        t.cancel();
        assert_eq!(t.transmit(), Some((SERVER_TID, error_packet(0, "cancelled"))));
        assert_ne!(t.status(), Status::Running);
    }
}
