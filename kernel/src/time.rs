//! Time: the PIT timer (IRQ0, 100 Hz) counts uptime and drives timeouts; the
//! CMOS real-time clock gives the date and time at boot; `ntp` can correct it.
//!
//! The RTC is read as UTC (QEMU and libvirt default to a UTC clock).

use crate::port::{inb, outb};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

pub const HZ: u64 = 100;
const PIT_FREQUENCY: u64 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);
/// Unix time of the RTC reading at boot (0 = no clock).
static BOOT_UNIX: AtomicU64 = AtomicU64::new(0);
/// Tick count when the RTC was read.
static BOOT_TICK: AtomicU64 = AtomicU64::new(0);

/// Program PIT channel 0 (rate generator) and read the RTC.
pub fn init() {
    let divisor = (PIT_FREQUENCY / HZ) as u16;
    unsafe {
        outb(0x43, 0x34); // channel 0, lobyte/hibyte, mode 2
        outb(0x40, divisor as u8);
        outb(0x40, (divisor >> 8) as u8);
    }
    if let Some(t) = read_rtc() {
        BOOT_UNIX.store(t, Ordering::Relaxed);
        BOOT_TICK.store(TICKS.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// Called from the IRQ0 handler.
pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    crate::console::on_tick();
}

pub fn uptime_ms() -> u64 {
    TICKS.load(Ordering::Relaxed) * 1000 / HZ
}

/// Seconds since 1970-01-01 UTC, if the machine has a readable clock.
pub fn now_unix() -> Option<u64> {
    let boot = BOOT_UNIX.load(Ordering::Relaxed);
    let ticks = TICKS.load(Ordering::Relaxed) - BOOT_TICK.load(Ordering::Relaxed);
    (boot != 0).then(|| boot + ticks / HZ)
}

/// Milliseconds since 1970-01-01 UTC, if the clock is set.
pub fn now_unix_ms() -> Option<u64> {
    let boot = BOOT_UNIX.load(Ordering::Relaxed);
    let ticks = TICKS.load(Ordering::Relaxed) - BOOT_TICK.load(Ordering::Relaxed);
    (boot != 0).then(|| boot * 1000 + ticks * 1000 / HZ)
}

/// Set the clock (from a time server). Uptime and timeouts are not affected.
pub fn set_unix(unix_secs: u64, millis: u32) {
    let ticks = TICKS.load(Ordering::Relaxed);
    // "Now" is `millis` past the full second: pretend the second began that many ticks ago.
    BOOT_TICK.store(ticks.saturating_sub(millis as u64 * HZ / 1000), Ordering::Relaxed);
    BOOT_UNIX.store(unix_secs, Ordering::Relaxed);
}

/// Current time as `2026-11-14T15:04:05Z`, or "unknown".
pub fn timestamp() -> String {
    now_unix().map(iso8601).unwrap_or_else(|| String::from("unknown"))
}

pub fn iso8601(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86400) as i64);
    let s = unix % 86400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

/// The time zone for local time (`timezone`): UTC, a fixed offset, or
/// Central European Time with the EU's summer time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Utc,
    /// Minutes east of UTC.
    Fixed(i32),
    Cet,
}

static ZONE: crate::ring::Global<Zone> = crate::ring::Global::new(Zone::Utc);

pub fn zone() -> Zone {
    *ZONE.get()
}

pub fn set_zone(zone: Zone) {
    *ZONE.get() = zone;
}

/// "UTC", "CET", "+05:30", "-03:00" (any case, around spaces ignored).
pub fn parse_zone(text: &str) -> Option<Zone> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("utc") || text.eq_ignore_ascii_case("gmt") {
        return Some(Zone::Utc);
    }
    if text.eq_ignore_ascii_case("cet") || text.eq_ignore_ascii_case("cest") {
        return Some(Zone::Cet);
    }
    // "UTC+05:30" (as `zone_name` writes it) is "+05:30".
    let text = match text.get(..3) {
        Some(head) if head.eq_ignore_ascii_case("utc") => &text[3..],
        _ => text,
    };
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (h, m) = text[1..].split_once(':').unwrap_or((&text[1..], "0"));
    let all_digits = |s: &str| !s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(h) || !all_digits(m) {
        return None;
    }
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    if h > 14 || m > 59 || (h == 14 && m > 0) {
        return None;
    }
    Some(if h == 0 && m == 0 { Zone::Utc } else { Zone::Fixed(sign * (h * 60 + m)) })
}

/// The last Sunday of a month, at 01:00 UTC, as Unix time (EU summer time
/// starts and ends then).
fn last_sunday_1am(year: i64, month: u32) -> u64 {
    let last = days_from_civil(year, month, 31); // March and October have 31 days
    let weekday = (last + 4).rem_euclid(7); // 1970-01-01 was a Thursday; 0 = Sunday
    ((last - weekday) * 86400 + 3600) as u64
}

