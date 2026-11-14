//! In-kernel self-tests: check kernel code that only runs inside the OS
//! (heap, queues, input decoders, shell parsing, BeeFS in the kernel).
//!
//! Run them with the `selftest` shell command, or at boot with `selftest` on
//! the kernel command line (`make check` does this in QEMU).
//!
//! Add a test: write `fn name() -> TestResult` below and list it in `TESTS`.
//! Tests must not touch hardware or the mounted disk.

use crate::input::Key;
use crate::layouts::{compose, Layout};
use crate::ring::Ring;
use crate::vga::Color;
use crate::json::{self, Obj, Value};
use crate::bootslots::{self, Phase};
use crate::{allocator, console, keyboard, serial, shell, time};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use beefs::{BlockDevice, FileKind, FileSystem, FsError, RamDisk};

type TestResult = Result<(), String>;

macro_rules! check {
    ($cond:expr) => {
        if !$cond {
            return Err(format!("line {}: check failed: {}", line!(), stringify!($cond)));
        }
    };
}

macro_rules! check_eq {
    ($left:expr, $right:expr) => {
        // `match` keeps temporaries alive for the comparison.
        match (&$left, &$right) {
            (l, r) => {
                if l != r {
                    return Err(format!("line {}: {} == {}\n      left:  {:?}\n      right: {:?}",
                        line!(), stringify!($left), stringify!($right), l, r));
                }
            }
        }
    };
}

const TESTS: &[(&str, fn() -> TestResult)] = &[
    ("heap: alloc and free", heap_alloc_free),
    ("heap: 1 MiB buffer", heap_large_buffer),
    ("ring: order and overflow", ring_buffer),
    ("serial: text and control keys", serial_decoder),
    ("serial: escape sequences and UTF-8", serial_escapes),
    ("keyboard: US layout", keyboard_us),
    ("keyboard: Belgian layout and dead keys", keyboard_be),
    ("keyboard: navigation and Ctrl", keyboard_nav),
    ("layouts: compose accents", layouts_compose),
    ("shell: tokenize", shell_tokenize),
    ("beefs: files and directories in RAM", beefs_ramdisk),
    ("json: build and escape", json_build),
    ("json: parse", json_parse),
    ("time: calendar conversion", time_calendar),
    ("shell: did-you-mean distance", shell_edit_distance),
    ("shell: calc expressions", shell_calc),
    ("slots: parse and patch limine.conf", slots_state),
];

/// Run every test silently (the health check of a trial kernel).
pub fn passes() -> bool {
    TESTS.iter().all(|(_, test)| test().is_ok())
}

/// Run every test and print the results. Returns true when all passed.
pub fn run() -> bool {
    println!("[test] running {} self-tests", TESTS.len());
    let mut failed = 0;
    for (name, test) in TESTS {
        match test() {
            Ok(()) => {
                console::set_color(Color::LightGreen, Color::Black);
                print!("  ok    ");
                console::set_color(Color::LightGray, Color::Black);
                println!("{name}");
            }
            Err(msg) => {
                failed += 1;
                console::set_color(Color::LightRed, Color::Black);
                print!("  FAIL  ");
                console::set_color(Color::LightGray, Color::Black);
                println!("{name}\n      {msg}");
            }
        }
    }
    println!("selftest: {} passed, {} failed", TESTS.len() - failed, failed);
    failed == 0
}

// --- tests ---------------------------------------------------------------------

fn heap_alloc_free() -> TestResult {
    let before = allocator::used();
    {
        let v: Vec<u64> = (0..4096).collect();
        let b = Box::new([7u8; 512]);
        check!(allocator::used() >= before + 4096 * 8 + 512);
        check_eq!(v[4095], 4095);
        check_eq!(b[511], 7);
    }
    check_eq!(allocator::used(), before);
    Ok(())
}

fn heap_large_buffer() -> TestResult {
    let mut buf = vec![0u8; 1 << 20];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    check!(buf.iter().enumerate().all(|(i, &b)| b == (i % 251) as u8));
    Ok(())
}

fn ring_buffer() -> TestResult {
    let r: Ring<4> = Ring::new(); // holds N-1 = 3 bytes
    check!(r.is_empty());
    for b in 1..=4 {
        r.push(b); // the 4th byte is dropped: queue full
    }
    check_eq!(r.pop(), Some(1));
    r.push(5);
    check_eq!([r.pop(), r.pop(), r.pop(), r.pop()], [Some(2), Some(3), Some(5), None]);
    check!(r.is_empty());
    Ok(())
}

