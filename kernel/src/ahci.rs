//! AHCI (SATA) disk driver: what real PCs have instead of the old IDE ports.
//! In QEMU: `-device ahci,id=ahci -drive file=disk.img,format=raw,if=none,id=d0
//! -device ide-hd,drive=d0,bus=ahci.0`.
//!
//! Kept simple: polling (no interrupts), one command at a time, one sector
//! per command, through a buffer of our own. Disks only (no CD drives), and
//! only disks with 48-bit addresses, which every SATA disk has.
//!
//! Nothing here waits without a limit, and nothing is written to a disk
//! unless the file system asks for it: finding a disk only reads.
//!
//! Memory: the controller reads and writes our buffers by physical address.
//! The kernel's memory is mapped one to one below 4 GiB, so an address is
//! the same either way.

use crate::{pci, time};
use alloc::alloc::{alloc_zeroed, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use beefs::{Block, BlockDevice, FsError, BLOCK_SIZE};
use core::ptr::{read_volatile, write_volatile};

/// Memory the boot page tables cover (see boot.s).
const MAPPED_LIMIT: u64 = 1 << 32;
const MAX_PORTS: usize = 32;

// Controller registers.
const HBA_GHC: usize = 0x04;
const HBA_PORTS_IMPLEMENTED: usize = 0x0C;
const HBA_CAP2: usize = 0x24;
const HBA_BIOS_HANDOFF: usize = 0x28;
const GHC_AHCI_ENABLE: u32 = 1 << 31;
const GHC_INTERRUPTS: u32 = 1 << 1;
const CAP2_BIOS_HANDOFF: u32 = 1 << 0;
const HANDOFF_BIOS_OWNS: u32 = 1 << 0;
const HANDOFF_OS_OWNS: u32 = 1 << 1;

// Port registers (port N starts at 0x100 + N * 0x80).
const PORT_COMMAND_LIST: usize = 0x00;
const PORT_FIS_BASE: usize = 0x08;
const PORT_INTERRUPT_STATUS: usize = 0x10;
const PORT_INTERRUPT_ENABLE: usize = 0x14;
const PORT_CMD: usize = 0x18;
const PORT_TASK_FILE: usize = 0x20;
const PORT_SIGNATURE: usize = 0x24;
const PORT_SATA_STATUS: usize = 0x28;
const PORT_SATA_ERROR: usize = 0x30;
const PORT_COMMAND_ISSUE: usize = 0x38;

const CMD_START: u32 = 1 << 0;
const CMD_FIS_RECEIVE: u32 = 1 << 4;
const CMD_FIS_RUNNING: u32 = 1 << 14;
const CMD_LIST_RUNNING: u32 = 1 << 15;
const INTERRUPT_TASK_FILE_ERROR: u32 = 1 << 30;
const TASK_FILE_ERROR: u32 = 1 << 0;
const TASK_FILE_DRQ: u32 = 1 << 3;
const TASK_FILE_BUSY: u32 = 1 << 7;
const SIGNATURE_DISK: u32 = 0x0000_0101;

const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_CACHE_EXT: u8 = 0xEA;

/// A command gets this long (a disk that was asleep needs seconds to spin up).
const COMMAND_TIMEOUT_MS: u64 = 10_000;
/// Starting and stopping a port: the specification allows 500 ms.
const PORT_TIMEOUT_MS: u64 = 1_000;
/// Second limit, in register reads, for when the clock does not tick
/// (interrupts off while a crash report is written).
const MAX_SPINS: u64 = 2_000_000_000;

const FIS_LEN: usize = 20;
/// Where the buffer descriptor sits in the command table.
const TABLE_DESCRIPTOR: usize = 0x80;

/// The 20 bytes that carry an ATA command to the disk ("register FIS, host
/// to device").
pub fn command_fis(command: u8, lba: u64, sectors: u16) -> [u8; FIS_LEN] {
    let mut fis = [0u8; FIS_LEN];
    fis[0] = 0x27; // register FIS, host to device
    fis[1] = 0x80; // this is a command
    fis[2] = command;
    fis[4..7].copy_from_slice(&lba.to_le_bytes()[0..3]);
    fis[7] = 0x40; // address by sector number (LBA)
    fis[8..11].copy_from_slice(&lba.to_le_bytes()[3..6]);
    fis[12..14].copy_from_slice(&sectors.to_le_bytes());
    fis
}

/// What a disk says about itself (the answer to IDENTIFY, 256 words).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub sectors: u64,
    pub model: String,
}

