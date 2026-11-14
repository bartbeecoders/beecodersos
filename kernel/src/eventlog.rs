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
