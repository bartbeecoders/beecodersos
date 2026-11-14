//! Boot-time storage: mount a BeeFS ATA disk if one is attached, otherwise
//! fall back to a freshly formatted RAM disk.

use crate::ata::AtaDisk;
use crate::crash;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::Ordering;
use alloc::boxed::Box;
use alloc::format;
use beefs::{BlockDevice, FileSystem, FsError, RamDisk};

pub type Fs = FileSystem<Box<dyn BlockDevice>>;

const RAM_DISK_SIZE: usize = 4 * 1024 * 1024;

pub struct Storage {
    fs: Fs,
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
        FsGuard(&mut self.fs)
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

pub fn mount() -> Storage {
    match AtaDisk::find() {
        Some(disk) => {
            let desc = format!("{} \"{}\", {} MiB", disk.position(), disk.model(), disk.sectors() / 2048);
            match mount_disk(disk) {
                Ok(Some((fs, boot))) => {
                    println!("[disk] {desc}: BeeFS mounted (boot #{boot})");
                    crash::PERSISTENT.store(true, Ordering::SeqCst);
                    return Storage { fs, persistent: true, boot };
                }
                Ok(None) => println!("[disk] {desc}: not BeeFS, left untouched (create one: make disk)"),
                Err(e) => println!("[disk] {desc}: error: {e}"),
            }
        }
        None => println!("[disk] no ATA disk attached"),
    }
    let mut fs: Fs = FileSystem::format(Box::new(RamDisk::new(RAM_DISK_SIZE)) as Box<dyn BlockDevice>)
        .expect("RAM disk format");
    let _ = fs.write_file("/welcome.txt", b"This is a RAM disk: changes are lost when you reboot.\n");
    println!("[ram]  using a {} MiB RAM disk (not persistent)", RAM_DISK_SIZE >> 20);
    Storage { fs, persistent: false, boot: 0 }
}

fn mount_disk(mut disk: AtaDisk) -> Result<Option<(Fs, u32)>, FsError> {
    // Never format an unknown disk automatically: it could hold someone's data.
    if !FileSystem::probe(&mut disk)? {
        return Ok(None);
    }
    let mut fs: Fs = FileSystem::mount(Box::new(disk) as Box<dyn BlockDevice>)?;
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
