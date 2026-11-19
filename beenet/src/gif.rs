//! GIF pictures, still and animated: reading every frame of a GIF87a or
//! GIF89a file (LZW, local and global color tables, transparency,
//! interlacing, the three ways a frame is cleared), and writing animated
//! GIFs (`Writer`, for screen recordings).
//!
//! Frames are kept as they are in the file (a rectangle of color indices),
//! and a `Player` paints them one after the other onto a whole picture, so
//! a long animation takes about a byte per changed pixel, not four bytes
//! per pixel of every frame. A file is untrusted: every length is checked,
//! the LZW codes are checked, and the decoded data is bounded
//! (`MAX_SIDE`, `MAX_DATA`, `MAX_FRAMES`).

use alloc::vec;
use alloc::vec::Vec;

pub const MAX_SIDE: usize = 4096;
/// At most this many bytes of decoded frames (one byte per pixel of a
/// frame's rectangle); frames past it are left out.
pub const MAX_DATA: usize = 64 * 1024 * 1024;
pub const MAX_FRAMES: usize = 5000;
/// Browsers show frames that ask for less than 20 ms for 100 ms; so does this.
const DEFAULT_DELAY_MS: u32 = 100;

pub fn is_gif(data: &[u8]) -> bool {
    data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")
}

/// One frame: a rectangle of color indices at (`left`, `top`) and how long
/// it stays.
#[derive(Debug, Clone)]
pub struct Frame {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    /// Line by line; may be shorter than width x height (a frame cut short
    /// paints what it has).
    indices: Vec<u8>,
    /// Which of `Gif::palettes`.
    palette: usize,
    transparent: Option<u8>,
    /// After the frame: 0/1 leave it, 2 clear its rectangle, 3 restore what was there.
    disposal: u8,
    pub delay_ms: u32,
}

#[derive(Debug, Clone)]
pub struct Gif {
    pub width: usize,
    pub height: usize,
    palettes: Vec<Vec<u32>>,
    pub frames: Vec<Frame>,
}

impl Gif {
    /// The first frame as a whole picture (0xRRGGBB, line by line).
    pub fn first(&self) -> Vec<u32> {
        Player::new(self).advance().0.to_vec()
    }

