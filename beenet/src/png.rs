//! PNG pictures (the parts a viewer needs): every color type (gray, RGB,
//! palette, gray + alpha, RGBA), bit depths 1 to 16, the five line filters
//! and Adam7 interlacing. Transparent pixels are blended onto black. A file
//! is untrusted: chunk lengths, sizes and the decompressed data are all
//! checked, and pictures are at most `MAX_SIDE` pixels on a side.

use crate::inflate;
use alloc::vec::Vec;

pub const MAX_SIDE: usize = 4096;
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// A decoded picture: line by line from the top, each pixel 0xRRGGBB.
#[derive(Debug, PartialEq, Eq)]
pub struct Png {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

pub fn is_png(data: &[u8]) -> bool {
    data.starts_with(&SIGNATURE)
}

struct Header {
    width: usize,
    height: usize,
    depth: u8,
    color: u8,
    interlaced: bool,
}

impl Header {
    /// Samples per pixel.
    fn channels(&self) -> usize {
        match self.color {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            _ => 4,
        }
    }

    /// Bytes in one line of `width` pixels (without the filter byte).
    fn line_bytes(&self, width: usize) -> usize {
        (width * self.channels() * self.depth as usize).div_ceil(8)
    }

    /// Bytes per whole pixel, at least 1 (what the filters look back by).
    fn pixel_bytes(&self) -> usize {
        (self.channels() * self.depth as usize).div_ceil(8).max(1)
    }
}

pub fn decode(data: &[u8]) -> Result<Png, &'static str> {
    if !is_png(data) {
        return Err("not a PNG file");
    }
    let mut at = 8;
    let mut header: Option<Header> = None;
    let mut palette: Vec<u32> = Vec::new();
    let mut alpha: Vec<u8> = Vec::new(); // palette transparency (tRNS)
    let mut compressed: Vec<u8> = Vec::new();
    loop {
        let len = u32::from_be_bytes(data.get(at..at + 4).ok_or("the PNG file is cut off")?.try_into().unwrap()) as usize;
        let kind = data.get(at + 4..at + 8).ok_or("the PNG file is cut off")?;
        let body = data.get(at + 8..at.saturating_add(8).saturating_add(len)).ok_or("the PNG file is cut off")?;
        match kind {
            b"IHDR" => {
                if body.len() != 13 {
                    return Err("a damaged PNG file");
                }
                let w = u32::from_be_bytes(body[0..4].try_into().unwrap()) as usize;
                let h = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
                let (depth, color, interlace) = (body[8], body[9], body[12]);
                let depth_ok = match color {
                    0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
                    3 => matches!(depth, 1 | 2 | 4 | 8),
                    2 | 4 | 6 => matches!(depth, 8 | 16),
                    _ => false,
                };
                if !depth_ok || body[10] != 0 || body[11] != 0 || interlace > 1 {
                    return Err("a kind of PNG this OS cannot show");
                }
                if w == 0 || h == 0 {
                    return Err("the picture has no pixels");
                }
                if w > MAX_SIDE || h > MAX_SIDE {
                    return Err("the picture is too large (4096 x 4096 pixels at most)");
                }
                header = Some(Header { width: w, height: h, depth, color, interlaced: interlace == 1 });
            }
            b"PLTE" => {
                if body.len() % 3 != 0 || body.len() > 256 * 3 {
                    return Err("a damaged PNG file");
                }
                palette = body.chunks(3).map(|c| (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).collect();
            }
            b"tRNS" => alpha = body.iter().copied().take(256).collect(),
            b"IDAT" => {
                if compressed.len() + body.len() > 64 * 1024 * 1024 {
                    return Err("the PNG file is too large");
                }
                compressed.extend_from_slice(body);
            }
            b"IEND" => break,
            _ => {} // text, color profiles, ...: not needed to show it
        }
        at = at + 12 + len; // length, type, body, CRC
    }
    let h = header.ok_or("a damaged PNG file")?;
    if h.color == 3 && palette.is_empty() {
        return Err("a damaged PNG file");
    }
    // What the decompressed data must hold: each pass's lines with a filter byte each.
    let passes: Vec<(usize, usize, usize, usize, usize, usize)> = if h.interlaced {
        // (x start, y start, x step, y step) of Adam7's seven passes.
        [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]
            .iter()
            .map(|&(x0, y0, dx, dy)| {
                let w = if h.width > x0 { (h.width - x0).div_ceil(dx) } else { 0 };
                let hh = if h.height > y0 { (h.height - y0).div_ceil(dy) } else { 0 };
                (x0, y0, dx, dy, w, hh)
            })
            .collect()
    } else {
        alloc::vec![(0, 0, 1, 1, h.width, h.height)]
    };
    let expected: usize = passes.iter().filter(|p| p.4 > 0 && p.5 > 0).map(|p| (h.line_bytes(p.4) + 1) * p.5).sum();
    let raw = inflate::zlib(&compressed, expected).map_err(|_| "a damaged PNG file")?;
    if raw.len() != expected {
        return Err("the PNG file is cut off");
    }
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(h.width * h.height).map_err(|_| "not enough memory for the picture")?;
    pixels.resize(h.width * h.height, 0);
    let mut at = 0;
    for &(x0, y0, dx, dy, pw, ph) in &passes {
        if pw == 0 || ph == 0 {
            continue;
        }
        let line = h.line_bytes(pw);
        let mut previous = alloc::vec![0u8; line];
        let mut current = alloc::vec![0u8; line];
        for row in 0..ph {
            let filter = raw[at];
            current.copy_from_slice(&raw[at + 1..at + 1 + line]);
            at += 1 + line;
            unfilter(filter, &mut current, &previous, h.pixel_bytes())?;
            for col in 0..pw {
                let rgb = pixel(&h, &current, col, &palette, &alpha);
                pixels[(y0 + row * dy) * h.width + x0 + col * dx] = rgb;
            }
            core::mem::swap(&mut previous, &mut current);
        }
    }
    Ok(Png { width: h.width, height: h.height, pixels })
}

fn unfilter(filter: u8, line: &mut [u8], previous: &[u8], bpp: usize) -> Result<(), &'static str> {
    for i in 0..line.len() {
        let left = if i >= bpp { line[i - bpp] } else { 0 };
        let up = previous[i];
        let up_left = if i >= bpp { previous[i - bpp] } else { 0 };
        let add = match filter {
            0 => 0,
            1 => left,
            2 => up,
            3 => ((left as u16 + up as u16) / 2) as u8,
            4 => {
                let p = left as i16 + up as i16 - up_left as i16;
                let (pa, pb, pc) = ((p - left as i16).abs(), (p - up as i16).abs(), (p - up_left as i16).abs());
                if pa <= pb && pa <= pc {
                    left
                } else if pb <= pc {
                    up
                } else {
                    up_left
                }
            }
            _ => return Err("a damaged PNG file"),
        };
        line[i] = line[i].wrapping_add(add);
    }
    Ok(())
}

