//! Sound without hardware: WAV files, tones, and finding the way through
//! an HD Audio codec from a converter (DAC) to a speaker or jack. The
//! driver (hda.rs) plays what this makes; everything here is covered by
//! self-tests.
//!
//! All sound is played as 48 kHz, 16-bit stereo ("48k frames": 4 bytes,
//! left then right, little-endian); other WAV formats are converted.

use alloc::vec::Vec;

pub const RATE: u32 = 48_000;
/// A sound is at most this long (11 MB of 48k frames).
pub const MAX_SECONDS: u32 = 60;

// --- WAV files ----------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct Wav {
    pub rate: u32,
    pub channels: u16,
    pub bits: u16,
    /// The samples as they are in the file.
    pub data: Vec<u8>,
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// A PCM WAV file: 8 or 16 bits, mono or stereo, 8 to 192 kHz.
pub fn parse_wav(file: &[u8]) -> Result<Wav, &'static str> {
    if file.get(..4) != Some(b"RIFF") || file.get(8..12) != Some(b"WAVE") {
        return Err("not a WAV file");
    }
    let bad = "a damaged WAV file";
    let mut at = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    while at + 8 <= file.len() {
        let id = &file[at..at + 4];
        let len = u32_at(file, at + 4).ok_or(bad)? as usize;
        let body = at + 8;
        if id == b"fmt " {
            let kind = u16_at(file, body).ok_or(bad)?;
            let channels = u16_at(file, body + 2).ok_or(bad)?;
            let rate = u32_at(file, body + 4).ok_or(bad)?;
            let bits = u16_at(file, body + 14).ok_or(bad)?;
            // 0xFFFE (extensible) is PCM too when its sub-format says so; accept it like PCM.
            if kind != 1 && kind != 0xFFFE {
                return Err("a compressed WAV file (only PCM can be played)");
            }
            format = Some((kind, channels, rate, bits));
        } else if id == b"data" {
            let (_, channels, rate, bits) = format.ok_or(bad)?;
            if !(1..=2).contains(&channels) || !(bits == 8 || bits == 16) || !(8_000..=192_000).contains(&rate) {
                return Err("a WAV format this OS cannot play (8 or 16 bits, mono or stereo, 8-192 kHz)");
            }
            // A file cut short plays what it has.
            let end = file.len().min(body.saturating_add(len));
            let frame = channels as usize * bits as usize / 8;
            let usable = (end - body.min(end)) / frame * frame;
            return Ok(Wav { rate, channels, bits, data: file[body..body + usable].to_vec() });
        }
        at = body.saturating_add(len).saturating_add(len & 1); // chunks are padded to even sizes
    }
    Err(bad)
}

impl Wav {
    pub fn frames(&self) -> usize {
        self.data.len() / (self.channels as usize * self.bits as usize / 8)
    }

    pub fn duration_ms(&self) -> u64 {
        self.frames() as u64 * 1000 / self.rate as u64
    }

    /// The length as `1.4 s` (tenths, rounded).
    pub fn duration_text(&self) -> alloc::string::String {
        let tenths = (self.duration_ms() + 50) / 100;
        alloc::format!("{}.{} s", tenths / 10, tenths % 10)
    }

    /// Sample `i` of `channel` as 16 bits.
    fn sample(&self, i: usize, channel: usize) -> i32 {
        let ch = channel.min(self.channels as usize - 1);
        let k = i * self.channels as usize + ch;
        if self.bits == 8 {
            (self.data[k] as i32 - 128) << 8
        } else {
            i16::from_le_bytes([self.data[2 * k], self.data[2 * k + 1]]) as i32
        }
    }

    /// The sound as 48k frames (straight-line steps between samples), at
    /// most `MAX_SECONDS`.
    pub fn to_48k(&self) -> Vec<u8> {
        let n = self.frames();
        let out_frames = ((n as u64 * RATE as u64 / self.rate as u64) as usize).min((RATE * MAX_SECONDS) as usize);
        let mut out = Vec::with_capacity(out_frames * 4);
        for j in 0..out_frames {
            // Position in the file, in 1/65536 of a sample.
            let pos = j as u64 * self.rate as u64 * 65536 / RATE as u64;
            let (i, frac) = ((pos >> 16) as usize, (pos & 0xFFFF) as i32);
            for ch in 0..2 {
                let a = self.sample(i.min(n - 1), ch);
                let b = self.sample((i + 1).min(n - 1), ch);
                let v = a + (((b - a) * frac) >> 16);
                out.extend_from_slice(&(v as i16).to_le_bytes());
            }
        }
        out
    }
}

// --- tones ------------------------------------------------------------------------------