fn serial_decoder() -> TestResult {
    use Key::*;
    check_eq!(serial::decode_for_test(b"ls\r\n"), vec![Char('l'), Char('s'), Enter]);
    check_eq!(serial::decode_for_test(b"a\nb"), vec![Char('a'), Enter, Char('b')]);
    check_eq!(serial::decode_for_test(&[0x7F, 0x08, 0x03, 0x01, b'\t']),
        vec![Backspace, Backspace, Ctrl('c'), Ctrl('a'), Tab]);
    Ok(())
}

fn serial_escapes() -> TestResult {
    use Key::*;
    check_eq!(serial::decode_for_test(b"\x1b[A\x1b[B\x1b[C\x1b[D"), vec![Up, Down, Right, Left]);
    check_eq!(serial::decode_for_test(b"\x1b[3~\x1b[1~\x1b[4~\x1bOH"), vec![Delete, Home, End, Home]);
    check_eq!(serial::decode_for_test("é€x".as_bytes()), vec![Char('é'), Char('€'), Char('x')]);
    check_eq!(serial::decode_for_test(b"\x1b[99~z"), vec![Char('z')]); // unknown sequence ignored
    Ok(())
}

fn keyboard_us() -> TestResult {
    use Key::*;
    // a, Shift+a, 1, Shift+1, space (with key releases in between).
    let codes = [0x1E, 0x9E, 0x2A, 0x1E, 0x9E, 0xAA, 0x02, 0x82, 0x36, 0x02, 0x82, 0xB6, 0x39, 0xB9];
    check_eq!(keyboard::decode_for_test(Layout::Us, &codes), vec![Char('a'), Char('A'), Char('1'), Char('!'), Char(' ')]);
    check_eq!(keyboard::decode_for_test(Layout::Us, &[0x1C, 0x0E, 0x0F, 0x01]), vec![Enter, Backspace, Tab, Escape]);
    Ok(())
}

fn keyboard_be() -> TestResult {
    use Key::*;
    // AZERTY: the key where US has 'q' types 'a'; the US '1' key types '&'.
    check_eq!(keyboard::decode_for_test(Layout::BelgianPeriod, &[0x10, 0x02]), vec![Char('a'), Char('&')]);
    // AltGr (E0 38) + the '2' key = '@'.
    check_eq!(keyboard::decode_for_test(Layout::BelgianPeriod, &[0xE0, 0x38, 0x03, 0x83, 0xE0, 0xB8]), vec![Char('@')]);
    // Dead key '^' then 'e' = 'ê'; '^' then 'x' = '^', 'x'; '^' then space = '^'.
    check_eq!(keyboard::decode_for_test(Layout::BelgianPeriod, &[0x1A, 0x12]), vec![Char('ê')]);
    check_eq!(keyboard::decode_for_test(Layout::BelgianPeriod, &[0x1A, 0x2D]), vec![Char('^'), Char('x')]);
    check_eq!(keyboard::decode_for_test(Layout::BelgianPeriod, &[0x1A, 0x39]), vec![Char('^')]);
    Ok(())
}

fn keyboard_nav() -> TestResult {
    use Key::*;
    // Arrow keys (E0 prefix), Ctrl+C, keypad digits with Num Lock on.
    let codes = [0xE0, 0x48, 0xE0, 0x50, 0xE0, 0x4B, 0xE0, 0x4D, 0x1D, 0x2E, 0x9D, 0x4F, 0x53];
    check_eq!(keyboard::decode_for_test(Layout::Us, &codes), vec![Up, Down, Left, Right, Ctrl('c'), Char('1'), Char('.')]);
    Ok(())
}

fn layouts_compose() -> TestResult {
    check_eq!(compose('^', 'e'), Some('ê'));
    check_eq!(compose('¨', 'U'), Some('Ü'));
    check_eq!(compose('´', 'y'), Some('ý'));
    check_eq!(compose('~', 'n'), Some('ñ'));
    check_eq!(compose('^', 'x'), None);
    Ok(())
}

