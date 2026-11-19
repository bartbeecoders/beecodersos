//! `install`: put BeeCodersOs on a disk of this machine.
//! Plan and reasons: Docs/install-plan.html.
//!
//! The boot area (boot code, the FAT partition with the bootloader and both
//! kernel slots) is not built here: the install medium carries it as one
//! ready image, made by tools/bootdisk.py when the ISO is built, and the
//! bootloader loads it into memory next to the kernel (bootmods.rs). This
//! module copies it to the disk, adds the partition table for this disk and
//! formats the two BeeFS partitions:
//!
//!   1 boot   16 MiB  FAT16   bootloader, kernel-a.elf, kernel-b.elf
//!   2 os     up to 256 MiB   BeeFS, the system's settings and logs
//!   3 data   the rest (up to 4 GiB for now)   BeeFS, the user's files
//!
//! On a disk that already holds BeeCodersOs, `upgrade` writes the new kernel
//! into the spare slot and marks it "on trial": the A/B rollback of
//! bootslots.rs then decides. `reinstall` writes boot and os again and keeps
//! the data partition.
//!
//! Nothing here decides to write by itself: the shell asks the user first
//! (shell.rs), and every function takes the disk it was told to use.

use crate::bootmods;
use crate::bootslots::{self, Phase};
use crate::fat::{self, Fat16};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beefs::{mbr_partitions, Block, BlockDevice, FileSystem, FsError, Partition, BEEFS_PARTITION_TYPE, BLOCK_SIZE};

/// The boot partition as tools/bootdisk.py lays it out.
pub const BOOT_START: u64 = 2048;
pub const BOOT_SECTORS: u64 = 16 * 2048;
/// The install image: everything up to the end of the boot partition.
pub const IMAGE_SECTORS: u64 = BOOT_START + BOOT_SECTORS;
const FAT_TYPE: u8 = 0xEF;
/// The data partition: BeeFS too, with a type of its own so the two are not mixed up.
pub const DATA_PARTITION_TYPE: u8 = 0x7E;

const MIB: u64 = 2048; // sectors
const OS_MAX: u64 = 256 * MIB;
/// BeeFS is not built for very large partitions yet (formatting writes a
/// table that grows with the size): the data partition stops here.
const DATA_MAX: u64 = 4096 * MIB;
/// Below this an os or data partition is not worth having.
const PART_MIN: u64 = 8 * MIB;

/// One partition: (type, first sector, sectors).
pub type Part = (u8, u64, u64);

/// Where boot, os and data go on a disk of this size. `None`: too small.
pub fn layout(disk_sectors: u64) -> Option<[Part; 3]> {
    // An MBR counts sectors in 32 bits: what lies beyond 2 TiB is not used.
    let end = disk_sectors.min(u32::MAX as u64) / MIB * MIB;
    let rest = end.checked_sub(IMAGE_SECTORS)?;
    if rest < 2 * PART_MIN {
        return None;
    }
    let os = (rest / 2 / MIB * MIB).min(OS_MAX);
    let data = (rest - os).min(DATA_MAX);
    Some([
        (FAT_TYPE, BOOT_START, BOOT_SECTORS),
        (BEEFS_PARTITION_TYPE, IMAGE_SECTORS, os),
        (DATA_PARTITION_TYPE, IMAGE_SECTORS + os, data),
    ])
}

