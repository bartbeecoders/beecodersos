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
use crate::pci::{self, Bar};
use crate::{allocator, console, e1000, keyboard, net, rand, serial, shell, time};
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
    ("serial port: missing or stuck hardware never hangs", uart_limits),
    ("sata: command bytes and what a disk says about itself", sata_formats),
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
    ("pci: config address and BAR decoding", pci_decoding),
    ("display: sizes, boot flags, pixel formats, framebuffer checks", display_basics),
    ("display: the bootloader's framebuffer, also wrong and cut short", bootfb_parsing),
    ("display: text drawn as pixels in every pixel format", fbcon_drawing),
    ("display: wrap, scroll, clear and cursor on a framebuffer", fbcon_screen),
    ("display: odd framebuffer sizes never write outside", fbcon_bounds),
    ("desktop: rectangles and drawing stay inside the screen", gfx_canvas),
    ("desktop: on by default, boot flag before saved choice", desktop_setting),
    ("desktop: where the terminal window goes", wm_layout),
    ("desktop: background, window frames, terminal inside its window", wm_drawing),
    ("pci: capability lists, also broken ones", pci_capability_lists),
    ("interrupts: a device line that keeps firing is switched off", interrupt_storm_guard),
    ("net: frames, MAC text, descriptor rings", net_basics),
    ("rand: generator", rand_generator),
    ("net: address settings", net_config),
    ("net: two stacks ping each other (beenet in the kernel)", net_stack_ping),
    ("net: TCP connection between two stacks", net_stack_tcp),
    ("net: web addresses, time replies, host:port (phase N5)", net_clients),
    ("httpd: saved setting", httpd_setting),
    ("remote shell: setting and password check", remote_shell_guard),
    ("threads: take turns, sleep, end", threads_take_turns),
    ("threads: no switch while the file system is in use", threads_respect_the_file_system),
    ("threads: a second shell runs in its own session", second_shell_session),
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

fn sata_formats() -> TestResult {
    use crate::ahci::{command_fis, parse_identity, Identity};
    // Read one sector at a 48-bit address.
    let fis = command_fis(0x25, 0x1122_3344_5566, 1);
    check_eq!(fis[..4], [0x27, 0x80, 0x25, 0]);
    check_eq!(fis[4..7], [0x66, 0x55, 0x44]); // low half of the address
    check_eq!(fis[7], 0x40);
    check_eq!(fis[8..11], [0x33, 0x22, 0x11]); // high half
    check_eq!(fis[12..14], [1, 0]);
    check!(fis[14..].iter().all(|b| *b == 0));
    // A command without data (flush): no sector count.
    check_eq!(command_fis(0xEA, 0, 0)[12..14], [0, 0]);

    // What a disk answers to IDENTIFY: 256 words, text with swapped bytes.
    let mut id = [0u8; 512];
    let mut set = |word: usize, value: u16| id[word * 2..word * 2 + 2].copy_from_slice(&value.to_le_bytes());
    set(83, 1 << 10); // 48-bit addresses
    set(100, 0x5678);
    set(101, 0x1234);
    set(102, 0x0001);
    for (i, pair) in b"BeeDisk 3000    ".chunks(2).enumerate() {
        set(27 + i, (pair[0] as u16) << 8 | pair[1] as u16);
    }
    check_eq!(parse_identity(&id), Some(Identity { sectors: 0x1_1234_5678, model: String::from("BeeDisk 3000") }));
    // Not usable: no 48-bit addresses, no size, not a disk, all zeros, all ones.
    let mut bad = id;
    bad[83 * 2..83 * 2 + 2].fill(0);
    check_eq!(parse_identity(&bad), None);
    let mut bad = id;
    bad[100 * 2..104 * 2].fill(0);
    check_eq!(parse_identity(&bad), None);
    let mut bad = id;
    bad[1] = 0x80;
    check_eq!(parse_identity(&bad), None);
    check_eq!(parse_identity(&[0u8; 512]), None);
    check_eq!(parse_identity(&[0xFFu8; 512]), None);
    // Odd bytes in the model text never get through.
    let mut odd = id;
    odd[27 * 2..47 * 2].fill(0x07);
    check_eq!(parse_identity(&odd).map(|i| i.model), Some(String::new()));
    Ok(())
}

/// A made-up serial port at COM1 for `uart_limits`.
struct FakePort {
    /// Is there a chip at all? Without one every read gives 0xFF.
    there: bool,
    scratch: u8,
    /// What the line status register says.
    status: u8,
    reads: usize,
    sent: Vec<u8>,
}

impl crate::uart::Ports for FakePort {
    fn read(&mut self, port: u16) -> u8 {
        self.reads += 1;
        match port - crate::uart::COM1 {
            _ if !self.there => 0xFF,
            5 => self.status,
            7 => self.scratch,
            _ => b'x',
        }
    }
    fn write(&mut self, port: u16, value: u8) {
        match port - crate::uart::COM1 {
            0 if self.there => self.sent.push(value),
            7 => self.scratch = value,
            _ => {}
        }
    }
}

