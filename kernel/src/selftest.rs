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
    ("net: frames, MAC text, descriptor rings", net_basics),
    ("rand: generator", rand_generator),
    ("net: address settings", net_config),
    ("net: two stacks ping each other (beenet in the kernel)", net_stack_ping),
    ("net: TCP connection between two stacks", net_stack_tcp),
    ("net: web addresses, time replies, host:port (phase N5)", net_clients),
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
