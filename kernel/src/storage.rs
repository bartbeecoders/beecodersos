//! Boot-time storage: mount a BeeFS ATA disk if one is attached, otherwise
//! fall back to a freshly formatted RAM disk.

use crate::ata::AtaDisk;
use crate::crash;
use crate::ring::Global;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::Ordering;
use alloc::boxed::Box;
use alloc::format;
use beefs::{BlockDevice, FileSystem, FsError, Partition, RamDisk};

pub type Fs = FileSystem<Box<dyn BlockDevice>>;

const RAM_DISK_SIZE: usize = 4 * 1024 * 1024;

/// The mounted file system. Threads take turns: none switches while it
/// holds an `FsGuard` (see thread.rs).
static FS: Global<Option<Fs>> = Global::new(None);
static HANDLE: Global<Option<Storage>> = Global::new(None);

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

pub struct FsGuard<'a>(&'a mut Fs);

impl Drop for FsGuard<'_> {
    fn drop(&mut self) {
        crash::FS_BUSY.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Deref for FsGuard<'_> {
    type Target = Fs;
    fn deref(&self) -> &Fs {
        self.0
    }
}

impl DerefMut for FsGuard<'_> {
    fn deref_mut(&mut self) -> &mut Fs {
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
    match AtaDisk::find() {
        Some(disk) => {
            let mut desc = format!("{} \"{}\", {} MiB", disk.position(), disk.model(), disk.sectors() / 2048);
            let dev = match open_beefs_device(disk) {
                Ok((dev, true)) => {
                    desc.push_str(", partition");
                    dev
                }
                Ok((dev, false)) => dev,
                Err(e) => {
                    println!("[disk] {desc}: error: {e}");
                    return ram_disk();
                }
            };
            match mount_disk(dev) {
                Ok(Some((fs, boot))) => {
                    println!("[disk] {desc}: BeeFS mounted (boot #{boot})");
                    crash::PERSISTENT.store(true, Ordering::SeqCst);
                    *FS.get() = Some(fs);
                    return Storage { persistent: true, boot };
                }
                Ok(None) => println!("[disk] {desc}: not BeeFS, left untouched (create one: make disk)"),
                Err(e) => println!("[disk] {desc}: error: {e}"),
            }
        }
        None => println!("[disk] no ATA disk attached"),
    }
    ram_disk()
}

/// The device BeeFS lives on: the BeeFS partition of a boot disk
/// (tools/bootdisk.py), or the whole disk. True when partitioned.
pub fn open_beefs_device(disk: AtaDisk) -> Result<(Box<dyn BlockDevice>, bool), FsError> {
    let (part, partitioned) = Partition::beefs_or_whole(disk)?;
    Ok((Box::new(part), partitioned))
}

fn ram_disk() -> Storage {
    let mut fs: Fs = FileSystem::format(Box::new(RamDisk::new(RAM_DISK_SIZE)) as Box<dyn BlockDevice>)
        .expect("RAM disk format");
    let _ = fs.write_file("/welcome.txt", b"This is a RAM disk: changes are lost when you reboot.\n");
    println!("[ram]  using a {} MiB RAM disk (not persistent)", RAM_DISK_SIZE >> 20);
    *FS.get() = Some(fs);
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