/// None: not a disk we can use (no 48-bit addresses, or no size).
pub fn parse_identity(sector: &[u8; BLOCK_SIZE]) -> Option<Identity> {
    let word = |i: usize| u16::from_le_bytes([sector[i * 2], sector[i * 2 + 1]]);
    if word(0) & 0x8000 != 0 || word(83) & (1 << 10) == 0 {
        return None; // not an ATA disk, or no 48-bit addresses
    }
    let sectors = (0..4).fold(0u64, |total, i| total | (word(100 + i) as u64) << (16 * i));
    if sectors == 0 {
        return None;
    }
    // Text is stored with the two bytes of every word swapped.
    let model: String = (27..47)
        .flat_map(|i| [(word(i) >> 8) as u8, word(i) as u8])
        .map(|b| if (0x20..0x7F).contains(&b) { b as char } else { ' ' })
        .collect();
    Some(Identity { sectors, model: String::from(model.trim()) })
}

/// Wait until `done()` holds; false when it took too long.
fn wait_for(timeout_ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = time::uptime_ms() + timeout_ms;
    let mut spins = 0u64;
    while !done() {
        spins += 1;
        if time::uptime_ms() > end || spins > MAX_SPINS {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// One page of zeroed memory below 4 GiB, never freed (the controller keeps using it).
fn dma_page() -> Option<*mut u8> {
    let page = unsafe { alloc_zeroed(Layout::from_size_align(4096, 4096).ok()?) };
    (!page.is_null() && (page as u64) + 4096 <= MAPPED_LIMIT).then_some(page)
}

pub struct AhciDisk {
    /// The port's registers.
    regs: *mut u8,
    port: usize,
    /// Command list (32 headers of 32 bytes), then the area for received FISes.
    lists: *mut u8,
    /// Command table: the command FIS and one buffer descriptor.
    table: *mut u8,
    /// One sector: the controller copies to and from here.
    buffer: *mut u8,
    identity: Identity,
}

impl AhciDisk {
    /// Every SATA disk on every AHCI controller. Only reads from the disks.
    pub fn find_all() -> Vec<AhciDisk> {
        let mut disks = Vec::new();
        for controller in pci::scan().iter().filter(|d| d.class == 0x01 && d.subclass == 0x06) {
            let pci::Bar::Memory { addr, .. } = controller.bar(5) else { continue };
            if addr == 0 || addr + 0x1100 > MAPPED_LIMIT {
                continue;
            }
            controller.enable(pci::CMD_MEMORY | pci::CMD_BUS_MASTER);
            let hba = addr as *mut u8;
            unsafe { take_over(hba) };
            let implemented = unsafe { read(hba, HBA_PORTS_IMPLEMENTED) };
            for port in (0..MAX_PORTS).filter(|p| implemented & (1 << p) != 0) {
                disks.extend(unsafe { Self::open(hba, port) });
            }
        }
        disks
    }

    pub fn sectors(&self) -> u64 {
        self.identity.sectors
    }

    pub fn model(&self) -> &str {
        &self.identity.model
    }

    pub fn port(&self) -> usize {
        self.port
    }

    /// Set up port `port` if a disk is attached to it and answers.
    unsafe fn open(hba: *mut u8, port: usize) -> Option<AhciDisk> {
        let regs = hba.add(0x100 + port * 0x80);
        let status = read(regs, PORT_SATA_STATUS);
        // A device is there, the link is up, and it is in the active power state.
        if status & 0xF != 3 || (status >> 8) & 0xF != 1 || read(regs, PORT_SIGNATURE) != SIGNATURE_DISK {
            return None;
        }
        let (lists, table, buffer) = (dma_page()?, dma_page()?, dma_page()?);
        let mut disk =
            AhciDisk { regs, port, lists, table, buffer, identity: Identity { sectors: 0, model: String::new() } };
        if !disk.stop() {
            return None;
        }
        write(regs, PORT_COMMAND_LIST, lists as u32);
        write(regs, PORT_COMMAND_LIST + 4, 0);
        write(regs, PORT_FIS_BASE, lists as u32 + 1024);
        write(regs, PORT_FIS_BASE + 4, 0);
        write(regs, PORT_INTERRUPT_ENABLE, 0); // we poll
        if !disk.start() {
            return None;
        }
        disk.command(ATA_IDENTIFY, 0, false, true).ok()?;
        disk.identity = parse_identity(&*(buffer as *const [u8; BLOCK_SIZE]))?;
        Some(disk)
    }

    /// Stop the port: no command list, no receiving. False when it does not obey.
    fn stop(&mut self) -> bool {
        let regs = self.regs;
        unsafe {
            write(regs, PORT_CMD, read(regs, PORT_CMD) & !CMD_START);
            if !wait_for(PORT_TIMEOUT_MS, || read(regs, PORT_CMD) & CMD_LIST_RUNNING == 0) {
                return false;
            }
            write(regs, PORT_CMD, read(regs, PORT_CMD) & !CMD_FIS_RECEIVE);
            wait_for(PORT_TIMEOUT_MS, || read(regs, PORT_CMD) & CMD_FIS_RUNNING == 0)
        }
    }

    /// Start the port with old errors cleared. False when the disk stays busy.
    fn start(&mut self) -> bool {
        let regs = self.regs;
        unsafe {
            write(regs, PORT_SATA_ERROR, 0xFFFF_FFFF); // written ones clear the bits
            write(regs, PORT_INTERRUPT_STATUS, 0xFFFF_FFFF);
            write(regs, PORT_CMD, read(regs, PORT_CMD) | CMD_FIS_RECEIVE);
            if !wait_for(COMMAND_TIMEOUT_MS, || read(regs, PORT_TASK_FILE) & (TASK_FILE_BUSY | TASK_FILE_DRQ) == 0) {
                return false;
            }
            write(regs, PORT_CMD, read(regs, PORT_CMD) | CMD_START);
        }
        true
    }

    /// Run one command. With `data`, one sector moves between the disk and
    /// `self.buffer` (to the disk when `to_disk`).
    fn command(&mut self, command: u8, lba: u64, to_disk: bool, data: bool) -> Result<(), FsError> {
        self.command_at(command, lba, to_disk, self.buffer, data as u16)
    }

    /// Run one command that moves `sectors` sectors (0: none) between the
    /// disk and the memory at `memory`: the controller reads or writes
    /// that memory itself, so it must stay where it is until the command
    /// is done, start at an even address and lie below 4 GiB with the
    /// rest of the kernel's memory.
    fn command_at(&mut self, command: u8, lba: u64, to_disk: bool, memory: *mut u8, sectors: u16) -> Result<(), FsError> {
        let regs = self.regs;
        let data = sectors > 0;
        let bytes = (sectors as u32).max(1) * BLOCK_SIZE as u32;
        unsafe {
            // The command table: the FIS, and where the data is.
            core::ptr::write_bytes(self.table, 0, 256);
            let fis = command_fis(command, lba, sectors);
            core::ptr::copy_nonoverlapping(fis.as_ptr(), self.table, FIS_LEN);
            let descriptor = self.table.add(TABLE_DESCRIPTOR) as *mut u32;
            write_volatile(descriptor, memory as u32);
            write_volatile(descriptor.add(1), 0);
            write_volatile(descriptor.add(3), bytes - 1);
            // Command header 0: FIS length in 32-bit words, direction, one descriptor.
            let header = self.lists as *mut u32;
            let flags = (FIS_LEN as u32 / 4) | (to_disk as u32) << 6 | (data as u32) << 16;
            write_volatile(header, flags);
            write_volatile(header.add(1), 0); // bytes moved: filled in by the controller
            write_volatile(header.add(2), self.table as u32);
            write_volatile(header.add(3), 0);

            write(regs, PORT_INTERRUPT_STATUS, 0xFFFF_FFFF);
            write(regs, PORT_COMMAND_ISSUE, 1);
            let finished = wait_for(COMMAND_TIMEOUT_MS, || {
                read(regs, PORT_COMMAND_ISSUE) & 1 == 0
                    || read(regs, PORT_INTERRUPT_STATUS) & INTERRUPT_TASK_FILE_ERROR != 0
            });
            let failed = read(regs, PORT_INTERRUPT_STATUS) & INTERRUPT_TASK_FILE_ERROR != 0
                || read(regs, PORT_TASK_FILE) & TASK_FILE_ERROR != 0;
            if !finished || failed {
                // Get the port back into a clean state for the next command.
                if self.stop() {
                    self.start();
                }
                return Err(FsError::Io);
            }
            if data && read_volatile(header.add(1)) != bytes {
                return Err(FsError::Io); // the disk moved less than was asked
            }
        }
        Ok(())
    }
}

impl BlockDevice for AhciDisk {
    fn block_count(&self) -> u64 {
        self.identity.sectors
    }

    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), FsError> {
        if index >= self.identity.sectors {
            return Err(FsError::OutOfRange);
        }
        self.command(ATA_READ_DMA_EXT, index, false, true)?;
        unsafe { core::ptr::copy_nonoverlapping(self.buffer, buf.as_mut_ptr(), BLOCK_SIZE) };
        Ok(())
    }

    fn write_block(&mut self, index: u64, buf: &Block) -> Result<(), FsError> {
        if index >= self.identity.sectors {
            return Err(FsError::OutOfRange);
        }
        unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), self.buffer, BLOCK_SIZE) };
        self.command(ATA_WRITE_DMA_EXT, index, true, true)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        self.command(ATA_FLUSH_CACHE_EXT, 0, false, false)
    }

    /// Many sectors per command, straight from and to the caller's memory:
    /// one command per sector is what made large files slow (4 MB/s on the HP).
    fn read_blocks(&mut self, first: u64, bufs: &mut [Block]) -> Result<(), FsError> {
        if !self.whole_run(first, bufs.len(), bufs.as_ptr() as usize) {
            return bufs.iter_mut().enumerate().try_for_each(|(i, buf)| self.read_block(first + i as u64, buf));
        }
        for (n, part) in bufs.chunks_mut(MAX_SECTORS).enumerate() {
            self.command_at(ATA_READ_DMA_EXT, first + (n * MAX_SECTORS) as u64, false, part.as_mut_ptr() as *mut u8, part.len() as u16)?;
        }
        Ok(())
    }

    fn write_blocks(&mut self, first: u64, bufs: &[Block]) -> Result<(), FsError> {
        if !self.whole_run(first, bufs.len(), bufs.as_ptr() as usize) {
            return bufs.iter().enumerate().try_for_each(|(i, buf)| self.write_block(first + i as u64, buf));
        }
        for (n, part) in bufs.chunks(MAX_SECTORS).enumerate() {
            // The controller only reads this memory.
            self.command_at(ATA_WRITE_DMA_EXT, first + (n * MAX_SECTORS) as u64, true, part.as_ptr() as *mut u8, part.len() as u16)?;
        }
        Ok(())
    }
}

