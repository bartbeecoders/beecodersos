//! Boot-time storage: mount the first disk (IDE or SATA) that holds BeeFS,
//! otherwise fall back to a freshly formatted RAM disk. A disk that is not
//! BeeFS is only read, never written.
//!
//! An installed disk (install.rs) has two BeeFS partitions: "os", mounted as
//! `/`, and "data", mounted at `/data`. `Mounts` sends each path to the right
//! one; everything else in the kernel keeps using plain paths.

use crate::crash;
use crate::disks::{self, DiskRef};
use crate::ring::Global;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::Ordering;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beefs::{mbr_partitions, Block, BlockDevice, DirEntry, FileSystem, FsError, FsStats, Metadata, Partition, RamDisk};

pub type Fs = FileSystem<Box<dyn BlockDevice>>;

const RAM_DISK_SIZE: usize = 4 * 1024 * 1024;

/// The mounted file system. Threads take turns: none switches while it
/// holds an `FsGuard` (see thread.rs).
static FS: Global<Option<Mounts>> = Global::new(None);

/// Where the data partition appears.
pub const DATA_DIR: &str = "/data";

/// The mounted file systems: `/` and, on an installed disk, `/data`.
/// File operations take a path and go to the file system it belongs to.
/// Used directly (`&mut Fs`), it is the root file system: that is where the
/// system's own files live (`/system`).
pub struct Mounts {
    root: Fs,
    data: Option<Fs>,
}

impl Deref for Mounts {
    type Target = Fs;
    fn deref(&self) -> &Fs {
        &self.root
    }
}

impl DerefMut for Mounts {
    fn deref_mut(&mut self) -> &mut Fs {
        &mut self.root
    }
}

/// Which file system a path is on, and its path there. `on_data`: is there
/// a data partition. Paths are cleaned first ("/data/../x" is on the root).
pub fn route(path: &str, has_data: bool) -> (bool, String) {
    let path = beefs::normalize(path);
    if has_data {
        if path == DATA_DIR {
            return (true, String::from("/"));
        }
        if let Some(rest) = path.strip_prefix(DATA_DIR).filter(|r| r.starts_with('/')) {
            return (true, String::from(rest));
        }
    }
    (false, path)
}

impl Mounts {
    fn new(root: Fs) -> Mounts {
        Mounts { root, data: None }
    }

    fn at(&mut self, path: &str) -> (&mut Fs, String) {
        match (route(path, self.data.is_some()), self.data.as_mut()) {
            ((true, path), Some(data)) => (data, path),
            ((_, path), _) => (&mut self.root, path),
        }
    }

    pub fn has_data(&self) -> bool {
        self.data.is_some()
    }

