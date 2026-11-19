//! JPEG pictures (ITU T.81), baseline and progressive: Huffman-coded 8x8
//! blocks, restart markers, grayscale or YCbCr with 4:4:4, 4:2:2 or 4:2:0
//! color subsampling (colors are taken from the nearest chroma sample).
//! Progressive files come in several scans that each add to the blocks'
//! coefficients (DC and AC bands, refined bit by bit). Lossless,
//! arithmetic-coded and CMYK JPEGs are refused with a clear message.
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
                return h.symbols.get(index as usize + (code - first) as usize).copied();
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
    id: u8,
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
    let mut scans = 0;
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
        if (0xD0..=0xD7).contains(&marker) {
            continue; // a stray restart marker
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
            0xC0..=0xC2 => frame = Some(read_frame(seg)?),
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return Err("a kind of JPEG this OS cannot show (lossless or arithmetic coding)"),
            0xDA => {
                // Motion JPEG (USB cameras, AVI) leaves the Huffman tables
                // out: then the standard ones of Annex K are meant.
                if dc.iter().chain(ac.iter()).all(|t| t.is_none()) {
                    for (i, t) in [STD_DC0, STD_AC0, STD_DC1, STD_AC1].iter().enumerate() {
                        let table = Huffman::new(t[1..17].try_into().map_err(|_| bad)?, &t[17..]).ok_or(bad)?;
                        if i % 2 == 0 {
                            dc[i / 2] = Some(table);
                        } else {
                            ac[i / 2] = Some(table);
                        }
                    }
                }
                let jpeg = frame.as_mut().ok_or(bad)?;
                let used = read_scan(jpeg, seg, &data[at + len..], &dc, &ac, restart_interval)?;
                scans += 1;
                if scans > 1000 {
                    return Err(bad);
                }
                // On to the next marker (not a stuffed byte, not a restart).
                let mut next = at + len + used;
                while next + 1 < data.len() && !(data[next] == 0xFF && data[next + 1] != 0 && !(0xD0..=0xD7).contains(&data[next + 1])) {
                    next += 1;
                }
                at = next;
                if at + 1 >= data.len() {
                    break; // no end marker: what came is shown
                }
                continue;
            }
            _ => {} // APPn, comments: skipped
        }
        at += len;
    }
    let mut jpeg = frame.ok_or(bad)?;
    if scans == 0 {
        return Err(bad);
    }
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
        comps.push(Component { id: c[0], h, v, quant: c[2] as usize, bw: 0, bh: 0, coef: Vec::new() });
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

/// What a scan adds to the blocks.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Band {
    /// The first and last coefficient (zigzag order) of the scan.
    ss: usize,
    se: usize,
    /// Successive approximation: a refining scan (ah > 0) adds bit `al`.
    ah: u8,
    al: u8,
}

