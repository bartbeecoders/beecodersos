//! Display drivers: who sets the screen resolution, and where the pixels go.
//!
//! A driver finds its hardware, sets a mode and hands out a linear
//! framebuffer (a block of memory, a few bytes per pixel). The framebuffer
//! console (fbcon.rs) draws text into it. Drivers are tried in the order of
//! `DRIVERS`; when none finds its hardware the VGA text screen stays.
//! Plan: Docs/display-driver-plan.html.
//!
//! Boot flags: `display=bochs|text` picks a driver (`text`: none), and
//! `mode=1920x1080` asks for a size.
//!
//! Add a driver: a file with a `probe` function, and one line in `DRIVERS`.

use crate::ring::Global;
use crate::{bochs, bootargs, pci};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// How one pixel is stored (bytes in memory order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4 bytes: blue, green, red, unused.
    Xrgb8888,
    /// 4 bytes: red, green, blue, unused.
    Xbgr8888,
    /// 3 bytes: blue, green, red.
    Rgb888,
    /// 2 bytes: 5 bits red, 6 green, 5 blue (little-endian).
    Rgb565,
}

impl PixelFormat {
    pub const fn bytes(self) -> usize {
        match self {
            PixelFormat::Xrgb8888 | PixelFormat::Xbgr8888 => 4,
            PixelFormat::Rgb888 => 3,
            PixelFormat::Rgb565 => 2,
        }
    }

