//! MBR partitions: find the BeeFS partition on a partitioned disk (the boot
//! disk has a FAT boot partition first, BeeFS second) and use it as a device.

use crate::device::{Block, BlockDevice};
use crate::{FsError, Result};

/// MBR partition type of a BeeFS partition (0x7F: "reserved for experimental use").
pub const BEEFS_PARTITION_TYPE: u8 = 0x7F;

/// A partition entry: (type, first block, number of blocks).
pub fn mbr_partitions(sector0: &Block) -> Option<[(u8, u64, u64); 4]> {
    if sector0[510] != 0x55 || sector0[511] != 0xAA {
        return None;
    }
    let mut out = [(0u8, 0u64, 0u64); 4];
    for (i, slot) in out.iter_mut().enumerate() {
        let e = &sector0[446 + i * 16..446 + (i + 1) * 16];
        let start = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
        let len = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
        *slot = (e[4], start, len);
    }
    Some(out)
}

/// (first block, number of blocks) of the BeeFS partition, if the disk has one.
pub fn find_beefs_partition(sector0: &Block) -> Option<(u64, u64)> {
    mbr_partitions(sector0)?
        .into_iter()
        .find(|&(t, start, len)| t == BEEFS_PARTITION_TYPE && start > 0 && len > 0)
        .map(|(_, start, len)| (start, len))
}

/// A window of blocks on a device.
pub struct Partition<D: BlockDevice> {
    dev: D,
    start: u64,
    len: u64,
}

impl<D: BlockDevice> Partition<D> {
    /// Fails when the partition does not fit on the device.
    pub fn new(dev: D, start: u64, len: u64) -> Result<Self> {
        if start.checked_add(len).is_none_or(|end| end > dev.block_count()) {
            return Err(FsError::OutOfRange);
        }
        Ok(Partition { dev, start, len })
    }

    /// The BeeFS partition of `dev`, or the whole device if it has no partition table entry for BeeFS.
    pub fn beefs_or_whole(mut dev: D) -> Result<(Self, bool)> {
        let mut b = [0u8; crate::BLOCK_SIZE];
        dev.read_block(0, &mut b)?;
        match find_beefs_partition(&b) {
            Some((start, len)) => Ok((Partition::new(dev, start, len)?, true)),
            None => {
                let len = dev.block_count();
                Ok((Partition { dev, start: 0, len }, false))
            }
        }
    }

    pub fn start(&self) -> u64 {
        self.start
    }
}

impl<D: BlockDevice> BlockDevice for Partition<D> {
    fn block_count(&self) -> u64 {
        self.len
    }
    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<()> {
        if index >= self.len {
            return Err(FsError::OutOfRange);
        }
        self.dev.read_block(self.start + index, buf)
    }
    fn write_block(&mut self, index: u64, buf: &Block) -> Result<()> {
        if index >= self.len {
            return Err(FsError::OutOfRange);
        }
        self.dev.write_block(self.start + index, buf)
    }
    fn flush(&mut self) -> Result<()> {
        self.dev.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSystem, RamDisk, BLOCK_SIZE};

    fn mbr(entries: &[(u8, u32, u32)]) -> Block {
        let mut b = [0u8; BLOCK_SIZE];
        for (i, &(t, start, len)) in entries.iter().enumerate() {
            let e = &mut b[446 + i * 16..446 + (i + 1) * 16];
            e[4] = t;
            e[8..12].copy_from_slice(&start.to_le_bytes());
            e[12..16].copy_from_slice(&len.to_le_bytes());
        }
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn finds_the_beefs_partition() {
        assert_eq!(find_beefs_partition(&mbr(&[(0xEF, 2048, 1000), (0x7F, 3048, 5000)])), Some((3048, 5000)));
        assert_eq!(find_beefs_partition(&mbr(&[(0xEF, 2048, 1000)])), None);
        assert_eq!(find_beefs_partition(&[0u8; BLOCK_SIZE]), None); // no MBR signature
    }

    #[test]
    fn beefs_lives_inside_its_partition() {
        let mut disk = RamDisk::new(4 * 1024 * 1024);
        disk.write_block(0, &mbr(&[(0xEF, 1, 99), (0x7F, 100, 4000)])).unwrap();
        disk.write_block(99, &[0xEE; BLOCK_SIZE]).unwrap(); // the FAT partition's last block
        let (part, partitioned) = Partition::beefs_or_whole(disk).unwrap();
        assert!(partitioned);
        let mut fs = FileSystem::format(part).unwrap();
        fs.write_file("/x", b"inside").unwrap();
        let mut disk = fs.into_device().dev;
        let mut b = [0u8; BLOCK_SIZE];
        disk.read_block(99, &mut b).unwrap();
        assert_eq!(b, [0xEE; BLOCK_SIZE]); // untouched
        let (part, _) = Partition::beefs_or_whole(disk).unwrap();
        assert_eq!(FileSystem::mount(part).unwrap().read_file("/x").unwrap(), b"inside");
        assert!(Partition::new(RamDisk::new(4096), 4, 5).is_err()); // past the end
    }
}
