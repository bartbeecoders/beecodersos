//! Reading a FAT16 file system, as far as the boot partition needs it: find
//! a file in the root directory and follow its chain of clusters. Nothing
//! here changes the file system's shape; callers may overwrite the contents
//! of clusters a file already has (boot slots, see bootslots.rs, install.rs).
//!
//! The partition is made by tools/bootdisk.py, but it is still data read
//! from a disk: every number is checked, and chains are bounded.

use alloc::vec::Vec;
use beefs::{Block, BlockDevice, BLOCK_SIZE};

/// Short (8.3) names tools/bootdisk.py gives the files we look for.
pub const CONF: &[u8; 11] = b"LIMINE~1CON";
pub const KERNEL_A: &[u8; 11] = b"KERNEL~1ELF";
pub const KERNEL_B: &[u8; 11] = b"KERNEL~2ELF";

/// Where things are in one FAT16 partition (sector numbers on the disk).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fat16 {
    per_cluster: u64,
    fat_start: u64,
    root_start: u64,
    root_sectors: u64,
    data_start: u64,
    clusters: u64,
}

/// A file: where its data starts and how many bytes it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File {
    pub first_cluster: u64,
    pub size: u64,
}

fn le16(b: &[u8], offset: usize) -> u64 {
    u16::from_le_bytes([b[offset], b[offset + 1]]) as u64
}

impl Fat16 {
    /// Read the boot sector of the partition that starts at `start` and has
    /// `sectors` sectors. `None`: not a FAT16 file system we can use.
    pub fn open<D: BlockDevice>(disk: &mut D, start: u64, sectors: u64) -> Option<Fat16> {
        let mut b: Block = [0; BLOCK_SIZE];
        disk.read_block(start, &mut b).ok()?;
        let (bytes, per_cluster, reserved, fats, root_entries, fat_sectors) =
            (le16(&b, 11), b[13] as u64, le16(&b, 14), b[16] as u64, le16(&b, 17), le16(&b, 22));
        if bytes != 512 || per_cluster == 0 || reserved == 0 || fats == 0 || fat_sectors == 0 || root_entries == 0 {
            return None;
        }
        let fat_start = start + reserved;
        let root_start = fat_start + fats * fat_sectors;
        let root_sectors = (root_entries * 32).div_ceil(512);
        let data_start = root_start + root_sectors;
        let end = start.checked_add(sectors)?;
        if data_start >= end {
            return None;
        }
        // As many clusters as fit in the partition and in the table.
        let clusters = ((end - data_start) / per_cluster).min(fat_sectors * 256 - 2).min(0xFFF0 - 2);
        Some(Fat16 { per_cluster, fat_start, root_start, root_sectors, data_start, clusters })
    }

    /// The first sector of a cluster; `None` for a number that is not a data cluster.
    pub fn sector_of(&self, cluster: u64) -> Option<u64> {
        (cluster >= 2 && cluster < self.clusters + 2).then(|| self.data_start + (cluster - 2) * self.per_cluster)
    }

    /// Find a file in the root directory by its short name.
    pub fn find<D: BlockDevice>(&self, disk: &mut D, short_name: &[u8; 11]) -> Option<File> {
        let mut b: Block = [0; BLOCK_SIZE];
        for sector in 0..self.root_sectors {
            disk.read_block(self.root_start + sector, &mut b).ok()?;
            for e in b.chunks(32) {
                match e[0] {
                    0 => return None,               // end of the directory
                    0xE5 => continue,               // deleted
                    _ if e[11] == 0x0F => continue, // part of a long name
                    _ if &e[..11] == short_name => {
                        let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]) as u64;
                        return Some(File { first_cluster: le16(e, 26), size });
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// The sectors that hold a file's data, in order. `None` when the chain
    /// is broken: it leaves the data area, goes in circles, or is shorter
    /// than the file's size says.
    pub fn sectors<D: BlockDevice>(&self, disk: &mut D, file: &File) -> Option<Vec<u64>> {
        let wanted = file.size.div_ceil(BLOCK_SIZE as u64);
        let mut sectors = Vec::new();
        let mut cluster = file.first_cluster;
        let mut table: Block = [0; BLOCK_SIZE];
        let mut loaded = u64::MAX;
        while (sectors.len() as u64) < wanted {
            let first = self.sector_of(cluster)?;
            if sectors.contains(&first) {
                return None; // the chain comes back to a cluster it already had: a circle
            }
            for i in 0..self.per_cluster {
                sectors.push(first + i);
            }
            if sectors.len() as u64 > self.clusters * self.per_cluster {
                return None; // more than the partition holds: a circle
            }
            let table_sector = self.fat_start + cluster / 256;
            if table_sector != loaded {
                disk.read_block(table_sector, &mut table).ok()?;
                loaded = table_sector;
            }
            cluster = le16(&table, (cluster % 256) as usize * 2);
        }
        sectors.truncate(wanted as usize);
        Some(sectors)
    }
}

/// A piece of memory as a read-only disk (the install image).
pub struct Memory<'a>(pub &'a [u8]);

impl BlockDevice for Memory<'_> {
    fn block_count(&self) -> u64 {
        (self.0.len() / BLOCK_SIZE) as u64
    }

    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), beefs::FsError> {
        let start = (index as usize).checked_mul(BLOCK_SIZE).ok_or(beefs::FsError::OutOfRange)?;
        let sector = self.0.get(start..start + BLOCK_SIZE).ok_or(beefs::FsError::OutOfRange)?;
        buf.copy_from_slice(sector);
        Ok(())
    }

    fn write_block(&mut self, _index: u64, _buf: &Block) -> Result<(), beefs::FsError> {
        Err(beefs::FsError::Io)
    }
}
