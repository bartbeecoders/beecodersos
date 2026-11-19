//! USB cameras (the USB Video Class, "UVC"): what a camera says about
//! itself, and how the pictures it sends are put back together.
//!
//! A camera describes, in its configuration descriptor, one "video control"
//! interface and one "video streaming" interface. The streaming interface
//! lists formats (MJPEG: every picture a JPEG; or uncompressed YUY2), each
//! with frame sizes, and alternate settings: the same interface with an
//! isochronous endpoint of a different packet size, so a computer can pick
//! how much of the bus it gives the camera.
//!
//! Pictures arrive in packets, each with a small header: a frame id bit that
//! flips with every new picture, an "end of frame" bit and an error bit.
//! [`Frames`] puts them together again.
//!
//! Everything comes from a device: every length is checked.

use alloc::string::String;
use alloc::vec::Vec;

const INTERFACE: u8 = 4;
const ENDPOINT: u8 = 5;
const CS_INTERFACE: u8 = 0x24;
const CLASS_VIDEO: u8 = 0x0E;
const SUBCLASS_CONTROL: u8 = 1;
const SUBCLASS_STREAMING: u8 = 2;

const VC_HEADER: u8 = 1;
const VS_FORMAT_UNCOMPRESSED: u8 = 4;
const VS_FRAME_UNCOMPRESSED: u8 = 5;
const VS_FORMAT_MJPEG: u8 = 6;
const VS_FRAME_MJPEG: u8 = 7;

/// Bounds on what we keep from a description, however long it is.
const MAX_FORMATS: usize = 8;
const MAX_FRAMES: usize = 32;
const MAX_ALTS: usize = 32;
const MAX_INTERVALS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Every picture is a JPEG.
    Mjpeg,
    /// Two pixels in four bytes: Y0 U Y1 V.
    Yuy2,
    /// Uncompressed, but another pixel layout (NV12, ...): not used.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The number to ask for this size with.
    pub index: u8,
    pub width: u16,
    pub height: u16,
    /// The time between pictures it offers, in units of 100 ns (333333: 30 per second).
    pub intervals: Vec<u32>,
    pub default_interval: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    pub index: u8,
    pub kind: Kind,
    pub frames: Vec<Frame>,
}

/// One alternate setting of the streaming interface with an isochronous
/// (or bulk) endpoint towards the computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alt {
    pub setting: u8,
    pub endpoint: u8,
    /// Bytes per packet.
    pub packet: u16,
    /// Packets per (micro)frame: 1 to 3.
    pub per_frame: u8,
    pub interval: u8,
    pub bulk: bool,
}

