//! A/B kernel slots with automatic rollback (phase 8).
//!
//! The boot disk (tools/bootdisk.py) has two kernels, kernel-a.elf and
//! kernel-b.elf, and Limine boots the one named by `default_entry` in
//! limine.conf. The first line of limine.conf holds the slot state, fixed width:
//!
//!     #slots good=a trial=b state=pending build=000012
//!     default_entry: 2
//!
//! 1. The host deploys a new kernel into the spare slot: state=pending,
//!    default_entry = the trial slot.
//! 2. The trial kernel boots and first *arms the rollback*: state=armed,
//!    default_entry = the good slot. If it now crashes or hangs, the next boot
//!    runs the good kernel again.
//! 3. Once the shell is up and the self-tests pass, the trial becomes the good
//!    slot (state=ok, default_entry = it).
//! 4. A good kernel that finds state=armed knows the trial failed: it marks it
//!    failed and reports it (event log, and a crash signal for the improvement loop).
//!
//! The kernel only rewrites those two lines, in place and at the same length,
//! in one sector, so the FAT file system never changes shape.

use crate::disks::{self, DiskRef};
use crate::fat::{self, Fat16};
use crate::bootargs;
use crate::ring::Global;
use alloc::format;
use alloc::string::String;
use beefs::{mbr_partitions, Block, BlockDevice};

const FAT_PARTITION_TYPES: [u8; 4] = [0xEF, 0x0E, 0x06, 0x04];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pending,
    Armed,
    Ok,
    Failed,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Pending => "pending",
            Phase::Armed => "armed",
            Phase::Ok => "ok",
            Phase::Failed => "failed",
        }
    }
}

/// The slot state line plus `default_entry`. Slots are b'a' / b'b'; trial may be b'-'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub good: u8,
    pub trial: u8,
    pub phase: Phase,
    pub build: u32,
    pub default: u8,
}

// Byte offsets in the first sector of limine.conf (see the module docs).
const GOOD: usize = 12;
const TRIAL: usize = 20;
const PHASE: usize = 28;
const BUILD: usize = 42;
const DEFAULT: usize = 64;

/// Parse the state from the first sector of limine.conf.
pub fn parse(s: &[u8]) -> Option<State> {
    if s.len() < DEFAULT + 2 || &s[..GOOD] != b"#slots good=" || &s[49..DEFAULT] != b"default_entry: " {
        return None;
    }
    let slot = |b: u8| matches!(b, b'a' | b'b').then_some(b);
    let phase = match core::str::from_utf8(&s[PHASE..PHASE + 7]).ok()?.trim_end() {
        "pending" => Phase::Pending,
        "armed" => Phase::Armed,
        "ok" => Phase::Ok,
        "failed" => Phase::Failed,
        _ => return None,
    };
    let build = core::str::from_utf8(&s[BUILD..BUILD + 6]).ok()?.parse().ok()?;
    let default = match s[DEFAULT] {
        b'1' => b'a',
        b'2' => b'b',
        _ => return None,
    };
    let trial = if s[TRIAL] == b'-' { b'-' } else { slot(s[TRIAL])? };
    Some(State { good: slot(s[GOOD])?, trial, phase, build, default })
}

/// Write `st` back into the sector (same layout, same length).
pub fn render(s: &mut [u8], st: &State) {
    s[GOOD] = st.good;
    s[TRIAL] = st.trial;
    let mut name = [b' '; 7];
    name[..st.phase.name().len()].copy_from_slice(st.phase.name().as_bytes());
    s[PHASE..PHASE + 7].copy_from_slice(&name);
    let digits = format!("{:06}", st.build.min(999_999));
    s[BUILD..BUILD + 6].copy_from_slice(digits.as_bytes());
    s[DEFAULT] = if st.default == b'a' { b'1' } else { b'2' };
}

/// limine.conf on the boot disk: the disk and the sector holding the state.
struct Conf {
    disk: DiskRef,
    lba: u64,
}

/// Find limine.conf in the FAT16 boot partition of one of the machine's
/// disks (IDE or SATA): the first disk that has one with a slot state.
fn locate() -> Option<Conf> {
    for index in 0..disks::count() {
        let mut disk = DiskRef(index);
        let mut b: Block = [0; 512];
        if disk.read_block(0, &mut b).is_err() {
            continue;
        }
        let Some(parts) = mbr_partitions(&b) else { continue };
        let Some(&(_, start, len)) = parts.iter().find(|p| FAT_PARTITION_TYPES.contains(&p.0) && p.1 > 0) else { continue };
        let Some(fat) = Fat16::open(&mut disk, start, len) else { continue };
        let Some(lba) = fat.find(&mut disk, fat::CONF).and_then(|file| fat.sector_of(file.first_cluster)) else { continue };
        if disk.read_block(lba, &mut b).is_ok() && parse(&b).is_some() {
            return Some(Conf { disk, lba });
        }
    }
    None
}