/// Decode one block's part of a scan.
fn decode_block(bits: &mut Bits, block: &mut [i16], dct: Option<&Huffman>, act: Option<&Huffman>, pred: &mut i32, eobrun: &mut u32, b: Band) -> Result<(), &'static str> {
    let bad = "a damaged JPEG file";
    let put = |v: i32| v.clamp(-32768, 32767) as i16;
    if b.ss == 0 {
        if b.ah == 0 {
            let s = bits.decode(dct.ok_or(bad)?).ok_or(bad)? as u32;
            *pred += bits.signed(s);
            block[0] = put(*pred << b.al);
        } else if bits.bit() == 1 {
            block[0] |= 1 << b.al;
        }
        if b.se == 0 {
            return Ok(());
        }
    }
    let act = act.ok_or(bad)?;
    let first = b.ss.max(1);
    if b.ah == 0 {
        // The first scan of a band (baseline: all of it at once).
        if *eobrun > 0 {
            *eobrun -= 1;
            return Ok(());
        }
        let mut k = first;
        while k <= b.se {
            let rs = bits.decode(act).ok_or(bad)?;
            let (r, s) = ((rs >> 4) as usize, (rs & 15) as u32);
            if s == 0 {
                if r < 15 {
                    *eobrun = (1u32 << r) - 1; // this block and r more bits' worth
                    if r > 0 {
                        *eobrun += bits.bits(r as u32);
                    }
                    break;
                }
                k += 16;
                continue;
            }
            k += r;
            if k > 63 {
                return Err(bad);
            }
            block[ZIGZAG[k]] = put(bits.signed(s) << b.al);
            k += 1;
        }
        return Ok(());
    }
    // Refining a band: one more bit for the coefficients already there,
    // and new coefficients of +-1 in between (as libjpeg reads them).
    let (p1, m1) = (1i32 << b.al, -1i32 << b.al);
    let mut k = first;
    if *eobrun == 0 {
        while k <= b.se {
            let rs = bits.decode(act).ok_or(bad)?;
            let (mut r, s) = ((rs >> 4) as i32, rs & 15);
            let mut value = 0;
            if s != 0 {
                if s != 1 {
                    return Err(bad);
                }
                value = if bits.bit() == 1 { p1 } else { m1 };
            } else if r != 15 {
                *eobrun = 1 << r;
                if r > 0 {
                    *eobrun += bits.bits(r as u32);
                }
                break;
            }
            while k <= b.se {
                let z = ZIGZAG[k];
                let c = block[z] as i32;
                if c != 0 {
                    if bits.bit() == 1 && c & p1 == 0 {
                        block[z] = put(if c >= 0 { c + p1 } else { c + m1 });
                    }
                } else {
                    if r == 0 {
                        break;
                    }
                    r -= 1;
                }
                k += 1;
            }
            if value != 0 && k <= b.se {
                block[ZIGZAG[k]] = put(value);
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        while k <= b.se {
            let z = ZIGZAG[k];
            let c = block[z] as i32;
            if c != 0 && bits.bit() == 1 && c & p1 == 0 {
                block[z] = put(if c >= 0 { c + p1 } else { c + m1 });
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Ok(())
}

/// Read a scan from the entropy-coded `data` after its header into the
/// blocks' coefficients; returns how many bytes of `data` it used.
fn read_scan(jpeg: &mut Jpeg, seg: &[u8], data: &[u8], dc: &[Option<Huffman>; 4], ac: &[Option<Huffman>; 4], restart_interval: usize) -> Result<usize, &'static str> {
    let bad = "a damaged JPEG file";
    let n = *seg.first().ok_or(bad)? as usize;
    if n == 0 || n > jpeg.comps.len() {
        return Err(bad);
    }
    let mut scomps = Vec::new();
    for i in 0..n {
        let s = seg.get(1 + i * 2..3 + i * 2).ok_or(bad)?;
        let ci = jpeg.comps.iter().position(|c| c.id == s[0]).ok_or(bad)?;
        let dct = dc.get((s[1] >> 4) as usize).and_then(|t| t.as_ref());
        let act = ac.get((s[1] & 15) as usize).and_then(|t| t.as_ref());
        scomps.push((ci, dct, act));
    }
    let p = 1 + 2 * n;
    let tail = seg.get(p..p + 3).ok_or(bad)?;
    let band = Band { ss: tail[0] as usize, se: tail[1] as usize, ah: tail[2] >> 4, al: tail[2] & 15 };
    let fits = band.ss <= band.se && band.se <= 63 && band.al <= 13 && (band.ss > 0 || band.se == 0 || (band.se == 63 && band.ah == 0 && band.al == 0));
    if !fits || (band.ss > 0 && n != 1) {
        return Err(bad);
    }
    let mut bits = Bits { data, pos: 0, acc: 0, n: 0, marker: false };
    let mut pred = vec![0i32; n];
    let mut eobrun = 0u32;
    // One component: its blocks in plain order (only the ones in the
    // picture); several: whole MCUs.
    let single = n == 1;
    let (hmax, vmax, width, height) = (jpeg.hmax, jpeg.vmax, jpeg.width, jpeg.height);
    let blocks_across = |c: &Component| (width * c.h).div_ceil(hmax).div_ceil(8);
    let units = if single {
        let c = &jpeg.comps[scomps[0].0];
        blocks_across(c) * (height * c.v).div_ceil(vmax).div_ceil(8)
    } else {
        jpeg.mcus_x * jpeg.mcus_y
    };
    for unit in 0..units {
        if restart_interval > 0 && unit > 0 && unit % restart_interval == 0 {
            bits.restart();
            pred.iter_mut().for_each(|p| *p = 0);
            eobrun = 0;
        }
        if single {
            let (ci, dct, act) = scomps[0];
            let c = &mut jpeg.comps[ci];
            let across = blocks_across(c);
            let (x, y) = (unit % across, unit / across);
            let block = &mut c.coef[(y * c.bw + x) * 64..][..64];
            decode_block(&mut bits, block, dct, act, &mut pred[0], &mut eobrun, band)?;
            continue;
        }
        let (mx, my) = (unit % jpeg.mcus_x, unit / jpeg.mcus_x);
        for (si, &(ci, dct, act)) in scomps.iter().enumerate() {
            let c = &mut jpeg.comps[ci];
            for by in 0..c.v {
                for bx in 0..c.h {
                    let (x, y) = (mx * c.h + bx, my * c.v + by);
                    let block = &mut c.coef[(y * c.bw + x) * 64..][..64];
                    decode_block(&mut bits, block, dct, act, &mut pred[si], &mut eobrun, band)?;
                }
            }
        }
    }
    Ok(bits.pos)
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
/// A Motion JPEG frame (no Huffman tables of its own) as a JPEG file any
/// program can open: the standard tables written in before the scan.
/// Pictures that have tables, or that cannot be read, come back unchanged.
pub fn with_tables(data: &[u8]) -> Vec<u8> {
    if !is_jpeg(data) {
        return data.to_vec();
    }
    let mut at = 2;
    while at + 4 <= data.len() && data[at] == 0xFF {
        let marker = data[at + 1];
        if marker == 0xC4 {
            return data.to_vec();
        }
        if marker == 0xDA {
            let mut out = data[..at].to_vec();
            for t in [STD_DC0, STD_AC0, STD_DC1, STD_AC1] {
                out.extend_from_slice(&[0xFF, 0xC4]);
                out.extend_from_slice(&((t.len() + 2) as u16).to_be_bytes());
                out.extend_from_slice(t);
            }
            out.extend_from_slice(&data[at..]);
            return out;
        }
        at += 2 + u16::from_be_bytes([data[at + 2], data[at + 3]]) as usize;
    }
    data.to_vec()
}

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

// --- writing JPEG ------------------------------------------------------------------------

/// Annex K's quantization tables (quality 50), luminance and chrominance, in zigzag order.
const STD_QUANT: [[u8; 64]; 2] = [[16, 11, 12, 14, 12, 10, 16, 14, 13, 14, 18, 17, 16, 19, 24, 40, 26, 24, 22, 22, 24, 49, 35, 37, 29, 40, 58, 51, 61, 60, 57, 51, 56, 55, 64, 72, 92, 78, 64, 68, 87, 69, 55, 56, 80, 109, 81, 87, 95, 98, 103, 104, 103, 62, 77, 113, 121, 112, 100, 120, 92, 101, 103, 99], [17, 18, 18, 24, 21, 24, 47, 26, 26, 47, 99, 66, 56, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99]];
/// Annex K's Huffman tables as DHT segment bodies: DC 0, AC 0, DC 1, AC 1.
const STD_DC0: &[u8] = &[0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const STD_AC0: &[u8] = &[16, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125, 1, 2, 3, 0, 4, 17, 5, 18, 33, 49, 65, 6, 19, 81, 97, 7, 34, 113, 20, 50, 129, 145, 161, 8, 35, 66, 177, 193, 21, 82, 209, 240, 36, 51, 98, 114, 130, 9, 10, 22, 23, 24, 25, 26, 37, 38, 39, 40, 41, 42, 52, 53, 54, 55, 56, 57, 58, 67, 68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103, 104, 105, 106, 115, 116, 117, 118, 119, 120, 121, 122, 131, 132, 133, 134, 135, 136, 137, 138, 146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166, 167, 168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196, 197, 198, 199, 200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250];
const STD_DC1: &[u8] = &[1, 0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const STD_AC1: &[u8] = &[17, 0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119, 0, 1, 2, 3, 17, 4, 5, 33, 49, 6, 18, 65, 81, 7, 97, 113, 19, 34, 50, 129, 8, 20, 66, 145, 161, 177, 193, 9, 35, 51, 82, 240, 21, 98, 114, 209, 10, 22, 36, 52, 225, 37, 241, 23, 24, 25, 26, 38, 39, 40, 41, 42, 53, 54, 55, 56, 57, 58, 67, 68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103, 104, 105, 106, 115, 116, 117, 118, 119, 120, 121, 122, 130, 131, 132, 133, 134, 135, 136, 137, 138, 146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166, 167, 168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196, 197, 198, 199, 200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218, 226, 227, 228, 229, 230, 231, 232, 233, 234, 242, 243, 244, 245, 246, 247, 248, 249, 250];

/// A Huffman code for writing: (code, length) per symbol.
struct Codes([(u16, u8); 256]);

impl Codes {
    fn from_dht(body: &[u8]) -> Codes {
        let mut codes = [(0u16, 0u8); 256];
        let counts = &body[1..17];
        let mut symbols = body[17..].iter();
        let mut code = 0u16;
        for (len, &n) in counts.iter().enumerate() {
            for _ in 0..n {
                if let Some(&s) = symbols.next() {
                    codes[s as usize] = (code, len as u8 + 1);
                }
                code += 1;
            }
            code <<= 1;
        }
        Codes(codes)
    }
}

/// Writes bits, most significant first, with a 0 after every 0xFF byte.
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, len: u32) {
        if len == 0 {
            return;
        }
        self.acc = self.acc << len | (value & ((1 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            let byte = (self.acc >> (self.n - 8)) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0);
            }
            self.n -= 8;
        }
        self.acc &= (1 << self.n) - 1;
    }

    fn flush(&mut self) {
        let pad = (8 - self.n % 8) % 8;
        self.put((1 << pad) - 1, pad); // fill with ones
    }
}

/// How many bits a value needs (JPEG's "category"), and the bits to write.
fn category(v: i32) -> (u32, u32) {
    let size = 32 - v.unsigned_abs().leading_zeros();
    let bits = if v < 0 { (v - 1) as u32 } else { v as u32 };
    (size, bits & ((1u32 << size) - 1))
}

/// A JPEG writer: 4:2:0 YCbCr, baseline, the standard Huffman tables and
/// quantization tables scaled for `quality` as libjpeg does. Each row of
/// 16-line MCUs is made into coefficients on its own (`row`), so rows can
/// be done on different processors; `finish` codes them in order.
pub struct Writer {
    pub width: usize,
    pub height: usize,
    quant: [[u16; 64]; 2],
}

impl Writer {
    pub fn new(width: usize, height: usize, quality: u32) -> Writer {
        let q = quality.clamp(1, 100);
        let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
        let mut quant = [[1u16; 64]; 2];
        for t in 0..2 {
            for k in 0..64 {
                quant[t][k] = ((STD_QUANT[t][k] as u32 * scale + 50) / 100).clamp(1, 255) as u16;
            }
        }
        Writer { width, height, quant }
    }

    pub fn mcu_rows(&self) -> usize {
        self.height.div_ceil(16)
    }

    /// The quantized coefficients (zigzag order) of MCU row `my`: for each
    /// MCU, four Y blocks, then Cb and Cr.
    pub fn row(&self, my: usize, pixel: impl Fn(usize, usize) -> u32) -> Vec<i16> {
        let mcus = self.width.div_ceil(16);
        let mut out = Vec::with_capacity(mcus * 6 * 64);
        let (wmax, hmax) = (self.width - 1, self.height - 1);
        let ycc = |x: usize, y: usize| -> (i32, i32, i32) {
            let p = pixel(x.min(wmax), y.min(hmax));
            let (r, g, b) = ((p >> 16 & 255) as i32, (p >> 8 & 255) as i32, (p & 255) as i32);
            let yy = (19_595 * r + 38_470 * g + 7_471 * b + 32_768) >> 16;
            let cb = ((-11_056 * r - 21_712 * g + 32_768 * b + 32_768) >> 16) + 128;
            let cr = ((32_768 * r - 27_440 * g - 5_328 * b + 32_768) >> 16) + 128;
            (yy, cb, cr)
        };
        for mx in 0..mcus {
            let (x0, y0) = (mx * 16, my * 16);
            let mut ys = [[0i32; 256]; 1];
            let mut cbs = [0i32; 64];
            let mut crs = [0i32; 64];
            for y in 0..16 {
                for x in 0..16 {
                    let (yy, cb, cr) = ycc(x0 + x, y0 + y);
                    ys[0][y * 16 + x] = yy;
                    cbs[(y / 2) * 8 + x / 2] += cb;
                    crs[(y / 2) * 8 + x / 2] += cr;
                }
            }
            for by in 0..2 {
                for bx in 0..2 {
                    let mut block = [0i32; 64];
                    for y in 0..8 {
                        for x in 0..8 {
                            block[y * 8 + x] = ys[0][(by * 8 + y) * 16 + bx * 8 + x];
                        }
                    }
                    out.extend_from_slice(&fdct_quant(&block, &self.quant[0]));
                }
            }
            for plane in [&mut cbs, &mut crs] {
                for v in plane.iter_mut() {
                    *v = (*v + 2) / 4;
                }
                out.extend_from_slice(&fdct_quant(plane, &self.quant[1]));
            }
        }
        out
    }

    /// The whole file from all rows' coefficients, in order.
    pub fn finish(&self, rows: &[Vec<i16>]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        let segment = |out: &mut Vec<u8>, marker: u8, body: &[u8]| {
            out.extend_from_slice(&[0xFF, marker]);
            out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(body);
        };
        segment(&mut out, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        for t in 0..2 {
            let mut body = vec![t as u8];
            body.extend(self.quant[t].iter().map(|q| *q as u8));
            segment(&mut out, 0xDB, &body);
        }
        let mut sof = vec![8];
        sof.extend_from_slice(&(self.height as u16).to_be_bytes());
        sof.extend_from_slice(&(self.width as u16).to_be_bytes());
        sof.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        segment(&mut out, 0xC0, &sof);
        for t in [STD_DC0, STD_AC0, STD_DC1, STD_AC1] {
            segment(&mut out, 0xC4, t);
        }
        segment(&mut out, 0xDA, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
        let (dc0, ac0, dc1, ac1) = (Codes::from_dht(STD_DC0), Codes::from_dht(STD_AC0), Codes::from_dht(STD_DC1), Codes::from_dht(STD_AC1));
        let mut bits = BitWriter { out, acc: 0, n: 0 };
        let mut pred = [0i32; 3];
        for row in rows {
            for (i, block) in row.chunks(64).enumerate() {
                let comp = match i % 6 {
                    0..=3 => 0,
                    4 => 1,
                    _ => 2,
                };
                let (dc, ac) = if comp == 0 { (&dc0, &ac0) } else { (&dc1, &ac1) };
                let diff = block[0] as i32 - pred[comp];
                pred[comp] = block[0] as i32;
                let (size, value) = category(diff);
                let (code, len) = dc.0[size as usize];
                bits.put(code as u32, len as u32);
                bits.put(value, size);
                let mut run = 0;
                for k in 1..64 {
                    let v = block[k] as i32;
                    if v == 0 {
                        run += 1;
                        continue;
                    }
                    while run > 15 {
                        let (code, len) = ac.0[0xF0];
                        bits.put(code as u32, len as u32);
                        run -= 16;
                    }
                    let (size, value) = category(v);
                    let (code, len) = ac.0[(run << 4 | size as usize) & 0xFF];
                    bits.put(code as u32, len as u32);
                    bits.put(value, size);
                    run = 0;
                }
                if run > 0 {
                    let (code, len) = ac.0[0x00];
                    bits.put(code as u32, len as u32);
                }
            }
        }
        bits.flush();
        let mut out = bits.out;
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }
}

/// Forward DCT of 8x8 samples (0-255), quantized, in zigzag order.
fn fdct_quant(block: &[i32; 64], quant: &[u16; 64]) -> [i16; 64] {
    let mut tmp = [0i64; 64];
    for y in 0..8 {
        for u in 0..8 {
            let mut s = 0i64;
            for x in 0..8 {
                s += (block[y * 8 + x] - 128) as i64 * COS[u][x] as i64;
            }
            tmp[y * 8 + u] = s;
        }
    }
    let mut out = [0i16; 64];
    for v in 0..8 {
        for u in 0..8 {
            let mut s = 0i64;
            for y in 0..8 {
                s += tmp[y * 8 + u] * COS[v][y] as i64;
            }
            // s = 4096^2 * 4 * F(u, v); round F / q.
            let q = quant[ZIGZAG.iter().position(|&z| z == v * 8 + u).unwrap_or(0)] as i64 * (4 << 24);
            let f = if s >= 0 { (s + q / 2) / q } else { -((-s + q / 2) / q) };
            let k = ZIGZAG.iter().position(|&z| z == v * 8 + u).unwrap_or(0);
            out[k] = f.clamp(-2047, 2047) as i16;
        }
    }
    out
}

/// A JPEG of the picture, in one go (the kernel spreads `Writer::row` over processors).
pub fn encode(width: usize, height: usize, quality: u32, pixel: impl Fn(usize, usize) -> u32) -> Vec<u8> {
    let w = Writer::new(width, height, quality);
    let rows: Vec<Vec<i16>> = (0..w.mcu_rows()).map(|my| w.row(my, &pixel)).collect();
    w.finish(&rows)
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
    fn motion_jpeg_without_tables() {
        // A camera frame: the encoder's output with its DHT segments taken out.
        let whole = encode(40, 24, 80, |x, y| (x * 6) as u32 * 0x10000 | (y * 10) as u32 * 0x100 | 0x40);
        let mut bare = whole[..2].to_vec();
        let mut at = 2;
        while whole[at + 1] != 0xDA {
            let len = u16::from_be_bytes([whole[at + 2], whole[at + 3]]) as usize;
            if whole[at + 1] != 0xC4 {
                bare.extend_from_slice(&whole[at..at + 2 + len]);
            }
            at += 2 + len;
        }
        bare.extend_from_slice(&whole[at..]);
        assert!(bare.len() < whole.len() - 400);
        assert_eq!(decode(&bare).unwrap(), decode(&whole).unwrap());
        // Saved as a file: the tables are written back in.
        let fixed = with_tables(&bare);
        assert_eq!(fixed.len(), whole.len());
        assert_eq!(decode(&fixed).unwrap(), decode(&whole).unwrap());
        assert_eq!(with_tables(&whole), whole);
        assert_eq!(with_tables(&bare[..10]), bare[..10].to_vec());
        assert_eq!(with_tables(b"nope"), b"nope".to_vec());
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
    fn progressive() {
        // Several scans each: DC first, then AC bands, refined bit by bit.
        same_as_magick(include_bytes!("../testdata/jprog.jpg"), include_bytes!("../testdata/jprog.rgb"), (37, 23));
        same_as_magick(include_bytes!("../testdata/jprog420.jpg"), include_bytes!("../testdata/jprog420.rgb"), (37, 23));
        same_as_magick(include_bytes!("../testdata/jproggray.jpg"), include_bytes!("../testdata/jproggray.rgb"), (37, 23));
        same_as_magick(include_bytes!("../testdata/jprogphoto.jpg"), include_bytes!("../testdata/jprogphoto.rgb"), (160, 120));
        let jpg: &[u8] = include_bytes!("../testdata/jprogphoto.jpg");
        for end in (0..jpg.len()).step_by(13) {
            let _ = decode(&jpg[..end]); // never a panic
        }
        for i in (2..jpg.len()).step_by(5) {
            let mut d = jpg.to_vec();
            d[i] ^= 0x21;
            let _ = decode(&d);
        }
    }

    #[test]
    fn what_is_refused() {
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

    #[test]
    fn written_and_read_back() {
        // A picture with smooth parts and edges, odd sizes: read back close to it.
        let colors = |x: usize, y: usize| -> u32 {
            if (x / 20 + y / 15) % 3 == 0 { 0x3060C0 } else { ((x * 3) as u32 & 255) << 16 | ((y * 5) as u32 & 255) << 8 | 0x40 }
        };
        let (w, h) = (101, 67);
        let jpg = encode(w, h, 90, colors);
        let (dw, dh, pixels) = decode(&jpg).unwrap();
        assert_eq!((dw, dh), (w, h));
        let mut total = 0u64;
        for (i, p) in pixels.iter().enumerate() {
            let want = colors(i % w, i / w);
            for shift in [16, 8, 0] {
                total += ((p >> shift & 255) as i64 - (want >> shift & 255) as i64).unsigned_abs();
            }
        }
        let mean = total as f64 / (w * h * 3) as f64;
        assert!(mean < 6.0, "mean difference {mean}");
        // Quality changes the size; tiny pictures work.
        assert!(encode(w, h, 30, colors).len() < jpg.len());
        assert_eq!(decode(&encode(1, 1, 75, |_, _| 0xFF8000)).unwrap().0, 1);
        assert_eq!(category(0), (0, 0));
        assert_eq!(category(-3), (2, 0));
        assert_eq!(category(5), (3, 5));
    }
}
