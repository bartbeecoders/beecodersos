//! # BeeFS — the BeeCodersOs filesystem
//!
//! A small inode-based filesystem that works on any [`BlockDevice`]:
//! a [`RamDisk`] in memory, an ATA disk in the kernel, or an image file on the
//! host (see `tools/beefs-tool`). The crate is `no_std` and only needs `alloc`.
//!
//! ## On-disk layout (block size 512 bytes, all integers little-endian)
//!
//! ```text
//! block 0                 superblock
//! block 1 ..              block bitmap   (1 bit per block, 1 = used)
//! after bitmap            inode table    (64-byte inodes, 8 per block)
//! after inode table       data blocks
//! ```
//!
//! Inodes hold 10 direct block pointers and one single-, one double- and
//! one triple-indirect pointer (max file size ≈ 1 GiB; the triple one was
//! added later, in bytes that were zero, so older disks read as before). Directories are files
//! made of 32-byte entries. Inode numbers start at 1; inode 1 is `/`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod device;
mod fs;
mod partition;

pub use device::{Block, BlockDevice, RamDisk};
pub use fs::{normalize, DirEntry, FileKind, FileSystem, FsStats, Metadata, MAX_FILE_SIZE, MAX_WHOLE_READ};
pub use partition::{
    crc32, find_beefs_gpt, find_beefs_partition, gpt_header, mbr_partitions, GptHeader, Partition, BEEFS_GPT_TYPE,
    BEEFS_GPT_TYPE_TEXT, BEEFS_PARTITION_TYPE, GPT_MAX_ENTRIES,
};

use core::fmt;

/// Size of one block / sector in bytes.
pub const BLOCK_SIZE: usize = 512;
/// Longest allowed file or directory name, in bytes.
pub const MAX_NAME_LEN: usize = 26;
/// Inode number of the root directory.
pub const ROOT_INODE: u32 = 1;

pub type Result<T> = core::result::Result<T, FsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// The underlying device reported an error.
    Io,
    /// Block index past the end of the device.
    OutOfRange,
    /// The device does not contain a BeeFS filesystem.
    BadMagic,
    /// The device is too small to hold a filesystem.
    DeviceTooSmall,
    /// On-disk structures are inconsistent.
    Corrupt,
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    InvalidPath,
    NameTooLong,
    /// No free data blocks left.
    NoSpace,
    /// No free inodes left.
    NoInodes,
    FileTooLarge,
    /// e.g. moving a directory into itself.
    InvalidOperation,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FsError::Io => "I/O error",
            FsError::OutOfRange => "block out of range",
            FsError::BadMagic => "not a BeeFS filesystem",
            FsError::DeviceTooSmall => "device too small",
            FsError::Corrupt => "filesystem corrupt",
            FsError::NotFound => "no such file or directory",
            FsError::AlreadyExists => "already exists",
            FsError::NotADirectory => "not a directory",
            FsError::IsADirectory => "is a directory",
            FsError::DirectoryNotEmpty => "directory not empty",
            FsError::InvalidPath => "invalid path",
            FsError::NameTooLong => "name too long",
            FsError::NoSpace => "no space left on device",
            FsError::NoInodes => "no free inodes",
            FsError::FileTooLarge => "file too large",
            FsError::InvalidOperation => "invalid operation",
        };
        f.write_str(s)
    }
}