fn uart_limits() -> TestResult {
    use crate::uart::{Uart, COM1, RECEIVE_LIMIT, SEND_SPINS};
    let fake = |there, status| FakePort { there, scratch: 0, status, reads: 0, sent: Vec::new() };

    // No port: found missing, and after that it costs nothing at all.
    let (mut port, mut io) = (Uart::new(COM1), fake(false, 0));
    check!(!port.init(&mut io));
    io.reads = 0;
    check!(!port.send(&mut io, b'a'));
    port.receive(&mut io, |_| {});
    port.enable_rx_interrupt(&mut io);
    check_eq!(io.reads, 0);

    // A working port: found, set up (the divisor is not data), bytes go out.
    let (mut port, mut io) = (Uart::new(COM1), fake(true, 0x60));
    check!(port.init(&mut io));
    io.sent.clear();
    check!(port.send(&mut io, b'a') && port.send(&mut io, b'b'));
    check_eq!(io.sent.as_slice(), b"ab");

    // A port that never becomes ready: one bounded wait, then no waiting...
    io.status = 0x00;
    io.reads = 0;
    check!(!port.send(&mut io, b'c'));
    check_eq!(io.reads, SEND_SPINS);
    check!(!port.send(&mut io, b'd') && !port.send(&mut io, b'e'));
    check_eq!(io.reads, SEND_SPINS + 2);
    // ...until it works again by itself.
    io.status = 0x20;
    check!(port.send(&mut io, b'f'));
    check_eq!(io.sent.as_slice(), b"abf");

    // A port that claims to have data forever: reading stops.
    io.status = 0x01;
    let mut taken = 0;
    port.receive(&mut io, |_| taken += 1);
    check_eq!(taken, RECEIVE_LIMIT);

    // The scratch register answers but the status reads as 0xFF, as empty
    // port addresses do: not treated as a port.
    let (mut port, mut io) = (Uart::new(COM1), fake(true, 0xFF));
    check!(!port.init(&mut io));
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

fn interrupt_storm_guard() -> TestResult {
    use crate::interrupts::{set_device_handler, storm_step, STORM_LIMIT};
    // Claimed interrupts never switch a line off, however many.
    let mut count = 0;
    for _ in 0..3 * STORM_LIMIT {
        let (next, off) = storm_step(count, true);
        check!(!off);
        count = next;
    }
    check_eq!(count, 0);
    // Unclaimed ones do, at the limit and not before; one claimed starts over.
    for i in 1..STORM_LIMIT {
        let (next, off) = storm_step(count, false);
        check!(!off);
        check_eq!(next, i);
        count = next;
    }
    check_eq!(storm_step(count, true), (0, false));
    check_eq!(storm_step(count, false), (STORM_LIMIT, true));
    check_eq!(storm_step(u32::MAX, false), (u32::MAX, true)); // no overflow
    // The system's own lines and lines that do not exist are refused.
    fn never() -> bool {
        false
    }
    for irq in [0, 1, 2, 3, 4, 16, 255] {
        check!(!set_device_handler(irq, never));
    }
    Ok(())
}

fn pci_capability_lists() -> TestResult {
    // A configuration space with three capabilities: MSI at 0x50, PCI
    // Express at 0x70, power management at 0xC8.
    let mut space = [0u8; 256];
    space[0x06] = 1 << 4;
    space[0x34] = 0x50;
    space[0x50..0x52].copy_from_slice(&[pci::CAP_MSI, 0x70]);
    space[0x70..0x72].copy_from_slice(&[pci::CAP_PCIE, 0xC8]);
    space[0xC8..0xCA].copy_from_slice(&[pci::CAP_POWER, 0x00]);
    let walk = |space: &[u8; 256]| pci::walk_capabilities(|offset| space[offset as usize]);
    check_eq!(walk(&space), alloc::vec![(pci::CAP_MSI, 0x50), (pci::CAP_PCIE, 0x70), (pci::CAP_POWER, 0xC8)]);
    // The low two bits of a pointer are not part of it.
    space[0x34] = 0x53;
    check_eq!(walk(&space).len(), 3);
    // A list that points back into itself ends there.
    space[0xC9] = 0x50;
    check_eq!(walk(&space).len(), 3);
    space[0x51] = 0x50;
    check_eq!(walk(&space), alloc::vec![(pci::CAP_MSI, 0x50)]);
    // A pointer into the standard header, or to the last byte, ends it too.
    space[0x51] = 0x3C;
    check_eq!(walk(&space).len(), 1);
    space[0x51] = 0xFF;
    space[0xFC] = 0x09;
    space[0xFD] = 0xFC;
    check_eq!(walk(&space), alloc::vec![(pci::CAP_MSI, 0x50), (0x09, 0xFC)]);
    // No list at all: the status bit is off, or nothing answers.
    space[0x06] = 0;
    check!(walk(&space).is_empty());
    check!(pci::walk_capabilities(|_| 0xFF).is_empty());
    // Every pointer leads to another entry: the walk still ends.
    let mut chain = [0u8; 256];
    chain[0x06] = 1 << 4;
    chain[0x34] = 0x40;
    for at in (0x40..0x100).step_by(4) {
        chain[at] = 0x09;
        chain[at + 1] = if at == 0xFC { 0x40 } else { at as u8 + 4 };
    }
    check_eq!(walk(&chain).len(), pci::MAX_CAPABILITIES);
    check_eq!(pci::capability_name(pci::CAP_MSIX), Some("MSI-X"));
    check_eq!(pci::capability_name(0x77), None);
    Ok(())
}

fn pci_decoding() -> TestResult {
    check_eq!(pci::config_address(0, 0, 0, 0), 0x8000_0000);
    check_eq!(pci::config_address(0, 3, 0, 0x10), 0x8000_1810);
    check_eq!(pci::config_address(1, 31, 7, 0x3F), 0x8001_FF3C); // offset rounded down to 4
    check_eq!(pci::decode_bar(0xFEBC_0000, 0), Bar::Memory { addr: 0xFEBC_0000, is_64bit: false });
    check_eq!(pci::decode_bar(0xFE00_000C, 0x1), Bar::Memory { addr: 0x1_FE00_0000, is_64bit: true });
    check_eq!(pci::decode_bar(0x0000_C041, 0), Bar::Io { port: 0xC040 });
    check_eq!(pci::decode_bar(0, 0xFFFF_FFFF), Bar::None);
    let d = pci::Device { bus: 0, dev: 3, func: 0, vendor: 0x8086, device: 0x100E, class: 2, subclass: 0, irq: 11 };
    check_eq!(d.class_name(), "Ethernet controller");
    Ok(())
}

fn net_basics() -> TestResult {
    let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    check_eq!(net::format_mac(&mac).as_str(), "52:54:00:12:34:56");
    let f = net::ethernet_frame(&[0xFF; 6], &mac, net::ETHERTYPE_TEST, b"hi");
    check_eq!(f, vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x52, 0x54, 0x00, 0x12, 0x34, 0x56, 0x88, 0xB5, b'h', b'i']);
    // The card and the driver must agree on the descriptor layout.
    check_eq!(core::mem::size_of::<e1000::RxDesc>(), 16);
    check_eq!(core::mem::size_of::<e1000::TxDesc>(), 16);
    check_eq!(core::mem::align_of::<e1000::RxDesc>(), 16);
    check_eq!(e1000::ring_next(0, e1000::RX_RING), 1);
    check_eq!(e1000::ring_next(e1000::RX_RING - 1, e1000::RX_RING), 0);
    check!(net::MAX_FRAME <= e1000::BUFFER_SIZE);
    Ok(())
}

fn rand_generator() -> TestResult {
    let mut state = 1u64;
    check_eq!(rand::xorshift(&mut state), 0x47E4_CE4B_896C_DD1D);
    check!(state != 1);
    let (a, b, c) = (rand::next_u64(), rand::next_u64(), rand::next_u64());
    check!(a != b && b != c && a != c);
    check!(rand::next_u32() != rand::next_u32() || rand::next_u32() != rand::next_u32());
    Ok(())
}

fn net_config() -> TestResult {
    let c = net::parse_config("192.168.1.50/24 192.168.1.1").map_err(String::from)?;
    check_eq!((c.ip, c.prefix, c.gateway), (beenet::Ipv4([192, 168, 1, 50]), 24, Some(beenet::Ipv4([192, 168, 1, 1]))));
    check_eq!(net::format_config(&c).as_str(), "192.168.1.50/24 192.168.1.1");
    check_eq!(net::parse_config("10.0.0.2/8").map(|c| c.gateway), Ok(None));
    for bad in ["", "10.0.0.2", "10.0.0.0/24", "10.0.0.255/24", "10.0.0.2/24 10.0.1.1", "10.0.0.2/24 10.0.0.2",
                "10.0.0.2/24 gw", "10.0.0.2/24 10.0.0.1 extra", "10.0.0.2/33", "300.0.0.2/24"] {
        check!(net::parse_config(bad).is_err());
    }
    // The saved settings file.
    let s = net::parse_settings("static 10.0.0.2/24 10.0.0.1\ndns 1.1.1.1:5353\n").map_err(String::from)?;
    check!(matches!(s.mode, net::Mode::Static(_)));
    check_eq!(s.dns, Some((beenet::Ipv4([1, 1, 1, 1]), 5353)));
    check_eq!(net::format_settings(&s).as_str(), "static 10.0.0.2/24 10.0.0.1\ndns 1.1.1.1:5353");
    check_eq!(net::parse_settings("dhcp\n"), Ok(net::Settings { mode: net::Mode::Dhcp, dns: None }));
    check_eq!(net::parse_settings("none").map(|s| s.mode), Ok(net::Mode::Off));
    check_eq!(net::parse_settings("").map(|s| s.mode), Ok(net::Mode::Dhcp));
    check_eq!(net::parse_dns_server("9.9.9.9"), Some((beenet::Ipv4([9, 9, 9, 9]), 53)));
    for bad in ["dhcp now", "static", "dns 1.2.3", "dns 1.2.3.4:0", "dns 1.2.3.4:99999", "bogus"] {
        check!(net::parse_settings(bad).is_err());
    }
    Ok(())
}

