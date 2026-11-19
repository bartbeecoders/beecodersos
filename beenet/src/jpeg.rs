//! JPEG pictures (baseline, ITU T.81): Huffman-coded 8x8 blocks, restart
//! markers, grayscale or YCbCr with 4:4:4, 4:2:2 or 4:2:0 color
//! subsampling (colors are taken from the nearest chroma sample).
//! Progressive and arithmetic-coded JPEGs are refused with a clear message.
//!
//! Decoding has two halves: `parse` reads the file and the Huffman codes
//! into the blocks' coefficients (one pass, in order), and `render_rows`
//! turns one row of MCUs into pixels (inverse DCT, color conversion). Rows
//! are independent, so the kernel renders them on every processor. A file
//! is untrusted: every length, table and code is checked, and sizes are
//! bounded (`MAX_SIDE`).

use alloc::vec;
use alloc::vec::Vec;

pub const MAX_SIDE: usize = 8192;
/// At most this many pixels (a 12-megapixel photo is 4000 x 3000).
pub const MAX_PIXELS: usize = 24 * 1024 * 1024;

pub fn is_jpeg(data: &[u8]) -> bool {
    data.starts_with(&[0xFF, 0xD8, 0xFF])
}

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22,
    15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// round(4096 * C(u) * cos((2x+1) u pi / 16)), C(0) = 1/sqrt(2): the
/// inverse DCT in whole numbers.
const COS: [[i32; 8]; 8] = [
    [2896, 2896, 2896, 2896, 2896, 2896, 2896, 2896],
    [4017, 3406, 2276, 799, -799, -2276, -3406, -4017],
    [3784, 1567, -1567, -3784, -3784, -1567, 1567, 3784],
    [3406, -799, -4017, -2276, 2276, 4017, 799, -3406],
    [2896, -2896, -2896, 2896, 2896, -2896, -2896, 2896],
    [2276, -4017, 799, 3406, -3406, -799, 4017, -2276],
    [1567, -3784, 3784, -1567, -1567, 3784, -3784, 1567],
    [799, -2276, 3406, -4017, 4017, -3406, 2276, -799],
];

/// A canonical Huffman table: codes by length, and the symbols in order.
#[derive(Clone, Default)]
struct Huffman {
    /// For each length 1..=16: the first code, its index in `symbols`, how many.
    first: [(u16, u16, u16); 17],
    symbols: Vec<u8>,
}

impl Huffman {
    fn new(counts: &[u8; 16], symbols: &[u8]) -> Option<Huffman> {
        let mut first = [(0u16, 0u16, 0u16); 17];
        let (mut code, mut index) = (0u32, 0u16);
        for len in 1..=16 {
            let n = counts[len - 1] as u32;
            first[len] = (code as u16, index, n as u16);
            code += n;
            if code > 1 << len {
                return None; // more codes than fit
            }
            index += n as u16;
            code <<= 1;
        }
        Some(Huffman { first, symbols: symbols.to_vec() })
    }
}

/// The bits of a scan: 0xFF 0x00 is a 0xFF byte, a marker ends the data.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u32,
    n: u32,
    /// A marker was reached: only zero bits from here.
    marker: bool,
}

impl<'a> Bits<'a> {
    fn fill(&mut self) {
        while self.n <= 24 {
            let mut byte = 0u8;
            if !self.marker {
                match (self.data.get(self.pos), self.data.get(self.pos + 1)) {
                    (Some(0xFF), Some(0x00)) => {
                        byte = 0xFF;
                        self.pos += 2;
                    }
                    (Some(0xFF), _) | (None, _) => self.marker = true,
                    (Some(&b), _) => {
                        byte = b;
                        self.pos += 1;
                    }
                }
            }
            self.acc |= (byte as u32) << (24 - self.n);
            self.n += 8;
        }
    }

    fn bit(&mut self) -> u32 {
        self.fill();
        let b = self.acc >> 31;
        self.acc <<= 1;
        self.n -= 1;
        b
    }

    fn bits(&mut self, count: u32) -> u32 {
        if count == 0 {
            return 0;
        }
        self.fill();
        let v = self.acc >> (32 - count);
        self.acc <<= count;
        self.n -= count;
        v
    }

