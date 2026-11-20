//! Structured event log: one JSON object per line in /system/log/events.jsonl
//! (boots, commands, crashes, wishes). The host reads it with
//! `tools/bee_report.py`, so the OS can see what it is missing.
//!
//! Commands are logged by name only (no arguments), so the log does not
//! collect what people type.

use crate::json::Obj;
use crate::storage::Fs;
use crate::time;
use alloc::string::String;
use beefs::FsError;

pub const LOG: &str = "/system/log/events.jsonl";
const OLD_LOG: &str = "/system/log/events.old.jsonl";
/// Rotate to events.old.jsonl past this size.
const MAX_SIZE: u64 = 64 * 1024;

/// Commands repeated in a row (a script or a remote tool asking `uptime`
/// five times a second) would push everything else out of the log: the
/// same command with the same outcome again within this time is not
/// written again; the next line written says how many were left out.
pub const REPEAT_MS: u64 = 60_000;

#[derive(Default)]
pub struct Repeats {
    last: Option<(String, String)>,
    at_ms: u64,
    left_out: u32,
}

impl Repeats {
    pub const fn new() -> Repeats {
        Repeats { last: None, at_ms: 0, left_out: 0 }
    }

    /// Should a command event be written now? `Some(n)`: yes, and `n`
    /// repeats of the one before were left out; `None`: it is a repeat.
    pub fn check(&mut self, cmd: &str, status: &str, now_ms: u64) -> Option<u32> {
        let same = self.last.as_ref().is_some_and(|(c, s)| c == cmd && s == status);
        if same && now_ms.saturating_sub(self.at_ms) < REPEAT_MS {
            self.left_out += 1;
            return None;
        }
        let left_out = core::mem::take(&mut self.left_out);
        self.last = Some((String::from(cmd), String::from(status)));
        self.at_ms = now_ms;
        Some(left_out)
    }
}

pub static REPEATS: crate::ring::Global<Repeats> = crate::ring::Global::new(Repeats::new());

/// Start an event: timestamp, uptime and event name are filled in.
pub fn event(name: &str) -> Obj {
    Obj::new()
        .str("t", &time::timestamp())
        .num("up_ms", time::uptime_ms() as i64)
        .str("ev", name)
}

/// Append a finished event line. Logging never fails loudly.
pub fn write(fs: &mut Fs, line: String) {
    let _ = append(fs, LOG, OLD_LOG, &line);
}

/// Append a line to a JSONL file, rotating it when it gets large.
pub fn append(fs: &mut Fs, path: &str, old: &str, line: &str) -> Result<(), FsError> {
    if let Some(dir) = path.rsplit_once('/').map(|(d, _)| d).filter(|d| !d.is_empty()) {
        fs.mkdir_all(dir)?;
    }
    if fs.metadata(path).map(|m| m.size > MAX_SIZE).unwrap_or(false) {
        let _ = fs.remove(old);
        fs.rename(path, old)?;
    }
    let mut l = String::from(line);
    l.push('\n');
    fs.append(path, l.as_bytes())
}