    /// The pixel value for a 0xRRGGBB color; its low `bytes()` bytes are
    /// stored little-endian.
    pub const fn encode(self, rgb: u32) -> u32 {
        let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
        match self {
            PixelFormat::Xrgb8888 | PixelFormat::Rgb888 => rgb & 0xFF_FFFF,
            PixelFormat::Xbgr8888 => b << 16 | g << 8 | r,
            PixelFormat::Rgb565 => (r >> 3) << 11 | (g >> 2) << 5 | b >> 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Bytes from the start of one line to the start of the next.
    pub pitch: u32,
    pub format: PixelFormat,
}

impl Mode {
    /// Bytes of framebuffer this mode uses.
    pub fn bytes(&self) -> u64 {
        self.pitch as u64 * self.height as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayError {
    /// No driver: this is the text screen.
    NoDriver,
    /// The driver cannot change the mode it was given.
    Fixed,
    /// Not a size we accept (see `parse_mode`).
    BadSize,
    /// The card has too little video memory for this size.
    TooLarge,
    /// The framebuffer lies outside the memory the kernel can reach.
    Unreachable,
    /// The card did not take the mode.
    Refused,
}

impl DisplayError {
    pub fn message(self) -> &'static str {
        match self {
            DisplayError::NoDriver => "no graphics driver is in use (this is the text screen)",
            DisplayError::Fixed => "this driver cannot change the mode",
            DisplayError::BadSize => "not a usable size (320x200 to 4096x2160, e.g. 1920x1080)",
            DisplayError::TooLarge => "too large for the card's video memory",
            DisplayError::Unreachable => "the framebuffer is outside the memory the kernel can reach",
            DisplayError::Refused => "the card refused this mode",
        }
    }
}

pub trait DisplayDriver {
    fn name(&self) -> &'static str;
    /// The current mode; `None` until one was set.
    fn mode(&self) -> Option<Mode>;
    fn framebuffer(&self) -> *mut u8;
    /// Change the resolution. A driver must check the mode with `usable`
    /// before it touches the card, and leave the old mode on when it fails.
    /// Drivers that only take over a mode somebody else set keep this default.
    fn set_mode(&mut self, _width: u32, _height: u32) -> Result<Mode, DisplayError> {
        Err(DisplayError::Fixed)
    }
}

type Probe = fn(&[pci::Device]) -> Option<Box<dyn DisplayDriver>>;

/// Tried from top to bottom; the first that finds its hardware wins.
const DRIVERS: &[(&str, Probe)] = &[("bochs", bochs::probe)];

/// Asked for when no `mode=` flag says otherwise, then these in turn.
pub const DEFAULT_MODE: (u32, u32) = (1920, 1080);
const SMALLER_MODES: &[(u32, u32)] = &[(1280, 720), (1024, 768), (800, 600), (640, 480)];

static DRIVER: Global<Option<Box<dyn DisplayDriver>>> = Global::new(None);

/// Where to draw: a framebuffer and its mode.
#[derive(Clone, Copy)]
pub struct Target {
    pub framebuffer: *mut u8,
    pub mode: Mode,
}

/// "1920x1080" -> (1920, 1080). Sizes outside 320x200 .. 4096x2160 are refused.
pub fn parse_mode(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.split_once('x')?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    valid_size(w, h).then_some((w, h))
}

pub fn valid_size(width: u32, height: u32) -> bool {
    (320..=4096).contains(&width) && (200..=2160).contains(&height)
}

/// May the console draw into a framebuffer at `addr` with this mode? The
/// values can come from a card or from firmware, so nothing is assumed: the
/// size must be sane, a line must hold all its pixels, and the whole area
/// must lie in the first 4 GiB (all that boot.s maps) and not at address 0.
pub fn usable(addr: u64, mode: &Mode) -> bool {
    const MAPPED: u64 = 4 << 30;
    valid_size(mode.width, mode.height)
        && mode.pitch as u64 >= mode.width as u64 * mode.format.bytes() as u64
        && addr >= 0x10_0000
        && addr.checked_add(mode.bytes()).is_some_and(|end| end <= MAPPED)
}

/// Find a driver and set a mode. Returns where to draw (`None`: stay on the
/// text screen) and notes for the user. It prints nothing itself: while it
/// runs, the screen may be between two modes.
pub fn init() -> (Option<Target>, Vec<String>) {
    let mut notes = Vec::new();
    let mut choice = bootargs::value("display");
    if choice == Some("text") {
        return (None, notes);
    }
    if let Some(name) = choice {
        if !DRIVERS.iter().any(|(n, _)| *n == name) {
            notes.push(format!("display={name}: no such driver (bochs, text); choosing one myself"));
            choice = None;
        }
    }
    let wanted = bootargs::value("mode").and_then(|text| {
        let size = parse_mode(text);
        if size.is_none() {
            notes.push(format!("mode={text}: {}", DisplayError::BadSize.message()));
        }
        size
    });
    let devices = pci::scan();
    for (name, probe) in DRIVERS {
        if choice.is_some_and(|c| c != *name) {
            continue;
        }
        let Some(mut driver) = probe(&devices) else { continue };
        let sizes = wanted.into_iter().chain([DEFAULT_MODE]).chain(SMALLER_MODES.iter().copied());
        for (width, height) in sizes {
            match driver.set_mode(width, height) {
                Ok(mode) => {
                    let target = Target { framebuffer: driver.framebuffer(), mode };
                    *DRIVER.get() = Some(driver);
                    return (Some(target), notes);
                }
                Err(e) => notes.push(format!("{name}: {width}x{height}: {}", e.message())),
            }
        }
    }
    (None, notes)
}

/// Name and mode of the driver in use.
pub fn current() -> Option<(&'static str, Mode)> {
    let driver = DRIVER.get().as_ref()?;
    Some((driver.name(), driver.mode()?))
}

/// Where the driver in use draws.
pub fn target() -> Option<Target> {
    let driver = DRIVER.get().as_ref()?;
    Some(Target { framebuffer: driver.framebuffer(), mode: driver.mode()? })
}

/// Change the resolution. The caller (console.rs) must lay the screen out
/// again for the new target.
pub fn set_mode(width: u32, height: u32) -> Result<Target, DisplayError> {
    if !valid_size(width, height) {
        return Err(DisplayError::BadSize);
    }
    let driver = DRIVER.get().as_mut().ok_or(DisplayError::NoDriver)?;
    let mode = driver.set_mode(width, height)?;
    Ok(Target { framebuffer: driver.framebuffer(), mode })
}