    fn decode(&mut self, h: &Huffman) -> Option<u8> {
        let mut code = 0u16;
        for len in 1..=16 {
            code = code << 1 | self.bit() as u16;
            let (first, index, count) = h.first[len];
            if count > 0 && code >= first && (code as u32) < first as u32 + count as u32 {
                return h.symbols.get((index + code - first) as usize).copied();
            }
        }
        None
    }

    /// `count` bits as a signed value (JPEG's "EXTEND").
    fn signed(&mut self, count: u32) -> i32 {
        if count == 0 || count > 16 {
            return 0;
        }
        let v = self.bits(count) as i32;
        if v < 1 << (count - 1) { v - (1 << count) + 1 } else { v }
    }

    /// Skip to the next restart marker (RSTn) and start again after it.
    fn restart(&mut self) {
        self.acc = 0;
        self.n = 0;
        self.marker = false;
        while self.pos + 1 < self.data.len() && !(self.data[self.pos] == 0xFF && (0xD0..=0xD7).contains(&self.data[self.pos + 1])) {
            self.pos += 1;
        }
        self.pos = (self.pos + 2).min(self.data.len());
    }
}

#[derive(Clone)]
struct Component {
    h: usize,
    v: usize,
    quant: usize,
    /// Blocks across and down (whole MCUs).
    bw: usize,
    bh: usize,
    /// The quantized coefficients, 64 per block, in natural order.
    coef: Vec<i16>,
}

/// A JPEG after `parse`: ready to be turned into pixels.
pub struct Jpeg {
    pub width: usize,
    pub height: usize,
    comps: Vec<Component>,
    quant: [[u16; 64]; 4],
    hmax: usize,
    vmax: usize,
    pub mcus_x: usize,
    pub mcus_y: usize,
}

fn u16_at(d: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?) as usize)
}

pub fn parse(data: &[u8]) -> Result<Jpeg, &'static str> {
    if !is_jpeg(data) {
        return Err("not a JPEG file");
    }
    let bad = "a damaged JPEG file";
    let mut quant = [[1u16; 64]; 4];
    let mut dc: [Option<Huffman>; 4] = Default::default();
    let mut ac: [Option<Huffman>; 4] = Default::default();
    let mut frame: Option<Jpeg> = None;
    let mut restart_interval = 0usize;
    let mut at = 2;
    loop {
        // Markers may be padded with extra 0xFF bytes.
        while data.get(at) == Some(&0xFF) && data.get(at + 1) == Some(&0xFF) {
            at += 1;
        }
        if data.get(at) != Some(&0xFF) {
            return Err(bad);
        }
        let marker = *data.get(at + 1).ok_or(bad)?;
        at += 2;
        if marker == 0xD9 {
            break; // end of image
        }
        let len = u16_at(data, at).ok_or(bad)?;
        let seg = data.get(at + 2..at + len).ok_or(bad)?;
        if len < 2 {
            return Err(bad);
        }
        match marker {
            0xDB => {
                let mut i = 0;
                while i < seg.len() {
                    let (precision, id) = (seg[i] >> 4, (seg[i] & 15) as usize);
                    if id > 3 {
                        return Err(bad);
                    }
                    let size = if precision == 0 { 1 } else { 2 };
                    let table = seg.get(i + 1..i + 1 + 64 * size).ok_or(bad)?;
                    for k in 0..64 {
                        let q = if size == 1 { table[k] as u16 } else { u16::from_be_bytes([table[2 * k], table[2 * k + 1]]) };
                        quant[id][ZIGZAG[k]] = q.max(1);
                    }
                    i += 1 + 64 * size;
                }
            }
            0xC4 => {
                let mut i = 0;
                while i < seg.len() {
                    let (class, id) = (seg[i] >> 4, (seg[i] & 15) as usize);
                    let counts: [u8; 16] = seg.get(i + 1..i + 17).ok_or(bad)?.try_into().map_err(|_| bad)?;
                    let total: usize = counts.iter().map(|c| *c as usize).sum();
                    let symbols = seg.get(i + 17..i + 17 + total).ok_or(bad)?;
                    let table = Huffman::new(&counts, symbols).ok_or(bad)?;
                    match (class, id) {
                        (0, 0..=3) => dc[id] = Some(table),
                        (1, 0..=3) => ac[id] = Some(table),
                        _ => return Err(bad),
                    }
                    i += 17 + total;
                }
            }
            0xDD => restart_interval = u16_at(seg, 0).ok_or(bad)?,
            0xC0 | 0xC1 => frame = Some(read_frame(seg)?),
            0xC2 | 0xC6 | 0xCA | 0xCE => return Err("a progressive JPEG (not supported yet)"),
            0xC3 | 0xC5 | 0xC7 | 0xC9..=0xCB | 0xCD | 0xCF => return Err("a kind of JPEG this OS cannot show (lossless or arithmetic coding)"),
            0xDA => {
                let jpeg = frame.as_mut().ok_or(bad)?;
                // Baseline images have all components in the first scan: done after it.
                read_scan(jpeg, seg, &data[at + len..], &dc, &ac, restart_interval)?;
                break;
            }
            _ => {} // APPn, comments: skipped
        }
        at += len;
    }
    let mut jpeg = frame.ok_or(bad)?;
    jpeg.quant = quant;
    Ok(jpeg)
}

