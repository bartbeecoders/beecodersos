//! Display driver for the framebuffer the bootloader hands over: the
//! bootloader (Limine) asks the firmware for a graphics mode (VBE in legacy
//! BIOS boot, GOP in UEFI boot) because the Multiboot header in boot.s
//! requests one, and tells the kernel where the pixels are. It works with
//! any graphics card, but the mode cannot be changed afterwards.
//!
//! What the bootloader says is checked before use (`parse`): a wrong
//! address or size would make the console write over other memory.

use crate::display::{self, DisplayDriver, Mode, PixelFormat};
use crate::pci;
use crate::ring::Global;
use alloc::boxed::Box;

/// The Multiboot info up to the end of its framebuffer fields.
pub const INFO_LEN: usize = 118;
/// Multiboot info flag: the framebuffer fields are valid.
const FLAG_FRAMEBUFFER: u32 = 1 << 12;

/// What the bootloader left on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boot {
    /// The VGA text screen (or the bootloader said nothing).
    Text,
    /// A graphics mode we can draw in: framebuffer address and mode.
    Graphics(u64, Mode),
    /// A graphics mode we cannot use, and why. There is no text screen either.
    Unusable(&'static str),
}

static BOOT: Global<Boot> = Global::new(Boot::Text);

/// The framebuffer fields as the bootloader gave them (offset 88 onwards),
/// to show when they cannot be used.
static RAW: Global<[u8; INFO_LEN - 88]> = Global::new([0; INFO_LEN - 88]);

/// Read the framebuffer fields of the Multiboot info. Must run before the
/// heap is set up: the heap may reuse the memory the bootloader put it in.
pub fn remember(multiboot_info: u32) {
    let mbi = multiboot_info as u64 as *const u8;
    let flags = unsafe { core::ptr::read_unaligned(mbi as *const u32) };
    if flags & FLAG_FRAMEBUFFER == 0 {
        return; // the fields are not there: do not read past the structure
    }
    let mut info = [0u8; INFO_LEN];
    unsafe { core::ptr::copy_nonoverlapping(mbi, info.as_mut_ptr(), INFO_LEN) };
    RAW.get().copy_from_slice(&info[88..]);
    *BOOT.get() = parse(&info);
}

/// What the bootloader said about its framebuffer, in words: for a report
/// from a machine where it could not be used. No addresses.
pub fn describe_raw() -> alloc::string::String {
    let raw = RAW.get();
    let u32_at = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
    alloc::format!(
        "{}x{}, {} bytes per line, {} bits per pixel, type {}, red {}+{} green {}+{} blue {}+{}, {} 4 GiB",
        u32_at(12), u32_at(16), u32_at(8), raw[20], raw[21],
        raw[24], raw[25], raw[26], raw[27], raw[28], raw[29],
        if u32_at(4) == 0 { "below" } else { "above" }
    )
}

pub fn boot() -> Boot {
    *BOOT.get()
}

/// Decode the framebuffer fields of a Multiboot info structure. Nothing in
/// it is trusted: only a framebuffer that passes `display::usable`, in a
/// pixel format we can draw, comes back as `Graphics`.
pub fn parse(info: &[u8]) -> Boot {
    let u32_at = |at: usize| info.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    match u32_at(0) {
        Some(flags) if flags & FLAG_FRAMEBUFFER != 0 => {}
        _ => return Boot::Text,
    }
    if info.len() < INFO_LEN {
        return Boot::Unusable("its description is cut short");
    }
    let field = |at: usize| u32_at(at).unwrap_or(0);
    let addr = field(88) as u64 | (field(92) as u64) << 32;
    let (pitch, width, height) = (field(96), field(100), field(104));
    let (bits, kind) = (info[108], info[109]);
    // Type 1: every pixel holds its own red, green and blue. For each color:
    // the bit where it starts, and how many bits it has. These six bytes
    // start at offset 112, not 110 as the table in the Multiboot text
    // suggests: bootloaders follow the C header, where two unused bytes come
    // first (seen with Limine; GRUB uses the same header).
    let masks = [(info[112], info[113]), (info[114], info[115]), (info[116], info[117])];
    let format = match (kind, bits, masks) {
        (2, _, _) => return Boot::Text, // EGA text mode
        (1, 32, [(16, 8), (8, 8), (0, 8)]) => PixelFormat::Xrgb8888,
        (1, 32, [(0, 8), (8, 8), (16, 8)]) => PixelFormat::Xbgr8888,
        (1, 24, [(16, 8), (8, 8), (0, 8)]) => PixelFormat::Rgb888,
        (1, 16, [(11, 5), (5, 6), (0, 5)]) => PixelFormat::Rgb565,
        (1, _, _) => return Boot::Unusable("a pixel format this kernel cannot draw"),
        _ => return Boot::Unusable("not a mode with red, green and blue in every pixel"),
    };
    let mode = Mode { width, height, pitch, format };
    if !display::usable(addr, &mode) {
        return Boot::Unusable("its address or size is outside what the kernel can use");
    }
    Boot::Graphics(addr, mode)
}

struct BootFb {
    framebuffer: u64,
    mode: Mode,
}

pub fn probe(_devices: &[pci::Device]) -> Option<Box<dyn DisplayDriver>> {
    match boot() {
        Boot::Graphics(framebuffer, mode) => Some(Box::new(BootFb { framebuffer, mode })),
        _ => None,
    }
}

impl DisplayDriver for BootFb {
    fn name(&self) -> &'static str {
        "bootfb"
    }

    fn mode(&self) -> Option<Mode> {
        Some(self.mode)
    }

    fn framebuffer(&self) -> *mut u8 {
        self.framebuffer as *mut u8
    }
}
