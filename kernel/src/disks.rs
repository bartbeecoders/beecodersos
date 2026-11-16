//! The machine's disks, each opened once. Everything that reads or writes a
//! disk (the mounted file system, `install`) goes through this list, so two
//! parts of the kernel never drive the same disk controller behind each
//! other's back.

use crate::ahci::AhciDisk;
use crate::ata::AtaDisk;
use crate::ring::Global;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beefs::{Block, BlockDevice, FsError};

pub struct Disk {
    /// What to call it on a command line: "ide0", "sata0", "sata1", ...
    pub name: String,
    /// For messages: `primary master "QEMU HARDDISK", 64 MiB`.
    pub description: String,
    pub model: String,
    pub sectors: u64,
    device: Box<dyn BlockDevice>,
}

static DISKS: Global<Option<Vec<Disk>>> = Global::new(None);

/// Find the disks (once): the legacy IDE ports first (QEMU, old PCs), then
/// SATA disks on AHCI controllers (real hardware).
fn all() -> &'static mut Vec<Disk> {
    DISKS.get().get_or_insert_with(|| {
        let mut disks = Vec::new();
        if let Some(disk) = AtaDisk::find() {
            disks.push(Disk {
                name: String::from("ide0"),
                description: format!("{} \"{}\", {} MiB", disk.position(), disk.model(), disk.sectors() / 2048),
                model: String::from(disk.model()),
                sectors: disk.sectors(),
                device: Box::new(disk),
            });
        }
        for disk in AhciDisk::find_all() {
            disks.push(Disk {
                name: format!("sata{}", disk.port()),
                description: format!("SATA port {} \"{}\", {} MiB", disk.port(), disk.model(), disk.sectors() / 2048),
                model: String::from(disk.model()),
                sectors: disk.sectors(),
                device: Box::new(disk),
            });
        }
        disks
    })
}

pub fn count() -> usize {
    all().len()
}

pub fn get(index: usize) -> Option<&'static Disk> {
    all().get(index)
}

pub fn find(name: &str) -> Option<usize> {
    all().iter().position(|d| d.name == name)
}

/// One of the disks, as a block device. Copies all mean the same disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskRef(pub usize);

impl BlockDevice for DiskRef {
    fn block_count(&self) -> u64 {
        get(self.0).map_or(0, |d| d.sectors)
    }

    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), FsError> {
        all().get_mut(self.0).ok_or(FsError::OutOfRange)?.device.read_block(index, buf)
    }

    fn write_block(&mut self, index: u64, buf: &Block) -> Result<(), FsError> {
        all().get_mut(self.0).ok_or(FsError::OutOfRange)?.device.write_block(index, buf)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        all().get_mut(self.0).ok_or(FsError::OutOfRange)?.device.flush()
    }
}
