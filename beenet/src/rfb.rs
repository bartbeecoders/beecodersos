//! RFB, the VNC remote desktop protocol (RFC 6143), server side: the
//! handshake, reading what a viewer sends, and the bytes of what the server
//! sends back. No sockets: the kernel (vnc.rs) moves the bytes.
//!
//! Only what is needed to show a screen and take a keyboard and mouse:
//! protocol versions 3.3, 3.7 and 3.8, "VNC Authentication" (a DES
//! challenge: it keeps strangers out, but the session is NOT encrypted),
//! true-color pixel formats of 8, 16 or 32 bits, and the Raw encoding.
//! A viewer is untrusted: every length is checked and every message is
//! bounded before anything is kept.

use alloc::string::String;
use alloc::vec::Vec;

/// What the server says first.
pub const SERVER_VERSION: &[u8; 12] = b"RFB 003.008\n";
/// The one security type offered: VNC Authentication.
pub const SECURITY_VNC_AUTH: u8 = 2;
/// Raw: the pixels of a rectangle, line by line.
pub const ENCODING_RAW: i32 = 0;
/// Pseudo-encoding: the viewer can follow a change of the screen's size.
pub const ENCODING_DESKTOP_SIZE: i32 = -223;
/// At most this many encodings are kept from a SetEncodings message.
const MAX_ENCODINGS: usize = 64;

/// The minor version a viewer asked for: 3, 7 or 8 (3.8 for anything newer).
pub fn parse_version(reply: &[u8]) -> Option<u8> {
    let text = core::str::from_utf8(reply.get(..12)?).ok()?;
    let rest = text.strip_prefix("RFB 003.")?.strip_suffix('\n')?;
    if rest.len() != 3 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match rest.parse::<u32>().ok()? {
        3..=6 => Some(3), // 3.4 and 3.6 are 3.3 with another number
        7 => Some(7),
        8.. => Some(8),
        _ => None,
    }
}

