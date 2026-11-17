//! Crash reports: when the kernel panics or hits a CPU exception, save a short
//! report to the BeeFS disk so the next boot (and the host) can see it.
//!
//! Safety rule: the report is only written when no filesystem operation was in
//! progress (see `storage::FsGuard`) and no disk driver was in the middle of a
//! command (`disks::busy`), by mounting afresh the disk the system runs from
//! (IDE or SATA). A crash in the middle of a disk write is reported on
//! screen/serial only, so a broken kernel can't make the damage worse.

use crate::disks::{self, DiskRef};
use crate::json::{self, Obj};
use crate::ring::Global;
use crate::storage::Fs;
use crate::{eventlog, storage, time};
use alloc::string::String;
use beefs::FileSystem;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const PENDING: &str = "/system/crash/pending.jsonl";
const HISTORY: &str = "/system/crash/history.jsonl";
const OLD_HISTORY: &str = "/system/crash/history.old.jsonl";

/// Number of live filesystem borrows (`storage::FsGuard`).
pub static FS_BUSY: AtomicUsize = AtomicUsize::new(0);
/// Set once a BeeFS disk is mounted: only then can a report survive.
pub static PERSISTENT: AtomicBool = AtomicBool::new(false);
static CRASHING: AtomicBool = AtomicBool::new(false);
static COMMAND: Global<FixedBuf<64>> = Global::new(FixedBuf::new());

/// Remember the command being run, for the report.
pub fn set_command(cmd: &str) {
    let b = COMMAND.get();
    b.clear();
    let _ = b.write_str(cmd);
}

/// Called by the panic and exception handlers, before halting.
pub fn record(kind: &str, msg: fmt::Arguments) {
    if CRASHING.swap(true, Ordering::SeqCst) {
        return; // crashed while reporting a crash
    }
    if !PERSISTENT.load(Ordering::SeqCst) {
        return; // RAM disk: nothing survives a reboot anyway
    }
    if FS_BUSY.load(Ordering::SeqCst) > 0 {
        println!("[crash] the filesystem was in use: report not saved to disk");
        return;
    }
    if disks::busy() {
        println!("[crash] a disk was in the middle of a command: report not saved to disk");
        return;
    }
    let mut buf = FixedBuf::<480>::new();
    let _ = buf.write_fmt(msg);
    // Panic messages read "panicked at file:line:col:\nmessage": keep one line.
    let text = buf.as_str().replace('\n', " ");
    let line = Obj::new()
        .str("t", &time::timestamp())
        .num("up_ms", time::uptime_ms() as i64)
        .str("kind", kind)
        .str("build", crate::OS_BUILD)
        .str("cmd", COMMAND.get().as_str())
        .str("msg", &text)
        .finish();
    let saved = storage::mounted_disk()
        .ok_or("no disk")
        .and_then(|d| storage::open_beefs_device(DiskRef(d)).map_err(|_| "no BeeFS device"))
        .and_then(|(dev, _)| FileSystem::mount(dev).map_err(|_| "mount failed"))
        .and_then(|mut fs: Fs| {
            eventlog::append(&mut fs, PENDING, "/system/crash/pending.old.jsonl", &line).map_err(|_| "write failed")?;
            let ev = eventlog::event("crash").str("kind", kind).str("msg", &text).finish();
            eventlog::write(&mut fs, ev);
            Ok(())
        });
    match saved {
        Ok(()) => println!("[crash] report saved to {PENDING}"),
        Err(e) => println!("[crash] report not saved: {e}"),
    }
}

/// A crash saved by the previous boot.
pub struct Report {
    pub kind: String,
    pub msg: String,
    pub cmd: String,
    pub build: String,
}

/// At boot: move pending reports into the history and return the last one.
pub fn take_pending(fs: &mut Fs) -> Option<Report> {
    let data = fs.read_file(PENDING).ok()?;
    let text = core::str::from_utf8(&data).ok()?;
    let mut last = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let _ = eventlog::append(fs, HISTORY, OLD_HISTORY, line);
        if let Some(o) = json::parse(line) {
            let s = |k| String::from(o.str(k).unwrap_or(""));
            last = Some(Report { kind: s("kind"), msg: s("msg"), cmd: s("cmd"), build: s("build") });
        }
    }
    let _ = fs.remove(PENDING);
    last
}

/// Fixed-size text buffer: formatting without the heap (it may be broken).
pub struct FixedBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> FixedBuf<N> {
    pub const fn new() -> Self {
        FixedBuf { buf: [0; N], len: 0 }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn as_str(&self) -> &str {
        // Only whole UTF-8 characters are ever copied in.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> Write for FixedBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            let mut tmp = [0u8; 4];
            let bytes = c.encode_utf8(&mut tmp).as_bytes();
            if self.len + bytes.len() > N {
                return Err(fmt::Error); // truncated
            }
            self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
        }
        Ok(())
    }
}
