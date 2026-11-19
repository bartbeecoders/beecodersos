//! AVI videos (RIFF): the frame rate and size from the headers, where each
//! video frame is, and the sound if it is PCM. The player (kernel
//! video.rs) shows Motion-JPEG videos, whose frames are JPEG pictures.
//! A file is untrusted: every chunk length is checked against the data.

use alloc::vec::Vec;

/// At most this many frames are listed.
pub const MAX_FRAMES: usize = 200_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sound {
    pub rate: u32,
    pub channels: u16,
    pub bits: u16,
    /// The samples, in order (the pieces between the frames put together).
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avi {
    pub width: usize,
    pub height: usize,
    /// How long a frame stays, in microseconds.
    pub frame_us: u32,
    /// The video's compression ("MJPG" is shown).
    pub codec: [u8; 4],
    /// Where each video frame is in the file: (offset, length).
    pub frames: Vec<(usize, usize)>,
    pub sound: Option<Sound>,
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

pub fn is_avi(data: &[u8]) -> bool {
    data.get(..4) == Some(b"RIFF") && data.get(8..12) == Some(b"AVI ")
}

/// The chunks in `data[start..end]`: (id, body start, body length);
/// a chunk running past the end is cut to what is there.
fn chunks(data: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut at = start;
    let end = end.min(data.len());
    while at + 8 <= end {
        let id: [u8; 4] = data[at..at + 4].try_into().unwrap_or([0; 4]);
        let len = u32_at(data, at + 4).unwrap_or(0) as usize;
        let body = at + 8;
        let len = len.min(end - body);
        out.push((id, body, len));
        at = body + len + (len & 1); // chunks are padded to even lengths
        if out.len() > MAX_FRAMES * 2 {
            break;
        }
    }
    out
}

pub fn parse(data: &[u8]) -> Result<Avi, &'static str> {
    if !is_avi(data) {
        return Err("not an AVI video");
    }
    let bad = "a damaged AVI video";
    let mut avi = Avi { width: 0, height: 0, frame_us: 0, codec: [0; 4], frames: Vec::new(), sound: None };
    // Which stream number is the video, which the sound.
    let (mut video, mut audio) = (None, None);
    let mut stream = 0u8;
    let mut sound_data = Vec::new();
    for (id, body, len) in chunks(data, 12, data.len()) {
        if &id != b"LIST" || len < 4 {
            continue;
        }
        let kind = &data[body..body + 4];
        if kind == b"hdrl" {
            for (id, b, l) in chunks(data, body + 4, body + len) {
                if &id == b"avih" && l >= 40 {
                    avi.frame_us = u32_at(data, b).ok_or(bad)?;
                    avi.width = u32_at(data, b + 32).ok_or(bad)? as usize;
                    avi.height = u32_at(data, b + 36).ok_or(bad)? as usize;
                } else if &id == b"LIST" && l >= 4 && &data[b..b + 4] == b"strl" {
                    let mut kind = [0u8; 4];
                    for (id, sb, sl) in chunks(data, b + 4, b + l) {
                        if &id == b"strh" && sl >= 8 {
                            kind.copy_from_slice(&data[sb..sb + 4]);
                            if &kind == b"vids" && video.is_none() {
                                video = Some(stream);
                                avi.codec.copy_from_slice(&data[sb + 4..sb + 8]);
                            }
                        } else if &id == b"strf" && &kind == b"auds" && audio.is_none() && sl >= 16 {
                            let format = u16_at(data, sb).ok_or(bad)?;
                            if format == 1 {
                                audio = Some(stream);
                                avi.sound = Some(Sound {
                                    channels: u16_at(data, sb + 2).ok_or(bad)?,
                                    rate: u32_at(data, sb + 4).ok_or(bad)?,
                                    bits: u16_at(data, sb + 14).ok_or(bad)?,
                                    data: Vec::new(),
                                });
                            }
                        } else if &id == b"strf" && &kind == b"vids" && sl >= 20 && avi.codec == [0; 4] {
                            avi.codec.copy_from_slice(&data[sb + 16..sb + 20]);
                        }
                    }
                    stream = stream.saturating_add(1);
                }
            }
        } else if kind == b"movi" {
            // Frames may sit in "rec " lists too.
            let mut todo = chunks(data, body + 4, body + len);
            let mut i = 0;
            while i < todo.len() {
                let (id, b, l) = todo[i];
                i += 1;
                if &id == b"LIST" && l >= 4 && &data[b..b + 4] == b"rec " {
                    todo.extend(chunks(data, b + 4, b + l));
                    continue;
                }
                let number = core::str::from_utf8(&id[..2]).ok().and_then(|n| n.parse::<u8>().ok());
                match (number, &id[2..]) {
                    (Some(n), b"dc" | b"db") if Some(n) == video && avi.frames.len() < MAX_FRAMES && l > 0 => avi.frames.push((b, l)),
                    (Some(n), b"wb") if Some(n) == audio && sound_data.len() + l <= 256 * 1024 * 1024 => sound_data.extend_from_slice(&data[b..b + l]),
                    _ => {}
                }
            }
        }
    }
    if video.is_none() || avi.width == 0 || avi.height == 0 {
        return Err("an AVI without a video stream");
    }
    if avi.frame_us == 0 {
        avi.frame_us = 40_000; // 25 frames a second when the file does not say
    }
    if let Some(s) = avi.sound.as_mut() {
        s.data = sound_data;
    }
    Ok(avi)
}

impl Avi {
    /// The bytes of frame `i` in the file it was read from.
    pub fn frame<'a>(&self, data: &'a [u8], i: usize) -> Option<&'a [u8]> {
        let &(at, len) = self.frames.get(i)?;
        data.get(at..at + len)
    }

    pub fn is_mjpeg(&self) -> bool {
        matches!(&self.codec, b"MJPG" | b"mjpg" | b"AVRn" | b"JPEG" | b"jpeg")
    }

    pub fn duration_ms(&self) -> u64 {
        self.frames.len() as u64 * self.frame_us as u64 / 1000
    }
}