fn net_stack_ping() -> TestResult {
    use beenet::{Config, Event, Ipv4, Stack};
    let (ip_a, ip_b) = (Ipv4([10, 9, 0, 1]), Ipv4([10, 9, 0, 2]));
    let mut a = Stack::new([2, 0, 0, 0, 0, 1], 1);
    let mut b = Stack::new([2, 0, 0, 0, 0, 2], 1);
    a.set_config(Some(Config { ip: ip_a, prefix: 24, gateway: None }));
    b.set_config(Some(Config { ip: ip_b, prefix: 24, gateway: None }));
    a.ping(ip_b, 7, 1, b"bee", 0).map_err(|e| String::from(e.message()))?;
    // Carry frames between the two until both are quiet.
    for _ in 0..10 {
        while let Some(f) = a.transmit() {
            b.receive(&f, 0);
        }
        while let Some(f) = b.transmit() {
            a.receive(&f, 0);
        }
    }
    check_eq!(a.take_event(), Some(Event::EchoReply { from: ip_b, id: 7, seq: 1, ttl: 64, len: 3 }));
    Ok(())
}

fn remote_shell_guard() -> TestResult {
    use crate::remote::{new_password, parse_setting, password_matches};
    check_eq!(parse_setting("on\n"), Some(23));
    check_eq!(parse_setting("on 2323"), Some(2323));
    for text in ["", "off", "on 0", "on 70000", "on 23 extra", "yes"] {
        check_eq!(parse_setting(text), None);
    }
    // Made-up passwords: the right shape, only clear characters, never twice the same.
    let (a, b) = (new_password(), new_password());
    check!(a != b);
    for p in [&a, &b] {
        check_eq!(p.len(), 14);
        for (i, c) in p.chars().enumerate() {
            check_eq!(c == '-', i == 4 || i == 9);
            check!(c == '-' || (c.is_ascii_lowercase() || c.is_ascii_digit()) && !"01ilo".contains(c));
        }
    }
    check!(password_matches(&a, &a));
    check!(password_matches(&a.replace('-', "").to_ascii_uppercase(), &a)); // typed without dashes, in capitals
    check!(!password_matches(&b, &a));
    check!(!password_matches(&a[..13], &a));
    check!(!password_matches(&format!("{a}x"), &a));
    check!(!password_matches("", &a));
    // No password, or a damaged file: nothing matches, not even the same text.
    check!(!password_matches("", ""));
    check!(!password_matches("abc", "abc"));
    check!(!password_matches("", "-- -"));
    Ok(())
}

fn httpd_setting() -> TestResult {
    use crate::httpd::parse_setting;
    check_eq!(parse_setting("on 8080\n"), Some(8080));
    check_eq!(parse_setting("on"), Some(80));
    // Anything unclear means off: the server never starts by accident.
    for text in ["", "off", "off 80", "on 0", "on 99999", "on eighty", "on 80 extra", "yes", "ON"] {
        check_eq!(parse_setting(text), None);
    }
    Ok(())
}

fn threads_take_turns() -> TestResult {
    use crate::thread;
    use alloc::rc::Rc;
    use core::cell::RefCell;
    let before = thread::list().len();
    let log = Rc::new(RefCell::new(String::new()));
    let mut ids = Vec::new();
    for name in ["a", "b", "c"] {
        let log = log.clone();
        let id = thread::spawn(name, None, move || {
            for round in 0..3 {
                log.borrow_mut().push_str(&format!("{name}{round} "));
                thread::yield_now();
            }
        });
        ids.push(id.ok_or("could not start a thread")?);
    }
    check_eq!(thread::list().len(), before + 3);
    for id in &ids {
        thread::join(*id);
    }
    check_eq!(log.borrow().as_str(), "a0 b0 c0 a1 b1 c1 a2 b2 c2 ");
    check!(ids.iter().all(|id| !thread::is_alive(*id)));

    // Sleeping threads wake in the order of their deadlines, and the main
    // thread keeps running meanwhile.
    log.borrow_mut().clear();
    let start = time::uptime_ms();
    let mut ids = Vec::new();
    for (name, ms) in [("slow", 60u64), ("fast", 20)] {
        let log = log.clone();
        ids.push(thread::spawn(name, None, move || {
            thread::sleep_ms(ms);
            log.borrow_mut().push_str(name);
        }));
    }
    let mut turns = 0u32;
    while ids.iter().any(|id| id.is_some_and(thread::is_alive)) {
        turns += 1;
        thread::pause(|| false);
    }
    check_eq!(log.borrow().as_str(), "fastslow");
    let took = time::uptime_ms() - start;
    check!((60..200).contains(&took));
    check!(turns >= 2);
    thread::yield_now(); // lets the scheduler clear away the ended threads
    check_eq!(thread::list().len(), before);
    check_eq!(thread::current_name().as_str(), thread::list().iter().find(|t| t.state == "running").map(|t| t.name.as_str()).unwrap_or(""));
    Ok(())
}

fn threads_respect_the_file_system() -> TestResult {
    use crate::thread;
    use alloc::rc::Rc;
    use core::cell::Cell;
    let Some(mut storage) = crate::storage::handle() else { return Err(String::from("no file system is mounted")) };
    let ran = Rc::new(Cell::new(false));
    let flag = ran.clone();
    let id = thread::spawn("other", None, move || flag.set(true)).ok_or("could not start a thread")?;
    {
        let mut fs = storage.fs();
        thread::yield_now();
        thread::pause(|| false); // sleeps until the next interrupt instead of switching
        check!(!ran.get());
        check!(fs.exists("/"));
    }
    thread::join(id);
    check!(ran.get());
    Ok(())
}

