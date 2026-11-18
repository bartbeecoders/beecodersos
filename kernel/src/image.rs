//! Pictures: reading BMP and PPM files (`view`), writing BMP (`screenshot`),
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

/// A BMP (uncompressed, 24 or 32 bits per pixel) or a binary PPM (P6, 8 bits).
pub fn decode(data: &[u8]) -> Result<Image, &'static str> {
    match data.get(..2) {
        Some(b"BM") => decode_bmp(data),
        Some(b"P6") => decode_ppm(data),
        _ => Err("not a picture this OS can show (BMP or PPM)"),
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