/// sin(2 pi t) for t in turns (any value), to about 0.1 %: a fifth-order
/// polynomial on a quarter turn.
pub fn sin_turns(t: f32) -> f32 {
    let t = t - (t as i64 as f32) + if t < 0.0 { 1.0 } else { 0.0 }; // 0..1
    let (q, sign) = if t < 0.5 { (t, 1.0) } else { (t - 0.5, -1.0) }; // 0..0.5
    let x = if q < 0.25 { q } else { 0.5 - q } * core::f32::consts::TAU; // 0..pi/2
    let x2 = x * x;
    sign * x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0)))
}

/// A sine tone of `hz` for `ms`, as 48k frames, at `volume` percent of
/// full scale; it fades in and out over 5 ms so it starts and ends without a click.
pub fn tone(hz: u32, ms: u32, volume: u32) -> Vec<u8> {
    let frames = (RATE as u64 * ms.min(MAX_SECONDS * 1000) as u64 / 1000) as usize;
    let fade = (RATE as usize / 200).min(frames / 2).max(1);
    let peak = 32767.0 * volume.min(100) as f32 / 100.0;
    let mut out = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let edge = i.min(frames - 1 - i).min(fade) as f32 / fade as f32;
        let turns = (i as u64 * hz as u64 % RATE as u64) as f32 / RATE as f32;
        let v = (sin_turns(turns) * peak * edge) as i16;
        out.extend_from_slice(&v.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Notes for `beep`: a name like `a4`, `c#5`, `eb3` to its frequency (equal
/// temperament, A4 = 440 Hz), or a plain number of Hz.
pub fn frequency(word: &str) -> Option<u32> {
    if let Ok(hz) = word.parse::<u32>() {
        return (20..=20_000).contains(&hz).then_some(hz);
    }
    let b = word.to_ascii_lowercase().into_bytes();
    let base = match b.first()? {
        b'c' => -9,
        b'd' => -7,
        b'e' => -5,
        b'f' => -4,
        b'g' => -2,
        b'a' => 0,
        b'b' => 2,
        _ => return None,
    };
    let (shift, rest) = match b.get(1) {
        Some(b'#') => (1, &b[2..]),
        Some(b'b') if b.len() > 2 => (-1, &b[2..]),
        _ => (0, &b[1..]),
    };
    let octave = core::str::from_utf8(rest).ok()?.parse::<i32>().ok().filter(|o| (0..=8).contains(o))?;
    let semitones = base + shift + (octave - 4) * 12;
    // 440 * 2^(semitones / 12), by whole octaves and a table for the rest.
    const STEPS: [f32; 12] = [1.0, 1.059_463, 1.122_462, 1.189_207, 1.259_921, 1.334_84, 1.414_214, 1.498_307, 1.587_401, 1.681_793, 1.781_797, 1.887_749];
    let (oct, step) = (semitones.div_euclid(12), semitones.rem_euclid(12));
    let hz = 440.0 * STEPS[step as usize] * if oct >= 0 { (1u32 << oct) as f32 } else { 1.0 / (1u32 << -oct) as f32 };
    Some((hz + 0.5) as u32)
}

// --- the way through a codec ---------------------------------------------------------

/// What a codec node is (bits 23:20 of its capabilities).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Output,
    Input,
    Mixer,
    Selector,
    Pin,
    Other,
}

impl Kind {
    pub fn from_caps(caps: u32) -> Kind {
        match caps >> 20 & 0xF {
            0 => Kind::Output,
            1 => Kind::Input,
            2 => Kind::Mixer,
            3 => Kind::Selector,
            4 => Kind::Pin,
            _ => Kind::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub nid: u8,
    pub kind: Kind,
    pub caps: u32,
    /// The nodes it can take its signal from.
    pub inputs: Vec<u8>,
    /// Pins: the configuration default (where the jack is, what it is for).
    pub config: u32,
    pub pin_caps: u32,
}

/// What a pin is for, from its configuration default; `None` for pins
/// that are not connected or are not analog outputs.
pub fn output_use(config: u32) -> Option<&'static str> {
    if config >> 30 == 1 {
        return None; // "no physical connection"
    }
    match config >> 20 & 0xF {
        0 => Some("line out"),
        1 => Some("speaker"),
        2 => Some("headphones"),
        _ => None,
    }
}

/// From a pin to a converter: the nodes on the way (the pin first, the
/// converter last) with, for each but the last, which of its inputs the
/// way takes. Breadth first, at most 6 steps, so the shortest way wins.
pub fn route(nodes: &[Node], pin: u8) -> Option<Vec<(u8, usize)>> {
    let find = |nid: u8| nodes.iter().find(|n| n.nid == nid);
    let mut paths: Vec<Vec<(u8, usize)>> = alloc::vec![alloc::vec![(pin, 0)]];
    for _ in 0..6 {
        let mut next = Vec::new();
        for path in &paths {
            let (last, _) = *path.last()?;
            let node = find(last)?;
            for (i, &input) in node.inputs.iter().enumerate() {
                if path.iter().any(|(n, _)| *n == input) {
                    continue; // no loops
                }
                let Some(to) = find(input) else { continue };
                let mut longer = path.clone();
                longer.last_mut()?.1 = i;
                longer.push((input, 0));
                match to.kind {
                    Kind::Output if to.caps & (1 << 9) == 0 => return Some(longer), // not digital
                    Kind::Mixer | Kind::Selector => next.push(longer),
                    _ => {}
                }
            }
        }
        if next.is_empty() {
            return None;
        }
        paths = next;
    }
    None
}

/// The stream format word for 48 kHz, 16 bits, stereo.
pub const FORMAT_48K_16_STEREO: u16 = 1 << 4 | 1;

/// A verb with a 12-bit command and 8-bit payload.
pub fn verb(codec: u8, nid: u8, command: u32, payload: u32) -> u32 {
    (codec as u32 & 0xF) << 28 | (nid as u32) << 20 | (command & 0xFFF) << 8 | (payload & 0xFF)
}

/// A verb with a 4-bit command and a 16-bit payload (format, amplifiers).
pub fn verb16(codec: u8, nid: u8, command: u32, payload: u32) -> u32 {
    (codec as u32 & 0xF) << 28 | (nid as u32) << 20 | (command & 0xF) << 16 | (payload & 0xFFFF)
}

/// The entries of a connection list from GET_CONNECTION_LIST answers
/// (`entry(offset)` gives the answer for that offset): short form, 4 per
/// answer, or long form, 2 per answer; ranges are expanded. At most 32.
pub fn connections(length_param: u32, entry: impl Fn(u32) -> u32) -> Vec<u8> {
    let long = length_param & 0x80 != 0;
    let count = (length_param & 0x7F) as usize;
    let (per, bits) = if long { (2, 16) } else { (4, 8) };
    let mut out: Vec<u8> = Vec::new();
    let mut last = 0u16;
    for i in 0..count.min(32) {
        let word = entry((i / per * per) as u32);
        let raw = (word >> ((i % per) * bits)) as u16 & if long { 0xFFFF } else { 0xFF };
        let is_range = raw & if long { 0x8000 } else { 0x80 } != 0;
        let nid = raw & if long { 0x7FFF } else { 0x7F };
        if is_range && last != 0 && nid > last && nid - last <= 16 {
            out.extend((last + 1..=nid).map(|n| n as u8));
        } else {
            out.push(nid as u8);
        }
        last = nid;
    }
    out.truncate(32);
    out
}

// --- tunes and writing WAV files -----------------------------------------------------------

/// The notes of a tune: `NOTE[:MS]` words (`c5`, `a#4:500`, `440:100`), `r`
/// or `-` for a rest; 250 ms when no length is given. At most 200 notes and
/// `MAX_SECONDS` in all. `None` is a rest.
pub fn parse_tune(words: &[&str]) -> Result<Vec<(Option<u32>, u32)>, &'static str> {
    let mut notes = Vec::new();
    let mut total = 0u32;
    for word in words {
        let (name, ms) = match word.split_once(':') {
            Some((n, ms)) => (n, ms.parse::<u32>().ok().filter(|ms| (10..=10_000).contains(ms)).ok_or("a note length is 10-10000 ms")?),
            None => (*word, 250),
        };
        let hz = match name {
            "r" | "-" => None,
            _ => Some(frequency(name).ok_or("not a note (like c5, a#4, 440, or r for a rest)")?),
        };
        total += ms;
        if notes.len() == 200 || total > MAX_SECONDS * 1000 {
            return Err("a tune is at most 200 notes and 60 seconds");
        }
        notes.push((hz, ms));
    }
    if notes.is_empty() {
        return Err("no notes");
    }
    Ok(notes)
}

/// A tune as 48k frames: each note a tone that fades in and out, so
/// repeated notes are heard apart.
pub fn render_tune(notes: &[(Option<u32>, u32)], volume: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for &(hz, ms) in notes {
        match hz {
            Some(hz) => out.extend_from_slice(&tone(hz, ms, volume)),
            None => out.resize(out.len() + (RATE as usize * ms as usize / 1000) * 4, 0),
        }
    }
    out
}

/// A WAV file (PCM, 16 bits, stereo, 48 kHz) of 48k frames.
pub fn encode_wav(frames: &[u8]) -> Vec<u8> {
    let data = &frames[..frames.len() & !3];
    let mut f = Vec::with_capacity(44 + data.len());
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes()); // PCM
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&RATE.to_le_bytes());
    f.extend_from_slice(&(RATE * 4).to_le_bytes());
    f.extend_from_slice(&4u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(data);
    f
}
