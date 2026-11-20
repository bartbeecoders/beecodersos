//! Kitchen timers (`timer 10m tea`) and alarms (`alarm 07:30 wake up`, at
//! the next 07:30 on the local clock): each one waits in a thread of its own;
//! when its time is up a window opens on the desktop with its message and
//! the speaker plays a short tune (when the machine has sound). `timers`
//! lists them, `timer off N` cancels one. They are kept in `/system/timers`
//! (when the clock is set) and come back after a restart; one that came due
//! while the machine was off rings at once, if it is at most an hour late.

use crate::ring::Global;
use crate::{thread, time};
use alloc::string::String;
use alloc::vec::Vec;

pub struct Timer {
    pub id: u32,
    pub due_ms: u64,
    pub message: String,
}

static TIMERS: Global<Vec<Timer>> = Global::new(Vec::new());
static NEXT_ID: Global<u32> = Global::new(1);

/// At most this many timers at once.
pub const MAX_TIMERS: usize = 16;
/// The longest timer: a day.
pub const MAX_MS: u64 = 24 * 3600 * 1000;

/// "90" or "90s", "5m", "1h", "1h30m", "2m30s" -> milliseconds; `None` for
/// anything else, nothing, zero or more than a day.
pub fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        let secs: u64 = text.parse().ok()?;
        return Some(secs * 1000).filter(|ms| (1..=MAX_MS).contains(ms));
    }
    let (mut total, mut number, mut seen_unit) = (0u64, String::new(), false);
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let unit = match c {
            'h' => 3_600_000,
            'm' => 60_000,
            's' => 1_000,
            _ => return None,
        };
        let n: u64 = number.parse().ok()?;
        total = total.checked_add(n.checked_mul(unit)?)?;
        number.clear();
        seen_unit = true;
    }
    if !number.is_empty() || !seen_unit {
        return None; // digits without a unit at the end ("1m30")
    }
    Some(total).filter(|ms| (1..=MAX_MS).contains(ms))
}

/// "07:30" or "7:30" -> minutes after midnight; `None` otherwise.
pub fn parse_clock(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Milliseconds from `unix` (seconds, UTC) until the next time the local
/// clock shows `minute` (after midnight), `offset_min` ahead of UTC. A time
/// that is now or past today means tomorrow.
pub fn ms_until(minute: u32, unix: u64, offset_min: i32) -> u64 {
    let local = unix as i64 + offset_min as i64 * 60;
    let now_in_day = local.rem_euclid(86_400);
    let mut wait = minute as i64 * 60 - now_in_day;
    if wait <= 0 {
        wait += 86_400;
    }
    wait as u64 * 1000
}

/// "1h 05m", "4m 10s", "12s": what is left, for `timers`.
pub fn left_text(ms: u64) -> String {
    let s = ms.div_ceil(1000);
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => alloc::format!("{s}s"),
        (0, m, s) => alloc::format!("{m}m {s:02}s"),
        (h, m, _) => alloc::format!("{h}h {m:02}m"),
    }
}

/// Where the timers are kept: one line each, "<due, Unix seconds> <message>".
pub const FILE: &str = "/system/timers";
/// A timer that came due while the machine was off rings if it is late by at most this.
pub const LATE_MS: u64 = 3600 * 1000;
const MAX_MESSAGE: usize = 200;

/// The file's text for these timers: (due in Unix seconds, message).
pub fn encode(timers: &[(u64, String)]) -> String {
    let mut text = String::new();
    for (due, message) in timers {
        let message: String = message.chars().filter(|c| !c.is_control()).take(MAX_MESSAGE).collect();
        text += &alloc::format!("{due} {message}\n");
    }
    text
}

/// The timers in the file's text; lines that make no sense are skipped.
pub fn decode(text: &str) -> Vec<(u64, String)> {
    text.lines()
        .filter_map(|line| {
            let (due, message) = line.split_once(' ').unwrap_or((line, ""));
            Some((due.parse::<u64>().ok()?, message.chars().take(MAX_MESSAGE).collect()))
        })
        .take(MAX_TIMERS)
        .collect()
}

/// What to do with a saved timer at boot (`now`, `due` in Unix seconds):
/// wait so many milliseconds, ring now (late), or forget it.
#[derive(Debug, PartialEq, Eq)]
pub enum Restore {
    Wait(u64),
    Late,
    Forget,
}