fn shell_tokenize() -> TestResult {
    let words = |w: &[&str]| -> Result<Vec<String>, &'static str> { Ok(w.iter().map(|s| String::from(*s)).collect()) };
    check_eq!(shell::tokenize("  ls   /home  "), words(&["ls", "/home"]));
    check_eq!(shell::tokenize(r#"echo "a  b" c"#), words(&["echo", "a  b", "c"]));
    check_eq!(shell::tokenize(r#"touch """#), words(&["touch", ""]));
    check_eq!(shell::tokenize(""), words(&[]));
    check_eq!(shell::tokenize(r#"echo "open"#), Err("missing closing quote"));
    Ok(())
}

fn beefs_ramdisk() -> TestResult {
    let dev = Box::new(RamDisk::new(1024 * 1024)) as Box<dyn BlockDevice>;
    let mut fs = FileSystem::format(dev).map_err(|e| format!("format: {e}"))?;
    let e = |e: FsError| format!("{e}");
    fs.mkdir_all("/a/b").map_err(e)?;
    fs.write_file("/a/b/f.txt", b"hello").map_err(e)?;
    fs.append("/a/b/f.txt", b" bee").map_err(e)?;
    check_eq!(fs.read_file("/a/b/f.txt").map_err(e)?, b"hello bee".to_vec());

    // A file larger than the 10 direct blocks (uses the indirect block).
    let big: Vec<u8> = (0..20_000u32).map(|i| (i % 253) as u8).collect();
    fs.write_file("/a/big", &big).map_err(e)?;
    check_eq!(fs.metadata("/a/big").map_err(e)?.size, big.len() as u64);
    check!(fs.read_file("/a/big").map_err(e)? == big);

    fs.rename("/a/b/f.txt", "/a/g.txt").map_err(e)?;
    check!(!fs.exists("/a/b/f.txt"));
    let mut names: Vec<_> = fs.read_dir("/a").map_err(e)?.into_iter().map(|d| (d.name, d.kind)).collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    check_eq!(names, vec![
        (String::from("b"), FileKind::Directory),
        (String::from("big"), FileKind::File),
        (String::from("g.txt"), FileKind::File),
    ]);
    check_eq!(fs.remove("/a"), Err(FsError::DirectoryNotEmpty));
    check_eq!(fs.read_file("/nope"), Err(FsError::NotFound));
    let free_before = fs.stats().free_blocks;
    fs.remove("/a/big").map_err(e)?;
    check!(fs.stats().free_blocks > free_before);
    Ok(())
}

fn json_build() -> TestResult {
    let line = Obj::new().str("type", "ask").num("id", -7).bool("ok", true).str("text", "a \"q\"\n\\ é\u{1}").finish();
    check_eq!(line.as_str(), r#"{"type":"ask","id":-7,"ok":true,"text":"a \"q\"\n\\ é\u0001"}"#);
    check_eq!(Obj::new().finish().as_str(), "{}");
    Ok(())
}

fn json_parse() -> TestResult {
    let o = json::parse(r#" {"type":"line","id":12,"text":"caf\u00e9 \"x\"\n\ud83d\ude00 ok","f":false,"n":null,"x":1.5e3} "#)
        .ok_or("parse failed")?;
    check_eq!(o.str("type"), Some("line"));
    check_eq!(o.num("id"), Some(12));
    check_eq!(o.str("text"), Some("café \"x\"\n😀 ok"));
    check_eq!(o.get("f"), Some(&Value::Bool(false)));
    check_eq!(o.get("n"), Some(&Value::Null));
    check_eq!(o.num("x"), Some(1));
    check!(json::parse("{}").is_some());
    check!(json::parse("é{\"a\":1}").is_none());
    for bad in [r#"{"a":1"#, r#"{"a":[1]}"#, r#"{"a":{"b":1}}"#, r#"{"a" 1}"#, r#"{"a":1} x"#, r#"{"a":"\q"}"#] {
        check!(json::parse(bad).is_none());
    }
    // Round trip of the builder's own output.
    let line = Obj::new().str("k", "tab\tquote\" end").finish();
    check_eq!(json::parse(&line).ok_or("round trip")?.str("k"), Some("tab\tquote\" end"));
    Ok(())
}

fn time_calendar() -> TestResult {
    check_eq!(time::days_from_civil(1970, 1, 1), 0);
    check_eq!(time::days_from_civil(2000, 3, 1), 11017);
    check_eq!(time::civil_from_days(11016), (2000, 2, 29)); // leap day
    check_eq!(time::iso8601(1_794_668_645), "2026-11-14T15:04:05Z");
    for days in [-1000i64, 0, 59, 10957, 20000, 40000] {
        let (y, m, d) = time::civil_from_days(days);
        check_eq!(time::days_from_civil(y, m, d), days);
    }
    check_eq!(time::format_duration(65_000), "1m 05s");
    check_eq!(time::format_duration(90_061_000), "1d 1h 01m 01s");
    Ok(())
}

fn shell_calc() -> TestResult {
    use shell::calc;
    // precedence and parentheses; spacing does not matter
    check_eq!(calc("2 + 3"), Ok(5));
    check_eq!(calc("2+3*4"), Ok(14));
    check_eq!(calc("(2 + 3) * 4"), Ok(20));
    check_eq!(calc("10 - 4 - 3"), Ok(3));
    check_eq!(calc("100 / 10 / 5"), Ok(2));
    check_eq!(calc(" ((7)) "), Ok(7));
    // unary minus; / and % truncate toward zero
    check_eq!(calc("-7 / 2"), Ok(-3));
    check_eq!(calc("7 % 3"), Ok(1));
    check_eq!(calc("-7 % 3"), Ok(-1));
    check_eq!(calc("2 * -3"), Ok(-6));
    check_eq!(calc("-(2 + 3)"), Ok(-5));
    check_eq!(calc("--4"), Ok(4));
    check_eq!(calc("-9223372036854775808"), Ok(i64::MIN));
    // errors, never panics
    check_eq!(calc("1 / 0"), Err("division by zero"));
    check_eq!(calc("1 % 0"), Err("division by zero"));
    check_eq!(calc("9223372036854775807 + 1"), Err("overflow"));
    check_eq!(calc("99999999999999999999"), Err("overflow"));
    check_eq!(calc("-9223372036854775808 / -1"), Err("overflow"));
    check_eq!(calc("-9223372036854775808 % -1"), Err("overflow"));
    check_eq!(calc("-(-9223372036854775808)"), Err("overflow"));
    check_eq!(calc("4294967296 * 4294967296"), Err("overflow"));
    check_eq!(calc("2 +"), Err("parse error"));
    check_eq!(calc("abc"), Err("parse error"));
    check_eq!(calc(""), Err("parse error"));
    check_eq!(calc("2 3"), Err("parse error"));
    check_eq!(calc("(2 + 3"), Err("missing ')'"));
    check!(calc(&"(".repeat(1000)).is_err());
    check!(calc(&"-".repeat(1000)).is_err());
    Ok(())
}

fn shell_edit_distance() -> TestResult {
    check_eq!(shell::edit_distance("ls", "ls"), 0);
    check_eq!(shell::edit_distance("sl", "ls"), 2);
    check_eq!(shell::edit_distance("mkdri", "mkdir"), 2);
    check_eq!(shell::edit_distance("cat", "cart"), 1);
    check_eq!(shell::edit_distance("", "abc"), 3);
    Ok(())
}

fn slots_state() -> TestResult {
    // The first lines of limine.conf as tools/bootdisk.py writes them.
    let conf = b"#slots good=a trial=b state=pending build=000012\ndefault_entry: 2\n# rest\ntimeout: 0\n";
    let mut sector = [0u8; 512];
    sector[..conf.len()].copy_from_slice(conf);
    let st = bootslots::parse(&sector).ok_or("parse failed")?;
    check_eq!((st.good, st.trial, st.phase, st.build, st.default), (b'a', b'b', Phase::Pending, 12, b'b'));
    let armed = bootslots::State { phase: Phase::Armed, default: b'a', ..st };
    let before = sector;
    bootslots::render(&mut sector, &armed);
    check_eq!(&sector[..65], &b"#slots good=a trial=b state=armed   build=000012\ndefault_entry: 1"[..]);
    check_eq!(&sector[66..], &before[66..]); // nothing else changes
    check_eq!(bootslots::parse(&sector), Some(armed));
    let ok = bootslots::State { good: b'b', trial: b'-', phase: Phase::Ok, build: 7, default: b'b' };
    bootslots::render(&mut sector, &ok);
    check_eq!(bootslots::parse(&sector), Some(ok));
    sector[0] = b'x';
    check!(bootslots::parse(&sector).is_none());
    Ok(())
}