/// Writes a Motion-JPEG AVI (video only): frames are added one by one,
/// `finish` puts the headers and the index around them.
pub struct Writer {
    width: usize,
    height: usize,
    frame_us: u32,
    /// The frames' chunks ("00dc", length, JPEG, padding), one after the other.
    movi: Vec<u8>,
    /// (offset in `movi` of each chunk, its length).
    index: Vec<(u32, u32)>,
}

impl Writer {
    pub fn new(width: usize, height: usize, frame_us: u32) -> Writer {
        Writer { width, height, frame_us, movi: Vec::new(), index: Vec::new() }
    }

    pub fn frame(&mut self, jpeg: &[u8]) {
        // Offsets in the index count from the "movi" word, 4 bytes before this data.
        self.index.push((self.movi.len() as u32 + 4, jpeg.len() as u32));
        self.movi.extend_from_slice(b"00dc");
        self.movi.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
        self.movi.extend_from_slice(jpeg);
        if jpeg.len() % 2 == 1 {
            self.movi.push(0);
        }
    }

    pub fn frames(&self) -> usize {
        self.index.len()
    }

    /// The time per frame, when it is known only at the end (a recording).
    pub fn set_frame_us(&mut self, frame_us: u32) {
        self.frame_us = frame_us.max(1);
    }