impl Alt {
    /// Bytes per (micro)frame.
    pub fn bandwidth(&self) -> u32 {
        self.packet as u32 * self.per_frame as u32
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Camera {
    pub configuration: u8,
    pub control_interface: u8,
    pub streaming_interface: u8,
    /// UVC version as BCD: 0x0100, 0x0110, 0x0150.
    pub version: u16,
    pub formats: Vec<Format>,
    pub alts: Vec<Alt>,
}

fn u16_at(d: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([d[at], d[at + 1]])
}

fn u32_at(d: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

/// Read a configuration descriptor; `None` when it describes no camera.
/// Only the first camera function counts.
pub fn parse(config: &[u8]) -> Option<Camera> {
    if config.len() < 9 || config[1] != 2 {
        return None;
    }
    let mut camera = Camera { configuration: config[5], ..Camera::default() };
    let (mut found_control, mut found_streaming) = (false, false);
    // Which interface the descriptors that follow belong to: (subclass, number, alt).
    let mut current: Option<(u8, u8, u8)> = None;
    let mut at = config[0] as usize;
    while at + 2 <= config.len() {
        let (len, kind) = (config[at] as usize, config[at + 1]);
        if len < 2 || at + len > config.len() {
            break;
        }
        let d = &config[at..at + len];
        at += len;
        match kind {
            INTERFACE if len >= 9 => {
                current = None;
                if d[5] != CLASS_VIDEO {
                    continue;
                }
                let (number, alt, subclass) = (d[2], d[3], d[6]);
                match subclass {
                    SUBCLASS_CONTROL if !found_control => {
                        found_control = true;
                        camera.control_interface = number;
                        current = Some((subclass, number, alt));
                    }
                    SUBCLASS_STREAMING if !found_streaming || number == camera.streaming_interface => {
                        found_streaming = true;
                        camera.streaming_interface = number;
                        current = Some((subclass, number, alt));
                    }
                    _ => {}
                }
            }
            CS_INTERFACE if len >= 3 => match (current, d[2]) {
                (Some((SUBCLASS_CONTROL, _, 0)), VC_HEADER) if len >= 5 => camera.version = u16_at(d, 3),
                (Some((SUBCLASS_STREAMING, _, 0)), VS_FORMAT_MJPEG | VS_FORMAT_UNCOMPRESSED) if len >= 5 => {
                    if camera.formats.len() >= MAX_FORMATS {
                        continue;
                    }
                    let kind = if d[2] == VS_FORMAT_MJPEG {
                        Kind::Mjpeg
                    } else if len >= 9 && &d[5..9] == b"YUY2" {
                        Kind::Yuy2
                    } else {
                        Kind::Other
                    };
                    camera.formats.push(Format { index: d[3], kind, frames: Vec::new() });
                }
                (Some((SUBCLASS_STREAMING, _, 0)), VS_FRAME_MJPEG | VS_FRAME_UNCOMPRESSED) if len >= 26 => {
                    let Some(format) = camera.formats.last_mut() else { continue };
                    if format.frames.len() >= MAX_FRAMES {
                        continue;
                    }
                    let count = d[25] as usize;
                    let mut intervals = Vec::new();
                    if count == 0 {
                        // Continuous: minimum, maximum, step. The two ends will do.
                        if len >= 34 {
                            intervals.push(u32_at(d, 26));
                            intervals.push(u32_at(d, 30));
                        }
                    } else {
                        for i in 0..count.min(MAX_INTERVALS) {
                            if 26 + i * 4 + 4 <= len {
                                intervals.push(u32_at(d, 26 + i * 4));
                            }
                        }
                    }
                    let (width, height) = (u16_at(d, 5), u16_at(d, 7));
                    if width == 0 || height == 0 {
                        continue;
                    }
                    format.frames.push(Frame { index: d[3], width, height, intervals, default_interval: u32_at(d, 21) });
                }
                _ => {}
            },
            ENDPOINT if len >= 7 => {
                let Some((SUBCLASS_STREAMING, _, alt)) = current else { continue };
                let (address, attributes, raw) = (d[2], d[3], u16_at(d, 4));
                let transfer = attributes & 3;
                if address & 0x80 == 0 || !(transfer == 1 || transfer == 2) || camera.alts.len() >= MAX_ALTS {
                    continue;
                }
                let packet = raw & 0x7FF;
                if packet == 0 {
                    continue;
                }
                camera.alts.push(Alt {
                    setting: alt,
                    endpoint: address,
                    packet,
                    per_frame: ((raw >> 11) & 3) as u8 + 1,
                    interval: d[6],
                    bulk: transfer == 2,
                });
            }
            _ => {}
        }
    }
    (found_control && found_streaming).then_some(camera)
}

impl Camera {
    /// The setting with the least bandwidth that still carries `needed`
    /// bytes per (micro)frame; the largest one when none does.
    pub fn alt_for(&self, needed: u32) -> Option<Alt> {
        let mut alts: Vec<Alt> = self.alts.iter().copied().filter(|a| !a.bulk && a.setting != 0).collect();
        alts.sort_by_key(|a| a.bandwidth());
        alts.iter().copied().find(|a| a.bandwidth() >= needed).or(alts.last().copied())
    }

    /// The MJPEG size closest to `width` x `height` (at least as wide when
    /// one is), or `None` without MJPEG.
    pub fn pick(&self, kind: Kind, width: u16, height: u16) -> Option<(&Format, &Frame)> {
        let format = self.formats.iter().find(|f| f.kind == kind)?;
        let mut frames: Vec<&Frame> = format.frames.iter().collect();
        frames.sort_by_key(|f| (f.width as u32) * (f.height as u32));
        let frame = frames
            .iter()
            .find(|f| f.width >= width && f.height >= height)
            .or(frames.last())
            .copied()?;
        Some((format, frame))
    }

    /// Every format and size, one line each.
    pub fn describe(&self) -> String {
        let mut text = alloc::format!("UVC {}.{:02x}", self.version >> 8, self.version & 0xFF);
        for format in self.formats.iter() {
            let name = match format.kind {
                Kind::Mjpeg => "MJPEG",
                Kind::Yuy2 => "YUY2",
                Kind::Other => "other (not supported)",
            };
            text += &alloc::format!("\n{name}:");
            for (i, frame) in format.frames.iter().enumerate() {
                let fps = frame_rate(frame.intervals.iter().copied().min().unwrap_or(frame.default_interval));
                text += &alloc::format!("{} {}x{} ({fps}/s)", if i == 0 { "" } else { "," }, frame.width, frame.height);
            }
        }
        text
    }
}

/// Pictures per second for a frame interval in 100 ns units (rounded).
pub fn frame_rate(interval: u32) -> u32 {
    if interval == 0 {
        0
    } else {
        (10_000_000 + interval / 2) / interval
    }
}

// --- probe and commit -------------------------------------------------------------

/// Length of the "probe" and "commit" blocks for a UVC version.
pub fn probe_len(version: u16) -> usize {
    match version {
        0..=0x0100 => 26,
        0x0101..=0x0110 => 34,
        _ => 48,
    }
}

/// The block that asks for a format, size and picture rate.
pub fn probe(version: u16, format: u8, frame: u8, interval: u32) -> Vec<u8> {
    let mut block = alloc::vec![0u8; probe_len(version)];
    block[0] = 1; // bmHint: keep the picture rate
    block[2] = format;
    block[3] = frame;
    block[4..8].copy_from_slice(&interval.to_le_bytes());
    block
}

/// From the camera's answer: the largest picture and the largest payload
/// (bytes per (micro)frame) it will send. `None` when cut short.
pub fn negotiated(block: &[u8]) -> Option<(u32, u32)> {
    if block.len() < 26 {
        return None;
    }
    Some((u32_at(block, 18), u32_at(block, 22)))
}

// --- putting pictures together ----------------------------------------------------

const HEADER_FID: u8 = 1 << 0;
const HEADER_EOF: u8 = 1 << 1;
const HEADER_ERROR: u8 = 1 << 6;

/// Collects the payload of packets into whole pictures.
pub struct Frames {
    data: Vec<u8>,
    fid: Option<bool>,
    damaged: bool,
    limit: usize,
    done: Option<Vec<u8>>,
    /// Pictures finished, and dropped (damaged or too large).
    pub finished: u32,
    pub dropped: u32,
}

impl Frames {
    /// `limit`: the largest picture kept (the camera's own maximum, say).
    pub fn new(limit: usize) -> Frames {
        Frames { data: Vec::new(), fid: None, damaged: false, limit, done: None, finished: 0, dropped: 0 }
    }

    fn finish(&mut self) {
        let data = core::mem::take(&mut self.data);
        if self.damaged || data.is_empty() {
            if !data.is_empty() {
                self.dropped += 1;
            }
        } else {
            self.finished += 1;
            self.done = Some(data);
        }
        self.damaged = false;
    }

    /// One packet as it came from the camera (header and all). Empty
    /// packets (the camera had nothing to send) are fine.
    pub fn push(&mut self, packet: &[u8]) {
        if packet.len() < 2 {
            return;
        }
        let (header, info) = (packet[0] as usize, packet[1]);
        if header < 2 || header > packet.len() {
            return; // not a header: skip the packet
        }
        let fid = info & HEADER_FID != 0;
        if self.fid.is_some_and(|f| f != fid) && !self.data.is_empty() {
            // A new picture began without an end-of-frame mark.
            self.finish();
        }
        self.fid = Some(fid);
        if info & HEADER_ERROR != 0 {
            self.damaged = true;
        }
        let payload = &packet[header..];
        if self.data.len() + payload.len() > self.limit {
            self.damaged = true;
        } else {
            self.data.extend_from_slice(payload);
        }
        if info & HEADER_EOF != 0 {
            self.finish();
        }
    }

    /// The latest whole picture, once.
    pub fn take(&mut self) -> Option<Vec<u8>> {
        self.done.take()
    }
}

/// YUY2 pixels to 0x00RRGGBB, `width` x `height` (missing bytes give black).
pub fn yuy2_to_rgb(data: &[u8], width: usize, height: usize) -> Vec<u32> {
    let mut out = alloc::vec![0u32; width * height];
    for (pair, chunk) in data.chunks_exact(4).take(width * height / 2).enumerate() {
        let (y0, u, y1, v) = (chunk[0] as i32, chunk[1] as i32 - 128, chunk[2] as i32, chunk[3] as i32 - 128);
        for (k, y) in [y0, y1].into_iter().enumerate() {
            let c = (y - 16).max(0) * 298;
            let r = ((c + 409 * v + 128) >> 8).clamp(0, 255) as u32;
            let g = ((c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u32;
            let b = ((c + 516 * u + 128) >> 8).clamp(0, 255) as u32;
            out[pair * 2 + k] = r << 16 | g << 8 | b;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small camera: VC header (UVC 1.00), one MJPEG format with two
    /// sizes, YUY2 with one, and two alternate settings.
    fn sample() -> Vec<u8> {
        let mut d = Vec::new();
        let mut body: Vec<Vec<u8>> = Vec::new();
        body.push(vec![9, 4, 0, 0, 1, 0x0E, 1, 0, 0]); // VC interface
        body.push(vec![13, 0x24, 1, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 1, 1]); // VC header 1.00
        body.push(vec![7, 5, 0x83, 3, 16, 0, 6]); // interrupt endpoint of the VC interface
        body.push(vec![9, 4, 1, 0, 0, 0x0E, 2, 0, 0]); // VS interface, alt 0
        body.push(vec![14, 0x24, 1, 2, 0, 0, 0x81, 0, 0, 0, 0, 0, 0, 0]); // input header
        body.push(vec![11, 0x24, 6, 1, 2, 0, 1, 0, 0, 0, 0]); // MJPEG format 1
        let frame = |index: u8, w: u16, h: u16| {
            let mut f = vec![30, 0x24, 7, index, 0];
            f.extend_from_slice(&w.to_le_bytes());
            f.extend_from_slice(&h.to_le_bytes());
            f.extend_from_slice(&[0; 12]);
            f.extend_from_slice(&333_333u32.to_le_bytes()); // default
            f.push(1);
            f.extend_from_slice(&333_333u32.to_le_bytes());
            f
        };
        body.push(frame(1, 640, 480));
        body.push(frame(2, 320, 240));
        let mut yuy = vec![27, 0x24, 4, 2, 1];
        yuy.extend_from_slice(b"YUY2");
        yuy.extend_from_slice(&[0; 12]);
        yuy.extend_from_slice(&[16, 1, 0, 0, 0, 0]);
        body.push(yuy);
        let mut f = frame(1, 160, 120);
        f[2] = 5;
        body.push(f);
        body.push(vec![9, 4, 1, 1, 1, 0x0E, 2, 0, 0]); // alt 1
        body.push(vec![7, 5, 0x81, 5, 0x80, 0x00, 1]); // 128 bytes
        body.push(vec![9, 4, 1, 2, 1, 0x0E, 2, 0, 0]); // alt 2
        body.push(vec![7, 5, 0x81, 5, 0x00, 0x14, 1]); // 1024 bytes x 3
        body.push(vec![9, 4, 2, 0, 0, 1, 1, 0, 0]); // an audio interface after it
        let total: usize = 9 + body.iter().map(|b| b.len()).sum::<usize>();
        d.extend_from_slice(&[9, 2, total as u8, (total >> 8) as u8, 3, 1, 0, 0x80, 50]);
        for b in body {
            d.extend_from_slice(&b);
        }
        d
    }

    #[test]
    fn reads_formats_sizes_and_settings() {
        let camera = parse(&sample()).unwrap();
        assert_eq!(camera.configuration, 1);
        assert_eq!((camera.control_interface, camera.streaming_interface, camera.version), (0, 1, 0x0100));
        assert_eq!(camera.formats.len(), 2);
        assert_eq!(camera.formats[0].kind, Kind::Mjpeg);
        assert_eq!(camera.formats[0].frames.len(), 2);
        assert_eq!((camera.formats[0].frames[1].width, camera.formats[0].frames[1].height), (320, 240));
        assert_eq!(camera.formats[1].kind, Kind::Yuy2);
        assert_eq!(camera.alts.len(), 2);
        assert_eq!(camera.alts[1], Alt { setting: 2, endpoint: 0x81, packet: 1024, per_frame: 3, interval: 1, bulk: false });
        assert_eq!(camera.describe(), "UVC 1.00\nMJPEG: 640x480 (30/s), 320x240 (30/s)\nYUY2: 160x120 (30/s)");
    }

    #[test]
    fn picks_size_and_bandwidth() {
        let camera = parse(&sample()).unwrap();
        let (format, frame) = camera.pick(Kind::Mjpeg, 320, 240).unwrap();
        assert_eq!((format.index, frame.index), (1, 2));
        let (_, frame) = camera.pick(Kind::Mjpeg, 1920, 1080).unwrap();
        assert_eq!(frame.index, 1); // the largest there is
        assert_eq!(camera.alt_for(100).unwrap().setting, 1);
        assert_eq!(camera.alt_for(2000).unwrap().setting, 2);
        assert_eq!(camera.alt_for(100_000).unwrap().setting, 2);
    }

    #[test]
    fn no_camera_and_cut_short() {
        assert!(parse(&[]).is_none());
        let mut keyboard = vec![9, 2, 18, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 1, 3, 1, 1, 0];
        assert!(parse(&keyboard).is_none());
        keyboard[9] = 200; // a length past the end
        assert!(parse(&keyboard).is_none());
        let whole = sample();
        for cut in 0..whole.len() {
            let _ = parse(&whole[..cut]); // never panics
        }
    }

    #[test]
    fn probe_blocks() {
        let block = probe(0x0100, 1, 2, 333_333);
        assert_eq!(block.len(), 26);
        assert_eq!(&block[..8], &[1, 0, 1, 2, 0x15, 0x16, 0x05, 0x00]);
        assert_eq!(probe(0x0110, 1, 1, 0).len(), 34);
        assert_eq!(probe(0x0150, 1, 1, 0).len(), 48);
        let mut answer = block.clone();
        answer[18..22].copy_from_slice(&614_400u32.to_le_bytes());
        answer[22..26].copy_from_slice(&3072u32.to_le_bytes());
        assert_eq!(negotiated(&answer), Some((614_400, 3072)));
        assert_eq!(negotiated(&answer[..20]), None);
        assert_eq!(frame_rate(333_333), 30);
        assert_eq!(frame_rate(0), 0);
    }

    #[test]
    fn puts_pictures_together() {
        let mut frames = Frames::new(1000);
        frames.push(&[2, 0x00, 0xFF, 0xD8]);
        frames.push(&[2, 0x00]); // empty
        frames.push(&[2, 0x02, 0xFF, 0xD9]); // end of frame
        assert_eq!(frames.take(), Some(vec![0xFF, 0xD8, 0xFF, 0xD9]));
        assert_eq!(frames.take(), None);
        // No end mark: the flipped frame id ends the picture.
        frames.push(&[2, 0x01, 1, 2]);
        frames.push(&[2, 0x00, 3]);
        assert_eq!(frames.take(), Some(vec![1, 2]));
        // An error bit spoils the picture; a header longer than the packet is skipped.
        frames.push(&[2, 0x40, 4]);
        frames.push(&[9, 0x00, 5]);
        frames.push(&[2, 0x02, 6]);
        assert_eq!(frames.take(), None);
        assert_eq!(frames.dropped, 1);
        // Too large for the limit.
        let mut small = Frames::new(3);
        small.push(&[2, 0x00, 1, 2]);
        small.push(&[2, 0x02, 3, 4]);
        assert_eq!(small.take(), None);
        assert_eq!((small.finished, small.dropped), (0, 1));
        // Header longer than 2 bytes (with a time stamp).
        let mut f = Frames::new(100);
        f.push(&[12, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 8]);
        assert_eq!(f.take(), Some(vec![7, 8]));
    }

    #[test]
    fn yuy2_colours() {
        // Black-ish, white-ish, and a reddish pair.
        let px = yuy2_to_rgb(&[16, 128, 235, 128, 81, 90, 81, 240], 2, 2);
        assert_eq!(px[0], 0x000000);
        assert_eq!(px[1], 0xFFFFFF);
        let (r, g, b) = (px[2] >> 16, px[2] >> 8 & 0xFF, px[2] & 0xFF);
        assert!(r > 200 && g < 60 && b < 60, "{r} {g} {b}");
        assert_eq!(yuy2_to_rgb(&[1, 2], 2, 1), vec![0, 0]); // too short: black
    }
}
