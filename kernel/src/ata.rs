//! ATA (IDE) disk driver, PIO mode, LBA28, polling (no interrupts).
//! In QEMU: `-drive file=disk.img,format=raw,if=ide,index=0,media=disk`.

use crate::port::{inb, insw, inw, outb, outsw};
use beefs::{Block, BlockDevice, FsError};

const REG_DATA: u16 = 0;
const REG_SECCOUNT: u16 = 2;
const REG_LBA0: u16 = 3;
const REG_LBA1: u16 = 4;
const REG_LBA2: u16 = 5;
const REG_DRIVE: u16 = 6;
const REG_STATUS: u16 = 7; // read
const REG_COMMAND: u16 = 7; // write

const ST_ERR: u8 = 0x01;
const ST_DRQ: u8 = 0x08;
const ST_DF: u8 = 0x20;
const ST_BSY: u8 = 0x80;

const CMD_READ: u8 = 0x20;
const CMD_WRITE: u8 = 0x30;
const CMD_FLUSH: u8 = 0xE7;
const CMD_IDENTIFY: u8 = 0xEC;

const TIMEOUT: u32 = 10_000_000;

pub struct AtaDisk {
    io: u16,
    ctrl: u16,
    slave: bool,
    sectors: u64,
    model: [u8; 40],
}

impl AtaDisk {
    /// Probe the four legacy IDE positions and return the first ATA disk.
    pub fn find() -> Option<Self> {
        [(0x1F0, 0x3F6, false), (0x1F0, 0x3F6, true), (0x170, 0x376, false), (0x170, 0x376, true)]
            .into_iter()
            .find_map(|(io, ctrl, slave)| unsafe { Self::identify(io, ctrl, slave) })
    }

    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    pub fn position(&self) -> &'static str {
        match (self.io, self.slave) {
            (0x1F0, false) => "primary master",
            (0x1F0, true) => "primary slave",
            (_, false) => "secondary master",
            (_, true) => "secondary slave",
        }
    }

    pub fn model(&self) -> &str {
        core::str::from_utf8(&self.model).unwrap_or("?").trim()
    }

    unsafe fn identify(io: u16, ctrl: u16, slave: bool) -> Option<Self> {
        outb(ctrl, 0x02); // nIEN: no interrupts, we poll
        if inb(io + REG_STATUS) == 0xFF {
            return None; // floating bus: no controller
        }
        outb(io + REG_DRIVE, if slave { 0xB0 } else { 0xA0 });
        delay_400ns(ctrl);
        for r in [REG_SECCOUNT, REG_LBA0, REG_LBA1, REG_LBA2] {
            outb(io + r, 0);
        }
        outb(io + REG_COMMAND, CMD_IDENTIFY);
        if inb(io + REG_STATUS) == 0 {
            return None; // no device
        }
        let mut n = 0;
        while inb(io + REG_STATUS) & ST_BSY != 0 {
            n += 1;
            if n > TIMEOUT {
                return None;
            }
        }
        if inb(io + REG_LBA1) != 0 || inb(io + REG_LBA2) != 0 {
            return None; // ATAPI (CD-ROM) or SATA signature, not a plain ATA disk
        }
        loop {
            let st = inb(io + REG_STATUS);
            if st & ST_ERR != 0 {
                return None;
            }
            if st & ST_DRQ != 0 {
                break;
            }
            n += 1;
            if n > TIMEOUT {
                return None;
            }
        }
        let mut id = [0u16; 256];
        for w in id.iter_mut() {
            *w = inw(io + REG_DATA);
        }
        let sectors = id[60] as u64 | (id[61] as u64) << 16;
        let mut model = [0u8; 40];
        for i in 0..20 {
            // ATA strings are big-endian within each word.
            model[i * 2] = (id[27 + i] >> 8) as u8;
            model[i * 2 + 1] = id[27 + i] as u8;
        }
        Some(AtaDisk { io, ctrl, slave, sectors, model })
    }

    fn wait_not_busy(&self) -> Result<u8, FsError> {
        for _ in 0..TIMEOUT {
            let st = unsafe { inb(self.io + REG_STATUS) };
            if st & ST_BSY == 0 {
                if st & (ST_ERR | ST_DF) != 0 {
                    return Err(FsError::Io);
                }
                return Ok(st);
            }
        }
        Err(FsError::Io)
    }

    fn wait_drq(&self) -> Result<(), FsError> {
        for _ in 0..TIMEOUT {
            let st = self.wait_not_busy()?;
            if st & ST_DRQ != 0 {
                return Ok(());
            }
        }
        Err(FsError::Io)
    }

    fn start(&self, lba: u64, cmd: u8) -> Result<(), FsError> {
        self.start_n(lba, 1, cmd)
    }

    /// A command for `count` sectors (1-256) from `lba`.
    fn start_n(&self, lba: u64, count: usize, cmd: u8) -> Result<(), FsError> {
        if lba + count as u64 > self.sectors.min(1 << 28) || !(1..=256).contains(&count) {
            return Err(FsError::OutOfRange);
        }
        self.wait_not_busy()?;
        unsafe {
            let drive = 0xE0 | (self.slave as u8) << 4 | ((lba >> 24) & 0x0F) as u8;
            outb(self.io + REG_DRIVE, drive);
            delay_400ns(self.ctrl);
            outb(self.io + REG_SECCOUNT, count as u8); // 256 is written as 0
            outb(self.io + REG_LBA0, lba as u8);
            outb(self.io + REG_LBA1, (lba >> 8) as u8);
            outb(self.io + REG_LBA2, (lba >> 16) as u8);
            outb(self.io + REG_COMMAND, cmd);
        }
        Ok(())
    }
}

