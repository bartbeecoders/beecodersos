//! Pictures: reading PNG (beenet::png), GIF (beenet::gif: the first frame;
//! `view` plays the rest), BMP and PPM files, writing BMP or PNG (`screenshot`),
//! and fitting a picture into a window. A file is untrusted data: every
//! size is checked before use, and pictures are at most `MAX_SIDE` pixels
//! wide and high.

use alloc::vec::Vec;

/// Larger pictures are refused (a 4096x4096 picture takes 64 MiB).
pub const MAX_SIDE: usize = 4096;

/// A picture: `width` x `height` pixels, line by line from the top, each
/// 0xRRGGBB.
#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

fn u16_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?) as u32)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn checked_size(width: usize, height: usize) -> Result<(), &'static str> {
    if width == 0 || height == 0 {
        return Err("the picture has no pixels");
    }
    if width > MAX_SIDE || height > MAX_SIDE {
        return Err("the picture is too large (4096 x 4096 pixels at most)");
    }
    Ok(())
}

/// A JPEG: the coefficients are read in one pass, then each row of
/// blocks is turned into pixels on whichever processor is free.
fn decode_jpeg(data: &[u8]) -> Result<Image, &'static str> {
    // parse bounds the size (photos may be up to 8192 pixels a side, 24 megapixels).
    let jpeg = beenet::jpeg::parse(data)?;
    let mut pixels = alloc::vec![0u32; jpeg.width * jpeg.height];
    let rows = jpeg.mcu_height() * jpeg.width;
    let out = crate::smp::Disjoint(pixels.as_mut_ptr());
    let total = pixels.len();
    let jpeg_ref = &jpeg;
    crate::smp::run(jpeg.mcus_y, &|my| {
        let start = my * rows;
        let len = rows.min(total.saturating_sub(start));
        // SAFETY: each job writes its own rows of pixels.
        let chunk = unsafe { core::slice::from_raw_parts_mut(out.get().add(start), len) };
        jpeg_ref.render_rows(my, chunk);
    });
    Ok(Image { width: jpeg.width, height: jpeg.height, pixels })
}

/// The size a picture file says it has (JPEG, PNG, GIF, BMP), without decoding it.
pub fn dimensions(data: &[u8]) -> Option<(usize, usize)> {
    if beenet::jpeg::is_jpeg(data) {
        return beenet::jpeg::dimensions(data);
    }
    let be = |at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?) as usize);
    if beenet::png::is_png(data) {
        return Some((be(16)?, be(20)?));
    }
    if beenet::gif::is_gif(data) {
        return Some((u16_at(data, 6)? as usize, u16_at(data, 8)? as usize));
    }
    if data.get(..2) == Some(b"BM") {
        let (w, h) = (u32_at(data, 18)? as i32, u32_at(data, 22)? as i32);
        return Some((w.unsigned_abs() as usize, h.unsigned_abs() as usize));
    }
    None
}

/// A JPEG (rows of blocks turned into pixels on every processor), a PNG,
/// a GIF (its first frame), a BMP (uncompressed, 24 or 32 bits per
/// pixel) or a binary PPM (P6, 8 bits).
pub fn decode(data: &[u8]) -> Result<Image, &'static str> {
    if beenet::png::is_png(data) {
        let png = beenet::png::decode(data)?;
        return Ok(Image { width: png.width, height: png.height, pixels: png.pixels });
    }
    if beenet::gif::is_gif(data) {
        let gif = beenet::gif::decode(data)?;
        return Ok(Image { width: gif.width, height: gif.height, pixels: gif.first() });
    }
    if beenet::jpeg::is_jpeg(data) {
        return decode_jpeg(data);
    }
    match data.get(..2) {
        Some(b"BM") => decode_bmp(data),
        Some(b"P6") => decode_ppm(data),
        _ => Err("not a picture this OS can show (JPEG, PNG, GIF, BMP or PPM)"),
    }
}