/// The first sector of the installed disk: the image's boot code, and the
/// partition table for this disk (the boot partition is the active one).
pub fn first_sector(image_sector: &Block, parts: &[Part; 3]) -> Block {
    let mut sector = *image_sector;
    sector[446..510].fill(0);
    for (i, &(kind, start, len)) in parts.iter().enumerate() {
        let e = &mut sector[446 + i * 16..462 + i * 16];
        e[0] = if i == 0 { 0x80 } else { 0 };
        // Cylinder/head/sector fields: "use the sector numbers" (as bootdisk.py writes them).
        e[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        e[4] = kind;
        e[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        e[8..12].copy_from_slice(&(start as u32).to_le_bytes());
        e[12..16].copy_from_slice(&(len as u32).to_le_bytes());
    }
    sector[510] = 0x55;
    sector[511] = 0xAA;
    sector
}

/// Is this an install image: the right size, a boot sector, and the boot
/// partition where we expect it?
pub fn is_image(data: &[u8]) -> bool {
    if data.len() as u64 != IMAGE_SECTORS * BLOCK_SIZE as u64 {
        return false;
    }
    let Ok(sector) = <&Block>::try_from(&data[..BLOCK_SIZE]) else { return false };
    mbr_partitions(sector).is_some_and(|p| p[0] == (FAT_TYPE, BOOT_START, BOOT_SECTORS))
}

/// The install image the bootloader loaded, when booted from an install medium.
pub fn image() -> Option<&'static [u8]> {
    bootmods::modules().find(|m| is_image(m))
}

/// The build number of the kernels inside an image or boot partition:
/// from the slot state line of its limine.conf.
pub fn build_in(data: &[u8]) -> Option<u32> {
    const MARK: &[u8] = b"#slots good=";
    // The file starts on a sector boundary.
    data.chunks(BLOCK_SIZE).find(|s| s.starts_with(MARK)).and_then(bootslots::parse).map(|state| state.build)
}

/// Which sectors of the image have to be written (and checked): everything
/// before the boot partition (the boot code lives there), the partition's
/// tables and directory, and the clusters its files use. Free clusters are
/// left out: what the disk holds there does not matter, and writing 17 MiB
/// sector by sector is slow. Ranges are (first sector, count), sector 0 not
/// included. A boot partition we cannot read as FAT16 is written whole.
pub fn used_ranges(image: &[u8]) -> Vec<(u64, u64)> {
    let whole = alloc::vec![(1, IMAGE_SECTORS - 1)];
    let at = |offset: usize, len: usize| image.get(BOOT_START as usize * BLOCK_SIZE + offset..).and_then(|s| s.get(..len));
    let le16 = |offset: usize| at(offset, 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as u64);
    let (bytes_per_sector, per_cluster, reserved, fats, root_entries, fat_sectors) =
        (le16(11), at(13, 1).map_or(0, |b| b[0] as u64), le16(14), at(16, 1).map_or(0, |b| b[0] as u64), le16(17), le16(22));
    if bytes_per_sector != 512 || per_cluster == 0 || reserved == 0 || fats == 0 || fat_sectors == 0 || root_entries == 0 {
        return whole;
    }
    let fat_start = BOOT_START + reserved;
    let data_start = fat_start + fats * fat_sectors + (root_entries * 32).div_ceil(512);
    if data_start >= IMAGE_SECTORS {
        return whole;
    }
    let mut ranges = alloc::vec![(1, data_start - 1)];
    let clusters = (IMAGE_SECTORS - data_start) / per_cluster;
    let fat = fat_start as usize * BLOCK_SIZE;
    for cluster in 2..clusters + 2 {
        let Some(entry) = image.get(fat + cluster as usize * 2..fat + cluster as usize * 2 + 2) else { return whole };
        if entry != [0, 0] {
            let first = data_start + (cluster - 2) * per_cluster;
            match ranges.last_mut() {
                Some(last) if last.0 + last.1 == first => last.1 += per_cluster,
                _ => ranges.push((first, per_cluster)),
            }
        }
    }
    ranges
}

/// What is on a disk now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contents {
    /// Nothing: no partition table and an empty start.
    Empty,
    /// BeeCodersOs: a boot partition and a BeeFS partition. The build in its
    /// boot partition, if it can be read; whether it has a data partition.
    Installed { build: Option<u32>, data: bool },
    /// An install that was interrupted: boot area written, no partition table yet.
    Unfinished,
    /// BeeFS over the whole disk (as `make disk` makes): files, but no system.
    BeeFsDisk,
    /// Anything else, described.
    Other(String),
}