    /// The length of one round of the animation.
    pub fn duration_ms(&self) -> u64 {
        self.frames.iter().map(|f| f.delay_ms as u64).sum()
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn palette(table: &[u8]) -> Vec<u32> {
    table.chunks(3).map(|c| (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).collect()
}

/// The data of the sub-blocks at `at` (each a length byte and that many
/// bytes, ending with a 0), and where they end.
fn sub_blocks(data: &[u8], mut at: usize, keep: bool) -> Result<(Vec<u8>, usize), &'static str> {
    let mut out = Vec::new();
    loop {
        let len = *data.get(at).ok_or("a damaged GIF file (cut short)")? as usize;
        at += 1;
        if len == 0 {
            return Ok((out, at));
        }
        let block = data.get(at..at + len).ok_or("a damaged GIF file (cut short)")?;
        if keep {
            out.extend_from_slice(block);
        }
        at += len;
    }
}

pub fn decode(data: &[u8]) -> Result<Gif, &'static str> {
    if !is_gif(data) {
        return Err("not a GIF file");
    }
    let bad = "a damaged GIF file";
    let width = u16_at(data, 6).ok_or(bad)?;
    let height = u16_at(data, 8).ok_or(bad)?;
    let flags = *data.get(10).ok_or(bad)?;
    if width == 0 || height == 0 {
        return Err("the picture has no pixels");
    }
    if width > MAX_SIDE || height > MAX_SIDE {
        return Err("the picture is too large (4096 x 4096 pixels at most)");
    }
    let mut at = 13;
    let mut palettes: Vec<Vec<u32>> = Vec::new();
    let global = if flags & 0x80 != 0 {
        let n = 3 * (2 << (flags & 7));
        palettes.push(palette(data.get(at..at + n).ok_or(bad)?));
        at += n;
        Some(0)
    } else {
        None
    };
    let mut frames: Vec<Frame> = Vec::new();
    let mut total = 0usize;
    // From the last graphic control extension: delay, transparency, disposal.
    let mut control = (DEFAULT_DELAY_MS, None, 0u8);
    loop {
        match data.get(at) {
            // Many files lack the end marker; what came before counts.
            None | Some(0x3B) => break,
            Some(0x21) => {
                let label = *data.get(at + 1).ok_or(bad)?;
                let (body, end) = sub_blocks(data, at + 2, label == 0xF9)?;
                if label == 0xF9 && body.len() >= 4 {
                    let cs = u16::from_le_bytes([body[1], body[2]]) as u32;
                    let delay = if cs < 2 { DEFAULT_DELAY_MS } else { cs * 10 };
                    let transparent = (body[0] & 1 != 0).then_some(body[3]);
                    control = (delay, transparent, (body[0] >> 2) & 7);
                }
                at = end;
            }
            Some(0x2C) => {
                let d = data.get(at + 1..at + 10).ok_or(bad)?;
                let (left, top) = (u16_at(d, 0).ok_or(bad)?, u16_at(d, 2).ok_or(bad)?);
                let (fw, fh) = (u16_at(d, 4).ok_or(bad)?, u16_at(d, 6).ok_or(bad)?);
                let fflags = d[8];
                at += 10;
                let palette_at = if fflags & 0x80 != 0 {
                    let n = 3 * (2 << (fflags & 7));
                    palettes.push(palette(data.get(at..at + n).ok_or(bad)?));
                    at += n;
                    palettes.len() - 1
                } else {
                    global.ok_or("a GIF frame without colors")?
                };
                let min = *data.get(at).ok_or(bad)?;
                let (compressed, end) = sub_blocks(data, at + 1, true)?;
                at = end;
                let size = fw * fh;
                if total + size > MAX_DATA || frames.len() == MAX_FRAMES {
                    break; // the rest of a very long animation is left out
                }
                let mut indices = lzw_decode(&compressed, min, size)?;
                if fflags & 0x40 != 0 && fw > 0 {
                    indices = deinterlace(&indices, fw, fh);
                }
                total += size;
                let (delay_ms, transparent, disposal) = control;
                frames.push(Frame { left, top, width: fw, height: fh, indices, palette: palette_at, transparent, disposal, delay_ms });
                control = (DEFAULT_DELAY_MS, None, 0);
            }
            Some(_) => {
                if frames.is_empty() {
                    return Err(bad);
                }
                break; // junk after the frames: show what came before
            }
        }
    }
    if frames.is_empty() {
        return Err("a GIF file without pictures");
    }
    Ok(Gif { width, height, palettes, frames })
}

/// Interlaced lines come every 8th from 0, every 8th from 4, every 4th
/// from 2, then every 2nd from 1: put them back in order.
fn deinterlace(indices: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height];
    for y in 0..height {
        let from = interlaced_position(y, height) * width;
        if let Some(line) = indices.get(from..from + width) {
            out[y * width..(y + 1) * width].copy_from_slice(line);
        }
    }
    // A frame cut short keeps the lines from the top that all arrived.
    let lines = indices.len() / width;
    let whole = (0..height).take_while(|&y| interlaced_position(y, height) < lines).count();
    out.truncate(whole * width);
    out
}

/// Where line `y` comes in an interlaced frame of `height` lines.
fn interlaced_position(y: usize, height: usize) -> usize {
    let pass1 = height.div_ceil(8);
    let pass2 = height.saturating_sub(4).div_ceil(8);
    let pass3 = height.saturating_sub(2).div_ceil(4);
    match y % 8 {
        0 => y / 8,
        4 => pass1 + y / 8,
        2 | 6 => pass1 + pass2 + y / 4,
        _ => pass1 + pass2 + pass3 + y / 2,
    }
}

