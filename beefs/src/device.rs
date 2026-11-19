//! Block devices: the storage BeeFS runs on.

use crate::{FsError, Result, BLOCK_SIZE};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

pub type Block = [u8; BLOCK_SIZE];

/// Anything that can read and write fixed-size blocks.
pub trait BlockDevice {
    /// Number of `BLOCK_SIZE` blocks on the device.
    fn block_count(&self) -> u64;
    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<()>;
    fn write_block(&mut self, index: u64, buf: &Block) -> Result<()>;
    /// Make sure written data has reached stable storage.
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    /// Read `bufs.len()` blocks from `first` on (devices that can do it in
    /// fewer commands override this).
    fn read_blocks(&mut self, first: u64, bufs: &mut [Block]) -> Result<()> {
        for (i, buf) in bufs.iter_mut().enumerate() {
            self.read_block(first + i as u64, buf)?;
        }
        Ok(())
    }
    /// Write `bufs.len()` blocks from `first` on.
    fn write_blocks(&mut self, first: u64, bufs: &[Block]) -> Result<()> {
        for (i, buf) in bufs.iter().enumerate() {
            self.write_block(first + i as u64, buf)?;
        }
        Ok(())
    }
}

/// Lets `FileSystem<Box<dyn BlockDevice>>` work over any device type.
impl<T: BlockDevice + ?Sized> BlockDevice for Box<T> {
    fn block_count(&self) -> u64 {
        (**self).block_count()
    }
    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<()> {
        (**self).read_block(index, buf)
    }
    fn write_block(&mut self, index: u64, buf: &Block) -> Result<()> {
        (**self).write_block(index, buf)
    }
    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
    fn read_blocks(&mut self, first: u64, bufs: &mut [Block]) -> Result<()> {
        (**self).read_blocks(first, bufs)
    }
    fn write_blocks(&mut self, first: u64, bufs: &[Block]) -> Result<()> {
        (**self).write_blocks(first, bufs)
    }
}

/// A block device backed by memory.
pub struct RamDisk {
    data: Vec<u8>,
}

impl RamDisk {
    /// Create a zeroed RAM disk of `size_bytes` (rounded down to whole blocks).
    pub fn new(size_bytes: usize) -> Self {
        let blocks = size_bytes / BLOCK_SIZE;
        RamDisk { data: vec![0; blocks * BLOCK_SIZE] }
    }

    /// Wrap an existing image (rounded down to whole blocks).
    pub fn from_bytes(mut data: Vec<u8>) -> Self {
        data.truncate(data.len() / BLOCK_SIZE * BLOCK_SIZE);
        RamDisk { data }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    fn range(&self, index: u64) -> Result<core::ops::Range<usize>> {
        if index >= self.block_count() {
            return Err(FsError::OutOfRange);
        }
        let start = index as usize * BLOCK_SIZE;
        Ok(start..start + BLOCK_SIZE)
    }
}

impl BlockDevice for RamDisk {
    fn block_count(&self) -> u64 {
        (self.data.len() / BLOCK_SIZE) as u64
    }

    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<()> {
        let r = self.range(index)?;
        buf.copy_from_slice(&self.data[r]);
        Ok(())
    }

    fn write_block(&mut self, index: u64, buf: &Block) -> Result<()> {
        let r = self.range(index)?;
        self.data[r].copy_from_slice(buf);
        Ok(())
    }
}