fn read_frame(seg: &[u8]) -> Result<Jpeg, &'static str> {
    let bad = "a damaged JPEG file";
    if *seg.first().ok_or(bad)? != 8 {
        return Err("a 12-bit JPEG (not supported)");
    }
    let height = u16_at(seg, 1).ok_or(bad)?;
    let width = u16_at(seg, 3).ok_or(bad)?;
    let n = *seg.get(5).ok_or(bad)? as usize;
    if width == 0 || height == 0 {
        return Err("a JPEG without pixels (or with its height given later, which is not supported)");
    }
    if width > MAX_SIDE || height > MAX_SIDE || width * height > MAX_PIXELS {
        return Err("the picture is too large");
    }
    if n != 1 && n != 3 {
        return Err("a JPEG with other colors than gray or YCbCr (e.g. CMYK)");
    }
    let mut comps = Vec::new();
    for i in 0..n {
        let c = seg.get(6 + i * 3..9 + i * 3).ok_or(bad)?;
        let (h, v) = ((c[1] >> 4) as usize, (c[1] & 15) as usize);
        if !(1..=2).contains(&h) || !(1..=2).contains(&v) || c[2] > 3 {
            return Err("a JPEG with a color layout this OS cannot show");
        }
        comps.push(Component { h, v, quant: c[2] as usize, bw: 0, bh: 0, coef: Vec::new() });
    }
    if n == 1 {
        comps[0].h = 1; // one component: blocks in plain order whatever it says
        comps[0].v = 1;
    }
    let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1);
    let (mcus_x, mcus_y) = (width.div_ceil(8 * hmax), height.div_ceil(8 * vmax));
    for c in &mut comps {
        c.bw = mcus_x * c.h;
        c.bh = mcus_y * c.v;
        c.coef = vec![0i16; c.bw * c.bh * 64];
    }
    Ok(Jpeg { width, height, comps, quant: [[1; 64]; 4], hmax, vmax, mcus_x, mcus_y })
}