    /// Bytes so far (about what the file will be).
    pub fn len(&self) -> usize {
        self.movi.len() + 16 * self.index.len() + 256
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn finish(self) -> Vec<u8> {
        let le = |v: u32| v.to_le_bytes();
        let (w, h, n) = (self.width as u32, self.height as u32, self.index.len() as u32);
        let biggest = self.index.iter().map(|(_, l)| *l).max().unwrap_or(0) + 8;
        let mut avih = Vec::new();
        for v in [self.frame_us, biggest.saturating_mul(1_000_000 / self.frame_us.max(1)), 0, 0x10, n, 0, 1, biggest, w, h, 0, 0, 0, 0] {
            avih.extend_from_slice(&le(v));
        }
        let mut strh = Vec::new();
        strh.extend_from_slice(b"vidsMJPG");
        for v in [0u32, 0, 0, self.frame_us, 1_000_000, 0, n, biggest, u32::MAX, 0] {
            strh.extend_from_slice(&le(v));
        }
        for v in [0u16, 0, w as u16, h as u16] {
            strh.extend_from_slice(&v.to_le_bytes());
        }
        let mut strf = Vec::new();
        strf.extend_from_slice(&le(40));
        strf.extend_from_slice(&le(w));
        strf.extend_from_slice(&le(h));
        strf.extend_from_slice(&1u16.to_le_bytes());
        strf.extend_from_slice(&24u16.to_le_bytes());
        strf.extend_from_slice(b"MJPG");
        for v in [w * h * 3, 0, 0, 0, 0] {
            strf.extend_from_slice(&le(v));
        }
        let chunk = |id: &[u8], body: &[u8]| -> Vec<u8> {
            let mut c = id.to_vec();
            c.extend_from_slice(&(body.len() as u32).to_le_bytes());
            c.extend_from_slice(body);
            if body.len() % 2 == 1 {
                c.push(0);
            }
            c
        };
        let list = |kind: &[u8], body: &[u8]| -> Vec<u8> {
            let mut inner = kind.to_vec();
            inner.extend_from_slice(body);
            chunk(b"LIST", &inner)
        };
        let mut strl = chunk(b"strh", &strh);
        strl.extend(chunk(b"strf", &strf));
        let mut hdrl = chunk(b"avih", &avih);
        hdrl.extend(list(b"strl", &strl));
        let mut idx1 = Vec::with_capacity(16 * self.index.len());
        for (offset, len) in &self.index {
            idx1.extend_from_slice(b"00dc");
            idx1.extend_from_slice(&le(0x10)); // a key frame
            idx1.extend_from_slice(&le(*offset));
            idx1.extend_from_slice(&le(*len));
        }
        let mut body = b"AVI ".to_vec();
        body.extend(list(b"hdrl", &hdrl));
        body.extend(list(b"movi", &self.movi));
        body.extend(chunk(b"idx1", &idx1));
        chunk(b"RIFF", &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_clip() {
        // ffmpeg: testsrc 64x48 at 10 frames a second, MJPEG, with a 440 Hz
        // sine at 8 kHz, 16-bit mono, for one second.
        let data: &[u8] = include_bytes!("../testdata/clip.avi");
        let avi = parse(data).unwrap();
        assert_eq!((avi.width, avi.height, avi.frame_us), (64, 48, 100_000));
        assert!(avi.is_mjpeg());
        assert_eq!(avi.frames.len(), 10);
        assert_eq!(avi.duration_ms(), 1000);
        for i in 0..10 {
            let (w, h, _) = crate::jpeg::decode(avi.frame(data, i).unwrap()).unwrap();
            assert_eq!((w, h), (64, 48));
        }
        let sound = avi.sound.unwrap();
        assert_eq!((sound.rate, sound.channels, sound.bits), (8000, 1, 16));
        assert_eq!(sound.data.len(), 8000 * 2);
    }

    #[test]
    fn damaged_videos_fail_cleanly() {
        let data: &[u8] = include_bytes!("../testdata/clip.avi");
        for end in (0..data.len()).step_by(97) {
            if let Ok(avi) = parse(&data[..end]) {
                for i in 0..avi.frames.len() {
                    let _ = avi.frame(&data[..end], i);
                }
            }
        }
        for i in (12..data.len()).step_by(31) {
            let mut d = data.to_vec();
            d[i] ^= 0xA5;
            let _ = parse(&d);
        }
        assert_eq!(parse(b"RIFF\0\0\0\0WAVE"), Err("not an AVI video"));
    }

    #[test]
    fn written_and_read_back() {
        let frames: Vec<Vec<u8>> = (0..5u32).map(|t| crate::jpeg::encode(40, 30, 80, |x, y| (x as u32 * 6 + t * 40) << 16 | (y as u32 * 8) << 8)).collect();
        let mut w = Writer::new(40, 30, 200_000);
        for f in &frames {
            w.frame(f);
        }
        assert_eq!(w.frames(), 5);
        let file = w.finish();
        let avi = parse(&file).unwrap();
        assert_eq!((avi.width, avi.height, avi.frame_us, avi.frames.len()), (40, 30, 200_000, 5));
        assert!(avi.is_mjpeg() && avi.sound.is_none());
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(avi.frame(&file, i).unwrap(), &f[..]);
        }
    }
}