fn second_shell_session() -> TestResult {
    use crate::{session, thread};
    let storage = crate::storage::handle().ok_or("no file system is mounted")?;
    let id = session::open().ok_or("no free session")?;
    let shell = thread::spawn("shell2", Some(id), move || crate::shell::Shell::unlogged(storage).run_session());
    let Some(shell) = shell else {
        session::remove(id);
        return Err(String::from("could not start a thread"));
    };
    // Type one command, let it run, and only then the next: both shells are
    // alive at the same time, each with its own state.
    session::type_text(id, "echo hello from shell two\n");
    let start = time::uptime_ms();
    let mut output = String::new();
    while !output.contains("hello from shell two\n") && time::uptime_ms() - start < 2000 {
        thread::pause(|| false);
        output += &session::take_output(id);
    }
    session::type_text(id, "mkdir /tmp-shell2\ncd /tmp-shell2\npwd\ncat /nope\ncalc 6 * 7\ncd /\nrm /tmp-shell2\n");
    session::close(id);
    thread::join(shell);
    output += &session::take_output(id);
    session::remove(id);
    let lines: Vec<&str> = output.lines().collect();
    let expected = [
        "bee:/$ ",
        "hello from shell two",
        "bee:/$ ",
        "bee:/$ ",
        "bee:/tmp-shell2$ ",
        "/tmp-shell2",
        "bee:/tmp-shell2$ ",
        "/nope: no such file or directory",
        "bee:/tmp-shell2$ ",
        "42",
        "bee:/tmp-shell2$ ",
        "bee:/$ ",
        "bee:/$ ",
    ];
    check_eq!(lines.as_slice(), &expected[..]);
    check!(!thread::is_alive(shell));
    Ok(())
}

fn net_clients() -> TestResult {
    use beenet::http::{HttpError, Response, Url};
    let url = Url::parse("http://Example.com:8080/a?b=1").map_err(|e| String::from(e.message()))?;
    check_eq!((url.host.as_str(), url.port, url.path.as_str()), ("example.com", 8080, "/a?b=1"));
    check_eq!(Url::parse("https://example.com/").err(), Some(HttpError::Https));
    check!(Url::parse("http://h/a b").is_err());
    let (mut response, mut body) = (Response::new(), Vec::new());
    for piece in b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n".chunks(5) {
        response.feed(piece, &mut body).map_err(|e| String::from(e.message()))?;
    }
    check!(response.complete());
    check_eq!((response.status(), body.as_slice()), (200, &b"abc"[..]));

    check_eq!(net::split_host_port("host", 69), Some(("host", 69)));
    check_eq!(net::split_host_port("10.0.2.2:6969", 69), Some(("10.0.2.2", 6969)));
    check_eq!(net::split_host_port("host:", 69), None);
    check_eq!(net::split_host_port("host:0", 69), None);
    check_eq!(net::split_host_port(":69", 69), None);
    check_eq!(net::split_host_port("", 69), None);

    let request = beenet::sntp::request(42);
    check_eq!(request.len(), 48);
    check!(beenet::sntp::parse_reply(&request, 42).is_err()); // our own request is not a reply
    Ok(())
}

fn net_stack_tcp() -> TestResult {
    use beenet::{CloseReason, Config, Ipv4, Stack, TcpError, TcpStatus};
    let (ip_a, ip_b) = (Ipv4([10, 9, 0, 1]), Ipv4([10, 9, 0, 2]));
    let mut a = Stack::new([2, 0, 0, 0, 0, 1], 11);
    let mut b = Stack::new([2, 0, 0, 0, 0, 2], 22);
    a.set_config(Some(Config { ip: ip_a, prefix: 24, gateway: None }));
    b.set_config(Some(Config { ip: ip_b, prefix: 24, gateway: None }));
    b.tcp_listen(7).map_err(|e| String::from(e.message()))?;
    let client = a.tcp_connect(ip_b, 7, 0).map_err(|e| String::from(e.message()))?;
    // Carry frames between the two; time stands still (nothing is lost, so no timers are needed).
    let carry = |a: &mut Stack, b: &mut Stack| {
        for _ in 0..20 {
            while let Some(f) = a.transmit() {
                b.receive(&f, 0);
            }
            while let Some(f) = b.transmit() {
                a.receive(&f, 0);
            }
        }
    };
    carry(&mut a, &mut b);
    check_eq!(a.tcp_status(client), Ok(TcpStatus::Open));
    let server = b.tcp_accept(7).ok_or("no connection at the server")?;
    let message: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect(); // several segments
    check_eq!(a.tcp_send(client, &message, 0), Ok(message.len()));
    a.tcp_close(client, 0).map_err(|e| String::from(e.message()))?;
    carry(&mut a, &mut b);
    let mut got = vec![0u8; 8192];
    let n = b.tcp_recv(server, &mut got, 0).map_err(|e| String::from(e.message()))?;
    check_eq!(n, message.len());
    check!(got[..n] == message[..]);
    check_eq!(b.tcp_recv(server, &mut got, 0), Err(TcpError::Closed(CloseReason::Normal)));
    Ok(())
}

fn display_basics() -> TestResult {
    use crate::display::{parse_mode, usable, Mode, PixelFormat};
    use crate::{bochs, bootargs};
    check_eq!(parse_mode("1920x1080"), Some((1920, 1080)));
    check_eq!(parse_mode("320x200"), Some((320, 200)));
    for bad in ["", "x", "1920", "1920x", "x1080", "0x0", "99999x1", "319x200", "1920x2161", "1920x1080x32", "-1x5", "a x b"] {
        check_eq!((bad, parse_mode(bad)), (bad, None));
    }
    // key=value words on the kernel command line.
    check_eq!(bootargs::value_in("test display=text mode=800x600", "display"), Some("text"));
    check_eq!(bootargs::value_in("test display=text mode=800x600", "mode"), Some("800x600"));
    check_eq!(bootargs::value_in("test displays=text nodisplay=x display", "display"), None);
    check_eq!(bootargs::value_in("display=", "display"), Some(""));
    // Colors in each pixel format.
    check_eq!(PixelFormat::Xrgb8888.encode(0x112233), 0x112233);
    check_eq!(PixelFormat::Xbgr8888.encode(0x112233), 0x332211);
    check_eq!(PixelFormat::Rgb888.encode(0xFF112233), 0x112233);
    check_eq!(PixelFormat::Rgb565.encode(0xFFFFFF), 0xFFFF);
    check_eq!(PixelFormat::Rgb565.encode(0xFF0000), 0xF800);
    check_eq!(PixelFormat::Rgb565.encode(0x00FF00), 0x07E0);
    check_eq!(PixelFormat::Rgb565.encode(0x0000FF), 0x001F);
    // A framebuffer is only used when all of it can be reached and its numbers add up.
    let hd = Mode { width: 1920, height: 1080, pitch: 1920 * 4, format: PixelFormat::Xrgb8888 };
    check!(usable(0xFD00_0000, &hd));
    check!(!usable(0, &hd)); // no address
    check!(!usable(0xB8000, &hd)); // inside low memory
    check!(!usable(0x1_0000_0000, &hd)); // above what is mapped
    check!(!usable(0xFFFF_0000, &hd)); // starts below 4 GiB, ends above
    check!(!usable(u64::MAX - 100, &hd)); // the end overflows
    check!(usable(0x1_0000_0000 - hd.bytes(), &hd)); // ends exactly at 4 GiB
    check!(!usable(0xFD00_0000, &Mode { pitch: 1920 * 4 - 1, ..hd })); // a line too short for its pixels
    check!(!usable(0xFD00_0000, &Mode { pitch: 0, ..hd }));
    check!(!usable(0xFD00_0000, &Mode { width: 0, ..hd }));
    check!(!usable(0xFD00_0000, &Mode { height: 0, ..hd }));
    check!(!usable(0xFD00_0000, &Mode { width: u32::MAX, pitch: u32::MAX, ..hd }));
    check!(usable(0xFD00_0000, &Mode { pitch: 1920 * 3, format: PixelFormat::Rgb888, ..hd }));
    // The Bochs card's ID register; 0xFFFF is what a machine without it answers.
    check!(bochs::id_ok(0xB0C5) && bochs::id_ok(0xB0C4));
    check!(!bochs::id_ok(0xB0C0) && !bochs::id_ok(0xFFFF) && !bochs::id_ok(0));
    Ok(())
}