/// Sample `n` of a line (in units of the bit depth), scaled to 0..=255.
fn sample(h: &Header, line: &[u8], n: usize) -> u32 {
    match h.depth {
        16 => line[n * 2] as u32,
        8 => line[n] as u32,
        d => {
            let per_byte = 8 / d as usize;
            let byte = line[n / per_byte];
            let shift = 8 - d as usize * (n % per_byte + 1);
            let v = (byte >> shift) as u32 & ((1 << d) - 1);
            if h.color == 3 { v } else { v * 255 / ((1 << d) - 1) }
        }
    }
}

fn pixel(h: &Header, line: &[u8], col: usize, palette: &[u32], alpha: &[u8]) -> u32 {
    let c = h.channels();
    let s = |k: usize| sample(h, line, col * c + k);
    let blend = |v: u32, a: u32| (v * a + 127) / 255;
    match h.color {
        0 => s(0) * 0x010101,
        2 => s(0) << 16 | s(1) << 8 | s(2),
        3 => {
            let i = s(0) as usize;
            let rgb = palette.get(i).copied().unwrap_or(0);
            let a = alpha.get(i).copied().unwrap_or(255) as u32;
            blend(rgb >> 16, a) << 16 | blend(rgb >> 8 & 0xFF, a) << 8 | blend(rgb & 0xFF, a)
        }
        4 => blend(s(0), s(1)) * 0x010101,
        _ => {
            let a = s(3);
            blend(s(0), a) << 16 | blend(s(1), a) << 8 | blend(s(2), a)
        }
    }
}

