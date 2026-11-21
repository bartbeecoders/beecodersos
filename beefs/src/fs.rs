//! BeeFS core: superblock, bitmap, inodes, directories and path operations.

use crate::device::{Block, BlockDevice};
use crate::{FsError, Result, BLOCK_SIZE, MAX_NAME_LEN, ROOT_INODE};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const MAGIC: [u8; 8] = *b"BEEFS\0v1";

const INODE_SIZE: usize = 64;
const INODES_PER_BLOCK: u32 = (BLOCK_SIZE / INODE_SIZE) as u32;
const BITS_PER_BLOCK: u32 = (BLOCK_SIZE * 8) as u32;
const PTRS_PER_BLOCK: u64 = (BLOCK_SIZE / 4) as u64;
const DIRECT: usize = 10;
const DIRENT_SIZE: usize = 32;

/// Largest file BeeFS can hold, in bytes (about 1 GiB): what the direct
/// pointers and the single-, double- and triple-indirect tables reach.
pub const MAX_FILE_SIZE: u64 = (DIRECT as u64 + PTRS_PER_BLOCK + PTRS_PER_BLOCK * PTRS_PER_BLOCK + PTRS_PER_BLOCK * PTRS_PER_BLOCK * PTRS_PER_BLOCK)
    * BLOCK_SIZE as u64;

/// Largest file `read_file` gives whole, in memory. A larger one is read in
/// pieces with `read_at`.
pub const MAX_WHOLE_READ: u64 = 16 * 1024 * 1024;

const KIND_FREE: u8 = 0;
const KIND_FILE: u8 = 1;
const KIND_DIR: u8 = 2;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    pub inode: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub kind: FileKind,
    pub size: u64,
    pub inode: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStats {
    pub block_size: u32,
    pub total_blocks: u32,
    pub free_blocks: u32,
    pub total_inodes: u32,
    pub free_inodes: u32,
}

// ---------------------------------------------------------------------------
// Little-endian helpers
// ---------------------------------------------------------------------------

fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn wr32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn rd64(b: &[u8], off: usize) -> u64 {
    rd32(b, off) as u64 | (rd32(b, off + 4) as u64) << 32
}

fn wr64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Superblock
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Superblock {
    block_count: u32,
    inode_count: u32,
    bitmap_start: u32,
    bitmap_blocks: u32,
    inode_start: u32,
    inode_blocks: u32,
    data_start: u32,
    free_blocks: u32,
    free_inodes: u32,
}

impl Superblock {
    fn encode(&self) -> Block {
        let mut b = [0u8; BLOCK_SIZE];
        b[0..8].copy_from_slice(&MAGIC);
        let fields = [
            self.block_count,
            self.inode_count,
            self.bitmap_start,
            self.bitmap_blocks,
            self.inode_start,
            self.inode_blocks,
            self.data_start,
            self.free_blocks,
            self.free_inodes,
        ];
        for (i, v) in fields.iter().enumerate() {
            wr32(&mut b, 8 + i * 4, *v);
        }
        b
    }

    fn decode(b: &Block) -> Result<Self> {
        if b[0..8] != MAGIC {
            return Err(FsError::BadMagic);
        }
        let f = |i: usize| rd32(b, 8 + i * 4);
        let sb = Superblock {
            block_count: f(0),
            inode_count: f(1),
            bitmap_start: f(2),
            bitmap_blocks: f(3),
            inode_start: f(4),
            inode_blocks: f(5),
            data_start: f(6),
            free_blocks: f(7),
            free_inodes: f(8),
        };
        let consistent = sb.bitmap_start == 1
            && sb.inode_start == sb.bitmap_start + sb.bitmap_blocks
            && sb.data_start == sb.inode_start + sb.inode_blocks
            && sb.data_start < sb.block_count
            && sb.bitmap_blocks == sb.block_count.div_ceil(BITS_PER_BLOCK)
            && sb.inode_count == sb.inode_blocks * INODES_PER_BLOCK;
        if !consistent {
            return Err(FsError::Corrupt);
        }
        Ok(sb)
    }
}

// ---------------------------------------------------------------------------
// Inodes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
struct Inode {
    kind: u8,
    size: u64,
    direct: [u32; DIRECT],
    indirect: u32,
    dindirect: u32,
    /// Triple-indirect table (0: none). Disks written before it existed
    /// have zeros there, and a file that needs none is stored as before.
    tindirect: u32,
}

impl Inode {
    fn new(kind: u8) -> Self {
        Inode { kind, ..Default::default() }
    }

    // layout: kind u8 @0 | pad | tindirect @4 | size u64 @8 | direct[10] @16 | indirect @56 | dindirect @60
    fn decode(b: &[u8]) -> Self {
        let mut direct = [0u32; DIRECT];
        for (i, d) in direct.iter_mut().enumerate() {
            *d = rd32(b, 16 + i * 4);
        }
        Inode {
            kind: b[0],
            size: rd64(b, 8),
            direct,
            indirect: rd32(b, 56),
            dindirect: rd32(b, 60),
            tindirect: rd32(b, 4),
        }
    }

    fn encode(&self, b: &mut [u8]) {
        b[..INODE_SIZE].fill(0);
        b[0] = self.kind;
        wr64(b, 8, self.size);
        for (i, d) in self.direct.iter().enumerate() {
            wr32(b, 16 + i * 4, *d);
        }
        wr32(b, 56, self.indirect);
        wr32(b, 60, self.dindirect);
        wr32(b, 4, self.tindirect);
    }