/// Read a scan's coefficients from the entropy-coded `data` after its header.
fn read_scan(jpeg: &mut Jpeg, seg: &[u8], data: &[u8], dc: &[Option<Huffman>; 4], ac: &[Option<Huffman>; 4], restart_interval: usize) -> Result<(), &'static str> {
    let bad = "a damaged JPEG file";
    let n = *seg.first().ok_or(bad)? as usize;
    if n != jpeg.comps.len() {
        return Err("a JPEG with its colors in separate scans (not supported yet)");
    }
    let mut tables = Vec::new();
    for i in 0..n {
        let s = seg.get(1 + i * 2..3 + i * 2).ok_or(bad)?;
        let (d, a) = ((s[1] >> 4) as usize, (s[1] & 15) as usize);
        let dct = dc.get(d).and_then(|t| t.as_ref()).ok_or(bad)?;
        let act = ac.get(a).and_then(|t| t.as_ref()).ok_or(bad)?;
        tables.push((dct, act));
    }
    let mut bits = Bits { data, pos: 0, acc: 0, n: 0, marker: false };
    let mut pred = vec![0i32; n];
    let total = jpeg.mcus_x * jpeg.mcus_y;
    for mcu in 0..total {
        if restart_interval > 0 && mcu > 0 && mcu % restart_interval == 0 {
            bits.restart();
            pred.iter_mut().for_each(|p| *p = 0);
        }
        let (mx, my) = (mcu % jpeg.mcus_x, mcu / jpeg.mcus_x);
        for (ci, comp) in jpeg.comps.iter_mut().enumerate() {
            let (dct, act) = tables[ci];
            for by in 0..comp.v {
                for bx in 0..comp.h {
                    let (x, y) = (mx * comp.h + bx, my * comp.v + by);
                    let block = &mut comp.coef[(y * comp.bw + x) * 64..][..64];
                    let s = bits.decode(dct).ok_or(bad)? as u32;
                    pred[ci] += bits.signed(s);
                    block[0] = pred[ci].clamp(-32768, 32767) as i16;
                    let mut k = 1;
                    while k < 64 {
                        let rs = bits.decode(act).ok_or(bad)?;
                        let (r, s) = ((rs >> 4) as usize, (rs & 15) as u32);
                        if s == 0 {
                            if r != 15 {
                                break; // end of block
                            }
                            k += 16;
                            continue;
                        }
                        k += r;
                        if k > 63 {
                            return Err(bad);
                        }
                        block[ZIGZAG[k]] = bits.signed(s).clamp(-32768, 32767) as i16;
                        k += 1;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The inverse DCT of one block of dequantized coefficients: 64 samples.
fn idct(coef: &[i16], quant: &[u16; 64], out: &mut [u8; 64]) {
    let mut tmp = [0i32; 64];
    // Rows: tmp[v][x] = sum_u F[v][u] * C(u) cos(...)
    for v in 0..8 {
        let row = &coef[v * 8..v * 8 + 8];
        if row.iter().all(|c| *c == 0) {
            continue;
        }
        for x in 0..8 {
            let mut s = 0i64;
            for u in 0..8 {
                s += row[u] as i64 * quant[v * 8 + u] as i64 * COS[u][x] as i64;
            }
            tmp[v * 8 + x] = ((s + 2048) >> 12).clamp(-(1 << 24), 1 << 24) as i32;
        }
    }
    // Columns, then /4 for the two halves, and the level shift.
    for x in 0..8 {
        for y in 0..8 {
            let mut s = 0i64;
            for v in 0..8 {
                s += tmp[v * 8 + x] as i64 * COS[v][y] as i64;
            }
            let value = ((s + (1 << 13)) >> 14) + 128;
            out[y * 8 + x] = value.clamp(0, 255) as u8;
        }
    }
}

impl Jpeg {
    /// Pixels (0xRRGGBB) of MCU row `my`: `out` holds the picture's lines
    /// from `my * mcu_height()` on (as many as are left, at most a whole
    /// MCU), `width` pixels each.
    pub fn render_rows(&self, my: usize, out: &mut [u32]) {
        let mh = 8 * self.vmax;
        let y0 = my * mh;
        let lines = mh.min(self.height.saturating_sub(y0));
        // Each component's samples for this MCU row, full width of its blocks.
        let mut planes: Vec<Vec<u8>> = Vec::new();
        for c in &self.comps {
            let (pw, ph) = (c.bw * 8, c.v * 8);
            let mut plane = vec![0u8; pw * ph];
            let mut block = [0u8; 64];
            for by in 0..c.v {
                for bx in 0..c.bw {
                    let at = ((my * c.v + by) * c.bw + bx) * 64;
                    idct(&c.coef[at..at + 64], &self.quant[c.quant], &mut block);
                    for yy in 0..8 {
                        plane[(by * 8 + yy) * pw + bx * 8..][..8].copy_from_slice(&block[yy * 8..yy * 8 + 8]);
                    }
                }
            }
            planes.push(plane);
        }
        for ly in 0..lines {
            for x in 0..self.width {
                let sample = |ci: usize| -> i32 {
                    let c = &self.comps[ci];
                    let (sx, sy) = (x * c.h / self.hmax, ly * c.v / self.vmax);
                    planes[ci][sy * c.bw * 8 + sx] as i32
                };
                let rgb = if self.comps.len() == 1 {
                    let g = sample(0) as u32;
                    g << 16 | g << 8 | g
                } else {
                    let (yv, cb, cr) = (sample(0) << 16, sample(1) - 128, sample(2) - 128);
                    let r = (yv + 91_881 * cr + 32768) >> 16;
                    let g = (yv - 22_554 * cb - 46_802 * cr + 32768) >> 16;
                    let b = (yv + 116_130 * cb + 32768) >> 16;
                    (r.clamp(0, 255) as u32) << 16 | (g.clamp(0, 255) as u32) << 8 | b.clamp(0, 255) as u32
                };
                if let Some(p) = out.get_mut(ly * self.width + x) {
                    *p = rgb;
                }
            }
        }
    }

    /// Lines in one row of MCUs.
    pub fn mcu_height(&self) -> usize {
        8 * self.vmax
    }
}

/// The whole picture (one row of MCUs after the other).
pub fn decode(data: &[u8]) -> Result<(usize, usize, Vec<u32>), &'static str> {
    let jpeg = parse(data)?;
    let mut pixels = vec![0u32; jpeg.width * jpeg.height];
    let rows = jpeg.mcu_height() * jpeg.width;
    for (my, chunk) in pixels.chunks_mut(rows).enumerate() {
        jpeg.render_rows(my, chunk);
    }
    Ok((jpeg.width, jpeg.height, pixels))
}

/// The size a JPEG says it has, without decoding it.
pub fn dimensions(data: &[u8]) -> Option<(usize, usize)> {
    if !is_jpeg(data) {
        return None;
    }
    let mut at = 2;
    while at + 9 < data.len() {
        if data[at] != 0xFF {
            return None;
        }
        let marker = data[at + 1];
        if marker == 0xFF {
            at += 1;
            continue;
        }
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            return Some((u16_at(data, at + 7)?, u16_at(data, at + 5)?));
        }
        at += 2 + u16_at(data, at + 2)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare with ImageMagick's pixels (libjpeg, colors not smoothed):
    /// the inverse DCT and color conversion may round a little differently.
    fn same_as_magick(jpg: &[u8], rgb: &[u8], size: (usize, usize)) {
        let (w, h, pixels) = decode(jpg).unwrap();
        assert_eq!((w, h), size);
        assert_eq!(rgb.len(), w * h * 3);
        let mut worst = 0;
        for (i, p) in pixels.iter().enumerate() {
            for (k, got) in [(p >> 16) as u8, (p >> 8) as u8, *p as u8].into_iter().enumerate() {
                worst = worst.max((got as i32 - rgb[i * 3 + k] as i32).abs());
            }
        }
        assert!(worst <= 4, "differs by up to {worst}");
    }

    #[test]
    fn every_layout() {
        same_as_magick(include_bytes!("../testdata/j444.jpg"), include_bytes!("../testdata/j444.rgb"), (37, 23));
        same_as_magick(include_bytes!("../testdata/j420.jpg"), include_bytes!("../testdata/j420.rgb"), (37, 23));
        same_as_magick(include_bytes!("../testdata/j422.jpg"), include_bytes!("../testdata/j422.rgb"), (37, 23));
        same_as_magick(include_bytes!("../testdata/jgray.jpg"), include_bytes!("../testdata/jgray.rgb"), (37, 23));
        same_as_magick(include_bytes!("../testdata/jrst.jpg"), include_bytes!("../testdata/jrst.rgb"), (37, 23));
        same_as_magick(include_bytes!("../testdata/jphoto.jpg"), include_bytes!("../testdata/jphoto.rgb"), (160, 120));
        assert_eq!(dimensions(include_bytes!("../testdata/jphoto.jpg")), Some((160, 120)));
    }

    #[test]
    fn what_is_refused() {
        assert_eq!(decode(include_bytes!("../testdata/jprog.jpg")).err(), Some("a progressive JPEG (not supported yet)"));
        assert_eq!(dimensions(include_bytes!("../testdata/jprog.jpg")), Some((37, 23)));
        assert_eq!(decode(b"GIF89a").err(), Some("not a JPEG file"));
        let jpg: &[u8] = include_bytes!("../testdata/j420.jpg");
        for end in 0..jpg.len() {
            let _ = decode(&jpg[..end]); // never a panic
        }
        for i in 2..jpg.len() {
            for bit in [0x01, 0x40] {
                let mut d = jpg.to_vec();
                d[i] ^= bit;
                let _ = decode(&d);
            }
        }
    }
}