/// A PNG of `width` x `height` pixels (`pixel(x, y)` gives 0xRRGGBB): RGB, 8
/// bits, the data in stored (uncompressed) DEFLATE blocks. Simple and valid
/// everywhere; about as large as a BMP.
pub fn encode(width: usize, height: usize, pixel: impl Fn(usize, usize) -> u32) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width * 3 + 1) * height);
    for y in 0..height {
        raw.push(0); // no filter
        for x in 0..width {
            let rgb = pixel(x, y);
            raw.extend_from_slice(&[(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8]);
        }
    }
    let mut z = alloc::vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(block) = blocks.next() {
        z.push(blocks.peek().is_none() as u8);
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&inflate::adler32(&raw).to_be_bytes());
    let mut out = SIGNATURE.to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// CRC-32 (IEEE), as PNG chunks carry it.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a test picture and compare with what ImageMagick made of it
    /// (raw RGB, transparency blended onto black); rounding of 16-bit
    /// samples and blending may differ by one.
    fn same_as_magick(png: &[u8], rgb: &[u8]) {
        let picture = decode(png).unwrap();
        assert_eq!((picture.width, picture.height), (37, 23));
        assert_eq!(rgb.len(), 37 * 23 * 3);
        for (i, p) in picture.pixels.iter().enumerate() {
            let want = &rgb[i * 3..i * 3 + 3];
            let got = [(p >> 16) as u8, (p >> 8) as u8, *p as u8];
            for k in 0..3 {
                assert!((got[k] as i16 - want[k] as i16).abs() <= 1, "pixel {i}: {got:?} != {want:?}");
            }
        }
    }

    #[test]
    fn every_color_type() {
        same_as_magick(include_bytes!("../testdata/rgb.png"), include_bytes!("../testdata/rgb.rgb"));
        same_as_magick(include_bytes!("../testdata/rgba.png"), include_bytes!("../testdata/rgba.rgb"));
        same_as_magick(include_bytes!("../testdata/gray.png"), include_bytes!("../testdata/gray.rgb"));
        same_as_magick(include_bytes!("../testdata/pal.png"), include_bytes!("../testdata/pal.rgb"));
        same_as_magick(include_bytes!("../testdata/graya.png"), include_bytes!("../testdata/graya.rgb"));
        same_as_magick(include_bytes!("../testdata/interlaced.png"), include_bytes!("../testdata/interlaced.rgb"));
    }

    #[test]
    fn written_and_read_back() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082); // as in every PNG file
        let colors = |x: usize, y: usize| ((x * 7) as u32) << 16 | ((y * 3) as u32) << 8 | (x ^ y) as u32;
        // Wide enough that the data needs more than one stored block.
        let png = encode(300, 80, colors);
        let back = decode(&png).unwrap();
        assert_eq!((back.width, back.height), (300, 80));
        assert!(back.pixels.iter().enumerate().all(|(i, p)| *p == colors(i % 300, i / 300) & 0xFFFFFF));
        assert_eq!(decode(&encode(1, 1, |_, _| 0x123456)).unwrap().pixels, alloc::vec![0x123456]);
    }

    #[test]
    fn damaged_files_fail_cleanly() {
        let png: &[u8] = include_bytes!("../testdata/rgb.png");
        // Cut anywhere before the end marker: refused. (Without the end
        // marker's own checksum the picture is complete, and shown.)
        for end in 0..png.len() - 12 {
            assert!(decode(&png[..end]).is_err(), "cut at {end}");
        }
        for i in 8..png.len() {
            let mut data = png.to_vec();
            data[i] ^= 0x55;
            let _ = decode(&data); // must never panic
        }
        assert_eq!(decode(b"GIF89a"), Err("not a PNG file"));
        // A huge size in the header is refused before anything is allocated.
        let mut huge = png.to_vec();
        huge[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        assert!(decode(&huge).is_err());
    }
}