/// A framebuffer in the heap for the display tests: every byte starts as
/// 0xEE, with `GUARD` spare bytes after the last line.
struct TestScreen {
    mode: crate::display::Mode,
    pixels: Vec<u8>,
}

impl TestScreen {
    const GUARD: usize = 64;

    fn new(width: u32, height: u32, extra_pitch: u32, format: crate::display::PixelFormat) -> Self {
        let pitch = width * format.bytes() as u32 + extra_pitch;
        let mode = crate::display::Mode { width, height, pitch, format };
        TestScreen { mode, pixels: vec![0xEE; mode.bytes() as usize + Self::GUARD] }
    }

    fn pixel(&self, x: usize, y: usize) -> u32 {
        let at = y * self.mode.pitch as usize + x * self.mode.format.bytes();
        let mut bytes = [0u8; 4];
        let n = self.mode.format.bytes();
        bytes[..n].copy_from_slice(&self.pixels[at..at + n]);
        u32::from_le_bytes(bytes)
    }

    fn color(&self, c: Color) -> u32 {
        self.mode.format.encode(crate::fbcon::PALETTE[c as usize])
    }

    /// Does the cell at (row, col) show exactly this character in these colors?
    fn shows(&self, row: usize, col: usize, c: char, fg: Color, bg: Color) -> bool {
        let palette = &crate::fbcon::PALETTE;
        self.shows_at(col * 8, row * 16, c, palette[fg as usize], palette[bg as usize])
    }

    /// Is this character drawn with its top left corner at pixel (x, y), in these 0xRRGGBB colors?
    fn shows_at(&self, x: usize, y: usize, c: char, fg: u32, bg: u32) -> bool {
        let glyph = &crate::font::FONT[crate::vga::cp437(c) as usize];
        (0..16).all(|gy| {
            (0..8).all(|gx| {
                let want = if glyph[gy] & (0x80 >> gx) != 0 { fg } else { bg };
                self.pixel(x + gx, y + gy) == self.mode.format.encode(want)
            })
        })
    }

    fn rgb(&self, x: usize, y: usize) -> u32 {
        self.pixel(x, y) // only for Xrgb8888 screens
    }

    /// True when nothing was written between the lines or after the last one.
    fn untouched_outside(&self) -> bool {
        let (pitch, used) = (self.mode.pitch as usize, self.mode.width as usize * self.mode.format.bytes());
        let end = self.mode.bytes() as usize;
        (0..self.mode.height as usize).all(|y| self.pixels[y * pitch + used..(y + 1) * pitch].iter().all(|&b| b == 0xEE))
            && self.pixels[end..].iter().all(|&b| b == 0xEE)
    }
}

fn fbcon_drawing() -> TestResult {
    use crate::display::PixelFormat::*;
    use crate::fbcon::FbCon;
    // The font has real letters: an A is not blank and differs from a B.
    let font = &crate::font::FONT;
    check!(font[b' ' as usize].iter().all(|&row| row == 0));
    check!(font[b'A' as usize].iter().any(|&row| row != 0));
    check!(font[b'A' as usize] != font[b'B' as usize]);
    for format in [Xrgb8888, Xbgr8888, Rgb888, Rgb565] {
        let mut t = TestScreen::new(64, 48, 5, format);
        let mut cells = vec![0u16; 64];
        let mut screen = FbCon::new(t.pixels.as_mut_ptr(), t.mode, &mut cells);
        check_eq!((screen.width(), screen.height()), (8, 3));
        screen.clear();
        screen.write_str("A");
        screen.set_color(Color::Yellow, Color::Blue);
        screen.write_str("é");
        screen.set_pos(2, 7);
        screen.set_color(Color::White, Color::Red);
        screen.write_char('\u{4e16}'); // not in the font: the filled box
        screen.set_cursor(8); // away from the cells we look at
        check!(t.shows(0, 0, 'A', Color::LightGray, Color::Black));
        check!(t.shows(0, 1, 'é', Color::Yellow, Color::Blue));
        check!(t.shows(0, 2, ' ', Color::LightGray, Color::Black));
        check!(t.shows(2, 7, '■', Color::White, Color::Red));
        check!(!t.shows(0, 0, 'B', Color::LightGray, Color::Black));
        check!(t.untouched_outside());
    }
    Ok(())
}

fn fbcon_screen() -> TestResult {
    use crate::display::PixelFormat::Xrgb8888;
    use crate::fbcon::{FbCon, BARS};
    let (gray, black) = (Color::LightGray, Color::Black);
    let mut t = TestScreen::new(64, 48, 0, Xrgb8888);
    let mut cells = vec![0u16; 24];
    let mut screen = FbCon::new(t.pixels.as_mut_ptr(), t.mode, &mut cells);
    screen.clear();
    // Everything is background now, except the cursor in the first cell.
    check!(t.pixels[64 * 4 * 16..t.mode.bytes() as usize].chunks(4).all(|p| p == [0, 0, 0, 0]));
    check_eq!((t.pixel(0, 13), t.pixel(7, 14), t.pixel(8, 13), t.pixel(0, 12)), (t.color(gray), t.color(gray), 0, 0));
    // A ninth character wraps to the next line, but only when it is written.
    screen.write_str("12345678");
    check_eq!(screen.pos(), (0, 8));
    screen.write_str("9");
    check_eq!(screen.pos(), (1, 1));
    check!(t.shows(0, 7, '8', gray, black) && t.shows(1, 0, '9', gray, black));
    // Tabs go to the next multiple of 4; \r returns to the start of the line.
    screen.write_str("\tx\ry");
    check!(t.shows(1, 4, 'x', gray, black) && t.shows(1, 0, 'y', gray, black));
    // The fourth line scrolls the screen up by one.
    screen.write_str("\nc");
    check_eq!((screen.scrolls(), screen.pos()), (0, (2, 1)));
    screen.write_str("\nd");
    check_eq!((screen.scrolls(), screen.pos()), (1, (2, 1)));
    check!(t.shows(0, 0, 'y', gray, black) && t.shows(0, 4, 'x', gray, black));
    check!(t.shows(0, 1, ' ', gray, black)); // the 2 of the old first line is gone
    check!(t.shows(1, 0, 'c', gray, black) && t.shows(2, 0, 'd', gray, black));
    // The cursor is an underline in the cell; moving it restores the cell.
    screen.sync_cursor();
    check!(!t.shows(2, 1, ' ', gray, black));
    check_eq!(t.pixel(8, 2 * 16 + 13), t.color(gray));
    check_eq!(t.pixel(8, 2 * 16 + 12), t.color(black));
    screen.set_cursor(0);
    check!(t.shows(2, 1, ' ', gray, black) && !t.shows(0, 0, 'y', gray, black));
    screen.set_cursor(10_000); // past the end: the last cell
    check!(t.shows(0, 0, 'y', gray, black) && !t.shows(2, 7, ' ', gray, black));
    // Positions outside the screen are pulled inside.
    screen.set_pos(99, 99);
    check_eq!(screen.pos(), (2, 8));
    screen.put_cell(3, 0, 0x0741); // below the last line: ignored
    screen.put_cell(0, 8, 0x0741);
    screen.put_cell(1, 1, 0x0741);
    check!(t.shows(1, 1, 'A', gray, black));
    // Color bars cover everything; a redraw brings the text back.
    screen.color_bars();
    for (i, bar) in BARS.iter().enumerate() {
        check_eq!(t.pixel(i * 8 + 4, 0), t.color(*bar));
        check_eq!(t.pixel(i * 8 + 4, 47), t.color(*bar));
    }
    screen.redraw();
    check!(t.shows(1, 1, 'A', gray, black) && t.shows(0, 0, 'y', gray, black) && t.shows(2, 0, 'd', gray, black));
    // Clearing uses the background color, everywhere.
    screen.set_color(Color::White, Color::Blue);
    screen.clear();
    check_eq!(screen.pos(), (0, 0));
    check!(t.shows(1, 3, ' ', Color::White, Color::Blue));
    check_eq!(t.pixel(63, 47), t.color(Color::Blue));
    check!(t.untouched_outside());
    Ok(())
}