// --- pixel formats ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// What the server offers: 32 bits, 8 per color, 0x00RRGGBB little endian.
    pub const SERVER: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_color: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    };

    pub fn encode(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = self.bits_per_pixel;
        b[1] = self.depth;
        b[2] = self.big_endian as u8;
        b[3] = self.true_color as u8;
        b[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        b[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        b[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        b[10] = self.red_shift;
        b[11] = self.green_shift;
        b[12] = self.blue_shift;
        b
    }

    fn decode(b: &[u8]) -> PixelFormat {
        PixelFormat {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_color: b[3] != 0,
            red_max: u16::from_be_bytes([b[4], b[5]]),
            green_max: u16::from_be_bytes([b[6], b[7]]),
            blue_max: u16::from_be_bytes([b[8], b[9]]),
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        }
    }

    /// Can this server send pixels in this format?
    pub fn supported(&self) -> bool {
        let fits = |max: u16, shift: u8| shift < 32 && (max as u64) << shift <= u32::MAX as u64;
        self.true_color
            && matches!(self.bits_per_pixel, 8 | 16 | 32)
            && fits(self.red_max, self.red_shift)
            && fits(self.green_max, self.green_shift)
            && fits(self.blue_max, self.blue_shift)
    }

    pub fn bytes(&self) -> usize {
        self.bits_per_pixel as usize / 8
    }

    /// One 0xRRGGBB pixel in this format, appended to `out`.
    pub fn put(&self, rgb: u32, out: &mut Vec<u8>) {
        let scale = |value: u32, max: u16| value * max as u32 / 255;
        let v = scale(rgb >> 16 & 0xFF, self.red_max) << self.red_shift
            | scale(rgb >> 8 & 0xFF, self.green_max) << self.green_shift
            | scale(rgb & 0xFF, self.blue_max) << self.blue_shift;
        match (self.bits_per_pixel, self.big_endian) {
            (8, _) => out.push(v as u8),
            (16, false) => out.extend_from_slice(&(v as u16).to_le_bytes()),
            (16, true) => out.extend_from_slice(&(v as u16).to_be_bytes()),
            (_, false) => out.extend_from_slice(&v.to_le_bytes()),
            (_, true) => out.extend_from_slice(&v.to_be_bytes()),
        }
    }
}

/// The ServerInit message: the screen's size, the pixel format, a name.
pub fn server_init(width: u16, height: u16, name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&PixelFormat::SERVER.encode());
    out.extend_from_slice(&(name.len() as u32).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

/// The start of a FramebufferUpdate with `rects` rectangles.
pub fn update_header(rects: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&rects.to_be_bytes());
}

/// The header of one rectangle; for Raw, the pixels follow.
pub fn rect_header(x: u16, y: u16, w: u16, h: u16, encoding: i32, out: &mut Vec<u8>) {
    for v in [x, y, w, h] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(&encoding.to_be_bytes());
}

/// SecurityResult: 0 = OK; a failure carries a reason (3.8 only).
pub fn security_result(ok: bool, minor: u8, reason: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&(if ok { 0u32 } else { 1u32 }).to_be_bytes());
    if !ok && minor >= 8 {
        out.extend_from_slice(&(reason.len() as u32).to_be_bytes());
        out.extend_from_slice(reason.as_bytes());
    }
}

// --- what a viewer sends --------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum ClientMessage {
    SetPixelFormat(PixelFormat),
    SetEncodings(Vec<i32>),
    UpdateRequest { incremental: bool, x: u16, y: u16, w: u16, h: u16 },
    /// An X11 keysym went down (true) or up.
    Key { down: bool, keysym: u32 },
    /// Buttons (bit 0: left) and where the pointer is.
    Pointer { buttons: u8, x: u16, y: u16 },
    /// Text from the viewer's clipboard (not kept: only skipped).
    CutText,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RfbError {
    UnknownMessage(u8),
}

/// Reads a viewer's messages from a stream of bytes that may arrive in
/// pieces of any size.
#[derive(Default)]
pub struct ClientReader {
    buf: Vec<u8>,
    /// Bytes of clipboard text still to skip.
    skip: u64,
}

impl ClientReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take in bytes; returns the messages that are complete.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<ClientMessage>, RfbError> {
        let mut out = Vec::new();
        let mut data = data;
        loop {
            // Skip what is being skipped (the end of a long clipboard or list).
            if self.skip > 0 {
                let n = (self.skip as usize).min(data.len());
                self.skip -= n as u64;
                data = &data[n..];
            }
            // Take in at most what the largest kept message needs, so the
            // buffer stays small whatever the viewer sends.
            let room = (8 + MAX_ENCODINGS * 4).saturating_sub(self.buf.len());
            let n = room.min(data.len());
            self.buf.extend_from_slice(&data[..n]);
            data = &data[n..];
            match self.next()? {
                Some((msg, used)) => {
                    self.buf.drain(..used);
                    out.extend(msg);
                }
                // Not complete yet: everything is in the buffer, or more is
                // needed than it holds (impossible: see `room`).
                None if data.is_empty() => return Ok(out),
                None => {}
            }
            if data.is_empty() && self.buf.is_empty() && self.skip == 0 {
                return Ok(out);
            }
        }
    }

    /// The first complete message in `buf` and how many bytes it took;
    /// `None` if it is not all there yet. A message that is only skipped
    /// gives `Some((None, used))`.
    fn next(&mut self) -> Result<Option<(Option<ClientMessage>, usize)>, RfbError> {
        let b = &self.buf;
        let Some(&kind) = b.first() else { return Ok(None) };
        let need = |n: usize| b.len() >= n;
        Ok(match kind {
            0 if need(20) => Some((Some(ClientMessage::SetPixelFormat(PixelFormat::decode(&b[4..20]))), 20)),
            2 if need(4) => {
                let count = u16::from_be_bytes([b[2], b[3]]) as usize;
                let len = 4 + count * 4;
                if b.len() < len.min(4 + MAX_ENCODINGS * 4) {
                    None
                } else {
                    let kept = count.min(MAX_ENCODINGS);
                    let list = (0..kept).map(|i| i32::from_be_bytes(b[4 + i * 4..8 + i * 4].try_into().unwrap())).collect();
                    // The part of a longer list that is not kept is skipped.
                    let available = b.len().min(len);
                    self.skip = (len - available) as u64;
                    Some((Some(ClientMessage::SetEncodings(list)), available))
                }
            }
            3 if need(10) => {
                let at = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
                Some((Some(ClientMessage::UpdateRequest { incremental: b[1] != 0, x: at(2), y: at(4), w: at(6), h: at(8) }), 10))
            }
            4 if need(8) => Some((Some(ClientMessage::Key { down: b[1] != 0, keysym: u32::from_be_bytes([b[4], b[5], b[6], b[7]]) }), 8)),
            5 if need(6) => {
                Some((Some(ClientMessage::Pointer { buttons: b[1], x: u16::from_be_bytes([b[2], b[3]]), y: u16::from_be_bytes([b[4], b[5]]) }), 6))
            }
            6 if need(8) => {
                let len = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as u64;
                let available = (b.len() - 8) as u64;
                let taken = available.min(len);
                self.skip = len - taken;
                Some((Some(ClientMessage::CutText), 8 + taken as usize))
            }
            0 | 2..=6 => None,
            other => return Err(RfbError::UnknownMessage(other)),
        })
    }
}

