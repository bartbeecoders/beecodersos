//! SNTP client (RFC 4330, network phase N5): one request, one reply over UDP,
//! to set the clock. No sockets here: the caller sends and receives.

pub const PORT: u16 = 123;
pub const PACKET_LEN: usize = 48;
/// Seconds between 1900-01-01 (NTP's start) and 1970-01-01 (Unix's).
const NTP_TO_UNIX: u64 = 2_208_988_800;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SntpError {
    /// Too short, not a server reply, or not an answer to our request.
    BadReply,
    /// The server has no usable time itself (or told us to go away).
    NotSynchronised,
}

impl SntpError {
    pub fn message(self) -> &'static str {
        match self {
            SntpError::BadReply => "the answer is not a valid time reply",
            SntpError::NotSynchronised => "the server does not know the time itself",
        }
    }
}

/// What a server said the time was when it sent its reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ServerTime {
    /// Seconds since 1970-01-01 UTC.
    pub unix_secs: u64,
    pub millis: u32,
    /// Distance from a reference clock: 1 = directly attached.
    pub stratum: u8,
}

/// A request. `nonce` (any random number) comes back in the reply, which
/// proves the reply answers this request.
pub fn request(nonce: u64) -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    p[0] = 4 << 3 | 3; // no leap warning, version 4, mode 3 (client)
    p[40..48].copy_from_slice(&nonce.to_be_bytes()); // transmit timestamp
    p
}

pub fn parse_reply(reply: &[u8], nonce: u64) -> Result<ServerTime, SntpError> {
    if reply.len() < PACKET_LEN {
        return Err(SntpError::BadReply);
    }
    let (leap, version, mode, stratum) = (reply[0] >> 6, reply[0] >> 3 & 7, reply[0] & 7, reply[1]);
    if mode != 4 || !(1..=4).contains(&version) || reply[24..32] != nonce.to_be_bytes() {
        return Err(SntpError::BadReply);
    }
    let secs = u32::from_be_bytes([reply[40], reply[41], reply[42], reply[43]]) as u64;
    let fraction = u32::from_be_bytes([reply[44], reply[45], reply[46], reply[47]]) as u64;
    if leap == 3 || stratum == 0 || stratum > 15 || (secs == 0 && fraction == 0) {
        return Err(SntpError::NotSynchronised);
    }
    // The 32-bit seconds wrap in 2036: small values belong to the next era.
    let ntp_secs = if secs & 0x8000_0000 == 0 { secs + (1 << 32) } else { secs };
    Ok(ServerTime { unix_secs: ntp_secs - NTP_TO_UNIX, millis: (fraction * 1000 >> 32) as u32, stratum })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: u64 = 0x0123_4567_89AB_CDEF;

    /// A server reply for Unix time `unix` + `millis`.
    fn reply(unix: u64, millis: u32) -> [u8; PACKET_LEN] {
        let mut p = [0u8; PACKET_LEN];
        p[0] = 4 << 3 | 4;
        p[1] = 2;
        p[24..32].copy_from_slice(&NONCE.to_be_bytes());
        p[40..44].copy_from_slice(&((unix + NTP_TO_UNIX) as u32).to_be_bytes());
        p[44..48].copy_from_slice(&(((millis as u64) << 32) / 1000).to_be_bytes()[4..]);
        p
    }

    #[test]
    fn request_layout() {
        let p = request(NONCE);
        assert_eq!(p[0], 0x23);
        assert!(p[1..40].iter().all(|b| *b == 0));
        assert_eq!(p[40..48], NONCE.to_be_bytes());
    }

    #[test]
    fn reply_gives_the_time() {
        // 2026-11-14T12:00:00Z
        let t = parse_reply(&reply(1_794_657_600, 500), NONCE).unwrap();
        assert_eq!((t.unix_secs, t.stratum), (1_794_657_600, 2));
        assert!((499..=500).contains(&t.millis));
    }

    #[test]
    fn times_after_2036_use_the_next_era() {
        let unix = 2_200_000_000; // 2039
        assert!((unix + NTP_TO_UNIX) > u32::MAX as u64);
        assert_eq!(parse_reply(&reply(unix, 0), NONCE).unwrap().unix_secs, unix);
    }

    #[test]
    fn bad_replies_are_refused() {
        let good = reply(1_794_657_600, 0);
        for len in 0..PACKET_LEN {
            assert_eq!(parse_reply(&good[..len], NONCE), Err(SntpError::BadReply), "length {len}");
        }
        assert_eq!(parse_reply(&good, NONCE + 1), Err(SntpError::BadReply)); // answers another request
        let mut p = good;
        p[0] = 4 << 3 | 3; // a client packet, not a reply
        assert_eq!(parse_reply(&p, NONCE), Err(SntpError::BadReply));
        p[0] = 4; // version 0
        assert_eq!(parse_reply(&p, NONCE), Err(SntpError::BadReply));
        let mut p = good;
        p[1] = 0; // "kiss of death"
        assert_eq!(parse_reply(&p, NONCE), Err(SntpError::NotSynchronised));
        p[1] = 16;
        assert_eq!(parse_reply(&p, NONCE), Err(SntpError::NotSynchronised));
        let mut p = good;
        p[0] |= 0xC0; // clock not synchronised
        assert_eq!(parse_reply(&p, NONCE), Err(SntpError::NotSynchronised));
        let mut p = good;
        p[40..48].fill(0);
        assert_eq!(parse_reply(&p, NONCE), Err(SntpError::NotSynchronised));
    }
}