fn fbcon_bounds() -> TestResult {
    use crate::display::{Mode, PixelFormat::*};
    use crate::fbcon::FbCon;
    let busy = |screen: &mut FbCon| {
        screen.clear();
        for i in 0..200 {
            screen.write_str("line of text that is longer than the screen is wide\t\n");
            screen.set_cursor(i * 7);
        }
        screen.put_cell(0, 0, 0x1F42);
        screen.sync_cursor();
        screen.color_bars();
        screen.redraw();
    };
    // Sizes that are not whole characters: the rest stays background.
    for (width, height, cols, rows) in [(13, 17, 1, 1), (8, 16, 1, 1), (100, 50, 12, 3), (9, 33, 1, 2)] {
        for format in [Xrgb8888, Rgb888, Rgb565] {
            let mut t = TestScreen::new(width, height, 3, format);
            let mut cells = vec![0u16; 64];
            let mut screen = FbCon::new(t.pixels.as_mut_ptr(), t.mode, &mut cells);
            check_eq!((width, screen.width(), screen.height()), (width, cols, rows));
            busy(&mut screen);
            check!(t.untouched_outside());
        }
    }
    // Too small for one character, or no cells to remember it in: nothing is drawn at all.
    for (width, height, cell_count) in [(7, 100, 64), (100, 15, 64), (1, 1, 64), (64, 48, 0), (64, 48, 7)] {
        let mut t = TestScreen::new(width, height, 0, Xrgb8888);
        let mut cells = vec![0u16; cell_count];
        let mut screen = FbCon::new(t.pixels.as_mut_ptr(), t.mode, &mut cells);
        check_eq!((width, height, screen.width(), screen.height()), (width, height, 0, 0));
        busy(&mut screen);
        check_eq!(screen.pos(), (0, 0));
        check!(t.pixels.iter().all(|&b| b == 0xEE));
    }
    // Fewer cells than the screen has room for: fewer lines, same rules.
    let mut t = TestScreen::new(64, 48, 0, Xrgb8888);
    let mut cells = vec![0u16; 17];
    let mut screen = FbCon::new(t.pixels.as_mut_ptr(), t.mode, &mut cells);
    check_eq!((screen.width(), screen.height()), (8, 2));
    busy(&mut screen);
    check!(t.untouched_outside());
    // Lines too short to hold their pixels (bad data from a card or firmware).
    let mut t = TestScreen::new(64, 48, 0, Xrgb8888);
    let bad = Mode { pitch: 64 * 4 - 1, ..t.mode };
    let mut cells = vec![0u16; 64];
    let mut screen = FbCon::new(t.pixels.as_mut_ptr(), bad, &mut cells);
    check_eq!((screen.width(), screen.height()), (0, 0));
    busy(&mut screen);
    check!(t.pixels.iter().all(|&b| b == 0xEE));
    Ok(())
}

fn gfx_canvas() -> TestResult {
    use crate::display::{Mode, PixelFormat::*};
    use crate::gfx::{Canvas, Rect};
    // Clipping, also with numbers that would overflow.
    let screen = Rect::new(0, 0, 100, 50);
    check_eq!(Rect::new(10, 10, 20, 20).clip(&screen), Rect::new(10, 10, 20, 20));
    check_eq!(Rect::new(90, 40, 20, 20).clip(&screen), Rect::new(90, 40, 10, 10));
    check_eq!(Rect::new(100, 50, 5, 5).clip(&screen), Rect::new(100, 50, 0, 0));
    check_eq!(Rect::new(500, 500, 5, 5).clip(&screen).width, 0);
    check_eq!(Rect::new(5, 5, usize::MAX, usize::MAX).clip(&screen), Rect::new(5, 5, 95, 45));
    check_eq!(Rect::new(usize::MAX, usize::MAX, usize::MAX, 9).clip(&screen).height, 0);
    check_eq!(Rect::new(0, 0, 30, 30).clip(&Rect::new(10, 20, 5, 5)), Rect::new(10, 20, 5, 5));
    for format in [Xrgb8888, Xbgr8888, Rgb888, Rgb565] {
        let mut t = TestScreen::new(40, 30, 3, format);
        let canvas = Canvas::new(t.pixels.as_mut_ptr(), t.mode);
        check_eq!(canvas.bounds(), Rect::new(0, 0, 40, 30));
        canvas.fill_rect(canvas.bounds(), 0x102030);
        canvas.fill_rect(Rect::new(30, 20, 500, 500), 0xFF0000); // sticks out: cut off
        canvas.fill_rect(Rect::new(40, 0, 10, 10), 0x00FF00); // outside
        canvas.fill_rect(Rect::new(usize::MAX - 2, usize::MAX - 2, usize::MAX, usize::MAX), 0x00FF00);
        check_eq!(t.pixel(0, 0), format.encode(0x102030));
        check_eq!(t.pixel(29, 19), format.encode(0x102030));
        check_eq!((t.pixel(30, 20), t.pixel(39, 29)), (format.encode(0xFF0000), format.encode(0xFF0000)));
        // Characters: drawn when they fit completely, otherwise not at all.
        canvas.glyph(32, 14, b'A', 0xFFFFFF, 0);
        check!(t.shows_at(32, 14, 'A', 0xFFFFFF, 0));
        canvas.glyph(33, 0, b'B', 0xFFFFFF, 0); // one pixel too far right
        canvas.glyph(0, 15, b'B', 0xFFFFFF, 0); // one pixel too low
        canvas.glyph(usize::MAX, usize::MAX, b'B', 0xFFFFFF, 0);
        check_eq!((t.pixel(33, 0), t.pixel(0, 15)), (format.encode(0x102030), format.encode(0x102030)));
        // Text stops at its width, and at the edge of the canvas.
        canvas.text(0, 0, "abcdefgh", 0xFFFFFF, 0, 17);
        check!(t.shows_at(0, 0, 'a', 0xFFFFFF, 0) && t.shows_at(8, 0, 'b', 0xFFFFFF, 0));
        check_eq!(t.pixel(20, 5), format.encode(0x102030));
        canvas.text(16, 0, "abcdefgh", 0xFFFFFF, 0, 1000);
        check!(t.shows_at(32, 0, 'c', 0xFFFFFF, 0));
        check!(t.untouched_outside());
    }
    // A canvas whose lines cannot hold their pixels draws nothing.
    let mut t = TestScreen::new(40, 30, 0, Xrgb8888);
    let canvas = Canvas::new(t.pixels.as_mut_ptr(), Mode { pitch: 40 * 4 - 1, ..t.mode });
    check_eq!(canvas.bounds(), Rect::new(0, 0, 0, 0));
    canvas.fill_rect(Rect::new(0, 0, 40, 30), 0xFFFFFF);
    canvas.glyph(0, 0, b'A', 0xFFFFFF, 0);
    check!(t.pixels.iter().all(|&b| b == 0xEE));
    Ok(())
}