// --- keys ------------------------------------------------------------------------------------

/// What an X11 keysym (from a key going down) means to the OS.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum KeyName {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Delete,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    /// Shift, Control, Alt and the like: they only change other keys.
    Modifier(Modifier),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Modifier {
    Shift,
    Control,
    Alt,
    /// Super (the Windows key): the desktop's shortcuts.
    Super,
    Other,
}

pub fn keysym(sym: u32) -> Option<KeyName> {
    Some(match sym {
        0x20..=0x7E | 0xA0..=0xFF => KeyName::Char(char::from_u32(sym)?),
        // Unicode keysyms: 0x01000000 + code point.
        0x0100_00A0..=0x0110_FFFF => KeyName::Char(char::from_u32(sym - 0x0100_0000)?),
        0xFF0D | 0xFF8D => KeyName::Enter,
        0xFF08 => KeyName::Backspace,
        0xFF09 => KeyName::Tab,
        0xFF1B => KeyName::Escape,
        0xFFFF => KeyName::Delete,
        0xFF63 => KeyName::Insert,
        0xFF51 => KeyName::Left,
        0xFF52 => KeyName::Up,
        0xFF53 => KeyName::Right,
        0xFF54 => KeyName::Down,
        0xFF50 => KeyName::Home,
        0xFF57 => KeyName::End,
        0xFF55 => KeyName::PageUp,
        0xFF56 => KeyName::PageDown,
        0xFFE1 | 0xFFE2 => KeyName::Modifier(Modifier::Shift),
        0xFFE3 | 0xFFE4 => KeyName::Modifier(Modifier::Control),
        0xFFE9 | 0xFFEA | 0xFFE7 | 0xFFE8 => KeyName::Modifier(Modifier::Alt),
        0xFFEB | 0xFFEC => KeyName::Modifier(Modifier::Super),
        0xFFE5..=0xFFEE => KeyName::Modifier(Modifier::Other),
        _ => return None,
    })
}

// --- VNC Authentication: DES ------------------------------------------------------------------

/// The answer to a VNC Authentication challenge for this password: the
/// challenge's two 8-byte halves encrypted with DES, the key being the
/// password's first 8 bytes (zero-padded), each byte's bits mirrored.
pub fn auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let keys = des_subkeys(u64::from_be_bytes(key));
    let mut out = [0u8; 16];
    for half in 0..2 {
        let block = u64::from_be_bytes(challenge[half * 8..half * 8 + 8].try_into().unwrap());
        out[half * 8..half * 8 + 8].copy_from_slice(&des_encrypt(block, &keys).to_be_bytes());
    }
    out
}

/// Is `response` the right answer? Compares every byte, whatever differs first.
pub fn auth_ok(password: &str, challenge: &[u8; 16], response: &[u8]) -> bool {
    let right = auth_response(password, challenge);
    response.len() == 16 && right.iter().zip(response).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6, 64, 56, 48, 40, 32, 24, 16, 8,
    57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30, 37, 5, 45, 13, 53, 21, 61, 29,
    36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27, 34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18, 19, 20, 21, 20, 21, 22, 23,
    24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
const P: [u8; 32] = [16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19, 13, 30, 6, 22, 11, 4, 25];
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60, 52, 44, 36, 63, 55, 47, 39,
    31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 28, 20, 12, 4,
];
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52, 31, 37, 47, 55, 30, 40, 51,
    45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
const S: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8, 4, 1, 14, 8, 13,
        6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5, 0, 14, 7, 11,
        10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1, 13, 6, 4, 9,
        8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9, 10, 6, 9, 0,
        12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6, 4, 2, 1, 11,
        10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8, 9, 14, 15, 5,
        2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6, 1, 4, 11, 13,
        12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2, 7, 11, 4, 1,
        9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// Take bits from `input` (`width` bits wide, bit 1 = the highest) in the order `table` lists.