fn read() -> Option<(State, Conf)> {
    let mut conf = locate()?;
    let mut b: Block = [0; 512];
    conf.disk.read_block(conf.lba, &mut b).ok()?;
    Some((parse(&b)?, conf))
}

fn write(conf: &mut Conf, st: &State) -> bool {
    let mut b: Block = [0; 512];
    if conf.disk.read_block(conf.lba, &mut b).is_err() || parse(&b).is_none() {
        return false;
    }
    render(&mut b, st);
    conf.disk.write_block(conf.lba, &b).is_ok() && conf.disk.flush().is_ok()
}

/// What happened at boot, for the event log and the improvement loop.
pub enum Report {
    /// This kernel is a trial; the rollback is armed.
    Trial,
    /// A trial of another slot failed and we rolled back to this one.
    RolledBack(String),
}

/// This boot: which slot, and whether it is a trial.
static CURRENT: Global<Option<(u8, bool)>> = Global::new(None);

/// The slot this kernel was booted from (`slot=a` on the command line).
pub fn booted_slot() -> Option<u8> {
    let w = bootargs::cmdline().split_whitespace().find(|w| w.starts_with("slot="))?;
    match w.as_bytes()[5..] {
        [s @ (b'a' | b'b')] => Some(s),
        _ => None,
    }
}

/// Early in boot, before anything risky: arm the rollback for a trial
/// kernel, or notice that a trial failed.
pub fn on_boot() -> Option<Report> {
    let slot = booted_slot()?;
    let Some((mut st, mut conf)) = read() else {
        println!("[slots] booted from slot {} but found no slot state on the disk", slot as char);
        return None;
    };
    let (s, good, trial) = (slot as char, st.good as char, st.trial as char);
    match st.phase {
        Phase::Pending | Phase::Armed if st.trial == slot => {
            st.phase = Phase::Armed;
            st.default = st.good;
            if !write(&mut conf, &st) {
                println!("[slots] could not arm the rollback (disk write failed)");
            }
            println!("[slots] trying slot {s} (b{}): rollback armed, slot {good} boots next unless this one passes its health check", st.build);
            *CURRENT.get() = Some((slot, true));
            Some(Report::Trial)
        }
        Phase::Armed if slot == st.good => {
            st.phase = Phase::Failed;
            st.default = st.good;
            write(&mut conf, &st);
            let msg = format!("trial of build b{} in slot {trial} failed (crashed or hung); rolled back to slot {s}", st.build);
            println!("[slots] {msg}");
            *CURRENT.get() = Some((slot, false));
            Some(Report::RolledBack(msg))
        }
        _ => {
            println!("[slots] slot {s} (good slot: {good})");
            *CURRENT.get() = Some((slot, false));
            None
        }
    }
}

/// True when this boot is a trial that has not passed its health check yet.
pub fn on_trial() -> bool {
    matches!(*CURRENT.get(), Some((_, true)))
}

/// After the shell is ready: a trial that passed `healthy` becomes the good
/// slot; otherwise it is marked failed (the caller reboots into the good slot).
/// Returns Some(passed) for a trial, None otherwise.
pub fn health_check(healthy: bool) -> Option<bool> {
    let (slot, true) = (*CURRENT.get())? else { return None };
    let (mut st, mut conf) = read()?;
    if st.trial != slot || st.phase != Phase::Armed {
        return None;
    }
    if healthy {
        st.good = slot;
        st.phase = Phase::Ok;
        st.default = slot;
    } else {
        st.phase = Phase::Failed;
        st.default = st.good;
    }
    let saved = write(&mut conf, &st);
    *CURRENT.get() = Some((slot, false));
    Some(healthy && saved)
}

/// The slot state on the boot disk, if this machine has one.
pub fn state() -> Option<State> {
    read().map(|(state, _)| state)
}

/// For the `slots` command.
pub fn describe() -> Option<String> {
    let booted = booted_slot()?;
    let (st, _) = read()?;
    Some(format!(
        "booted slot: {}{}\ngood slot:   {}\nlast trial:  {} ({}, build b{})\nnext boot:   slot {}",
        booted as char,
        if on_trial() { " (on trial)" } else { "" },
        st.good as char,
        st.trial as char,
        st.phase.name(),
        st.build,
        st.default as char
    ))
}