fn wm_layout() -> TestResult {
    use crate::gfx::Rect;
    use crate::wm::{terminal_frame, Window};
    let content = |frame: Rect| Window { title: String::new(), frame }.content();
    // Full HD: a window in the middle with desktop all around.
    let frame = terminal_frame(1920, 1080).ok_or("no window on a full HD screen")?;
    let inside = content(frame);
    check_eq!((inside.width / 8, inside.height / 16), (221, 56));
    check_eq!(frame.x, 1920 - frame.x - frame.width); // centered
    check!(frame.x >= 60 && frame.y >= 60);
    // Every screen size: the window is inside the screen, the content inside
    // the window, and the content is a whole number of characters.
    let mut with_window = 0;
    for width in (0..2600).step_by(53) {
        for height in (0..1700).step_by(47) {
            let Some(frame) = terminal_frame(width, height) else { continue };
            with_window += 1;
            let inside = content(frame);
            check!(frame.x + frame.width <= width && frame.y + frame.height <= height);
            check!(inside.x > frame.x && inside.x + inside.width < frame.x + frame.width);
            check!(inside.y > frame.y && inside.y + inside.height < frame.y + frame.height);
            check_eq!((inside.width % 8, inside.height % 16), (0, 0));
            check!(inside.width / 8 >= 20 && inside.height / 16 >= 4);
        }
    }
    check!(with_window > 500);
    // Too small for a window.
    for (width, height) in [(0, 0), (1, 1), (100, 1000), (1000, 60), (150, 120)] {
        check_eq!((width, terminal_frame(width, height)), (width, None));
    }
    check!(terminal_frame(320, 200).is_some() && terminal_frame(640, 480).is_some());
    // A window frame too small to have an inside does not underflow.
    check_eq!(content(Rect::new(5, 5, 3, 3)).width, 0);
    check_eq!(content(Rect::new(5, 5, 3, 3)).height, 0);
    Ok(())
}

fn wm_drawing() -> TestResult {
    use crate::display::PixelFormat::*;
    use crate::fbcon::FbCon;
    use crate::gfx::{Canvas, Rect};
    use crate::wm::{self, Desktop};
    let mut t = TestScreen::new(640, 480, 0, Xrgb8888);
    let canvas = Canvas::new(t.pixels.as_mut_ptr(), t.mode);
    let frame = wm::terminal_frame(640, 480).ok_or("no window at 640x480")?;
    let mut desk = Desktop::new();
    let id = desk.open("Terminal", frame);
    desk.draw(&canvas);
    let inside = desk.window(id).ok_or("the window is gone")?.content();
    check!(desk.window(id + 1).is_none());
    // Light blue in all four corners; the window in the middle.
    for (x, y) in [(0, 0), (639, 0), (0, 479), (639, 479), (frame.x - 1, 240), (320, frame.y - 1)] {
        check_eq!((x, y, t.rgb(x, y)), (x, y, wm::BACKGROUND));
    }
    check!(t.rgb(frame.x, frame.y) != wm::BACKGROUND); // the border
    check_eq!(t.rgb(frame.x + frame.width / 2, frame.y + 3), wm::TITLE_BAR);
    check!(t.shows_at(frame.x + 2 + 8, frame.y + 2 + 4, 'T', 0xFFFFFF, wm::TITLE_BAR));
    check!(t.shows_at(frame.x + 2 + 16, frame.y + 2 + 4, 'e', 0xFFFFFF, wm::TITLE_BAR));
    check_eq!(t.rgb(inside.x, inside.y), wm::CONTENT);
    check_eq!(t.rgb(inside.x + inside.width - 1, inside.y + inside.height - 1), wm::CONTENT);
    // The terminal draws inside the window and nowhere else.
    let mut cells = vec![0u16; 8000];
    let mut term = FbCon::in_area(canvas, inside, &mut cells);
    check_eq!((term.width(), term.height()), (inside.width / 8, inside.height / 16));
    term.clear();
    term.write_str("A");
    term.set_color(Color::White, Color::Blue);
    for _ in 0..term.height() + 3 {
        term.write_str("\nline"); // scrolls
    }
    term.set_cursor(0);
    check!(t.shows_at(inside.x + 8, inside.y + (term.height() - 1) * 16, 'i', 0xFFFFFF, 0x0000AA));
    check!(t.shows_at(inside.x + 8, inside.y, 'i', 0xFFFFFF, 0x0000AA));
    term.clear();
    check_eq!(t.rgb(inside.x, inside.y), 0x0000AA);
    check_eq!(t.rgb(inside.x + inside.width - 1, inside.y + inside.height - 1), 0x0000AA);
    check_eq!(t.rgb(inside.x - 1, inside.y), wm::CONTENT); // the padding is not the terminal's
    check_eq!(t.rgb(frame.x + frame.width / 2, frame.y + 3), wm::TITLE_BAR);
    check_eq!((t.rgb(0, 0), t.rgb(639, 479)), (wm::BACKGROUND, wm::BACKGROUND));
    // Color bars cover the desktop too; drawing the desktop and the terminal again restores it.
    term.write_str("back");
    term.color_bars();
    check_eq!(t.rgb(0, 0), 0xFFFFFF);
    desk.draw(&canvas);
    term.redraw();
    check_eq!(t.rgb(0, 0), wm::BACKGROUND);
    check!(t.shows_at(inside.x + 8, inside.y, 'a', 0xFFFFFF, 0x0000AA)); // the cursor is on the b
    // A second window takes the focus: the first one's title bar turns gray.
    desk.open("Second", Rect::new(frame.x + 200, frame.y + 100, 200, 120));
    desk.draw(&canvas);
    check!(t.rgb(frame.x + 40, frame.y + 3) != wm::TITLE_BAR);
    check_eq!(t.rgb(frame.x + 200 + 100, frame.y + 100 + 3), wm::TITLE_BAR);
    // Windows that stick out of the screen, or are tiny, are cut off: no write outside.
    for format in [Xrgb8888, Rgb888, Rgb565] {
        let mut t = TestScreen::new(200, 100, 5, format);
        let canvas = Canvas::new(t.pixels.as_mut_ptr(), t.mode);
        let mut desk = Desktop::new();
        for frame in [Rect::new(150, 60, 300, 300), Rect::new(190, 0, 5, 5), Rect::new(0, 95, 1, 1),
                      Rect::new(500, 500, 50, 50), Rect::new(10, 10, 0, 0), Rect::new(0, 0, 200, 100)] {
            let id = desk.open("A window with a very long title that does not fit in its title bar", frame);
            desk.draw(&canvas);
            let inside = desk.window(id).ok_or("the window is gone")?.content();
            let mut cells = vec![0u16; 400];
            let mut term = FbCon::in_area(canvas, inside, &mut cells);
            term.clear();
            term.write_str("some text\nmore text");
            term.sync_cursor();
        }
        check_eq!(t.pixel(0, 0) == format.encode(wm::BACKGROUND), false); // the last window covers it
        check!(t.untouched_outside());
    }
    Ok(())
}

