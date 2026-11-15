//! Display driver for the Bochs / QEMU graphics card ("Bochs VBE", QEMU's
//! `-vga std`): the OS sets any resolution itself by writing a few registers
//! through I/O ports 0x1CE (register number) and 0x1CF (value). QEMU's QXL
//! and virtio-vga cards and VirtualBox's card answer on the same ports.
//! The framebuffer is the card's first memory area (BAR 0).

use crate::display::{self, DisplayDriver, DisplayError, Mode, PixelFormat};
use crate::pci::{self, Bar};
use crate::port::{inw, outw};
use alloc::boxed::Box;

const INDEX_PORT: u16 = 0x1CE;
const DATA_PORT: u16 = 0x1CF;

const REG_ID: u16 = 0;
const REG_XRES: u16 = 1;
const REG_YRES: u16 = 2;
const REG_BPP: u16 = 3;
const REG_ENABLE: u16 = 4;
const REG_X_OFFSET: u16 = 8;
const REG_Y_OFFSET: u16 = 9;
const REG_VIDEO_MEMORY_64K: u16 = 0x0A;

const ENABLED: u16 = 0x01;
const LINEAR_FRAMEBUFFER: u16 = 0x40;

/// (vendor, device): QEMU std VGA, QEMU QXL, virtio-vga, VirtualBox.
const CARDS: &[(u16, u16)] = &[(0x1234, 0x1111), (0x1b36, 0x0100), (0x1af4, 0x1050), (0x80ee, 0xbeef)];

fn read(reg: u16) -> u16 {
    unsafe {
        outw(INDEX_PORT, reg);
        inw(DATA_PORT)
    }
}

fn write(reg: u16, value: u16) {
    unsafe {
        outw(INDEX_PORT, reg);
        outw(DATA_PORT, value);
    }
}

/// Is this the value of the ID register of an interface new enough for us
/// (0xB0C4: it can tell how much video memory it has)? A machine without
/// the card gives 0xFFFF here.
pub fn id_ok(id: u16) -> bool {
    (0xB0C4..=0xB0CF).contains(&id)
}

pub struct Bochs {
    framebuffer: u64,
    /// Bytes of video memory.
    memory: u64,
    mode: Option<Mode>,
}

/// The ports are only touched when PCI shows one of the cards, so real
/// hardware never sees a stray write.
pub fn probe(devices: &[pci::Device]) -> Option<Box<dyn DisplayDriver>> {
    let card = devices.iter().find(|d| CARDS.contains(&(d.vendor, d.device)))?;
    if !id_ok(read(REG_ID)) {
        return None;
    }
    let Bar::Memory { addr, .. } = card.bar(0) else { return None };
    card.enable(pci::CMD_MEMORY);
    let memory = read(REG_VIDEO_MEMORY_64K) as u64 * 65536;
    Some(Box::new(Bochs { framebuffer: addr, memory, mode: None }))
}

impl Bochs {
    /// Write a mode to the card; true when the card took it.
    fn program(mode: &Mode) -> bool {
        write(REG_ENABLE, 0);
        write(REG_XRES, mode.width as u16);
        write(REG_YRES, mode.height as u16);
        write(REG_BPP, 32);
        write(REG_X_OFFSET, 0);
        write(REG_Y_OFFSET, 0);
        write(REG_ENABLE, ENABLED | LINEAR_FRAMEBUFFER);
        read(REG_XRES) as u32 == mode.width
            && read(REG_YRES) as u32 == mode.height
            && read(REG_BPP) == 32
            && read(REG_ENABLE) & ENABLED != 0
    }
}

impl DisplayDriver for Bochs {
    fn name(&self) -> &'static str {
        "bochs"
    }

    fn mode(&self) -> Option<Mode> {
        self.mode
    }

    fn framebuffer(&self) -> *mut u8 {
        self.framebuffer as *mut u8
    }

    fn set_mode(&mut self, width: u32, height: u32) -> Result<Mode, DisplayError> {
        if !display::valid_size(width, height) {
            return Err(DisplayError::BadSize);
        }
        let mode = Mode { width, height, pitch: width * 4, format: PixelFormat::Xrgb8888 };
        if mode.bytes() > self.memory {
            return Err(DisplayError::TooLarge);
        }
        if !display::usable(self.framebuffer, &mode) {
            return Err(DisplayError::Unreachable);
        }
        if !Self::program(&mode) {
            // Back to what was on: the old mode, or the text screen.
            match &self.mode {
                Some(old) => {
                    Self::program(old);
                }
                None => write(REG_ENABLE, 0),
            }
            return Err(DisplayError::Refused);
        }
        self.mode = Some(mode);
        Ok(mode)
    }
}