    fn file_kind(&self) -> Result<FileKind> {
        match self.kind {
            KIND_FILE => Ok(FileKind::File),
            KIND_DIR => Ok(FileKind::Directory),
            _ => Err(FsError::Corrupt),
        }
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Split a path into normalized components (`.` dropped, `..` resolved).
/// Paths are always interpreted from the root.
fn components(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Normalize a path to its absolute canonical form, e.g. `a/./b/../c` -> `/a/c`.
pub fn normalize(path: &str) -> String {
    let mut out = String::new();
    for c in components(path) {
        out.push('/');
        out.push_str(c);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('\0') {
        return Err(FsError::InvalidPath);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The filesystem
// ---------------------------------------------------------------------------

/// A mounted BeeFS filesystem on block device `D`.
pub struct FileSystem<D: BlockDevice> {
    dev: D,
    sb: Superblock,
    sb_dirty: bool,
    /// Bookkeeping blocks (bitmap, pointer tables) read or changed during
    /// the current operation: (block, content, changed). Written to the
    /// device once, when the operation ends (`commit`), instead of after
    /// every data block. Nothing else reads these blocks meanwhile.
    meta: Vec<(u64, Block, bool)>,
    /// The bitmap block where the last free block was found: the next
    /// search starts there.
    bitmap_hint: u32,
    /// The data block about to be allocated will be filled completely by
    /// its writer: it need not be zeroed first.
    whole_block: bool,
}

/// At most this many bookkeeping blocks are kept (bitmap, tables at two levels).
const META_CACHE: usize = 6;

impl<D: BlockDevice> FileSystem<D> {
    // ----- lifecycle --------------------------------------------------------

    /// Create a fresh, empty filesystem on `dev` (erases its metadata).
    pub fn format(mut dev: D) -> Result<Self> {
        let total = dev.block_count().min(u32::MAX as u64) as u32;
        let bitmap_blocks = total.div_ceil(BITS_PER_BLOCK);
        // One inode per 8 blocks (4 KiB of data), at least one inode block.
        let inode_blocks = (total / 8).div_ceil(INODES_PER_BLOCK).max(1);
        let inode_count = inode_blocks * INODES_PER_BLOCK;
        let data_start = 1 + bitmap_blocks + inode_blocks;
        if total < data_start + 8 {
            return Err(FsError::DeviceTooSmall);
        }

        // Bitmap: metadata blocks and the tail past the device end are "used".
        for i in 0..bitmap_blocks {
            let mut b = [0u8; BLOCK_SIZE];
            for bit in 0..BITS_PER_BLOCK {
                let block = i * BITS_PER_BLOCK + bit;
                if block < data_start || block >= total {
                    b[(bit / 8) as usize] |= 1 << (bit % 8);
                }
            }
            dev.write_block((1 + i) as u64, &b)?;
        }
        let zero = [0u8; BLOCK_SIZE];
        for i in 0..inode_blocks {
            dev.write_block((1 + bitmap_blocks + i) as u64, &zero)?;
        }

        let sb = Superblock {
            block_count: total,
            inode_count,
            bitmap_start: 1,
            bitmap_blocks,
            inode_start: 1 + bitmap_blocks,
            inode_blocks,
            data_start,
            free_blocks: total - data_start,
            free_inodes: inode_count - 1,
        };
        let mut fs = FileSystem { dev, sb, sb_dirty: true, meta: Vec::new(), bitmap_hint: 0, whole_block: false };
        fs.write_inode(ROOT_INODE, &Inode::new(KIND_DIR))?;
        fs.commit()?;
        Ok(fs)
    }

    /// Mount an existing filesystem.
    pub fn mount(mut dev: D) -> Result<Self> {
        let mut b = [0u8; BLOCK_SIZE];
        dev.read_block(0, &mut b)?;
        let sb = Superblock::decode(&b)?;
        if sb.block_count as u64 > dev.block_count() {
            return Err(FsError::Corrupt);
        }
        Ok(FileSystem { dev, sb, sb_dirty: false, meta: Vec::new(), bitmap_hint: 0, whole_block: false })
    }

    /// Does `dev` contain a BeeFS filesystem? (Reads block 0 only.)
    pub fn probe(dev: &mut D) -> Result<bool> {
        let mut b = [0u8; BLOCK_SIZE];
        dev.read_block(0, &mut b)?;
        Ok(b[0..8] == MAGIC)
    }

    /// Give the block device back.
    pub fn into_device(self) -> D {
        self.dev
    }

    pub fn device(&mut self) -> &mut D {
        &mut self.dev
    }

    pub fn stats(&self) -> FsStats {
        FsStats {
            block_size: BLOCK_SIZE as u32,
            total_blocks: self.sb.block_count,
            free_blocks: self.sb.free_blocks,
            total_inodes: self.sb.inode_count,
            free_inodes: self.sb.free_inodes,
        }
    }

    // ----- public file operations -------------------------------------------

    /// Create an empty file. Fails if it already exists.
    pub fn create(&mut self, path: &str) -> Result<()> {
        let r = self.create_node(path, KIND_FILE).map(|_| ());
        self.finish(r)
    }

    /// Create a directory. Fails if it already exists.
    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        let r = self.create_node(path, KIND_DIR).map(|_| ());
        self.finish(r)
    }

    /// Create every missing directory along `path` (like `mkdir -p`).
    pub fn mkdir_all(&mut self, path: &str) -> Result<()> {
        let comps = components(path);
        let mut r = Ok(());
        let mut cur = String::new();
        for c in comps {
            cur.push('/');
            cur.push_str(c);
            match self.create_node(&cur, KIND_DIR) {
                Ok(_) => {}
                Err(FsError::AlreadyExists) if self.is_dir(&cur) => {}
                Err(FsError::AlreadyExists) => {
                    r = Err(FsError::NotADirectory);
                    break;
                }
                Err(e) => {
                    r = Err(e);
                    break;
                }
            }
        }
        self.finish(r)
    }

    /// Replace the contents of a file (created if missing).
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let r = self.write_impl(path, data, false);
        self.finish(r)
    }

    /// Append to a file (created if missing).
    pub fn append(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let r = self.write_impl(path, data, true);
        self.finish(r)
    }

    /// Read a whole file.
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let ino = self.resolve(path)?;
        let mut inode = self.read_inode(ino)?;
        if inode.kind == KIND_DIR {
            return Err(FsError::IsADirectory);
        }
        if inode.size > MAX_WHOLE_READ {
            return Err(FsError::FileTooLarge); // too much for memory: read_at
        }
        let mut out = vec![0u8; inode.size as usize];
        self.read_range(&mut inode, 0, &mut out)?;
        Ok(out)
    }

    /// Read up to `buf.len()` bytes starting at `offset`. Returns bytes read.
    pub fn read_at(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let ino = self.resolve(path)?;
        let mut inode = self.read_inode(ino)?;
        if inode.kind == KIND_DIR {
            return Err(FsError::IsADirectory);
        }
        self.read_range(&mut inode, offset, buf)
    }

    /// Remove a file or an empty directory.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        let r = self.remove_impl(path);
        self.finish(r)
    }

    /// Move / rename a file or directory. The destination must not exist.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let r = self.rename_impl(from, to);
        self.finish(r)
    }

    /// List a directory (sorted by name).
    pub fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let dir = self.resolve(path)?;
        let raw = self.dir_entries(dir)?;
        let mut out = Vec::with_capacity(raw.len());
        for (_, ino, name) in raw {
            let inode = self.read_inode(ino)?;
            out.push(DirEntry { name, kind: inode.file_kind()?, size: inode.size, inode: ino });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn metadata(&mut self, path: &str) -> Result<Metadata> {
        let ino = self.resolve(path)?;
        let inode = self.read_inode(ino)?;
        Ok(Metadata { kind: inode.file_kind()?, size: inode.size, inode: ino })
    }

    pub fn exists(&mut self, path: &str) -> bool {
        self.resolve(path).is_ok()
    }

    pub fn is_dir(&mut self, path: &str) -> bool {
        matches!(self.metadata(path), Ok(m) if m.kind == FileKind::Directory)
    }

    // ----- operation implementations ----------------------------------------

    /// Write the superblock if counters changed and flush the device.
    /// The operation's own error wins over a commit error.
    fn finish<T>(&mut self, r: Result<T>) -> Result<T> {
        let c = self.commit();
        let v = r?;
        c?;
        Ok(v)
    }

    fn commit(&mut self) -> Result<()> {
        self.write_meta()?;
        if self.sb_dirty {
            let b = self.sb.encode();
            self.dev.write_block(0, &b)?;
            self.sb_dirty = false;
        }
        self.dev.flush()
    }

    fn create_node(&mut self, path: &str, kind: u8) -> Result<u32> {
        let (parent, name) = self.resolve_parent(path)?;
        if self.dir_find(parent, name)?.is_some() {
            return Err(FsError::AlreadyExists);
        }
        let ino = self.alloc_inode(kind)?;
        if let Err(e) = self.dir_insert(parent, name, ino, kind) {
            self.free_inode(ino)?;
            return Err(e);
        }
        Ok(ino)
    }

    fn write_impl(&mut self, path: &str, data: &[u8], append: bool) -> Result<()> {
        let ino = match self.resolve(path) {
            Ok(ino) => ino,
            Err(FsError::NotFound) => self.create_node(path, KIND_FILE)?,
            Err(e) => return Err(e),
        };
        let mut inode = self.read_inode(ino)?;
        if inode.kind == KIND_DIR {
            return Err(FsError::IsADirectory);
        }
        if !append {
            self.free_all_blocks(&mut inode)?;
        }
        let offset = inode.size;
        if offset + data.len() as u64 > MAX_FILE_SIZE {
            self.write_inode(ino, &inode)?;
            return Err(FsError::FileTooLarge);
        }
        // Always persist the inode, even on failure, so no blocks leak.
        let r = self.write_range(&mut inode, offset, data);
        self.write_inode(ino, &inode)?;
        r
    }

    fn remove_impl(&mut self, path: &str) -> Result<()> {
        let (parent, name) = self.resolve_parent(path)?;
        let (slot, ino) = self.dir_find(parent, name)?.ok_or(FsError::NotFound)?;
        let mut inode = self.read_inode(ino)?;
        if inode.kind == KIND_DIR && !self.dir_entries(ino)?.is_empty() {
            return Err(FsError::DirectoryNotEmpty);
        }
        self.dir_clear_slot(parent, slot)?;
        self.free_all_blocks(&mut inode)?;
        self.free_inode(ino)
    }

    fn rename_impl(&mut self, from: &str, to: &str) -> Result<()> {
        let from_c = components(from);
        let to_c = components(to);
        // A directory cannot be moved inside itself.
        if to_c.len() >= from_c.len() && to_c[..from_c.len()] == from_c[..] {
            return Err(FsError::InvalidOperation);
        }
        let (src_parent, src_name) = self.resolve_parent(from)?;
        let (slot, ino) = self.dir_find(src_parent, src_name)?.ok_or(FsError::NotFound)?;
        let (dst_parent, dst_name) = self.resolve_parent(to)?;
        if self.dir_find(dst_parent, dst_name)?.is_some() {
            return Err(FsError::AlreadyExists);
        }
        let kind = self.read_inode(ino)?.kind;
        // Insert first: inserting never moves existing slots, so `slot` stays valid.
        self.dir_insert(dst_parent, dst_name, ino, kind)?;
        self.dir_clear_slot(src_parent, slot)
    }

    // ----- path resolution --------------------------------------------------

    fn walk(&mut self, comps: &[&str]) -> Result<u32> {
        let mut ino = ROOT_INODE;
        for c in comps {
            if self.read_inode(ino)?.kind != KIND_DIR {
                return Err(FsError::NotADirectory);
            }
            ino = self.dir_find(ino, c)?.ok_or(FsError::NotFound)?.1;
        }
        Ok(ino)
    }

    fn resolve(&mut self, path: &str) -> Result<u32> {
        self.walk(&components(path))
    }

    /// Returns (parent directory inode, final name component).
    fn resolve_parent<'p>(&mut self, path: &'p str) -> Result<(u32, &'p str)> {
        let comps = components(path);
        let (name, dirs) = comps.split_last().ok_or(FsError::InvalidPath)?;
        check_name(name)?;
        let parent = self.walk(dirs)?;
        if self.read_inode(parent)?.kind != KIND_DIR {
            return Err(FsError::NotADirectory);
        }
        Ok((parent, name))
    }

    // ----- directories ------------------------------------------------------

    /// All used entries of a directory: (slot, inode, name).
    fn dir_entries(&mut self, dir: u32) -> Result<Vec<(usize, u32, String)>> {
        let mut inode = self.read_inode(dir)?;
        if inode.kind != KIND_DIR {
            return Err(FsError::NotADirectory);
        }
        let mut data = vec![0u8; inode.size as usize];
        self.read_range(&mut inode, 0, &mut data)?;
        let mut out = Vec::new();
        for (slot, e) in data.chunks_exact(DIRENT_SIZE).enumerate() {
            let ino = rd32(e, 0);
            if ino == 0 {
                continue;
            }
            let len = (e[5] as usize).min(MAX_NAME_LEN);
            let name = core::str::from_utf8(&e[6..6 + len]).map_err(|_| FsError::Corrupt)?;
            out.push((slot, ino, String::from(name)));
        }
        Ok(out)
    }

    fn dir_find(&mut self, dir: u32, name: &str) -> Result<Option<(usize, u32)>> {
        Ok(self
            .dir_entries(dir)?
            .into_iter()
            .find(|(_, _, n)| n == name)
            .map(|(slot, ino, _)| (slot, ino)))
    }

    // entry layout: inode u32 @0 | kind u8 @4 | name_len u8 @5 | name[26] @6
    fn dir_insert(&mut self, dir: u32, name: &str, ino: u32, kind: u8) -> Result<()> {
        check_name(name)?;
        let mut inode = self.read_inode(dir)?;
        let mut data = vec![0u8; inode.size as usize];
        self.read_range(&mut inode, 0, &mut data)?;
        let slot = data
            .chunks_exact(DIRENT_SIZE)
            .position(|e| rd32(e, 0) == 0)
            .unwrap_or(data.len() / DIRENT_SIZE);

        let mut e = [0u8; DIRENT_SIZE];
        wr32(&mut e, 0, ino);
        e[4] = kind;
        e[5] = name.len() as u8;
        e[6..6 + name.len()].copy_from_slice(name.as_bytes());

        let r = self.write_range(&mut inode, (slot * DIRENT_SIZE) as u64, &e);
        self.write_inode(dir, &inode)?;
        r
    }

    fn dir_clear_slot(&mut self, dir: u32, slot: usize) -> Result<()> {
        let mut inode = self.read_inode(dir)?;
        let r = self.write_range(&mut inode, (slot * DIRENT_SIZE) as u64, &[0u8; DIRENT_SIZE]);
        self.write_inode(dir, &inode)?;
        r
    }

    // ----- file data --------------------------------------------------------

    fn read_range(&mut self, inode: &mut Inode, offset: u64, out: &mut [u8]) -> Result<usize> {
        if offset >= inode.size {
            return Ok(0);
        }
        let len = out.len().min((inode.size - offset) as usize);
        let mut buf = [0u8; BLOCK_SIZE];
        let mut done = 0;
        while done < len {
            let pos = offset + done as u64;
            let block_off = (pos % BLOCK_SIZE as u64) as usize;
            let n = (BLOCK_SIZE - block_off).min(len - done);
            let block = self.map_block(inode, pos / BLOCK_SIZE as u64, false)?;
            if block == 0 {
                out[done..done + n].fill(0); // sparse hole
            } else {
                self.dev.read_block(block as u64, &mut buf)?;
                out[done..done + n].copy_from_slice(&buf[block_off..block_off + n]);
            }
            done += n;
        }
        Ok(len)
    }

    /// Write `data` at `offset`, allocating blocks as needed. Caller persists the inode.
    fn write_range(&mut self, inode: &mut Inode, offset: u64, data: &[u8]) -> Result<()> {
        let mut buf = [0u8; BLOCK_SIZE];
        let mut done = 0;
        while done < data.len() {
            let pos = offset + done as u64;
            let block_off = (pos % BLOCK_SIZE as u64) as usize;
            let n = (BLOCK_SIZE - block_off).min(data.len() - done);
            // A data block that this write fills completely need not be
            // zeroed first (pointer tables allocated on the way always are).
            self.whole_block = n == BLOCK_SIZE;
            let block = self.map_block(inode, pos / BLOCK_SIZE as u64, true);
            self.whole_block = false;
            let block = block?;
            if n < BLOCK_SIZE {
                self.dev.read_block(block as u64, &mut buf)?;
            }
            buf[block_off..block_off + n].copy_from_slice(&data[done..done + n]);
            self.dev.write_block(block as u64, &buf)?;
            done += n;
            inode.size = inode.size.max(pos + n as u64);
        }
        Ok(())
    }

    /// Physical block for file block `idx` (0 = not allocated, if `!alloc`).
    fn map_block(&mut self, inode: &mut Inode, idx: u64, alloc: bool) -> Result<u32> {
        let p = PTRS_PER_BLOCK;
        if idx < DIRECT as u64 {
            return self.ensure(&mut inode.direct[idx as usize], alloc, false);
        }
        let idx = idx - DIRECT as u64;
        if idx < p {
            let table = self.ensure(&mut inode.indirect, alloc, true)?;
            if table == 0 {
                return Ok(0);
            }
            return self.table_entry(table, idx as usize, alloc, false);
        }
        let idx = idx - p;
        if idx < p * p {
            let top = self.ensure(&mut inode.dindirect, alloc, true)?;
            if top == 0 {
                return Ok(0);
            }
            let mid = self.table_entry(top, (idx / p) as usize, alloc, true)?;
            if mid == 0 {
                return Ok(0);
            }
            return self.table_entry(mid, (idx % p) as usize, alloc, false);
        }
        let idx = idx - p * p;
        if idx < p * p * p {
            let top = self.ensure(&mut inode.tindirect, alloc, true)?;
            if top == 0 {
                return Ok(0);
            }
            let upper = self.table_entry(top, (idx / (p * p)) as usize, alloc, true)?;
            if upper == 0 {
                return Ok(0);
            }
            let mid = self.table_entry(upper, (idx / p % p) as usize, alloc, true)?;
            if mid == 0 {
                return Ok(0);
            }
            return self.table_entry(mid, (idx % p) as usize, alloc, false);
        }
        Err(FsError::FileTooLarge)
    }

    /// The block in `slot`, allocated if it is 0 and `alloc`. `table`: the
    /// new block is a pointer table, which is always zeroed.
    fn ensure(&mut self, slot: &mut u32, alloc: bool, table: bool) -> Result<u32> {
        if *slot == 0 && alloc {
            *slot = if table { self.alloc_table()? } else { self.alloc_block()? };
        }
        Ok(*slot)
    }

    /// A new pointer table: zeroed, whatever the data block after it needs.
    fn alloc_table(&mut self) -> Result<u32> {
        let whole = core::mem::replace(&mut self.whole_block, false);
        let r = self.alloc_block();
        self.whole_block = whole;
        r
    }

    fn table_entry(&mut self, table: u32, i: usize, alloc: bool, new_is_table: bool) -> Result<u32> {
        let mut buf = self.meta_read(table as u64)?;
        let v = rd32(&buf, i * 4);
        if v != 0 || !alloc {
            return Ok(v);
        }
        let new = if new_is_table { self.alloc_table()? } else { self.alloc_block()? };
        wr32(&mut buf, i * 4, new);
        self.meta_write(table as u64, &buf)?;
        Ok(new)
    }

    /// Release every data block of an inode and set its size to 0.
    fn free_all_blocks(&mut self, inode: &mut Inode) -> Result<()> {
        for i in 0..DIRECT {
            if inode.direct[i] != 0 {
                self.free_block(inode.direct[i])?;
            }
        }
        if inode.indirect != 0 {
            self.free_table(inode.indirect, 1)?;
        }
        if inode.dindirect != 0 {
            self.free_table(inode.dindirect, 2)?;
        }
        if inode.tindirect != 0 {
            self.free_table(inode.tindirect, 3)?;
        }
        *inode = Inode::new(inode.kind);
        Ok(())
    }

    /// Free a pointer table, its children (`depth` levels), and itself.
    fn free_table(&mut self, table: u32, depth: u32) -> Result<()> {
        let buf = self.meta_read(table as u64)?;
        for i in 0..PTRS_PER_BLOCK as usize {
            let b = rd32(&buf, i * 4);
            if b == 0 {
                continue;
            }
            if depth > 1 {
                self.free_table(b, depth - 1)?;
            } else {
                self.free_block(b)?;
            }
        }
        self.free_block(table)
    }

    // ----- bookkeeping blocks, kept during an operation -----------------------

    fn meta_read(&mut self, block: u64) -> Result<Block> {
        if let Some(at) = self.meta.iter().position(|(b, _, _)| *b == block) {
            // The most recently used stays at the end.
            let entry = self.meta.remove(at);
            let content = entry.1;
            self.meta.push(entry);
            return Ok(content);
        }
        let mut buf = [0u8; BLOCK_SIZE];
        self.dev.read_block(block, &mut buf)?;
        self.meta_keep(block, buf, false)?;
        Ok(buf)
    }

    fn meta_write(&mut self, block: u64, buf: &Block) -> Result<()> {
        self.meta.retain(|(b, _, _)| *b != block);
        self.meta_keep(block, *buf, true)
    }

    fn meta_keep(&mut self, block: u64, buf: Block, changed: bool) -> Result<()> {
        if self.meta.len() >= META_CACHE {
            let (old, content, old_changed) = self.meta.remove(0);
            if old_changed {
                self.dev.write_block(old, &content)?;
            }
        }
        self.meta.push((block, buf, changed));
        Ok(())
    }

    /// A block was freed or handed out anew: what was kept of it is stale.
    fn meta_forget(&mut self, block: u64) {
        self.meta.retain(|(b, _, _)| *b != block);
    }

    fn write_meta(&mut self) -> Result<()> {
        for (block, content, changed) in core::mem::take(&mut self.meta) {
            if changed {
                self.dev.write_block(block, &content)?;
            }
        }
        Ok(())
    }

    // ----- block allocation ---------------------------------------------------

    fn alloc_block(&mut self) -> Result<u32> {
        if self.sb.free_blocks == 0 {
            return Err(FsError::NoSpace);
        }
        // From where the last free block was found, round to the start.
        let count = self.sb.bitmap_blocks;
        for n in 0..count {
            let i = (self.bitmap_hint + n) % count;
            let bm = (self.sb.bitmap_start + i) as u64;
            let mut buf = self.meta_read(bm)?;
            let Some(byte) = buf.iter().position(|&b| b != 0xFF) else { continue };
            let bit = (!buf[byte]).trailing_zeros();
            let block = i * BITS_PER_BLOCK + byte as u32 * 8 + bit;
            if block < self.sb.data_start || block >= self.sb.block_count {
                return Err(FsError::Corrupt);
            }
            buf[byte] |= 1 << bit;
            self.meta_write(bm, &buf)?;
            self.bitmap_hint = i;
            self.sb.free_blocks -= 1;
            self.sb_dirty = true;
            self.meta_forget(block as u64);
            if !core::mem::take(&mut self.whole_block) {
                self.dev.write_block(block as u64, &[0u8; BLOCK_SIZE])?;
            }
            return Ok(block);
        }
        Err(FsError::NoSpace)
    }

    fn free_block(&mut self, block: u32) -> Result<()> {
        if block < self.sb.data_start || block >= self.sb.block_count {
            return Err(FsError::Corrupt);
        }
        let bm = (self.sb.bitmap_start + block / BITS_PER_BLOCK) as u64;
        let bit = block % BITS_PER_BLOCK;
        self.meta_forget(block as u64);
        let mut buf = self.meta_read(bm)?;
        let mask = 1u8 << (bit % 8);
        if buf[(bit / 8) as usize] & mask != 0 {
            buf[(bit / 8) as usize] &= !mask;
            self.meta_write(bm, &buf)?;
            self.sb.free_blocks += 1;
            self.sb_dirty = true;
        }
        Ok(())
    }

    // ----- inode table --------------------------------------------------------

    fn inode_location(&self, ino: u32) -> Result<(u64, usize)> {
        if ino == 0 || ino > self.sb.inode_count {
            return Err(FsError::Corrupt);
        }
        let idx = ino - 1;
        let block = self.sb.inode_start + idx / INODES_PER_BLOCK;
        Ok((block as u64, (idx % INODES_PER_BLOCK) as usize * INODE_SIZE))
    }

    fn read_inode(&mut self, ino: u32) -> Result<Inode> {
        let (block, off) = self.inode_location(ino)?;
        let mut buf = [0u8; BLOCK_SIZE];
        self.dev.read_block(block, &mut buf)?;
        Ok(Inode::decode(&buf[off..off + INODE_SIZE]))
    }

    fn write_inode(&mut self, ino: u32, inode: &Inode) -> Result<()> {
        // What the inode points to (tables, the bitmap) reaches the disk
        // first, as before the bookkeeping blocks were kept: a crash in
        // between never leaves an inode pointing at tables not written yet.
        self.write_meta()?;
        let (block, off) = self.inode_location(ino)?;
        let mut buf = [0u8; BLOCK_SIZE];
        self.dev.read_block(block, &mut buf)?;
        inode.encode(&mut buf[off..off + INODE_SIZE]);
        self.dev.write_block(block, &buf)
    }

    fn alloc_inode(&mut self, kind: u8) -> Result<u32> {
        if self.sb.free_inodes == 0 {
            return Err(FsError::NoInodes);
        }
        let mut buf = [0u8; BLOCK_SIZE];
        for i in 0..self.sb.inode_blocks {
            let block = (self.sb.inode_start + i) as u64;
            self.dev.read_block(block, &mut buf)?;
            for slot in 0..INODES_PER_BLOCK as usize {
                if buf[slot * INODE_SIZE] != KIND_FREE {
                    continue;
                }
                let ino = i * INODES_PER_BLOCK + slot as u32 + 1;
                Inode::new(kind).encode(&mut buf[slot * INODE_SIZE..]);
                self.dev.write_block(block, &buf)?;
                self.sb.free_inodes -= 1;
                self.sb_dirty = true;
                return Ok(ino);
            }
        }
        Err(FsError::NoInodes)
    }

    fn free_inode(&mut self, ino: u32) -> Result<()> {
        self.write_inode(ino, &Inode::new(KIND_FREE))?;
        self.sb.free_inodes += 1;
        self.sb_dirty = true;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests (run on the host: `cargo test` in beefs/)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RamDisk;

    fn fresh() -> FileSystem<RamDisk> {
        FileSystem::format(RamDisk::new(4 * 1024 * 1024)).unwrap()
    }

    #[test]
    fn format_and_mount() {
        let fs = fresh();
        let stats = fs.stats();
        let dev = fs.into_device();
        let mut fs = FileSystem::mount(dev).unwrap();
        assert_eq!(fs.stats(), stats);
        assert!(fs.read_dir("/").unwrap().is_empty());
    }

    #[test]
    fn mount_rejects_unformatted() {
        let mut dev = RamDisk::new(64 * 1024);
        assert!(!FileSystem::probe(&mut dev).unwrap());
        assert_eq!(FileSystem::mount(dev).err(), Some(FsError::BadMagic));
    }

    #[test]
    fn too_small() {
        assert_eq!(FileSystem::format(RamDisk::new(2048)).err(), Some(FsError::DeviceTooSmall));
    }

    #[test]
    fn write_read_append() {
        let mut fs = fresh();
        fs.write_file("/hello.txt", b"Hello").unwrap();
        fs.append("/hello.txt", b", BeeFS!").unwrap();
        assert_eq!(fs.read_file("/hello.txt").unwrap(), b"Hello, BeeFS!");
        fs.write_file("/hello.txt", b"new").unwrap();
        assert_eq!(fs.read_file("/hello.txt").unwrap(), b"new");
        let mut buf = [0u8; 2];
        assert_eq!(fs.read_at("/hello.txt", 1, &mut buf).unwrap(), 2);
        assert_eq!(&buf, b"ew");
    }

    #[test]
    fn directories_and_paths() {
        let mut fs = fresh();
        fs.mkdir("/a").unwrap();
        fs.mkdir_all("/a/b/c").unwrap();
        fs.write_file("/a/b/c/../file", b"x").unwrap();
        assert!(fs.exists("/a/b/file"));
        assert!(fs.is_dir("a/b/c"));
        let names: Vec<_> = fs.read_dir("/a/b").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["c", "file"]);
        assert_eq!(fs.mkdir("/a").err(), Some(FsError::AlreadyExists));
        assert_eq!(fs.read_file("/a").err(), Some(FsError::IsADirectory));
        assert_eq!(fs.read_dir("/a/b/file").err(), Some(FsError::NotADirectory));
        assert_eq!(fs.create("/a/b/file/x").err(), Some(FsError::NotADirectory));
        assert_eq!(fs.read_file("/nope").err(), Some(FsError::NotFound));
        assert_eq!(fs.mkdir("/").err(), Some(FsError::InvalidPath));
        let long = "/".to_string() + &"n".repeat(MAX_NAME_LEN + 1);
        assert_eq!(fs.create(&long).err(), Some(FsError::NameTooLong));
    }

    #[test]
    fn remove_and_reclaim() {
        let mut fs = fresh();
        let before = fs.stats();
        fs.mkdir("/d").unwrap();
        fs.write_file("/d/big", &vec![7u8; 300_000]).unwrap();
        assert_eq!(fs.remove("/d").err(), Some(FsError::DirectoryNotEmpty));
        fs.remove("/d/big").unwrap();
        fs.remove("/d").unwrap();
        assert!(!fs.exists("/d"));
        // Root keeps its (now empty) directory data block.
        let after = fs.stats();
        assert_eq!(after.free_inodes, before.free_inodes);
        assert_eq!(after.free_blocks, before.free_blocks - 1);
    }

    #[test]
    fn large_file_uses_double_indirect() {
        let mut fs = fresh();
        // > (10 + 128) blocks, so it needs the double-indirect table.
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 31 % 251) as u8).collect();
        fs.write_file("/big.bin", &data).unwrap();
        let mut fs = FileSystem::mount(fs.into_device()).unwrap();
        assert_eq!(fs.read_file("/big.bin").unwrap(), data);
        assert_eq!(fs.metadata("/big.bin").unwrap().size, 200_000);
    }

    #[test]
    fn very_large_file_uses_triple_indirect() {
        // Past 10 + 128 + 128 * 128 blocks (about 8.3 MiB) the triple-indirect table is needed.
        let old_limit = (DIRECT as u64 + PTRS_PER_BLOCK + PTRS_PER_BLOCK * PTRS_PER_BLOCK) * BLOCK_SIZE as u64;
        let mut fs = FileSystem::format(RamDisk::new(24 * 1024 * 1024)).unwrap();
        // The root folder gets its block of entries with its first file, and keeps it.
        fs.write_file("/first.txt", b"x").unwrap();
        fs.remove("/first.txt").unwrap();
        let free_at_start = fs.stats().free_blocks;
        let byte = |i: u64| (i * 31 % 251) as u8;
        // Written in pieces, as a download or an upload arrives.
        let piece = 300_000u64;
        let total = old_limit + 3 * piece + 12_345;
        let mut written = 0;
        while written < total {
            let n = piece.min(total - written);
            let data: Vec<u8> = (written..written + n).map(byte).collect();
            fs.append("/huge.bin", &data).unwrap();
            written += n;
        }
        let mut fs = FileSystem::mount(fs.into_device()).unwrap();
        assert_eq!(fs.metadata("/huge.bin").unwrap().size, total);
        // Whole, and in pieces around where the old limit was and at the end.
        let whole = fs.read_file("/huge.bin").unwrap();
        assert!(whole.iter().enumerate().all(|(i, b)| *b == byte(i as u64)));
        for at in [0, old_limit - 700, old_limit - 1, old_limit, old_limit + 511, total - 1000] {
            let mut buf = [0u8; 1000];
            let n = fs.read_at("/huge.bin", at, &mut buf).unwrap();
            assert_eq!(n as u64, 1000.min(total - at));
            assert!(buf[..n].iter().enumerate().all(|(i, b)| *b == byte(at + i as u64)), "at {at}");
        }
        let mut buf = [0u8; 10];
        assert_eq!(fs.read_at("/huge.bin", total, &mut buf).unwrap(), 0);
        // Written over, it shrinks; removed, every block (tables too) is free again.
        fs.write_file("/huge.bin", b"small now").unwrap();
        assert_eq!(fs.read_file("/huge.bin").unwrap(), b"small now");
        fs.remove("/huge.bin").unwrap();
        assert_eq!(fs.stats().free_blocks, free_at_start);
    }

    #[test]
    fn a_file_too_large_for_memory_is_read_in_pieces() {
        let mut fs = FileSystem::format(RamDisk::new(24 * 1024 * 1024)).unwrap();
        let chunk = vec![7u8; 1024 * 1024];
        for _ in 0..17 {
            fs.append("/video.avi", &chunk).unwrap();
        }
        assert_eq!(fs.metadata("/video.avi").unwrap().size, 17 * 1024 * 1024);
        assert_eq!(fs.read_file("/video.avi").err(), Some(FsError::FileTooLarge));
        let mut buf = [0u8; 4096];
        assert_eq!(fs.read_at("/video.avi", MAX_WHOLE_READ + 5, &mut buf).unwrap(), 4096);
        assert!(buf.iter().all(|b| *b == 7));
        // The disk is full before the file's own limit: said so.
        let mut result = Ok(());
        for _ in 0..8 {
            result = fs.append("/video.avi", &chunk);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result.err(), Some(FsError::NoSpace));
        let free = fs.stats().free_blocks;
        fs.remove("/video.avi").unwrap();
        assert!(fs.stats().free_blocks > free + 17 * 2048);
    }

    #[test]
    fn inodes_of_older_disks_read_and_write_as_before() {
        // An inode as kernels before the triple-indirect table wrote it:
        // kind, seven zero bytes, size, ten direct pointers, single, double.
        let mut old = [0u8; INODE_SIZE];
        old[0] = KIND_FILE;
        wr64(&mut old, 8, 123_456);
        for i in 0..DIRECT {
            wr32(&mut old, 16 + i * 4, 1000 + i as u32);
        }
        wr32(&mut old, 56, 2000);
        wr32(&mut old, 60, 3000);
        let inode = Inode::decode(&old);
        assert_eq!((inode.size, inode.direct[9], inode.indirect, inode.dindirect, inode.tindirect), (123_456, 1009, 2000, 3000, 0));
        // Written back it is the same 64 bytes: an older kernel reads it too.
        let mut again = [0xFFu8; INODE_SIZE];
        inode.encode(&mut again);
        assert_eq!(again, old);
        // A file that needs the new table keeps it at bytes 4..8.
        let big = Inode { tindirect: 4000, ..inode };
        big.encode(&mut again);
        assert_eq!(rd32(&again, 4), 4000);
        assert_eq!(Inode::decode(&again).tindirect, 4000);
    }

    /// A RamDisk that counts what is done to it.
    struct Counting {
        disk: RamDisk,
        reads: u64,
        writes: u64,
    }

    impl BlockDevice for Counting {
        fn block_count(&self) -> u64 {
            self.disk.block_count()
        }
        fn read_block(&mut self, i: u64, b: &mut Block) -> Result<()> {
            self.reads += 1;
            self.disk.read_block(i, b)
        }
        fn write_block(&mut self, i: u64, b: &Block) -> Result<()> {
            self.writes += 1;
            self.disk.write_block(i, b)
        }
    }

    #[test]
    fn large_writes_do_little_bookkeeping() {
        let dev = Counting { disk: RamDisk::new(4 * 1024 * 1024), reads: 0, writes: 0 };
        let mut fs = FileSystem::format(dev).unwrap();
        let data: Vec<u8> = (0..1_440_054u32).map(|i| (i * 7 % 253) as u8).collect();
        let blocks = data.len().div_ceil(BLOCK_SIZE) as u64;
        let (r0, w0) = (fs.device().reads, fs.device().writes);
        fs.write_file("/paper.bmp", &data).unwrap();
        let (reads, writes) = (fs.device().reads - r0, fs.device().writes - w0);
        // Each data block once, plus a little bookkeeping (was: 4 writes
        // and 3 reads per block).
        assert!(writes <= blocks + blocks / 20, "{writes} writes for {blocks} blocks");
        assert!(reads <= blocks / 20, "{reads} reads for {blocks} blocks");
        // Everything is on the device: a fresh mount reads it back.
        let mut fs = FileSystem::mount(fs.into_device().disk).unwrap();
        assert_eq!(fs.read_file("/paper.bmp").unwrap(), data);
    }

    #[test]
    fn rewrite_reuses_freed_tables_as_data() {
        // Writing a file again frees its blocks (pointer tables too) and
        // allocates new ones in the same operation: a freed table that comes
        // back as data must hold the data, not the old table.
        let mut fs = fresh();
        let first: Vec<u8> = (0..400_000u32).map(|i| (i % 249) as u8).collect();
        let second: Vec<u8> = (0..390_000u32).map(|i| (i * 13 % 241) as u8).collect();
        fs.write_file("/f", &first).unwrap();
        let free = fs.stats().free_blocks;
        fs.write_file("/f", &second).unwrap();
        assert_eq!(fs.read_file("/f").unwrap(), second);
        fs.write_file("/f", b"small").unwrap();
        fs.append("/f", &first[..1000]).unwrap();
        let mut fs = FileSystem::mount(fs.into_device()).unwrap();
        let mut expected = b"small".to_vec();
        expected.extend_from_slice(&first[..1000]);
        assert_eq!(fs.read_file("/f").unwrap(), expected);
        // Writing the big file again leaves as many blocks free as before.
        fs.write_file("/f", &first).unwrap();
        assert_eq!(fs.stats().free_blocks, free);
        // A partial block written to a fresh block reads back zeros after it.
        fs.write_file("/g", &[9u8; 100]).unwrap();
        fs.append("/g", &[8u8; 10]).unwrap();
        let g = fs.read_file("/g").unwrap();
        assert_eq!(g.len(), 110);
        assert!(g[..100].iter().all(|b| *b == 9) && g[100..].iter().all(|b| *b == 8));
    }

    #[test]
    fn rename_and_move() {
        let mut fs = fresh();
        fs.mkdir_all("/x/y").unwrap();
        fs.write_file("/x/f", b"data").unwrap();
        fs.rename("/x/f", "/x/y/g").unwrap();
        assert_eq!(fs.read_file("/x/y/g").unwrap(), b"data");
        assert!(!fs.exists("/x/f"));
        fs.rename("/x/y", "/z").unwrap();
        assert_eq!(fs.read_file("/z/g").unwrap(), b"data");
        assert_eq!(fs.rename("/z", "/z/inner").err(), Some(FsError::InvalidOperation));
        fs.write_file("/w", b"").unwrap();
        assert_eq!(fs.rename("/w", "/z/g").err(), Some(FsError::AlreadyExists));
    }

    #[test]
    fn out_of_space() {
        let mut fs = FileSystem::format(RamDisk::new(64 * 1024)).unwrap();
        assert_eq!(fs.write_file("/f", &vec![1u8; 100_000]).err(), Some(FsError::NoSpace));
        // Filesystem is still usable and consistent after the failure.
        fs.remove("/f").unwrap();
        fs.write_file("/g", b"ok").unwrap();
        assert_eq!(fs.read_file("/g").unwrap(), b"ok");
    }

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize(""), "/");
        assert_eq!(normalize("a/./b/../c/"), "/a/c");
        assert_eq!(normalize("/../.."), "/");
    }

    #[test]
    fn slots_are_reused() {
        let mut fs = fresh();
        for i in 0..20 {
            fs.create(&format!("/f{i}")).unwrap();
        }
        let size = fs.metadata("/").unwrap().size;
        for i in 0..20 {
            fs.remove(&format!("/f{i}")).unwrap();
        }
        for i in 0..20 {
            fs.create(&format!("/g{i}")).unwrap();
        }
        assert_eq!(fs.metadata("/").unwrap().size, size);
        assert_eq!(fs.read_dir("/").unwrap().len(), 20);
    }
}
