//! Partitions: find the BeeFS partition on a partitioned disk and use it as
//! a device. Two table formats: MBR (the boot disk has a FAT boot partition
//! first, BeeFS second) and GPT (what real disks use today).
//!
//! A partition table is read from a disk we do not trust: every length is
//! checked, the number of entries read is bounded, and a GPT is only used
//! when both its checksums are right. Anything else means "no BeeFS
//! partition here", never a guess.

use crate::device::{Block, BlockDevice};
use crate::{FsError, Result, BLOCK_SIZE};

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

/// GPT partition type of a BeeFS partition:
/// FE35CA6A-9309-4008-996D-254793C95A38, in the byte order used on disk.
pub const BEEFS_GPT_TYPE: [u8; 16] =
    [0x6A, 0xCA, 0x35, 0xFE, 0x09, 0x93, 0x08, 0x40, 0x99, 0x6D, 0x25, 0x47, 0x93, 0xC9, 0x5A, 0x38];
/// The same, as partitioning tools write it.
pub const BEEFS_GPT_TYPE_TEXT: &str = "FE35CA6A-9309-4008-996D-254793C95A38";
/// MBR type of the one entry a GPT disk keeps in its MBR ("protective").
const GPT_PROTECTIVE_TYPE: u8 = 0xEE;
/// More entries than this are not looked at (the usual table has 128).
pub const GPT_MAX_ENTRIES: u32 = 1024;