/// Minutes east of UTC at `unix`, and the zone's short name then.
pub fn offset(zone: Zone, unix: u64) -> (i32, String) {
    match zone {
        Zone::Utc => (0, String::from("UTC")),
        Zone::Fixed(minutes) => {
            let (sign, m) = if minutes < 0 { ('-', -minutes) } else { ('+', minutes) };
            (minutes, format!("UTC{sign}{:02}:{:02}", m / 60, m % 60))
        }
        Zone::Cet => {
            let (year, _, _) = civil_from_days((unix / 86400) as i64);
            let summer = unix >= last_sunday_1am(year, 3) && unix < last_sunday_1am(year, 10);
            if summer { (120, String::from("CEST")) } else { (60, String::from("CET")) }
        }
    }
}

/// The zone as it is written in `/system/timezone` and by `timezone`.
pub fn zone_name(zone: Zone) -> String {
    match zone {
        Zone::Cet => String::from("CET"),
        other => offset(other, 0).1,
    }
}

/// Local time: "2026-11-18 20:05:31 CET".
pub fn local(zone: Zone, unix: u64) -> String {
    let (minutes, name) = offset(zone, unix);
    let t = (unix as i64 + minutes as i64 * 60).max(0) as u64;
    let (y, m, d) = civil_from_days((t / 86400) as i64);
    let s = t % 86400;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} {name}", s / 3600, s / 60 % 60, s % 60)
}

/// "3d 4h 05m 06s"-style duration.
pub fn format_duration(ms: u64) -> String {
    let s = ms / 1000;
    let (d, h, m, s) = (s / 86400, s / 3600 % 24, s / 60 % 60, s % 60);
    match (d, h) {
        (0, 0) => format!("{m}m {s:02}s"),
        (0, _) => format!("{h}h {m:02}m {s:02}s"),
        _ => format!("{d}d {h}h {m:02}m {s:02}s"),
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of `days_from_civil`: (year, month, day).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

// --- CMOS real-time clock ------------------------------------------------------

fn cmos(reg: u8) -> u8 {
    unsafe {
        outb(0x70, reg);
        inb(0x71)
    }
}

fn rtc_fields() -> Option<[u8; 6]> {
    // Wait (bounded) until no update is in progress.
    (0..100_000).find(|_| cmos(0x0A) & 0x80 == 0)?;
    Some([cmos(0x00), cmos(0x02), cmos(0x04), cmos(0x07), cmos(0x08), cmos(0x09)])
}

fn read_rtc() -> Option<u64> {
    // Read until two readings agree (the clock may tick in between).
    let mut last = rtc_fields()?;
    for _ in 0..10 {
        let now = rtc_fields()?;
        if now == last {
            break;
        }
        last = now;
    }
    let reg_b = cmos(0x0B);
    let bcd = reg_b & 0x04 == 0;
    let conv = |v: u8| if bcd { (v & 0x0F) + (v >> 4) * 10 } else { v };
    let [sec, min, hour_raw, day, month, year] = last;
    let pm = hour_raw & 0x80 != 0;
    let mut hour = conv(hour_raw & 0x7F) as u64;
    if reg_b & 0x02 == 0 {
        // 12-hour clock
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let (day, month, year) = (conv(day) as u32, conv(month) as u32, 2000 + conv(year) as i64);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days as u64 * 86400 + hour * 3600 + conv(min) as u64 * 60 + conv(sec) as u64)
}

// --- precise time (for ping round trips) -------------------------------------------

static TSC_PER_MS: AtomicU64 = AtomicU64::new(0);

pub fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

/// Timestamp-counter ticks per millisecond, measured once against the 100 Hz
/// timer (takes about 60 ms the first time; interrupts must be enabled).
fn tsc_per_ms() -> u64 {
    let known = TSC_PER_MS.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }
    let wait_for_tick = |t: u64| {
        while TICKS.load(Ordering::Relaxed) < t {
            core::hint::spin_loop();
        }
    };
    let start = TICKS.load(Ordering::Relaxed) + 1;
    wait_for_tick(start); // start on a tick edge
    let t0 = rdtsc();
    wait_for_tick(start + 5);
    let per_ms = ((rdtsc() - t0) / (5 * 1000 / HZ)).max(1);
    TSC_PER_MS.store(per_ms, Ordering::Relaxed);
    per_ms
}

/// The speed of the processor's clock (its timestamp counter) in MHz.
pub fn tsc_mhz() -> u64 {
    tsc_per_ms() / 1000
}

/// Microseconds since an arbitrary start; for measuring short intervals.
pub fn micros() -> u64 {
    let per_ms = tsc_per_ms();
    (rdtsc() as u128 * 1000 / per_ms as u128) as u64
}