/// A device borrowed for a moment (file system code wants to own its device).
struct Borrowed<'a, D: BlockDevice>(&'a mut D);

impl<D: BlockDevice> BlockDevice for Borrowed<'_, D> {
    fn block_count(&self) -> u64 {
        self.0.block_count()
    }
    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), FsError> {
        self.0.read_block(index, buf)
    }
    fn write_block(&mut self, index: u64, buf: &Block) -> Result<(), FsError> {
        self.0.write_block(index, buf)
    }
    fn flush(&mut self) -> Result<(), FsError> {
        self.0.flush()
    }
}

/// Look at a disk without changing it.
pub fn inspect<D: BlockDevice>(disk: &mut D) -> Result<Contents, FsError> {
    let mut sector: Block = [0; BLOCK_SIZE];
    disk.read_block(0, &mut sector)?;
    if FileSystem::probe(&mut Borrowed(disk))? {
        return Ok(Contents::BeeFsDisk);
    }
    let parts: Vec<Part> = mbr_partitions(&sector).map(|p| p.into_iter().filter(|p| p.0 != 0).collect()).unwrap_or_default();
    if parts.is_empty() {
        // No partition table (or an empty one). Empty means: nothing in the first sectors either.
        let mut blank = sector.iter().all(|b| *b == 0);
        let mut probe: Block = [0; BLOCK_SIZE];
        for index in 1..64.min(disk.block_count()) {
            if !blank {
                break;
            }
            disk.read_block(index, &mut probe)?;
            blank = probe.iter().all(|b| *b == 0);
        }
        if blank {
            return Ok(Contents::Empty);
        }
        // Our own boot area without the table that is written last?
        if sector.iter().all(|b| *b == 0) && disk.block_count() > BOOT_START {
            disk.read_block(BOOT_START, &mut probe)?;
            if probe[510] == 0x55 && probe[511] == 0xAA && &probe[54..59] == b"FAT16" {
                return Ok(Contents::Unfinished);
            }
        }
        return Ok(Contents::Other(String::from("data without a partition table")));
    }
    let ours = parts[0] == (FAT_TYPE, BOOT_START, BOOT_SECTORS)
        && parts.iter().skip(1).all(|p| p.0 == BEEFS_PARTITION_TYPE || p.0 == DATA_PARTITION_TYPE)
        && parts.iter().any(|p| p.0 == BEEFS_PARTITION_TYPE);
    if ours {
        // The slot state is near the start of the boot partition: look for it there.
        let mut build = None;
        for index in BOOT_START..(BOOT_START + 2048).min(disk.block_count()) {
            disk.read_block(index, &mut sector)?;
            if let Some(found) = build_in(&sector) {
                build = Some(found);
                break;
            }
        }
        return Ok(Contents::Installed { build, data: parts.iter().any(|p| p.0 == DATA_PARTITION_TYPE) });
    }
    let kinds: Vec<String> = parts.iter().map(|p| format!("{:02x}", p.0)).collect();
    let what = if parts.iter().any(|p| p.0 == 0xEE) { "a GPT partition table" } else { "partitions of another system" };
    Ok(Contents::Other(format!("{what} ({} partition{}, type {})", parts.len(), if parts.len() == 1 { "" } else { "s" }, kinds.join(" "))))
}

/// What the user must type to erase a disk that holds something else: the
/// size and model `install` just showed. Capitals and extra spaces do not matter.
pub fn erase_sentence(sectors: u64, model: &str) -> String {
    let model: Vec<&str> = model.split_whitespace().collect();
    format!("erase {} {}", size_text(sectors), model.join(" ")).trim_end().into()
}

pub fn sentence_matches(typed: &str, wanted: &str) -> bool {
    let tidy = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    tidy(typed) == tidy(wanted)
}