fn permute(input: u64, width: u32, table: &[u8]) -> u64 {
    table.iter().fold(0u64, |out, &pos| out << 1 | (input >> (width - pos as u32)) & 1)
}

fn des_subkeys(key: u64) -> [u64; 16] {
    let pc1 = permute(key, 64, &PC1);
    let (mut c, mut d) = ((pc1 >> 28) & 0x0FFF_FFFF, pc1 & 0x0FFF_FFFF);
    let mut keys = [0u64; 16];
    for (k, &shift) in keys.iter_mut().zip(SHIFTS.iter()) {
        let rotate = |v: u64| ((v << shift) | (v >> (28 - shift as u32))) & 0x0FFF_FFFF;
        c = rotate(c);
        d = rotate(d);
        *k = permute(c << 28 | d, 56, &PC2);
    }
    keys
}

fn des_encrypt(block: u64, keys: &[u64; 16]) -> u64 {
    let ip = permute(block, 64, &IP);
    let (mut l, mut r) = (ip >> 32, ip & 0xFFFF_FFFF);
    for k in keys {
        let x = permute(r, 32, &E) ^ k;
        let mut f = 0u64;
        for (i, sbox) in S.iter().enumerate() {
            let six = (x >> (42 - 6 * i)) & 0x3F;
            let row = ((six & 0x20) >> 4) | (six & 1);
            let col = (six >> 1) & 0xF;
            f = f << 4 | sbox[(row * 16 + col) as usize] as u64;
        }
        let next = l ^ permute(f, 32, &P);
        l = r;
        r = next;
    }
    permute(r << 32 | l, 64, &FP)
}