/// The most sectors one command moves (1 MiB: one descriptor holds up to 4 MiB).
const MAX_SECTORS: usize = 2048;

/// May `count` sectors from `first` on move in whole commands from or to
/// memory at `address`? (All on the disk, and memory the controller can use.)
pub fn run_fits(first: u64, count: usize, sectors: u64, address: usize) -> bool {
    let on_disk = first.checked_add(count as u64).is_some_and(|end| end <= sectors);
    let end = address as u64 + (count * BLOCK_SIZE) as u64;
    on_disk && count > 0 && address % 2 == 0 && end <= 1 << 32
}

impl AhciDisk {
    fn whole_run(&self, first: u64, count: usize, address: usize) -> bool {
        run_fits(first, count, self.identity.sectors, address)
    }
}

/// Switch the controller to AHCI mode, ours to use, without interrupts.
unsafe fn take_over(hba: *mut u8) {
    write(hba, HBA_GHC, read(hba, HBA_GHC) | GHC_AHCI_ENABLE);
    // Firmware that still uses the controller hands it over when asked.
    if read(hba, HBA_CAP2) & CAP2_BIOS_HANDOFF != 0 {
        write(hba, HBA_BIOS_HANDOFF, read(hba, HBA_BIOS_HANDOFF) | HANDOFF_OS_OWNS);
        wait_for(PORT_TIMEOUT_MS, || read(hba, HBA_BIOS_HANDOFF) & HANDOFF_BIOS_OWNS == 0);
    }
    write(hba, HBA_GHC, read(hba, HBA_GHC) & !GHC_INTERRUPTS);
}

unsafe fn read(base: *mut u8, register: usize) -> u32 {
    read_volatile(base.add(register) as *const u32)
}

unsafe fn write(base: *mut u8, register: usize, value: u32) {
    write_volatile(base.add(register) as *mut u32, value);
}
