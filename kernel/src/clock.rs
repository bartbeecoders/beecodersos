//! The clock at the top right of the desktop: a thread ("clock") that
//! shows the local time (`time::zone`) and changes it when the minute does.

use crate::{console, thread, time};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

/// Set to show the clock again now (the time zone changed).
static AGAIN: AtomicBool = AtomicBool::new(false);

/// What the clock shows at `unix` in `zone`: "Wed 18 Nov 20:05".
pub fn text(zone: time::Zone, unix: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (minutes, _) = time::offset(zone, unix);
    let t = (unix as i64 + minutes as i64 * 60).max(0) as u64;
    let days = (t / 86400) as i64;
    let (_, m, d) = time::civil_from_days(days);
    let s = t % 86400;
    format!("{} {d} {} {:02}:{:02}", DAYS[days.rem_euclid(7) as usize], MONTHS[(m - 1) as usize % 12], s / 3600, s / 60 % 60)
}

/// Show the clock again now (after `timezone`).
pub fn refresh() {
    AGAIN.store(true, Ordering::SeqCst);
}

/// Start the clock's thread. Without a real-time clock there is nothing to show.
pub fn start() {
    if time::now_unix().is_none() {
        return;
    }
    thread::spawn("clock", None, || loop {
        let Some(now) = time::now_unix() else { return };
        console::set_clock(&text(time::zone(), now));
        let minute = now / 60;
        thread::pause(|| AGAIN.swap(false, Ordering::SeqCst) || time::now_unix().is_some_and(|t| t / 60 != minute));
    });
}
