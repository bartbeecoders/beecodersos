//! The clock at the top right of the desktop: a thread ("clock") that
//! shows the local time (`time::zone`) and changes it when the minute does,
//! with the soonest timer and what is left of it.

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

/// What the clock adds for the soonest timer: " · tea 4m" (whole minutes,
/// rounded up; the message cut to 12 characters, "timer" without one).
pub fn timer_note(left_ms: u64, message: &str) -> String {
    let minutes = left_ms.div_ceil(60_000);
    let cut: String = message.trim().chars().take(12).collect();
    let name = if cut.trim().is_empty() { String::from("timer") } else { String::from(cut.trim_end()) };
    let left = if minutes >= 60 { format!("{}h{:02}", minutes / 60, minutes % 60) } else { format!("{minutes}m") };
    format!(" · {name} {left}")
}

/// Show the clock again now (after `timezone`, or a timer that changed).
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
        let mut shown = text(time::zone(), now);
        if let Some((left, message)) = crate::timer::list().into_iter().next().map(|(_, left, m)| (left, m)) {
            shown += &timer_note(left, &message);
        }
        console::set_clock(&shown);
        let minute = now / 60;
        thread::pause(|| AGAIN.swap(false, Ordering::SeqCst) || time::now_unix().is_some_and(|t| t / 60 != minute));
    });
}