/// LZW as GIF uses it: codes from `min + 1` up to 12 bits, least
/// significant bit first. At most `limit` indices come out.
fn lzw_decode(data: &[u8], min: u8, limit: usize) -> Result<Vec<u8>, &'static str> {
    let bad = "a damaged GIF file (bad picture data)";
    if !(1..=8).contains(&min) {
        return Err(bad);
    }
    let clear = 1u16 << min;
    let end = clear + 1;
    let mut prefix = [0u16; 4096];
    let mut suffix = [0u8; 4096];
    let mut first = [0u8; 4096];
    for i in 0..clear {
        suffix[i as usize] = i as u8;
        first[i as usize] = i as u8;
    }
    let mut out: Vec<u8> = Vec::with_capacity(limit.min(1 << 20));
    let mut stack: Vec<u8> = Vec::with_capacity(4096);
    let (mut size, mut next) = (min as u32 + 1, end + 1);
    let mut prev: Option<u16> = None;
    let (mut bits, mut nbits, mut pos) = (0u32, 0u32, 0usize);
    while out.len() < limit {
        while nbits < size {
            let Some(&byte) = data.get(pos) else { return Ok(out) }; // no end code: done
            bits |= (byte as u32) << nbits;
            nbits += 8;
            pos += 1;
        }
        let code = (bits & ((1 << size) - 1)) as u16;
        bits >>= size;
        nbits -= size;
        if code == clear {
            size = min as u32 + 1;
            next = end + 1;
            prev = None;
            continue;
        }
        if code == end {
            break;
        }
        let head = match prev {
            None if code < clear => code,
            None => return Err(bad),
            Some(_) if code < next => code,
            // The code being made right now: the previous string and its first index.
            Some(p) if code == next => p,
            Some(_) => return Err(bad),
        };
        stack.clear();
        let mut c = head;
        while c >= clear {
            stack.push(suffix[c as usize]);
            c = prefix[c as usize];
        }
        stack.push(c as u8);
        out.extend(stack.iter().rev());
        if head != code {
            out.push(first[head as usize]);
        }
        if let Some(p) = prev {
            if next < 4096 {
                // The previous string and the first index of this one.
                prefix[next as usize] = p;
                suffix[next as usize] = first[head as usize];
                first[next as usize] = first[p as usize];
                next += 1;
                if next == 1 << size && size < 12 {
                    size += 1;
                }
            }
        }
        prev = Some(code);
    }
    out.truncate(limit);
    Ok(out)
}

/// Paints the frames of a GIF one after the other, as a viewer shows them.
pub struct Player<'a> {
    gif: &'a Gif,
    canvas: Vec<u32>,
    /// What the last frame's rectangle covered, for disposal 3.
    saved: Option<Vec<u32>>,
    next: usize,
}

impl<'a> Player<'a> {
    pub fn new(gif: &'a Gif) -> Self {
        Player { gif, canvas: vec![0; gif.width * gif.height], saved: None, next: 0 }
    }

    /// The index of the frame `advance` shows next.
    pub fn position(&self) -> usize {
        self.next
    }

    /// Paint the next frame (after the last one, the first again) and
    /// give the whole picture and how long it stays.
    pub fn advance(&mut self) -> (&[u32], u32) {
        let gif = self.gif;
        let n = gif.frames.len();
        if self.next == 0 {
            self.canvas.fill(0);
            self.saved = None;
        } else {
            let last = &gif.frames[self.next - 1];
            match last.disposal {
                2 => self.rect(last, |_, _| Some(0)),
                3 => {
                    if let Some(saved) = self.saved.take() {
                        let w = last.width;
                        self.rect(last, |x, y| saved.get(y * w + x).copied());
                    }
                }
                _ => {}
            }
        }
        let frame = &gif.frames[self.next];
        if frame.disposal == 3 {
            let mut saved = Vec::new();
            for y in 0..frame.height {
                for x in 0..frame.width {
                    let (cx, cy) = (frame.left + x, frame.top + y);
                    saved.push(if cx < gif.width && cy < gif.height { self.canvas[cy * gif.width + cx] } else { 0 });
                }
            }
            self.saved = Some(saved);
        }
        let colors = &gif.palettes[frame.palette];
        let w = frame.width;
        self.rect(frame, |x, y| {
            let index = *frame.indices.get(y * w + x)?;
            if Some(index) == frame.transparent {
                return None;
            }
            Some(colors.get(index as usize).copied().unwrap_or(0))
        });
        self.next = (self.next + 1) % n;
        (&self.canvas, frame.delay_ms)
    }

    /// Set the pixels of `frame`'s rectangle that `color(x, y)` gives
    /// (inside the picture only).
    fn rect(&mut self, frame: &Frame, color: impl Fn(usize, usize) -> Option<u32>) {
        let gw = self.gif.width;
        for y in 0..frame.height {
            let cy = frame.top + y;
            if cy >= self.gif.height {
                break;
            }
            for x in 0..frame.width {
                let cx = frame.left + x;
                if cx >= gw {
                    break;
                }
                if let Some(c) = color(x, y) {
                    self.canvas[cy * gw + cx] = c;
                }
            }
        }
    }
}