/// "465 GiB" or "64 MiB".
pub fn size_text(sectors: u64) -> String {
    let mib = sectors / MIB;
    if mib >= 2048 {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

/// A fresh install: everything on the disk is replaced. The steps are in an
/// order that never leaves a half-valid disk: the first sector (boot code
/// and partition table) is wiped first and written last, so a power cut
/// leaves a disk without a partition table, which `inspect` recognises.
/// `step` is told what is being done.
/// `stop_after` is for the tests: give up after that many steps, as a power
/// cut would (boot flag `installstop=N`).
pub fn install<D: BlockDevice>(disk: &mut D, image: &[u8], stop_after: Option<u32>, mut step: impl FnMut(&str)) -> Result<[Part; 3], String> {
    let fail = |what: &str, e: FsError| format!("{what}: {e}");
    if !is_image(image) {
        return Err(String::from("this is not an install image"));
    }
    let parts = layout(disk.block_count()).ok_or("the disk is too small (at least 33 MiB)")?;
    let sector = |index: u64| -> &Block { <&Block>::try_from(&image[index as usize * BLOCK_SIZE..][..BLOCK_SIZE]).unwrap() };

    let mut done = 0;
    let mut cut = |disk: &mut D| -> Result<(), String> {
        done += 1;
        if stop_after == Some(done) {
            let _ = disk.flush();
            return Err(format!("stopped after step {done} for a test (installstop)"));
        }
        Ok(())
    };

    step("clearing the partition table");
    disk.write_block(0, &[0; BLOCK_SIZE]).and_then(|_| disk.flush()).map_err(|e| fail("clearing the first sector", e))?;
    cut(disk)?;

    step("writing the boot area (bootloader and both kernel slots)");
    let ranges = used_ranges(image);
    let used = || ranges.iter().flat_map(|&(first, count)| first..first + count);
    for index in used() {
        disk.write_block(index, sector(index)).map_err(|e| fail("writing the boot area", e))?;
    }

    cut(disk)?;

    step("making the os file system");
    format_part(disk, parts[1], &["/system"], None).map_err(|e| fail("the os partition", e))?;
    step("making the data file system");
    let note = format!("Your files live here, on the data partition of BeeCodersOs {}.\n", crate::OS_VERSION);
    format_part(disk, parts[2], &[], Some(("/welcome.txt", note.as_bytes()))).map_err(|e| fail("the data partition", e))?;
    disk.flush().map_err(|e| fail("writing to the disk", e))?;
    cut(disk)?;

    step("checking what was written");
    let mut read: Block = [0; BLOCK_SIZE];
    for index in used() {
        disk.read_block(index, &mut read).map_err(|e| fail("reading the boot area back", e))?;
        if &read != sector(index) {
            return Err(format!("the disk gave back something else than was written (sector {index}): it may be failing"));
        }
    }
    cut(disk)?;

    step("writing the partition table");
    disk.write_block(0, &first_sector(sector(0), &parts)).and_then(|_| disk.flush()).map_err(|e| fail("writing the first sector", e))?;
    Ok(parts)
}

/// Make an empty BeeFS in a partition, with some directories and one file.
fn format_part<D: BlockDevice>(disk: &mut D, part: Part, dirs: &[&str], file: Option<(&str, &[u8])>) -> Result<(), FsError> {
    let mut fs = FileSystem::format(Partition::new(Borrowed(disk), part.1, part.2)?)?;
    for dir in dirs {
        fs.mkdir_all(dir)?;
    }
    if let Some((path, content)) = file {
        fs.write_file(path, content)?;
    }
    Ok(())
}

/// The kernel file of a slot in the boot partition.
fn slot_file(slot: u8) -> &'static [u8; 11] {
    if slot == b'a' {
        fat::KERNEL_A
    } else {
        fat::KERNEL_B
    }
}

/// Upgrade a disk that holds BeeCodersOs with the kernel of an install
/// image: see `write_trial`. Returns the slot written and the build number.
pub fn upgrade<D: BlockDevice>(disk: &mut D, image: &[u8], step: impl FnMut(&str)) -> Result<(u8, u32), String> {
    if !is_image(image) {
        return Err(String::from("this is not an install image"));
    }
    let build = build_in(image).ok_or("the install image does not say which build it holds")?;
    let mut source = fat::Memory(image);
    let new = Fat16::open(&mut source, BOOT_START, BOOT_SECTORS)
        .and_then(|f| Some((f, f.find(&mut source, fat::KERNEL_A)?)))
        .and_then(|(f, file)| f.sectors(&mut source, &file));
    let sectors = new.ok_or("the install image has no kernel in it")?;
    let mut kernel = Vec::with_capacity(sectors.len() * BLOCK_SIZE);
    for from in sectors {
        kernel.extend_from_slice(&image[from as usize * BLOCK_SIZE..][..BLOCK_SIZE]);
    }
    write_trial(disk, &kernel, build, step).map(|slot| (slot, build))
}

/// The disk this machine's installed system is on: the first one that
/// holds BeeCodersOs with kernel slots.
pub fn boot_disk() -> Option<crate::disks::DiskRef> {
    (0..crate::disks::count())
        .map(crate::disks::DiskRef)
        .find(|disk| matches!(inspect(&mut disk.clone()), Ok(Contents::Installed { .. })))
}

/// Write a new kernel into the slot that is not the good one and put it on
/// trial. At the next boot the new kernel arms the rollback, checks itself
/// and becomes the good slot; if it fails, the old one boots again
/// (bootslots.rs). os and data are not touched. Only the contents of clusters
/// the slot file already has are written: the FAT file system keeps its
/// shape. `kernel` may end in zeros (a padded slot file). Returns the slot.
pub fn write_trial<D: BlockDevice>(disk: &mut D, kernel: &[u8], build: u32, mut step: impl FnMut(&str)) -> Result<u8, String> {
    let fail = |what: &str, e: FsError| format!("{what}: {e}");
    // What counts is the kernel without the zeros a slot file is filled up with.
    let real = kernel.len() - kernel.iter().rev().take_while(|b| **b == 0).count();
    if real < 64 || &kernel[..4] != b"\x7fELF" {
        return Err(String::from("that is not a kernel"));
    }
    let boot = Fat16::open(disk, BOOT_START, BOOT_SECTORS).ok_or("the disk's boot partition cannot be read")?;
    let conf = boot.find(disk, fat::CONF).and_then(|f| boot.sector_of(f.first_cluster)).ok_or("the disk's boot settings were not found")?;
    let mut sector: Block = [0; BLOCK_SIZE];
    disk.read_block(conf, &mut sector).map_err(|e| fail("reading the boot settings", e))?;
    let mut state = bootslots::parse(&sector).ok_or("the disk's boot settings have no slot state")?;
    if matches!(state.phase, Phase::Pending | Phase::Armed) {
        return Err(String::from("a kernel is still on trial on this disk: start the disk once, then upgrade"));
    }
    let spare = if state.good == b'a' { b'b' } else { b'a' };
    let old = boot.find(disk, slot_file(spare)).ok_or("the disk has no file for the spare kernel slot")?;
    if old.size < real as u64 {
        return Err(format!(
            "the kernel slots on this disk hold {} KiB and the new kernel needs {} KiB: this disk was made by an older version (use --reinstall, or --erase)",
            old.size / 1024,
            real.div_ceil(1024)
        ));
    }
    let target = boot.sectors(disk, &old).ok_or("the spare kernel slot on the disk is damaged")?;

    step(&format!("writing the new kernel (b{build}) into slot {}", spare as char));
    let content = |i: usize| -> Block {
        let mut b: Block = [0; BLOCK_SIZE];
        if let Some(from) = kernel.get(i * BLOCK_SIZE..) {
            let n = from.len().min(BLOCK_SIZE);
            b[..n].copy_from_slice(&from[..n]);
        }
        b // past the new kernel's end: zeros
    };
    // The slot's sectors in runs of neighbours: one command per run.
    let mut runs: Vec<(usize, usize)> = Vec::new(); // (index in target, length)
    for i in 0..target.len() {
        match runs.last_mut() {
            Some((start, len)) if target[*start] + *len as u64 == target[i] => *len += 1,
            _ => runs.push((i, 1)),
        }
    }
    for &(start, len) in &runs {
        let blocks: Vec<Block> = (start..start + len).map(content).collect();
        disk.write_blocks(target[start], &blocks).map_err(|e| fail("writing the kernel", e))?;
    }
    disk.flush().map_err(|e| fail("writing the kernel", e))?;
    step("checking what was written");
    for &(start, len) in &runs {
        let mut blocks: Vec<Block> = alloc::vec![[0; BLOCK_SIZE]; len];
        disk.read_blocks(target[start], &mut blocks).map_err(|e| fail("reading the kernel back", e))?;
        if blocks.iter().enumerate().any(|(k, b)| *b != content(start + k)) {
            return Err(String::from("the disk gave back something else than was written: the old kernel is still the one that boots"));
        }
    }

    step(&format!("putting slot {} on trial", spare as char));
    disk.read_block(conf, &mut sector).map_err(|e| fail("reading the boot settings", e))?;
    state.trial = spare;
    state.phase = Phase::Pending;
    state.build = build;
    state.default = spare;
    bootslots::render(&mut sector, &state);
    disk.write_block(conf, &sector).and_then(|_| disk.flush()).map_err(|e| fail("writing the boot settings", e))?;
    Ok(spare)
}

/// Write the boot area and the os partition again; keep the data partition
/// exactly where and as it is. The partition table is not cleared at any
/// moment (it is what says where the data is): a power cut leaves a disk
/// that may not start, but whose data a second `--reinstall` still finds.
pub fn reinstall<D: BlockDevice>(disk: &mut D, image: &[u8], mut step: impl FnMut(&str)) -> Result<(), String> {
    let fail = |what: &str, e: FsError| format!("{what}: {e}");
    if !is_image(image) {
        return Err(String::from("this is not an install image"));
    }
    let mut first: Block = [0; BLOCK_SIZE];
    disk.read_block(0, &mut first).map_err(|e| fail("reading the partition table", e))?;
    let table = mbr_partitions(&first).ok_or("the disk has no partition table")?;
    let find = |kind: u8| table.iter().copied().find(|p| p.0 == kind && p.1 >= IMAGE_SECTORS && p.2 >= PART_MIN);
    let (Some(os), Some(data)) = (find(BEEFS_PARTITION_TYPE), find(DATA_PARTITION_TYPE)) else {
        return Err(String::from("this disk has no os and data partition as install makes them: use --erase"));
    };
    if table[0] != (FAT_TYPE, BOOT_START, BOOT_SECTORS) {
        return Err(String::from("this disk's boot partition is not where install puts it: use --erase"));
    }
    let sector = |index: u64| -> &Block { <&Block>::try_from(&image[index as usize * BLOCK_SIZE..][..BLOCK_SIZE]).unwrap() };

    step("writing the boot area (bootloader and both kernel slots)");
    let ranges = used_ranges(image);
    let used = || ranges.iter().flat_map(|&(first, count)| first..first + count);
    for index in used() {
        disk.write_block(index, sector(index)).map_err(|e| fail("writing the boot area", e))?;
    }
    step("making the os file system again");
    format_part(disk, os, &["/system"], None).map_err(|e| fail("the os partition", e))?;
    disk.flush().map_err(|e| fail("writing to the disk", e))?;
    step("checking what was written");
    let mut read: Block = [0; BLOCK_SIZE];
    for index in used() {
        disk.read_block(index, &mut read).map_err(|e| fail("reading the boot area back", e))?;
        if &read != sector(index) {
            return Err(format!("the disk gave back something else than was written (sector {index}): it may be failing"));
        }
    }
    step("writing the boot code (the partition table stays as it is)");
    disk.write_block(0, &first_sector(sector(0), &[table[0], os, data])).and_then(|_| disk.flush()).map_err(|e| fail("writing the first sector", e))?;
    Ok(())
}
