//! USB: what does not depend on the host controller. Descriptors (what a
//! device says about itself), the keyboard and mouse "boot protocol", and
//! the list of devices for `lsusb`. The controller driver is xhci.rs.
//!
//! Everything a device sends is untrusted: every length is checked.
//!
//! Supported: keyboards and mice that speak the boot protocol, plugged
//! straight into the machine. Not yet: hubs, sticks and disks.

use crate::ring::Global;
use alloc::string::String;
use alloc::vec::Vec;

pub const CLASS_HID: u8 = 3;
pub const CLASS_HUB: u8 = 9;
const SUBCLASS_BOOT: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Keyboard,
    Mouse,
}

/// One keyboard or mouse function of a device: where its reports come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidInterface {
    pub number: u8,
    pub kind: Kind,
    /// Endpoint address (bit 7 set: towards the computer).
    pub endpoint: u8,
    pub max_packet: u16,
    /// As the device gives it: milliseconds, or a power of two at high speed.
    pub interval: u8,
}

/// What a configuration descriptor says, as far as we use it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// The number to select this configuration with.
    pub value: u8,
    pub interfaces: Vec<HidInterface>,
    /// (class, subclass, protocol) of every interface, for `lsusb`.
    pub classes: Vec<(u8, u8, u8)>,
}

/// Most interfaces we look at in one device; a longer list is cut off.
const MAX_INTERFACES: usize = 16;

/// Read a configuration descriptor: a row of smaller descriptors, each
/// starting with its length and type. Anything cut short or with a length
/// that makes no sense ends the walk; what was understood so far is kept.
pub fn parse_config(data: &[u8]) -> Config {
    let mut config = Config::default();
    if data.len() < 9 || data[0] < 9 || data[1] != 2 {
        return config;
    }
    config.value = data[5];
    let mut at = data[0] as usize;
    // The interface the endpoints that follow belong to: (number, kind).
    let mut current: Option<(u8, Kind)> = None;
    while at + 2 <= data.len() {
        let (len, kind) = (data[at] as usize, data[at + 1]);
        if len < 2 || at + len > data.len() {
            break;
        }
        let d = &data[at..at + len];
        match kind {
            4 if len >= 9 => {
                current = None;
                if config.classes.len() >= MAX_INTERFACES {
                    break;
                }
                config.classes.push((d[5], d[6], d[7]));
                if d[5] == CLASS_HID && d[6] == SUBCLASS_BOOT && d[3] == 0 {
                    current = match d[7] {
                        1 => Some((d[2], Kind::Keyboard)),
                        2 => Some((d[2], Kind::Mouse)),
                        _ => None,
                    };
                }
            }
            5 if len >= 7 => {
                // The first interrupt endpoint towards the computer carries the reports.
                let max_packet = u16::from_le_bytes([d[4], d[5]]) & 0x7FF;
                if let Some((number, kind)) = current {
                    if d[2] & 0x80 != 0 && d[2] & 0x0F != 0 && d[3] & 3 == 3 && max_packet > 0 {
                        config.interfaces.push(HidInterface { number, kind, endpoint: d[2], max_packet, interval: d[6] });
                        current = None;
                    }
                }
            }
            _ => {}
        }
        at += len;
    }
    config
}

/// The name of a device class, for `lsusb`.
pub fn class_name(class: u8, subclass: u8, protocol: u8) -> &'static str {
    match (class, subclass, protocol) {
        (CLASS_HID, SUBCLASS_BOOT, 1) => "keyboard",
        (CLASS_HID, SUBCLASS_BOOT, 2) => "mouse",
        (CLASS_HID, _, _) => "input device (no boot protocol)",
        (CLASS_HUB, _, _) => "hub",
        (8, _, _) => "storage",
        (1, _, _) => "audio",
        (0x0E, _, _) => "camera",
        (0xE0, _, _) => "wireless",
        _ => "other device",
    }
}

// --- keyboard ----------------------------------------------------------------------