fn desktop_setting() -> TestResult {
    let choice = console::desktop_choice;
    // Nothing said: on.
    check!(choice("", None) && choice("test slot=a", None));
    // The saved choice; anything that is not "off" means on.
    check!(!choice("test", Some("off")) && !choice("test", Some(" off\n")));
    check!(choice("test", Some("on\n")) && choice("test", Some("")) && choice("test", Some("offline")));
    // A boot flag wins over what was saved.
    check!(choice("test desktop=on", Some("off")) && choice("desktop", Some("off")));
    check!(!choice("test desktop=off", Some("on")) && !choice("desktop=off", None));
    // Other words that only look like the flag change nothing.
    check!(!choice("nodesktop desktops=off", Some("off")) && choice("desktops=off", None));
    Ok(())
}

fn bootfb_parsing() -> TestResult {
    use crate::bootfb::{parse, Boot, INFO_LEN};
    use crate::display::{usable, Mode, PixelFormat::*};
    // A Multiboot info with a framebuffer: 1920x1080, 32 bits, at 0xE0000000.
    let info = |addr: u64, pitch: u32, width: u32, height: u32, bits: u8, kind: u8, masks: [u8; 6]| {
        let mut b = vec![0u8; INFO_LEN];
        b[0..4].copy_from_slice(&(1u32 << 12 | 0x47).to_le_bytes());
        b[88..96].copy_from_slice(&addr.to_le_bytes());
        b[96..100].copy_from_slice(&pitch.to_le_bytes());
        b[100..104].copy_from_slice(&width.to_le_bytes());
        b[104..108].copy_from_slice(&height.to_le_bytes());
        b[108] = bits;
        b[109] = kind;
        b[112..118].copy_from_slice(&masks);
        b
    };
    const BGRX: [u8; 6] = [16, 8, 8, 8, 0, 8];
    let hd = |format, pitch| Mode { width: 1920, height: 1080, pitch, format };
    let good = info(0xE000_0000, 7680, 1920, 1080, 32, 1, BGRX);
    check_eq!(parse(&good), Boot::Graphics(0xE000_0000, hd(Xrgb8888, 7680)));
    check_eq!(parse(&info(0xE000_0000, 7680, 1920, 1080, 32, 1, [0, 8, 8, 8, 16, 8])), Boot::Graphics(0xE000_0000, hd(Xbgr8888, 7680)));
    check_eq!(parse(&info(0xE000_0000, 5760, 1920, 1080, 24, 1, BGRX)), Boot::Graphics(0xE000_0000, hd(Rgb888, 5760)));
    check_eq!(parse(&info(0xE000_0000, 3840, 1920, 1080, 16, 1, [11, 5, 5, 6, 0, 5])), Boot::Graphics(0xE000_0000, hd(Rgb565, 3840)));
    check_eq!(parse(&info(0xE000_0000, 8192, 1920, 1080, 32, 1, BGRX)), Boot::Graphics(0xE000_0000, hd(Xrgb8888, 8192)));
    // No framebuffer, or the text screen.
    let mut no_flag = good.clone();
    no_flag[1] = 0;
    check_eq!(parse(&no_flag), Boot::Text);
    check_eq!(parse(&info(0xB8000, 160, 80, 25, 16, 2, [0; 6])), Boot::Text);
    check_eq!(parse(&[]), Boot::Text);
    check_eq!(parse(&[0xFF, 0xFF]), Boot::Text);
    // A graphics mode we must not draw in.
    let unusable = |b: &[u8]| matches!(parse(b), Boot::Unusable(_));
    check!(unusable(&info(0xE000_0000, 7680, 1920, 1080, 32, 0, BGRX))); // colors from a palette
    check!(unusable(&info(0xE000_0000, 7680, 1920, 1080, 32, 7, BGRX))); // no such type
    check!(unusable(&info(0xE000_0000, 7680, 1920, 1080, 32, 1, [8, 8, 16, 8, 24, 8]))); // odd color order
    check!(unusable(&info(0xE000_0000, 1920, 1920, 1080, 8, 1, BGRX)));
    check!(unusable(&info(0xE000_0000, 3840, 1920, 1080, 15, 1, [10, 5, 5, 5, 0, 5])));
    check!(unusable(&info(0xE000_0000, 7680, 1920, 1080, 0, 1, BGRX)));
    check!(unusable(&info(0xE000_0000, 7679, 1920, 1080, 32, 1, BGRX))); // lines too short
    check!(unusable(&info(0xE000_0000, 0, 1920, 1080, 32, 1, BGRX)));
    check!(unusable(&info(0xE000_0000, 7680, 0, 1080, 32, 1, BGRX)));
    check!(unusable(&info(0xE000_0000, 7680, 1920, 0, 32, 1, BGRX)));
    check!(unusable(&info(0, 7680, 1920, 1080, 32, 1, BGRX)));
    check!(unusable(&info(0x1_0000_0000, 7680, 1920, 1080, 32, 1, BGRX))); // above 4 GiB
    check!(unusable(&info(0xFFFF_0000, 7680, 1920, 1080, 32, 1, BGRX))); // runs past 4 GiB
    check!(unusable(&info(u64::MAX, u32::MAX, u32::MAX, u32::MAX, 32, 1, BGRX)));
    // Cut short at any length: never a framebuffer.
    for len in 0..INFO_LEN {
        check!(!matches!(parse(&good[..len]), Boot::Graphics(..)));
    }
    check!(unusable(&good[..INFO_LEN - 1]));
    // Nonsense: whatever comes out as usable passes the framebuffer check.
    let mut seed = 0x1234_5678u32;
    for _ in 0..2000 {
        let mut b = good.clone();
        for byte in b[88..].iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            if seed >> 28 < 3 {
                *byte = (seed >> 16) as u8;
            }
        }
        if let Boot::Graphics(addr, mode) = parse(&b) {
            check!(usable(addr, &mode));
        }
    }
    Ok(())
}