fn decode_bmp(data: &[u8]) -> Result<Image, &'static str> {
    let bad = "a damaged BMP file";
    let offset = u32_at(data, 10).ok_or(bad)? as usize;
    let header = u32_at(data, 14).ok_or(bad)?;
    if header < 40 {
        return Err("an old kind of BMP this OS cannot show");
    }
    let width = u32_at(data, 18).ok_or(bad)? as i32;
    let height = u32_at(data, 22).ok_or(bad)? as i32;
    let bits = u16_at(data, 28).ok_or(bad)?;
    let compression = u32_at(data, 30).ok_or(bad)?;
    // 0: none; 3 (bit fields) with 32 bits is the usual BGRA layout.
    if !(compression == 0 || (compression == 3 && bits == 32)) || !(bits == 24 || bits == 32) {
        return Err("only uncompressed BMP files with 24 or 32 bits per pixel can be shown");
    }
    if width <= 0 || height == 0 || height == i32::MIN {
        return Err(bad);
    }
    // A negative height: the lines are stored from the top.
    let (width, top_down, height) = (width as usize, height < 0, height.unsigned_abs() as usize);
    checked_size(width, height)?;
    let bytes = bits as usize / 8;
    let stride = (width * bytes).div_ceil(4) * 4;
    let end = offset.checked_add(stride.checked_mul(height).ok_or(bad)?).ok_or(bad)?;
    if end > data.len() {
        return Err("the BMP file is cut off");
    }
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(width * height).map_err(|_| "not enough memory for the picture")?;
    for y in 0..height {
        let stored = if top_down { y } else { height - 1 - y };
        let line = &data[offset + stored * stride..][..width * bytes];
        pixels.extend(line.chunks_exact(bytes).map(|p| (p[2] as u32) << 16 | (p[1] as u32) << 8 | p[0] as u32));
    }
    Ok(Image { width, height, pixels })
}

fn decode_ppm(data: &[u8]) -> Result<Image, &'static str> {
    let bad = "a damaged PPM file";
    // "P6", then width, height and the largest value, separated by white
    // space (and # comments), then one white space character and the pixels.
    let mut at = 2;
    let mut numbers = [0usize; 3];
    for number in numbers.iter_mut() {
        loop {
            match data.get(at) {
                Some(b) if b.is_ascii_whitespace() => at += 1,
                Some(b'#') => {
                    while data.get(at).is_some_and(|b| *b != b'\n') {
                        at += 1;
                    }
                }
                Some(b) if b.is_ascii_digit() => break,
                _ => return Err(bad),
            }
        }
        let start = at;
        while data.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
            if at - start > 6 {
                return Err(bad);
            }
        }
        *number = core::str::from_utf8(&data[start..at]).ok().and_then(|s| s.parse().ok()).ok_or(bad)?;
    }
    let [width, height, max] = numbers;
    if max != 255 {
        return Err("only PPM files with 8 bits per color can be shown");
    }
    checked_size(width, height)?;
    if !data.get(at).is_some_and(u8::is_ascii_whitespace) {
        return Err(bad);
    }
    let start = at + 1;
    let body = data.get(start..start + width * height * 3).ok_or("the PPM file is cut off")?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(width * height).map_err(|_| "not enough memory for the picture")?;
    pixels.extend(body.chunks_exact(3).map(|p| (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32));
    Ok(Image { width, height, pixels })
}