/// The colors a written GIF uses: 6 levels of red, 7 of green and 6 of
/// blue (252 colors; the eye tells greens apart best).
pub fn color_index(rgb: u32) -> u8 {
    let level = |v: u32, n: u32| (v * (n - 1) + 127) / 255;
    let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
    (level(r, 6) * 42 + level(g, 7) * 6 + level(b, 6)) as u8
}

fn color_of(index: u8) -> u32 {
    let i = index as u32;
    let (r, g, b) = (i / 42, i / 6 % 7, i % 6);
    (r * 255 / 5) << 16 | (g * 255 / 6) << 8 | b * 255 / 5
}

/// Writes an animated GIF frame by frame: only the rectangle that changed
/// since the frame before is stored, and a frame without changes makes
/// the one before last longer.
pub struct Writer {
    width: usize,
    height: usize,
    out: Vec<u8>,
    previous: Option<Vec<u8>>,
    /// Where the delay of the last frame is, and its value (centiseconds).
    last_delay: Option<(usize, u32)>,
    pub frames: usize,
}

impl Writer {
    pub fn new(width: usize, height: usize) -> Writer {
        let mut out = b"GIF89a".to_vec();
        out.extend_from_slice(&(width as u16).to_le_bytes());
        out.extend_from_slice(&(height as u16).to_le_bytes());
        out.extend_from_slice(&[0xF7, 0, 0]); // a global table of 256 colors
        for i in 0..=255u8 {
            let c = if i < 252 { color_of(i) } else { 0 };
            out.extend_from_slice(&[(c >> 16) as u8, (c >> 8) as u8, c as u8]);
        }
        // Play again and again (the Netscape extension every browser knows).
        out.extend_from_slice(&[0x21, 0xFF, 11]);
        out.extend_from_slice(b"NETSCAPE2.0");
        out.extend_from_slice(&[3, 1, 0, 0, 0]);
        Writer { width, height, out, previous: None, last_delay: None, frames: 0 }
    }

    /// Add a frame that stays `delay_ms`; `pixel(x, y)` gives 0xRRGGBB.
    pub fn frame(&mut self, delay_ms: u32, pixel: impl Fn(usize, usize) -> u32) {
        let (w, h) = (self.width, self.height);
        let mut indices = Vec::with_capacity(w * h);
        for y in 0..h {
            indices.extend((0..w).map(|x| color_index(pixel(x, y))));
        }
        self.frame_indices(delay_ms, indices);
    }

    /// Add a frame already turned into colors (`color_index`, line by
    /// line, width x height of them).
    pub fn frame_indices(&mut self, delay_ms: u32, indices: Vec<u8>) {
        let (w, h) = (self.width, self.height);
        if indices.len() != w * h {
            return;
        }
        let cs = delay_ms.div_ceil(10).max(2);
        // The rectangle that changed.
        let (mut x0, mut y0, mut x1, mut y1) = (0, 0, w, h);
        if let Some(previous) = &self.previous {
            let changed = |i: usize| indices[i] != previous[i];
            let rows: Vec<usize> = (0..h).filter(|&y| (0..w).any(|x| changed(y * w + x))).collect();
            let (Some(&top), Some(&bottom)) = (rows.first(), rows.last()) else {
                // Nothing changed: the frame before stays longer.
                if let Some((at, before)) = self.last_delay {
                    let longer = (before + cs).min(65535);
                    self.out[at..at + 2].copy_from_slice(&(longer as u16).to_le_bytes());
                    self.last_delay = Some((at, longer));
                }
                return;
            };
            let columns = |x: usize| (top..=bottom).any(|y| changed(y * w + x));
            x0 = (0..w).find(|&x| columns(x)).unwrap_or(0);
            x1 = (0..w).rev().find(|&x| columns(x)).map_or(w, |x| x + 1);
            (y0, y1) = (top, bottom + 1);
        }
        // Graphic control: leave the frame in place, this delay.
        self.out.extend_from_slice(&[0x21, 0xF9, 4, 0x04]);
        self.last_delay = Some((self.out.len(), cs));
        self.out.extend_from_slice(&(cs as u16).to_le_bytes());
        self.out.extend_from_slice(&[0, 0]);
        self.out.push(0x2C);
        for v in [x0, y0, x1 - x0, y1 - y0] {
            self.out.extend_from_slice(&(v as u16).to_le_bytes());
        }
        self.out.push(0);
        let mut rect = Vec::with_capacity((x1 - x0) * (y1 - y0));
        for y in y0..y1 {
            rect.extend_from_slice(&indices[y * w + x0..y * w + x1]);
        }
        self.out.push(8);
        let data = lzw_encode(&rect);
        for block in data.chunks(255) {
            self.out.push(block.len() as u8);
            self.out.extend_from_slice(block);
        }
        self.out.push(0);
        self.previous = Some(indices);
        self.frames += 1;
    }