    /// Space on the data partition, if there is one (`stats` is the root's).
    pub fn data_stats(&self) -> Option<FsStats> {
        self.data.as_ref().map(|fs| fs.stats())
    }

    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, FsError> {
        let (fs, path) = self.at(path);
        fs.read_file(&path)
    }

    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let (fs, path) = self.at(path);
        fs.write_file(&path, data)
    }

    pub fn append(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let (fs, path) = self.at(path);
        fs.append(&path, data)
    }

    pub fn create(&mut self, path: &str) -> Result<(), FsError> {
        let (fs, path) = self.at(path);
        fs.create(&path)
    }

    pub fn mkdir_all(&mut self, path: &str) -> Result<(), FsError> {
        let (fs, path) = self.at(path);
        fs.mkdir_all(&path)
    }

    pub fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let (fs, path) = self.at(path);
        fs.read_dir(&path)
    }

    pub fn metadata(&mut self, path: &str) -> Result<Metadata, FsError> {
        let (fs, path) = self.at(path);
        fs.metadata(&path)
    }

    pub fn exists(&mut self, path: &str) -> bool {
        let (fs, path) = self.at(path);
        fs.exists(&path)
    }

    pub fn is_dir(&mut self, path: &str) -> bool {
        let (fs, path) = self.at(path);
        fs.is_dir(&path)
    }

    /// The place where the data partition is mounted cannot be removed.
    pub fn remove(&mut self, path: &str) -> Result<(), FsError> {
        if self.data.is_some() && beefs::normalize(path) == DATA_DIR {
            return Err(FsError::DirectoryNotEmpty);
        }
        let (fs, path) = self.at(path);
        fs.remove(&path)
    }

    /// Within one file system: a rename. From one to the other a file is
    /// copied and then removed; directories cannot cross.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        let has_data = self.data.is_some();
        let ((from_data, from_path), (to_data, to_path)) = (route(from, has_data), route(to, has_data));
        if has_data && (from_path == "/" && from_data || beefs::normalize(from) == DATA_DIR) {
            return Err(FsError::InvalidPath);
        }
        if from_data == to_data {
            return self.at(from).0.rename(&from_path, &to_path);
        }
        if self.at(from).0.is_dir(&from_path) {
            return Err(FsError::IsADirectory);
        }
        if self.at(to).0.exists(&to_path) {
            return Err(FsError::AlreadyExists);
        }
        let content = self.at(from).0.read_file(&from_path)?;
        self.at(to).0.write_file(&to_path, &content)?;
        self.at(from).0.remove(&from_path)
    }
}
static HANDLE: Global<Option<Storage>> = Global::new(None);
/// Which disk (disks.rs) the file system is on; `None` on the RAM disk.
static MOUNTED: Global<Option<usize>> = Global::new(None);

/// A handle to the mounted file system; every shell has one.
#[derive(Clone)]
pub struct Storage {
    /// True when backed by a disk (changes survive a reboot).
    pub persistent: bool,
    /// Boot number from /system/boot_count (0 on a RAM disk).
    pub boot: u32,
}

impl Storage {
    /// Borrow the filesystem. While the guard lives, a crash won't write its
    /// report to disk (see `crash::record`).
    pub fn fs(&mut self) -> FsGuard<'_> {
        crash::FS_BUSY.fetch_add(1, Ordering::SeqCst);
        FsGuard(FS.get().as_mut().expect("no file system is mounted"))
    }
}

pub struct FsGuard<'a>(&'a mut Mounts);

impl Drop for FsGuard<'_> {
    fn drop(&mut self) {
        crash::FS_BUSY.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Deref for FsGuard<'_> {
    type Target = Mounts;
    fn deref(&self) -> &Mounts {
        self.0
    }
}

impl DerefMut for FsGuard<'_> {
    fn deref_mut(&mut self) -> &mut Mounts {
        self.0
    }
}

/// A handle to the mounted file system, once `mount` has run.
pub fn handle() -> Option<Storage> {
    HANDLE.get().clone()
}

pub fn mount() -> Storage {
    let storage = mount_any();
    *HANDLE.get() = Some(storage.clone());
    storage
}

fn mount_any() -> Storage {
    // Every disk we have a driver for (disks.rs). The first one that holds
    // BeeFS is mounted. The others are only read.
    if disks::count() == 0 {
        println!("[disk] no disk found (IDE or SATA)");
    }
    for index in 0..disks::count() {
        let mut desc = disks::get(index).map(|d| d.description.clone()).unwrap_or_default();
        let dev = match open_beefs_device(DiskRef(index)) {
            Ok((dev, true)) => {
                desc.push_str(", partition");
                dev
            }
            Ok((dev, false)) => dev,
            Err(e) => {
                println!("[disk] {desc}: error: {e}");
                continue;
            }
        };
        match mount_disk(dev) {
            Ok(Some((fs, boot))) => {
                println!("[disk] {desc}: BeeFS mounted (boot #{boot})");
                crash::PERSISTENT.store(true, Ordering::SeqCst);
                let mut mounts = Mounts::new(fs);
                match mount_data(index, &mut mounts) {
                    Ok(Some(mib)) => println!("[disk] data partition, {mib} MiB: mounted at {DATA_DIR}"),
                    Ok(None) => {}
                    Err(e) => println!("[disk] the data partition could not be mounted: {e}"),
                }
                *FS.get() = Some(mounts);
                *MOUNTED.get() = Some(index);
                return Storage { persistent: true, boot };
            }
            Ok(None) => println!("[disk] {desc}: not BeeFS, left untouched ('install' can put the OS on it)"),
            Err(e) => println!("[disk] {desc}: error: {e}"),
        }
    }
    ram_disk()
}