/// A 24-bit BMP of `width` x `height` pixels; `pixel(x, y)` gives each as 0xRRGGBB.
pub fn encode_bmp(width: usize, height: usize, pixel: impl Fn(usize, usize) -> u32) -> Vec<u8> {
    let stride = (width * 3).div_ceil(4) * 4;
    let size = 54 + stride * height;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    // Negative: the lines from the top, the order they are read in.
    out.extend_from_slice(&(-(height as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]); // no compression; sizes and colors left to the reader
    for y in 0..height {
        for x in 0..width {
            let rgb = pixel(x, y);
            out.extend_from_slice(&[rgb as u8, (rgb >> 8) as u8, (rgb >> 16) as u8]);
        }
        out.resize(out.len() + (stride - width * 3), 0);
    }
    out
}

/// The size to show a `width` x `height` picture at in a `room_w` x
/// `room_h` space: as large as fits, never more than twice its size, and
/// as a percentage of its size.
pub fn fit(width: usize, height: usize, room_w: usize, room_h: usize) -> (usize, usize, usize) {
    if width == 0 || height == 0 || room_w == 0 || room_h == 0 {
        return (0, 0, 0);
    }
    // The side that fills its room first decides; at most twice the size.
    let (w, h) = if room_w * height <= room_h * width {
        let w = room_w.min(width * 2);
        (w, height * w / width)
    } else {
        let h = room_h.min(height * 2);
        (width * h / height, h)
    };
    (w.clamp(1, room_w), h.clamp(1, room_h), (w * 100 / width).max(1))
}

/// Which pixel of a `width` x `height` picture goes at (x, y) of a
/// `room_w` x `room_h` space that it covers completely: scaled to fill it
/// (keeping its shape) and cut off evenly on the sides that stick out.
pub fn cover_pixel(width: usize, height: usize, room_w: usize, room_h: usize, x: usize, y: usize) -> (usize, usize) {
    // Scaled size: the side that needs the larger factor decides.
    let (sw, sh) = if room_w * height >= room_h * width {
        (room_w, (height * room_w).div_ceil(width).max(room_h))
    } else {
        ((width * room_h).div_ceil(height).max(room_w), room_h)
    };
    let (dx, dy) = ((sw - room_w) / 2, (sh - room_h) / 2);
    (((x + dx) * width / sw).min(width - 1), ((y + dy) * height / sh).min(height - 1))
}

/// A PNG made on every processor in use: the lines are read and filtered
/// in parallel, then compressed in `PIECES` pieces at once (each piece on
/// its own, joined into one stream; beenet::deflate). The same file as
/// `beenet::png::encode` would make, but for where the pieces start.
pub fn encode_png(width: usize, height: usize, pixel: &(dyn Fn(usize, usize) -> u32 + Sync)) -> Vec<u8> {
    use crate::smp::{self, Disjoint};
    const PIECES: usize = 16;
    let line = width * 3;
    let mut rgb = alloc::vec![0u8; line * height];
    let rgb_at = Disjoint(rgb.as_mut_ptr());
    smp::run(height, &|y| {
        // SAFETY: each job writes its own line.
        let out = unsafe { core::slice::from_raw_parts_mut(rgb_at.get().add(y * line), line) };
        for x in 0..width {
            let c = pixel(x, y);
            out[x * 3..x * 3 + 3].copy_from_slice(&[(c >> 16) as u8, (c >> 8) as u8, c as u8]);
        }
    });
    let mut raw = alloc::vec![0u8; (line + 1) * height];
    let raw_at = Disjoint(raw.as_mut_ptr());
    let rgb = &rgb;
    smp::run(height, &|y| {
        // SAFETY: each job writes its own filtered line; the RGB lines are only read.
        let out = unsafe { core::slice::from_raw_parts_mut(raw_at.get().add(y * (line + 1)), line + 1) };
        let prev = if y > 0 { Some(&rgb[(y - 1) * line..y * line]) } else { None };
        beenet::png::filter_row(prev, &rgb[y * line..(y + 1) * line], out);
    });
    // Pieces of whole lines.
    let lines_per = height.div_ceil(PIECES).max(1);
    let chunks: Vec<&[u8]> = raw.chunks(lines_per * (line + 1)).collect();
    let last = chunks.len().saturating_sub(1);
    let mut pieces: Vec<Vec<u8>> = (0..chunks.len()).map(|_| Vec::new()).collect();
    let pieces_at = Disjoint(pieces.as_mut_ptr());
    let chunks_ref = &chunks;
    smp::run(chunks.len(), &|i| {
        // SAFETY: each job sets its own piece.
        unsafe { *pieces_at.get().add(i) = beenet::deflate::deflate_piece(chunks_ref[i], i == last) };
    });
    let z = beenet::deflate::zlib_from_pieces(&pieces, beenet::inflate::adler32(&raw));
    beenet::png::from_zlib(width, height, &z)
}

/// The picture made smaller to fit in `max_w` x `max_h` (never larger),
/// its shape kept: each pixel the average of those it covers.
pub fn shrunk(picture: &Image, max_w: usize, max_h: usize) -> Image {
    let (w, h) = (picture.width.max(1), picture.height.max(1));
    let (max_w, max_h) = (max_w.max(1), max_h.max(1));
    // The scale that fits both ways, as a fraction tw/w or th/h.
    let (tw, th) = if w * max_h >= h * max_w { (max_w.min(w), (h * max_w.min(w) / w).max(1)) } else { ((w * max_h.min(h) / h).max(1), max_h.min(h)) };
    let mut pixels = Vec::with_capacity(tw * th);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1));
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1.min(h) {
                for &p in &picture.pixels[y * w + x0..y * w + x1.min(w)] {
                    r += p >> 16 & 0xFF;
                    g += p >> 8 & 0xFF;
                    b += p & 0xFF;
                    n += 1;
                }
            }
            let n = n.max(1);
            pixels.push((r / n) << 16 | (g / n) << 8 | b / n);
        }
    }
    Image { width: tw, height: th, pixels }
}

/// A JPEG of `width` x `height` pixels at `quality`: the rows of 16 lines
/// are turned into coefficients on every processor, then coded in order.
pub fn encode_jpeg(width: usize, height: usize, quality: u32, pixel: &(dyn Fn(usize, usize) -> u32 + Sync)) -> Vec<u8> {
    let writer = beenet::jpeg::Writer::new(width, height, quality);
    let mut rows: Vec<Vec<i16>> = (0..writer.mcu_rows()).map(|_| Vec::new()).collect();
    let out = crate::smp::Disjoint(rows.as_mut_ptr());
    let w = &writer;
    crate::smp::run(writer.mcu_rows(), &|my| {
        // SAFETY: each job sets its own row.
        unsafe { *out.get().add(my) = w.row(my, |x, y| pixel(x, y)) };
    });
    writer.finish(&rows)
}