/// A short name for the session, as viewers show it.
pub fn desktop_name(machine: &str) -> String {
    let mut name = String::from("BeeCodersOs");
    if !machine.is_empty() {
        name.push_str(" on ");
        name.push_str(&machine[..machine.len().min(40)]);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn des_known_answers() {
        // The textbook example (also what openssl gives).
        let keys = des_subkeys(0x1334_5779_9BBC_DFF1);
        assert_eq!(des_encrypt(0x0123_4567_89AB_CDEF, &keys), 0x85E8_1354_0F0A_B405);
        // VNC Authentication, checked against openssl with the mirrored key.
        let challenge: [u8; 16] = *b"challenge-16byte";
        let expect = [0xa1, 0xb7, 0x57, 0x15, 0x63, 0x71, 0x8e, 0x26, 0xf6, 0xf6, 0x12, 0x1c, 0x7f, 0xb2, 0xf7, 0xb0];
        assert_eq!(auth_response("bee12345", &challenge), expect);
        assert!(auth_ok("bee12345", &challenge, &expect));
        assert!(!auth_ok("bee12346", &challenge, &expect));
        assert!(!auth_ok("bee12345", &challenge, &expect[..15]));
        // Only the first 8 characters count, as in every VNC viewer.
        assert_eq!(auth_response("bee12345-and-more", &challenge), expect);
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version(b"RFB 003.008\n"), Some(8));
        assert_eq!(parse_version(b"RFB 003.007\n"), Some(7));
        assert_eq!(parse_version(b"RFB 003.003\n"), Some(3));
        assert_eq!(parse_version(b"RFB 003.889\n"), Some(8)); // Apple's viewer
        assert_eq!(parse_version(b"RFB 003.002\n"), None);
        assert_eq!(parse_version(b"HTTP/1.1 200"), None);
        assert_eq!(parse_version(b"RFB 003.00"), None);
    }

    #[test]
    fn messages_in_pieces() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[3, 1, 0, 0, 0, 0, 0x05, 0, 0x04, 0]); // update request, incremental, 1280x1024
        bytes.extend_from_slice(&[4, 1, 0, 0, 0, 0, 0, b'a' as u8]); // key 'a' down
        bytes.extend_from_slice(&[5, 1, 0, 10, 0, 20]); // left button at 10,20
        bytes.extend_from_slice(&[2, 0, 0, 2, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0x21]); // Raw, DesktopSize
        bytes.extend_from_slice(&[6, 0, 0, 0, 0, 0, 0, 3, b'x', b'y', b'z']); // clipboard "xyz"
        let mut fmt = [0u8; 20];
        fmt[4..20].copy_from_slice(&PixelFormat::SERVER.encode());
        bytes.extend_from_slice(&fmt);
        let expected = vec![
            ClientMessage::UpdateRequest { incremental: true, x: 0, y: 0, w: 1280, h: 1024 },
            ClientMessage::Key { down: true, keysym: b'a' as u32 },
            ClientMessage::Pointer { buttons: 1, x: 10, y: 20 },
            ClientMessage::SetEncodings(vec![0, -223]),
            ClientMessage::CutText,
            ClientMessage::SetPixelFormat(PixelFormat::SERVER),
        ];
        // All at once, and one byte at a time: the same messages.
        assert_eq!(ClientReader::new().feed(&bytes), Ok(expected));
        let mut reader = ClientReader::new();
        let mut got = Vec::new();
        for b in &bytes {
            got.extend(reader.feed(&[*b]).unwrap());
        }
        assert_eq!(got.len(), 6);
        assert_eq!(got[5], ClientMessage::SetPixelFormat(PixelFormat::SERVER));
        assert_eq!(ClientReader::new().feed(&[9]), Err(RfbError::UnknownMessage(9)));
    }

    #[test]
    fn hostile_lengths_are_bounded() {
        // A clipboard of 4 GB: skipped as it comes, never kept.
        let mut reader = ClientReader::new();
        assert_eq!(reader.feed(&[6, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap(), vec![ClientMessage::CutText]);
        assert!(reader.feed(&[b'x'; 10_000]).unwrap().is_empty());
        assert!(reader.buf.capacity() < 64 * 1024);
        // 65535 encodings: the first MAX_ENCODINGS are kept, the rest skipped.
        let mut reader = ClientReader::new();
        let mut msg = vec![2, 0, 0xFF, 0xFF];
        msg.extend(core::iter::repeat_n(0u8, 65535 * 4));
        msg.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0, b'q']);
        let got = reader.feed(&msg).unwrap();
        assert_eq!(got.len(), 2);
        assert!(matches!(&got[0], ClientMessage::SetEncodings(list) if list.len() == MAX_ENCODINGS));
        assert_eq!(got[1], ClientMessage::Key { down: false, keysym: b'q' as u32 });
    }

    #[test]
    fn pixels_in_other_formats() {
        let mut out = Vec::new();
        PixelFormat::SERVER.put(0x112233, &mut out);
        assert_eq!(out, [0x33, 0x22, 0x11, 0x00]);
        // 16 bits, RGB 565, little endian.
        let rgb565 = PixelFormat { bits_per_pixel: 16, depth: 16, big_endian: false, true_color: true, red_max: 31, green_max: 63, blue_max: 31, red_shift: 11, green_shift: 5, blue_shift: 0 };
        assert!(rgb565.supported());
        let mut out = Vec::new();
        rgb565.put(0xFFFFFF, &mut out);
        assert_eq!(out, [0xFF, 0xFF]);
        let mut out = Vec::new();
        rgb565.put(0xFF0000, &mut out);
        assert_eq!(out, 0xF800u16.to_le_bytes());
        // A palette, or shifts beyond 32 bits: refused.
        assert!(!PixelFormat { true_color: false, ..PixelFormat::SERVER }.supported());
        assert!(!PixelFormat { red_shift: 30, ..PixelFormat::SERVER }.supported());
        assert!(!PixelFormat { bits_per_pixel: 24, ..PixelFormat::SERVER }.supported());
    }

    #[test]
    fn keysyms() {
        assert_eq!(keysym(b'a' as u32), Some(KeyName::Char('a')));
        assert_eq!(keysym(0xE9), Some(KeyName::Char('é')));
        assert_eq!(keysym(0x0100_20AC), Some(KeyName::Char('€')));
        assert_eq!(keysym(0xFF0D), Some(KeyName::Enter));
        assert_eq!(keysym(0xFFE3), Some(KeyName::Modifier(Modifier::Control)));
        assert_eq!(keysym(0xFFBE), None); // F1: not used yet
        assert_eq!(keysym(0x0111_0000), None);
    }

    #[test]
    fn server_messages() {
        let init = server_init(1280, 1024, "BeeCodersOs");
        assert_eq!(&init[..4], &[0x05, 0x00, 0x04, 0x00]);
        assert_eq!(init[4], 32);
        assert_eq!(&init[20..24], &11u32.to_be_bytes());
        let mut out = Vec::new();
        security_result(false, 8, "wrong password", &mut out);
        assert_eq!(&out[..4], &1u32.to_be_bytes());
        assert_eq!(&out[8..], b"wrong password");
        let mut out = Vec::new();
        security_result(false, 3, "ignored", &mut out);
        assert_eq!(out.len(), 4);
    }
}