pub fn restore_action(due: u64, now: u64) -> Restore {
    if due > now {
        Restore::Wait(((due - now) * 1000).min(MAX_MS))
    } else if (now - due) * 1000 <= LATE_MS {
        Restore::Late
    } else {
        Restore::Forget
    }
}

/// Write the running timers to the file (nothing without a clock).
fn save() {
    let Some(unix) = time::now_unix() else { return };
    let now = time::uptime_ms();
    let list: Vec<(u64, String)> = TIMERS.get().iter().map(|t| (unix + t.due_ms.saturating_sub(now).div_ceil(1000), t.message.clone())).collect();
    if let Some(mut storage) = crate::storage::handle() {
        let _ = if list.is_empty() { storage.fs().remove(FILE) } else { storage.fs().write_file(FILE, encode(&list).as_bytes()) };
    }
}

/// At boot: the saved timers again. Returns a line for the boot log.
pub fn restore() -> Option<String> {
    let unix = time::now_unix()?;
    let text = crate::storage::handle()?.fs().read_file(FILE).ok()?;
    let (mut waiting, mut late) = (0, 0);
    for (due, message) in decode(&String::from_utf8_lossy(&text)) {
        match restore_action(due, unix) {
            Restore::Wait(ms) => {
                if start(ms, &message).is_ok() {
                    waiting += 1;
                }
            }
            Restore::Late => {
                let note = if message.is_empty() { String::from("(missed while the machine was off)") } else { alloc::format!("{message} (missed while the machine was off)") };
                if start(1000, &note).is_ok() {
                    late += 1;
                }
            }
            Restore::Forget => {}
        }
    }
    save();
    (waiting + late > 0).then(|| alloc::format!("{waiting} timer(s) waiting, {late} missed while off ('timers' lists them)"))
}

/// Start a timer; returns its number.
pub fn start(ms: u64, message: &str) -> Result<u32, &'static str> {
    if TIMERS.get().len() >= MAX_TIMERS {
        return Err("too many timers (16): cancel one with 'timer off N'");
    }
    let id = *NEXT_ID.get();
    *NEXT_ID.get() += 1;
    let due_ms = time::uptime_ms() + ms;
    let message: String = message.chars().filter(|c| !c.is_control()).take(MAX_MESSAGE).collect();
    TIMERS.get().push(Timer { id, due_ms, message: message.clone() });
    let spawned = thread::spawn("timer", None, move || {
        thread::wait_until(|| time::uptime_ms() >= due_ms || !TIMERS.get().iter().any(|t| t.id == id));
        let Some(at) = TIMERS.get().iter().position(|t| t.id == id) else { return };
        let timer = TIMERS.get().remove(at);
        save();
        ring(id, &timer.message);
    });
    if spawned.is_none() {
        TIMERS.get().retain(|t| t.id != id);
        return Err("could not start a thread");
    }
    save();
    Ok(id)
}

/// Cancel a timer; false when there is no such one.
pub fn cancel(id: u32) -> bool {
    let timers = TIMERS.get();
    let before = timers.len();
    timers.retain(|t| t.id != id);
    let gone = timers.len() != before;
    if gone {
        save();
    }
    gone
}

/// The running timers: (number, milliseconds left, message), soonest first.
pub fn list() -> Vec<(u32, u64, String)> {
    let now = time::uptime_ms();
    let mut all: Vec<(u32, u64, String)> = TIMERS.get().iter().map(|t| (t.id, t.due_ms.saturating_sub(now), t.message.clone())).collect();
    all.sort_by_key(|t| t.1);
    all
}

/// Time is up: a window with the message, and a tune.
fn ring(id: u32, message: &str) {
    if crate::hda::available() {
        let _ = thread::spawn("timer-tune", None, || {
            if let Ok(notes) = crate::sound::parse_tune(&["c6", "e6", "g6", "c7", "-", "c6", "e6", "g6", "c7"]) {
                let _ = crate::hda::play(&crate::sound::render_tune(&notes, 60), || false);
            }
        });
    }
    let text = if message.is_empty() { String::from("Time is up.") } else { alloc::format!("Time is up: {message}") };
    let _ = crate::desk::open_timer_alert(id, &text);
}