/// A key number of the USB keyboard tables -> the scancode a PS/2 keyboard
/// sends for the same key (set 1), and whether it has the 0xE0 prefix.
/// The rest of the kernel (layouts, dead keys) then works unchanged.
pub fn usage_to_scancode(usage: u8) -> Option<(bool, u8)> {
    const LETTERS: [u8; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, // a-m
        0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C, // n-z
    ];
    // 0x28..=0x38: Enter Esc Backspace Tab Space - = [ ] \ # ; ' ` , . /
    const PUNCTUATION: [u8; 17] =
        [0x1C, 0x01, 0x0E, 0x0F, 0x39, 0x0C, 0x0D, 0x1A, 0x1B, 0x2B, 0x2B, 0x27, 0x28, 0x29, 0x33, 0x34, 0x35];
    // 0x59..=0x63: keypad 1-9, 0 and .
    const KEYPAD: [u8; 11] = [0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49, 0x52, 0x53];
    Some(match usage {
        0x04..=0x1D => (false, LETTERS[(usage - 0x04) as usize]),
        0x1E..=0x27 => (false, 0x02 + (usage - 0x1E)), // 1-9, 0
        0x28..=0x38 => (false, PUNCTUATION[(usage - 0x28) as usize]),
        0x39 => (false, 0x3A),                         // Caps Lock
        0x3A..=0x43 => (false, 0x3B + (usage - 0x3A)), // F1-F10
        0x44 => (false, 0x57),
        0x45 => (false, 0x58),
        0x47 => (false, 0x46), // Scroll Lock
        0x49 => (true, 0x52),  // Insert
        0x4A => (true, 0x47),  // Home
        0x4B => (true, 0x49),  // Page Up
        0x4C => (true, 0x53),  // Delete
        0x4D => (true, 0x4F),  // End
        0x4E => (true, 0x51),  // Page Down
        0x4F => (true, 0x4D),  // Right
        0x50 => (true, 0x4B),  // Left
        0x51 => (true, 0x50),  // Down
        0x52 => (true, 0x48),  // Up
        0x53 => (false, 0x45), // Num Lock
        0x54 => (true, 0x35),  // keypad /
        0x55 => (false, 0x37),
        0x56 => (false, 0x4A),
        0x57 => (false, 0x4E),
        0x58 => (true, 0x1C), // keypad Enter
        0x59..=0x63 => (false, KEYPAD[(usage - 0x59) as usize]),
        0x64 => (false, 0x56), // the extra key next to left Shift
        0x65 => (true, 0x5D),  // Menu
        _ => return None,
    })
}

/// The eight modifier bits of a report: left Ctrl, Shift, Alt, GUI, then the right ones.
const MODIFIERS: [(bool, u8); 8] =
    [(false, 0x1D), (false, 0x2A), (false, 0x38), (true, 0x5B), (true, 0x1D), (false, 0x36), (true, 0x38), (true, 0x5C)];

fn push_key(out: &mut Vec<u8>, key: (bool, u8), pressed: bool) {
    if key.0 {
        out.push(0xE0);
    }
    out.push(if pressed { key.1 } else { key.1 | 0x80 });
}

/// Compare two keyboard reports (8 bytes: modifier bits, a spare byte, up
/// to six keys held down) and give the PS/2 scancodes for what changed:
/// releases first, then presses. A report that says "too many keys" changes nothing.
pub fn keyboard_changes(old: &[u8; 8], new: &[u8; 8]) -> Vec<u8> {
    let mut out = Vec::new();
    if new[2..].contains(&1) {
        return out;
    }
    for (bit, key) in MODIFIERS.iter().enumerate() {
        let (was, is) = (old[0] >> bit & 1, new[0] >> bit & 1);
        if was != is {
            push_key(&mut out, *key, is == 1);
        }
    }
    for &usage in old[2..].iter().filter(|u| **u > 3 && !new[2..].contains(u)) {
        if let Some(key) = usage_to_scancode(usage) {
            push_key(&mut out, key, false);
        }
    }
    for &usage in new[2..].iter().filter(|u| **u > 3 && !old[2..].contains(u)) {
        if let Some(key) = usage_to_scancode(usage) {
            push_key(&mut out, key, true);
        }
    }
    out
}

/// The key that repeats while held: the newest one in the report (the last
/// non-empty place), unless nothing but modifiers is down.
pub fn repeat_key(report: &[u8; 8]) -> Option<u8> {
    report[2..].iter().rev().copied().find(|u| *u > 3)
}

/// Before a held key repeats, and between repeats (milliseconds).
pub const REPEAT_DELAY_MS: u64 = 500;
pub const REPEAT_EVERY_MS: u64 = 40;

// --- mouse -------------------------------------------------------------------------

/// A mouse report in the boot protocol: buttons (bit 0 left, 1 right,
/// 2 middle), then how far it moved right and down. Shorter reports are ignored.
pub fn mouse_report(data: &[u8]) -> Option<(u8, i32, i32)> {
    if data.len() < 3 {
        return None;
    }
    Some((data[0] & 7, data[1] as i8 as i32, data[2] as i8 as i32))
}

// --- what `lsusb` shows ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub port: u8,
    pub speed: &'static str,
    pub vendor: u16,
    pub product: u16,
    /// What it is ("keyboard", "mouse + keyboard", ...).
    pub what: String,
    /// "in use", or why not.
    pub state: String,
}

/// The controller in one line, and the devices: written by the usb thread.
pub static STATUS: Global<Option<String>> = Global::new(None);
pub static DEVICES: Global<Vec<DeviceInfo>> = Global::new(Vec::new());
/// Set once the controller has looked at all its ports for the first time
/// (or turned out not to work).
pub static READY: Global<bool> = Global::new(false);

pub fn describe() -> String {
    let Some(status) = STATUS.get().as_ref() else {
        return String::from("no USB controller in use (supported: xHCI; see lspci)");
    };
    let mut text = status.clone();
    for d in DEVICES.get().iter() {
        text += &alloc::format!("\nport {}: {:04x}:{:04x} {}, {} ({})", d.port, d.vendor, d.product, d.what, d.speed, d.state);
    }
    if DEVICES.get().is_empty() {
        text += "\nno devices";
    }
    text
}