impl BlockDevice for AtaDisk {
    fn block_count(&self) -> u64 {
        self.sectors.min(1 << 28) // LBA28 limit (128 GiB)
    }

    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), FsError> {
        self.start(index, CMD_READ)?;
        self.wait_drq()?;
        unsafe { insw(self.io + REG_DATA, buf) };
        Ok(())
    }

    fn write_block(&mut self, index: u64, buf: &Block) -> Result<(), FsError> {
        self.start(index, CMD_WRITE)?;
        self.wait_drq()?;
        // The whole sector with one instruction: under a hypervisor every
        // single port access is slow, and an install writes thousands of sectors.
        unsafe { outsw(self.io + REG_DATA, buf) };
        self.wait_not_busy()?;
        Ok(())
    }

    /// Many sectors per command (up to 256): far fewer port accesses, which
    /// are what is slow under a hypervisor.
    fn read_blocks(&mut self, first: u64, bufs: &mut [Block]) -> Result<(), FsError> {
        for (n, part) in bufs.chunks_mut(256).enumerate() {
            self.start_n(first + n as u64 * 256, part.len(), CMD_READ)?;
            for buf in part.iter_mut() {
                self.wait_drq()?;
                unsafe { insw(self.io + REG_DATA, buf) };
            }
        }
        Ok(())
    }

    fn write_blocks(&mut self, first: u64, bufs: &[Block]) -> Result<(), FsError> {
        for (n, part) in bufs.chunks(256).enumerate() {
            self.start_n(first + n as u64 * 256, part.len(), CMD_WRITE)?;
            for buf in part {
                self.wait_drq()?;
                unsafe { outsw(self.io + REG_DATA, buf) };
            }
            self.wait_not_busy()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), FsError> {
        self.wait_not_busy()?;
        unsafe { outb(self.io + REG_COMMAND, CMD_FLUSH) };
        self.wait_not_busy()?;
        Ok(())
    }
}

/// Reading the alternate status register 4 times takes ~400ns.
fn delay_400ns(ctrl: u16) {
    for _ in 0..4 {
        unsafe { inb(ctrl) };
    }
}