    /// The bytes so far (without the end marker).
    pub fn len(&self) -> usize {
        self.out.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.out.push(0x3B);
        self.out
    }
}

/// LZW with 8-bit indices: codes of 9 to 12 bits, a clear code when the
/// table is full. A small hash table finds the strings seen before.
fn lzw_encode(indices: &[u8]) -> Vec<u8> {
    const CLEAR: u32 = 256;
    const END: u32 = 257;
    const SLOTS: usize = 8192;
    let mut out = Vec::new();
    let (mut bits, mut nbits) = (0u64, 0u32);
    let mut put = |code: u32, size: u32, out: &mut Vec<u8>| {
        bits |= (code as u64) << nbits;
        nbits += size;
        while nbits >= 8 {
            out.push(bits as u8);
            bits >>= 8;
            nbits -= 8;
        }
    };
    // (prefix << 8 | index) + 1 -> code; 0 is empty.
    let mut keys = vec![0u32; SLOTS];
    let mut codes = vec![0u16; SLOTS];
    let slot_of = |key: u32| (key.wrapping_mul(2_654_435_761) >> 19) as usize & (SLOTS - 1);
    let (mut size, mut next) = (9u32, END + 1);
    put(CLEAR, size, &mut out);
    let Some((&head, rest)) = indices.split_first() else {
        put(END, size, &mut out);
        put(0, 7, &mut out); // flush
        return out;
    };
    let mut current = head as u32;
    for &index in rest {
        let key = (current << 8 | index as u32) + 1;
        let mut slot = slot_of(key);
        while keys[slot] != 0 && keys[slot] != key {
            slot = (slot + 1) & (SLOTS - 1);
        }
        if keys[slot] == key {
            current = codes[slot] as u32;
            continue;
        }
        put(current, size, &mut out);
        keys[slot] = key;
        codes[slot] = next as u16;
        next += 1;
        if next > 1 << size && size < 12 {
            size += 1;
        }
        if next == 4096 {
            put(CLEAR, size, &mut out);
            keys.fill(0);
            (size, next) = (9, END + 1);
        }
        current = index as u32;
    }
    put(current, size, &mut out);
    // The reader adds a string for that code too, which may widen the end code.
    if next == 1 << size && size < 12 && next != END + 1 {
        size += 1;
    }
    put(END, size, &mut out);
    put(0, 7, &mut out); // the last bits out
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(gif: &Gif) -> Vec<(Vec<u32>, u32)> {
        let mut player = Player::new(gif);
        (0..gif.frames.len()).map(|_| { let (p, d) = player.advance(); (p.to_vec(), d) }).collect()
    }

    /// Compare with what ImageMagick made of the same file (raw RGB of
    /// every frame, `-coalesce`).
    #[test]
    fn same_as_magick() {
        let gif = decode(include_bytes!("../testdata/pal.gif")).unwrap();
        let rgb: &[u8] = include_bytes!("../testdata/pal.gif.rgb");
        assert_eq!((gif.width, gif.height, gif.frames.len()), (37, 23, 1));
        let first = gif.first();
        for (i, p) in first.iter().enumerate() {
            assert_eq!([(p >> 16) as u8, (p >> 8) as u8, *p as u8], rgb[i * 3..i * 3 + 3], "pixel {i}");
        }
        let gif = decode(include_bytes!("../testdata/anim.gif")).unwrap();
        let rgb: &[u8] = include_bytes!("../testdata/anim.rgb");
        let n = gif.width * gif.height * 3;
        assert_eq!(rgb.len() % n, 0);
        let shown = frames(&gif);
        assert_eq!(shown.len(), rgb.len() / n);
        for (f, (pixels, _)) in shown.iter().enumerate() {
            for (i, p) in pixels.iter().enumerate() {
                let want = &rgb[f * n + i * 3..f * n + i * 3 + 3];
                assert_eq!([(p >> 16) as u8, (p >> 8) as u8, *p as u8], want, "frame {f} pixel {i}");
            }
        }
        let gif = decode(include_bytes!("../testdata/interlaced.gif")).unwrap();
        let rgb: &[u8] = include_bytes!("../testdata/interlaced.gif.rgb");
        for (i, p) in gif.first().iter().enumerate() {
            assert_eq!([(p >> 16) as u8, (p >> 8) as u8, *p as u8], rgb[i * 3..i * 3 + 3], "pixel {i}");
        }
    }

    #[test]
    fn written_and_read_back() {
        // A moving square on a gradient, 300 x 200: big enough for the LZW
        // table to fill and be cleared many times.
        let picture = |t: usize| move |x: usize, y: usize| -> u32 {
            if (t * 20..t * 20 + 30).contains(&x) && (50..80).contains(&y) {
                0xFF0000
            } else {
                ((x * 255 / 299) as u32) << 8 | (y * 255 / 199) as u32 | ((x ^ y) as u32 & 0xFF) << 16
            }
        };
        let mut writer = Writer::new(300, 200);
        for t in 0..4 {
            writer.frame(200, picture(t));
        }
        writer.frame(200, picture(3)); // the same again: the last one stays longer
        assert_eq!(writer.frames, 4);
        let gif = decode(&writer.finish()).unwrap();
        assert_eq!((gif.width, gif.height, gif.frames.len()), (300, 200, 4));
        assert_eq!(gif.duration_ms(), 1000);
        // Later frames only hold the rectangle that changed.
        assert!(gif.frames[1].width <= 50 && gif.frames[1].height == 30);
        for (t, (pixels, _)) in frames(&gif).iter().enumerate() {
            for (i, &p) in pixels.iter().enumerate() {
                assert_eq!(p, color_of(color_index(picture(t)(i % 300, i / 300))), "frame {t} pixel {i}");
            }
        }
        // Every color of the palette comes back as itself.
        for i in 0..252u8 {
            assert_eq!(color_index(color_of(i)), i);
        }
    }

    #[test]
    fn lzw_round_trips() {
        let mut seed = 12345u32;
        let mut random = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![7],
            vec![0; 100_000],
            (0..70_000).map(|_| random()).collect(),
            (0..70_000).map(|_| random() & 3).collect(),
            (0..=255u8).cycle().take(40_000).collect(),
        ];
        for data in cases {
            let code = lzw_encode(&data);
            assert_eq!(lzw_decode(&code, 8, data.len()).unwrap(), data);
        }
        // Every length around where the code width grows.
        for len in 250..1200 {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            assert_eq!(lzw_decode(&lzw_encode(&data), 8, len).unwrap(), data, "{len}");
        }
    }

    #[test]
    fn interlaced_lines() {
        for height in 1..40 {
            let mut seen = vec![false; height];
            for y in 0..height {
                let p = interlaced_position(y, height);
                assert!(p < height && !seen[p], "height {height} line {y}");
                seen[p] = true;
            }
        }
    }

    #[test]
    fn damaged_files_fail_cleanly() {
        let gif: &[u8] = include_bytes!("../testdata/anim.gif");
        for end in 0..gif.len() {
            let _ = decode(&gif[..end]); // must never panic
        }
        for end in 0..30 {
            assert!(decode(&gif[..end]).is_err(), "cut at {end}");
        }
        for i in 6..gif.len() {
            for bit in [0x01, 0x10, 0x80] {
                let mut data = gif.to_vec();
                data[i] ^= bit;
                if let Ok(g) = decode(&data) {
                    let mut player = Player::new(&g);
                    for _ in 0..g.frames.len() {
                        player.advance();
                    }
                }
            }
        }
        assert_eq!(decode(b"\x89PNG").err(), Some("not a GIF file"));
        let mut huge = gif.to_vec();
        huge[6..8].copy_from_slice(&5000u16.to_le_bytes());
        assert!(decode(&huge).is_err());
        assert!(lzw_decode(&[0xFF, 0xFF], 8, 10).is_err()); // a code before any string
    }
}