/// CRC-32 as GPT uses it (the common one: zip, Ethernet). `state` is 0 for
/// the first piece of data, and the previous result for every next piece.
pub fn crc32(state: u32, data: &[u8]) -> u32 {
    let mut crc = !state;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { crc >> 1 ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// What we need from a GPT header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GptHeader {
    /// First block of the partition entries.
    pub entries_start: u64,
    pub entry_count: u32,
    /// Bytes per entry: 128, 256 or 512.
    pub entry_size: u32,
    pub entries_crc: u32,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u32_at(b, at) as u64 | (u32_at(b, at + 4) as u64) << 32
}

/// Read the GPT header in block 1 of a disk of `disk_blocks` blocks. None
/// unless everything about it is right.
pub fn gpt_header(block1: &Block, disk_blocks: u64) -> Option<GptHeader> {
    if &block1[..8] != b"EFI PART" {
        return None;
    }
    let header_size = u32_at(block1, 12) as usize;
    if !(92..=BLOCK_SIZE).contains(&header_size) {
        return None;
    }
    // The checksum covers the header with its own field as zero.
    let mut copy = *block1;
    copy[16..20].fill(0);
    if crc32(0, &copy[..header_size]) != u32_at(block1, 16) || u64_at(block1, 24) != 1 {
        return None;
    }
    let header = GptHeader {
        entries_start: u64_at(block1, 72),
        entry_count: u32_at(block1, 80),
        entry_size: u32_at(block1, 84),
        entries_crc: u32_at(block1, 88),
    };
    if !matches!(header.entry_size, 128 | 256 | 512) || header.entry_count > GPT_MAX_ENTRIES {
        return None;
    }
    let bytes = header.entry_count as u64 * header.entry_size as u64;
    let end = header.entries_start.checked_add(bytes.div_ceil(BLOCK_SIZE as u64))?;
    (header.entries_start >= 2 && end <= disk_blocks).then_some(header)
}

/// (first block, number of blocks) of the first BeeFS partition in the GPT
/// of `dev`. None when the disk has no valid GPT or no such partition.
pub fn find_beefs_gpt<D: BlockDevice>(dev: &mut D) -> Result<Option<(u64, u64)>> {
    let disk_blocks = dev.block_count();
    if disk_blocks < 3 {
        return Ok(None);
    }
    let mut block = [0u8; BLOCK_SIZE];
    dev.read_block(1, &mut block)?;
    let Some(header) = gpt_header(&block, disk_blocks) else { return Ok(None) };
    let size = header.entry_size as usize;
    let mut left = header.entry_count as usize * size;
    let mut crc = 0;
    let mut found = None;
    let mut at = header.entries_start;
    while left > 0 {
        dev.read_block(at, &mut block)?;
        let used = left.min(BLOCK_SIZE);
        crc = crc32(crc, &block[..used]);
        for entry in block[..used].chunks_exact(size) {
            let (first, last) = (u64_at(entry, 32), u64_at(entry, 40));
            // Inside the disk, and behind the table itself.
            if found.is_none() && entry[..16] == BEEFS_GPT_TYPE && first >= 2 && first <= last && last < disk_blocks {
                found = Some((first, last - first + 1));
            }
        }
        left -= used;
        at += 1;
    }
    // A table with a wrong checksum is not believed at all.
    Ok(if crc == header.entries_crc { found } else { None })
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

    /// The BeeFS partition of `dev` (MBR or GPT), or the whole device if it has no partition table entry for BeeFS.
    pub fn beefs_or_whole(mut dev: D) -> Result<(Self, bool)> {
        let mut b = [0u8; crate::BLOCK_SIZE];
        dev.read_block(0, &mut b)?;
        let mut found = find_beefs_partition(&b);
        if found.is_none() && mbr_partitions(&b).is_some_and(|p| p.iter().any(|e| e.0 == GPT_PROTECTIVE_TYPE)) {
            found = find_beefs_gpt(&mut dev)?;
        }
        match found {
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
    use crate::{FileSystem, RamDisk};

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

    const OTHER_TYPE: [u8; 16] = [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B];

    /// A disk with a GPT: protective MBR, header in block 1, entries from block 2.
    fn gpt_disk(blocks: u64, entries: &[([u8; 16], u64, u64)], entry_size: usize, count: u32) -> RamDisk {
        let mut disk = RamDisk::new(blocks as usize * BLOCK_SIZE);
        disk.write_block(0, &mbr(&[(0xEE, 1, blocks as u32 - 1)])).unwrap();
        let mut table = alloc::vec![0u8; (count as usize * entry_size).div_ceil(BLOCK_SIZE) * BLOCK_SIZE];
        for (i, (kind, first, last)) in entries.iter().enumerate() {
            let e = &mut table[i * entry_size..];
            e[..16].copy_from_slice(kind);
            e[16] = i as u8 + 1; // unique id
            e[32..40].copy_from_slice(&first.to_le_bytes());
            e[40..48].copy_from_slice(&last.to_le_bytes());
        }
        for (i, chunk) in table.chunks(BLOCK_SIZE).enumerate() {
            disk.write_block(2 + i as u64, chunk.try_into().unwrap()).unwrap();
        }
        let mut h = [0u8; BLOCK_SIZE];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&1u64.to_le_bytes());
        h[72..80].copy_from_slice(&2u64.to_le_bytes());
        h[80..84].copy_from_slice(&count.to_le_bytes());
        h[84..88].copy_from_slice(&(entry_size as u32).to_le_bytes());
        h[88..92].copy_from_slice(&crc32(0, &table[..count as usize * entry_size]).to_le_bytes());
        let crc = crc32(0, &h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        disk.write_block(1, &h).unwrap();
        disk
    }

    /// Change one header field and make the header checksum right again.
    fn patch_header(disk: &mut RamDisk, at: usize, bytes: &[u8]) {
        let mut h = [0u8; BLOCK_SIZE];
        disk.read_block(1, &mut h).unwrap();
        h[at..at + bytes.len()].copy_from_slice(bytes);
        h[16..20].fill(0);
        let crc = crc32(0, &h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        disk.write_block(1, &h).unwrap();
    }

    #[test]
    fn crc32_is_the_common_one() {
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(crc32(0, b"1234"), b"56789"), 0xCBF4_3926); // in pieces
        assert_eq!(crc32(0, b""), 0);
    }

    #[test]
    fn finds_the_beefs_partition_in_a_gpt() {
        let mut disk = gpt_disk(8192, &[(OTHER_TYPE, 40, 1000), (BEEFS_GPT_TYPE, 2048, 8000)], 128, 128);
        assert_eq!(find_beefs_gpt(&mut disk).unwrap(), Some((2048, 5953)));
        // Larger entries, and a table that ends in the middle of a block.
        let mut disk = gpt_disk(8192, &[(OTHER_TYPE, 40, 1000), (BEEFS_GPT_TYPE, 2048, 8000)], 256, 3);
        assert_eq!(find_beefs_gpt(&mut disk).unwrap(), Some((2048, 5953)));
        // No BeeFS partition, an empty table, no GPT at all.
        assert_eq!(find_beefs_gpt(&mut gpt_disk(8192, &[(OTHER_TYPE, 40, 1000)], 128, 128)).unwrap(), None);
        assert_eq!(find_beefs_gpt(&mut gpt_disk(8192, &[], 128, 0)).unwrap(), None);
        assert_eq!(find_beefs_gpt(&mut RamDisk::new(8192 * BLOCK_SIZE)).unwrap(), None);
        assert_eq!(find_beefs_gpt(&mut RamDisk::new(BLOCK_SIZE)).unwrap(), None); // too small to hold one
    }

    #[test]
    fn a_damaged_gpt_is_not_believed() {
        let good = || gpt_disk(8192, &[(BEEFS_GPT_TYPE, 2048, 8000)], 128, 128);
        // One changed byte anywhere in the header, or in the entries.
        for at in [0, 9, 12, 16, 24, 40, 72, 80, 84, 88, 91] {
            let mut disk = good();
            let mut h = [0u8; BLOCK_SIZE];
            disk.read_block(1, &mut h).unwrap();
            h[at] ^= 0x01;
            disk.write_block(1, &h).unwrap();
            assert_eq!(find_beefs_gpt(&mut disk).unwrap(), None, "header byte {at}");
        }
        let mut disk = good();
        let mut b = [0u8; BLOCK_SIZE];
        disk.read_block(5, &mut b).unwrap();
        b[7] ^= 0x80;
        disk.write_block(5, &b).unwrap();
        assert_eq!(find_beefs_gpt(&mut disk).unwrap(), None);
        // A header cut short: every length from nothing to almost all of it.
        for keep in 0..92 {
            let mut disk = good();
            let mut h = [0u8; BLOCK_SIZE];
            disk.read_block(1, &mut h).unwrap();
            h[keep..].fill(0);
            disk.write_block(1, &h).unwrap();
            assert_eq!(find_beefs_gpt(&mut disk).unwrap(), None, "cut at {keep}");
        }
    }

    #[test]
    fn a_gpt_with_right_checksums_and_wrong_values_is_refused() {
        let good = || gpt_disk(8192, &[(BEEFS_GPT_TYPE, 2048, 8000)], 128, 128);
        let cases: [(usize, &[u8]); 8] = [
            (12, &91u32.to_le_bytes()),           // header too short
            (12, &513u32.to_le_bytes()),          // header longer than its block
            (24, &2u64.to_le_bytes()),            // not the header of block 1
            (72, &1u64.to_le_bytes()),            // entries on top of the header
            (72, &8190u64.to_le_bytes()),         // entries past the end of the disk
            (72, &u64::MAX.to_le_bytes()),        // ... far past it
            (80, &u32::MAX.to_le_bytes()),        // an absurd number of entries
            (84, &100u32.to_le_bytes()),          // an entry size that is none
        ];
        for (at, bytes) in cases {
            let mut disk = good();
            patch_header(&mut disk, at, bytes);
            assert_eq!(find_beefs_gpt(&mut disk).unwrap(), None, "field at {at} = {bytes:?}");
        }
        // Partitions that do not fit: past the end, backwards, on the table.
        for (first, last) in [(2048, 8192), (2048, u64::MAX), (3000, 2999), (0, 100), (1, 100)] {
            let mut disk = gpt_disk(8192, &[(BEEFS_GPT_TYPE, first, last)], 128, 128);
            assert_eq!(find_beefs_gpt(&mut disk).unwrap(), None, "{first}..{last}");
        }
    }

    #[test]
    fn beefs_lives_inside_its_gpt_partition() {
        let mut disk = gpt_disk(8192, &[(OTHER_TYPE, 40, 99), (BEEFS_GPT_TYPE, 100, 8000)], 128, 128);
        disk.write_block(99, &[0xEE; BLOCK_SIZE]).unwrap(); // the other partition's last block
        let (part, partitioned) = Partition::beefs_or_whole(disk).unwrap();
        assert!(partitioned);
        assert_eq!(part.start(), 100);
        let mut fs = FileSystem::format(part).unwrap();
        fs.write_file("/x", b"inside").unwrap();
        let mut disk = fs.into_device().dev;
        let mut b = [0u8; BLOCK_SIZE];
        disk.read_block(99, &mut b).unwrap();
        assert_eq!(b, [0xEE; BLOCK_SIZE]); // untouched
        let (part, _) = Partition::beefs_or_whole(disk).unwrap();
        assert_eq!(FileSystem::mount(part).unwrap().read_file("/x").unwrap(), b"inside");
        // A GPT disk without a BeeFS partition is the whole disk: not BeeFS, so never mounted.
        let (mut whole, partitioned) = Partition::beefs_or_whole(gpt_disk(8192, &[(OTHER_TYPE, 40, 99)], 128, 128)).unwrap();
        assert!(!partitioned);
        assert!(!FileSystem::probe(&mut whole).unwrap());
    }
}