/// The data partition of an installed disk (install.rs), if this disk has
/// one: mount it at /data. Returns its size in MiB.
fn mount_data(index: usize, mounts: &mut Mounts) -> Result<Option<u64>, FsError> {
    let mut disk = DiskRef(index);
    let mut sector: Block = [0; 512];
    disk.read_block(0, &mut sector)?;
    let found = mbr_partitions(&sector).and_then(|p| p.into_iter().find(|p| p.0 == crate::install::DATA_PARTITION_TYPE && p.1 > 0));
    let Some((_, start, len)) = found else { return Ok(None) };
    let mut part: Box<dyn BlockDevice> = Box::new(Partition::new(disk, start, len)?);
    if !FileSystem::probe(&mut part)? {
        return Err(FsError::BadMagic);
    }
    let data: Fs = FileSystem::mount(part)?;
    mounts.root.mkdir_all(DATA_DIR)?; // the place it appears in `ls /`
    mounts.data = Some(data);
    Ok(Some(len / 2048))
}

/// Is a data partition mounted at /data?
pub fn has_data() -> bool {
    FS.get().as_ref().is_some_and(|m| m.has_data())
}

/// The disk (number in disks.rs) the mounted file system is on, if any.
pub fn mounted_disk() -> Option<usize> {
    *MOUNTED.get()
}

/// Let go of the disk and go on with an empty RAM disk: `install` does this
/// before it rewrites the disk the system was running from. Open files do
/// not exist in this kernel, so nothing is left hanging.
pub fn detach() -> Storage {
    crash::PERSISTENT.store(false, Ordering::SeqCst);
    *MOUNTED.get() = None;
    let storage = ram_disk();
    *HANDLE.get() = Some(storage.clone());
    storage
}

/// The device BeeFS lives on: the BeeFS partition of a boot disk
/// (tools/bootdisk.py), or the whole disk. True when partitioned.
pub fn open_beefs_device<D: BlockDevice + 'static>(disk: D) -> Result<(Box<dyn BlockDevice>, bool), FsError> {
    let (part, partitioned) = Partition::beefs_or_whole(disk)?;
    Ok((Box::new(part), partitioned))
}

fn ram_disk() -> Storage {
    let mut fs: Fs = FileSystem::format(Box::new(RamDisk::new(RAM_DISK_SIZE)) as Box<dyn BlockDevice>)
        .expect("RAM disk format");
    let _ = fs.write_file("/welcome.txt", b"This is a RAM disk: changes are lost when you reboot.\n");
    println!("[ram]  using a {} MiB RAM disk (not persistent)", RAM_DISK_SIZE >> 20);
    *FS.get() = Some(Mounts::new(fs));
    Storage { persistent: false, boot: 0 }
}

fn mount_disk(mut disk: Box<dyn BlockDevice>) -> Result<Option<(Fs, u32)>, FsError> {
    // Never format an unknown disk automatically: it could hold someone's data.
    if !FileSystem::probe(&mut disk)? {
        return Ok(None);
    }
    let mut fs: Fs = FileSystem::mount(disk)?;
    let boot = match fs.read_file("/system/boot_count") {
        Ok(d) => core::str::from_utf8(&d).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0),
        Err(FsError::NotFound) => 0u32,
        Err(e) => return Err(e),
    } + 1;
    fs.mkdir_all("/system")?;
    fs.write_file("/system/boot_count", format!("{boot}\n").as_bytes())?;
    fs.append("/system/boot.log", format!("boot #{boot}\n").as_bytes())?;
    Ok(Some((fs, boot)))
}
