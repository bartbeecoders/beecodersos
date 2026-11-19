//! `update`: fetch a new kernel from beehive (the AI host) and put it on
//! trial. The host offers a kernel (`beehive.py offer`); this machine asks,
//! downloads it over the link it already has (serial or network), checks
//! its SHA-256, writes it into the spare kernel slot and marks it "on
//! trial". The A/B rollback (bootslots.rs) does the rest at the next boot.
//!
//! With `update auto on` a thread asks by itself every minute and restarts
//! the machine when it has fetched a kernel: the host can then deploy a new
//! build without anyone at the keyboard. Off unless switched on at the console.
//!
//! Over a serial link (a VM's COM2) a kernel takes most of a minute: the
//! port's speed. Over the network it takes seconds.
//!
//! Who may send a kernel: whoever is at the other end of the link. Over the
//! network that is the beehive this machine was paired with (every line is
//! sealed with the pairing key, beenet::hive). There is no further signature.

use crate::aibridge;
use crate::bootslots::{self, Phase};
use crate::install;
use crate::json::Obj;
use crate::ring::Global;
use crate::{thread, time};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use beenet::{base64, hive, sha256};

/// No kernel is larger than this (a slot file is 4 MiB).
const MAX_KERNEL: usize = 4 << 20;
/// Auto mode: how soon to look again while the host is not there yet, and
/// the time between looks once it is.
const TRY_AGAIN_MS: u64 = 5_000;
const LOOK_EVERY_MS: u64 = 60_000;

static AUTO: Global<bool> = Global::new(false);
static THREAD: Global<bool> = Global::new(false);

/// What a look for an update found.
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    /// Nothing newer on offer.
    Nothing,
    /// This build is on the disk already (running, or tried before).
    Known(u32, &'static str),
    /// Fetched and written: (build, slot). It is tried at the next boot.
    Ready(u32, u8),
}

/// Should a kernel on offer be fetched? `running`: this kernel's build;
/// `state`: (phase, build) of the last trial on the boot disk.
pub fn wanted(offered: u32, running: u32, state: (Phase, u32)) -> Result<(), &'static str> {
    if offered == running {
        return Err("it is the kernel that is running");
    }
    match state {
        (Phase::Failed, build) if build == offered => Err("it was tried on this machine and failed"),
        (Phase::Ok, build) if build == offered => Err("it is installed already"),
        (Phase::Pending | Phase::Armed, _) => Err("another kernel is still on trial"),
        _ => Ok(()),
    }
}

/// Ask the host for a newer kernel; fetch, check and install it.
pub fn fetch(mut step: impl FnMut(&str)) -> Result<Found, String> {
    let mut disk = install::boot_disk().ok_or("this system does not run from a disk with kernel slots (see install)")?;
    let state = bootslots::state().ok_or("the boot disk has no slot state")?;
    let running: u32 = crate::OS_BUILD.parse().unwrap_or(0);
    let offer = aibridge::call("update-check", Obj::new().str("build", crate::OS_BUILD), "update", 5_000)?;
    let Some(build) = offer.num("build").filter(|b| *b > 0 && *b < 1_000_000).map(|b| b as u32) else {
        return Ok(Found::Nothing);
    };
    if let Err(why) = wanted(build, running, (state.phase, state.build)) {
        return Ok(Found::Known(build, why));
    }
    let size = offer.num("size").unwrap_or(0) as usize;
    let digest = offer.str("sha256").unwrap_or("");
    if !(1024..=MAX_KERNEL).contains(&size) || digest.len() != 64 {
        return Err(String::from("the host's offer makes no sense"));
    }
    step(&format!("fetching kernel b{build} ({} KiB)", size / 1024));
    let mut kernel: Vec<u8> = Vec::with_capacity(size);
    // The host sends pieces of one size (learnt from the first one); then
    // BATCH pieces are asked for at once.
    const BATCH: usize = 8;
    let mut chunk = 0usize;
    while kernel.len() < size {
        let wanted = if chunk == 0 { 1 } else { (size - kernel.len()).div_ceil(chunk).min(BATCH) };
        let asks = (0..wanted).map(|i| Obj::new().num("offset", (kernel.len() + i * chunk) as i64)).collect();
        for part in aibridge::call_many("update-chunk", asks, "update-data", 10_000)? {
            let data = part.str("data").and_then(base64::decode).unwrap_or_default();
            if part.num("offset") != Some(kernel.len() as i64) || data.is_empty() || kernel.len() + data.len() > size {
                return Err(String::from("the download went wrong (a piece was missing or too long)"));
            }
            chunk = chunk.max(data.len());
            kernel.extend_from_slice(&data);
        }
    }
    if !hive::same(hive::hex(&sha256::sha256(&kernel)).as_bytes(), digest.as_bytes()) {
        return Err(String::from("what arrived is not the kernel that was offered (the check sum differs)"));
    }
    let slot = install::write_trial(&mut disk, &kernel, build, step)?;
    Ok(Found::Ready(build, slot))
}

pub fn auto() -> bool {
    *AUTO.get()
}

/// `update auto on|off`: look for kernels in the background and restart to
/// try one. The thread starts at the first "on" and idles while off.
pub fn set_auto(on: bool) {
    *AUTO.get() = on;
    if on && !*THREAD.get() {
        *THREAD.get() = thread::spawn("update", None, watch).is_some();
    }
}

fn watch() {
    let mut next = time::uptime_ms() + TRY_AGAIN_MS;
    loop {
        thread::wait_until(|| time::uptime_ms() >= next);
        next = time::uptime_ms() + TRY_AGAIN_MS;
        if !*AUTO.get() || !aibridge::enabled() || aibridge::peer().is_none() {
            continue;
        }
        next = time::uptime_ms() + LOOK_EVERY_MS;
        if let Ok(Found::Ready(build, slot)) = fetch(|_| {}) {
            println!("\n[update] kernel b{build} fetched into slot {}: restarting to try it", slot as char);
            // Let the line reach the screen and the host, then go.
            thread::sleep_ms(500);
            crate::power::reboot();
        }
    }
}
