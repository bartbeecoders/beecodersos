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
    ("crash: restart or halt afterwards", crash_then),
    ("gfx: the fast drawing paths give the same picture", gfx_fast_paths),
    ("screen in memory: changed parts are copied, in batches", screen_in_memory),
    ("windows: order, focus by click, close box, contents", wm_windows),
    ("text editor: typing, moving, lines, what the window shows", text_editor),
    ("remote control: the keys a text stands for", remote_keys),
    ("file manager: order, choosing, questions, what the window shows", file_manager),
    ("mouse: clicks inside windows, desktop buttons", mouse_clicks),
    ("cpu: name, features, the list of processors", cpu_facts),
    ("processors: every piece of a job runs exactly once, also nested", smp_jobs),
    ("processors: started at boot or not", smp_choice),
    ("web viewer: links, scrolling, back, addresses", web_viewer),
    ("text tools: head, tail, wc, grep", text_tools),
    ("do: the plan in an answer, commands it may not run", do_plan),
    ("monitor: bars, sizes, what the window shows", monitor_render),
    ("time zones: UTC, offsets, CET with summer time, the desktop clock", time_zones),
    ("pictures: BMP and PPM in and out, damaged files, fitting", pictures),
    ("wallpaper: covering the screen, behind the windows", wallpaper),
    ("sound: WAV files, tones, notes", sound_files),
    ("sound: the way from a speaker to a converter", sound_routes),
    ("file manager icons: grid, moving, clicks, thumbnails", file_icons),
    ("crypto for TLS: X25519, HKDF, AES-128-GCM (published vectors)", tls_crypto),
    ("certificates: real RSA and ECDSA chains to trusted roots, wrong names and dates refused", certificates),
    ("web pictures: lines kept for them, where they are drawn", web_pictures),
    ("remote desktop: a viewer's keys, the tiles that changed", vnc_parts),
    ("accounts: the file, passwords, what needs an admin", accounts),
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
    ("display: double-size font, saved settings, write-combining pages", display_g3),
    ("beehive link: saved settings, the hash the proof is built on", hive_link),
    ("install: partition layout, first sector, the sentence to erase a disk", install_layout),
    ("install: what is on a disk (empty, ours, unfinished, another system)", install_inspect),
    ("install: files in the boot partition, also damaged ones", install_fat),
    ("install: memory kept for the bootloader's files; paths on / and /data", install_memory_and_paths),
    ("remote work: which kernel to fetch, where a command's output ends, reading the screen", remote_work),
    ("usb: what devices say about themselves, also cut short", usb_descriptors),
    ("usb: keyboard reports become key presses, mouse reports movement", usb_reports),
    ("usb: xHCI rings and requests", xhci_rings),
    ("desktop: mouse pointer, dragging a window, repairing the screen", wm_mouse),
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
    use beenet::http::{Response, Url};
    let url = Url::parse("http://Example.com:8080/a?b=1").map_err(|e| String::from(e.message()))?;
    check_eq!((url.host.as_str(), url.port, url.path.as_str()), ("example.com", 8080, "/a?b=1"));
    check_eq!(Url::parse("https://example.com/").map(|u| (u.secure, u.port)), Ok((true, 443)));
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
        canvas.glyph(32, 14, b'A', 0xFFFFFF, 0, 1);
        check!(t.shows_at(32, 14, 'A', 0xFFFFFF, 0));
        canvas.glyph(33, 0, b'B', 0xFFFFFF, 0, 1); // one pixel too far right
        canvas.glyph(0, 15, b'B', 0xFFFFFF, 0, 1); // one pixel too low
        canvas.glyph(usize::MAX, usize::MAX, b'B', 0xFFFFFF, 0, 1);
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
    canvas.glyph(0, 0, b'A', 0xFFFFFF, 0, 1);
    check!(t.pixels.iter().all(|&b| b == 0xEE));
    Ok(())
}

fn wm_layout() -> TestResult {
    use crate::gfx::Rect;
    use crate::wm::{terminal_frame, Window};
    let content = |frame: Rect| Window { title: String::new(), frame }.content();
    // Full HD: a window in the middle with desktop all around.
    let frame = terminal_frame(1920, 1080, 1).ok_or("no window on a full HD screen")?;
    let inside = content(frame);
    check_eq!((inside.width / 8, inside.height / 16), (221, 56));
    check_eq!(frame.x, 1920 - frame.x - frame.width); // centered
    check!(frame.x >= 60 && frame.y >= 60);
    // Every screen size: the window is inside the screen, the content inside
    // the window, and the content is a whole number of characters.
    let mut with_window = 0;
    for width in (0..2600).step_by(53) {
        for height in (0..1700).step_by(47) {
            let Some(frame) = terminal_frame(width, height, 1) else { continue };
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
        check_eq!((width, terminal_frame(width, height, 1)), (width, None));
    }
    check!(terminal_frame(320, 200, 1).is_some() && terminal_frame(640, 480, 1).is_some());
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
    let frame = wm::terminal_frame(640, 480, 1).ok_or("no window at 640x480")?;
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
    let mut term = FbCon::in_area(canvas, inside, &mut cells, 1);
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
            let mut term = FbCon::in_area(canvas, inside, &mut cells, 1);
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

fn display_g3() -> TestResult {
    use crate::display::{parse_font, parse_settings, PixelFormat::*};
    use crate::fbcon::FbCon;
    use crate::gfx::Canvas;
    use crate::{paging, wm};
    // Font size: what was asked for, or double above full HD.
    check_eq!((console::font_scale(0, 1080), console::font_scale(0, 1440), console::font_scale(0, 2160)), (1, 2, 2));
    check_eq!((console::font_scale(1, 2160), console::font_scale(2, 480), console::font_scale(7, 600)), (1, 2, 1));
    check_eq!((parse_font("1"), parse_font("2"), parse_font("auto")), (Some(1), Some(2), Some(0)));
    check_eq!((parse_font("3"), parse_font(""), parse_font("0")), (None, None, None));
    // The settings file: lines in any order, nonsense skipped, the last good one counts.
    check_eq!(parse_settings("mode 1280x720\nfont 2\n"), (Some((1280, 720)), Some(2)));
    check_eq!(parse_settings("font auto\n\nmode 800x600"), (Some((800, 600)), Some(0)));
    check_eq!(parse_settings(""), (None, None));
    check_eq!(parse_settings("mode 1x1\nfont 9\nmode\nfont 1 2\nhello\n\u{0}"), (None, None));
    check_eq!(parse_settings("mode 800x600\nmode 99999x1\nfont 2\nfont x"), (Some((800, 600)), Some(2)));
    // A double-size character: every font pixel is 2x2 screen pixels.
    for format in [Xrgb8888, Rgb888, Rgb565] {
        let mut t = TestScreen::new(70, 50, 3, format);
        let canvas = Canvas::new(t.pixels.as_mut_ptr(), t.mode);
        canvas.fill_rect(canvas.bounds(), 0x102030);
        canvas.glyph(4, 2, b'A', 0xFFFFFF, 0, 2);
        let glyph = &crate::font::FONT[b'A' as usize];
        for y in 0..32 {
            for x in 0..16 {
                let want = if glyph[y / 2] & (0x80 >> (x / 2)) != 0 { 0xFFFFFF } else { 0 };
                check_eq!((x, y, t.pixel(4 + x, 2 + y)), (x, y, format.encode(want)));
            }
        }
        check_eq!((t.pixel(3, 2), t.pixel(20, 2), t.pixel(4, 34)), (format.encode(0x102030), format.encode(0x102030), format.encode(0x102030)));
        canvas.glyph(55, 0, b'B', 0xFFFFFF, 0, 2); // 16 wide: one pixel too far right
        canvas.glyph(0, 19, b'B', 0xFFFFFF, 0, 2); // 32 high: one pixel too low
        canvas.glyph(40, 10, b'B', 0xFFFFFF, 0, 99); // at most 4 times: does not fit
        check_eq!((t.pixel(60, 5), t.pixel(5, 40), t.pixel(45, 15)), (format.encode(0x102030), format.encode(0x102030), format.encode(0x102030)));
        check!(t.untouched_outside());
    }
    // A console with the double font: half the characters, same rules.
    let mut t = TestScreen::new(70, 70, 1, Xrgb8888);
    let canvas = Canvas::new(t.pixels.as_mut_ptr(), t.mode);
    let mut cells = vec![0u16; 64];
    let mut screen = FbCon::in_area(canvas, canvas.bounds(), &mut cells, 2);
    check_eq!((screen.width(), screen.height(), screen.scale()), (4, 2, 2));
    screen.clear();
    screen.write_str("ab\ncd\nef");
    check_eq!((screen.scrolls(), screen.pos()), (1, (1, 2)));
    screen.sync_cursor();
    check_eq!(t.rgb(16, 0), t.rgb(17, 1)); // 2x2 blocks
    check_eq!((t.rgb(2 * 16, 32 + 26), t.rgb(2 * 16 + 15, 32 + 29), t.rgb(2 * 16, 32 + 25)), (0xAAAAAA, 0xAAAAAA, 0)); // the cursor, doubled
    for _ in 0..50 {
        screen.write_str("long line that wraps and scrolls\t\n");
    }
    screen.redraw();
    check!(t.untouched_outside());
    let mut cells = vec![0u16; 64];
    check_eq!(FbCon::in_area(canvas, canvas.bounds(), &mut cells, 0).scale(), 1);
    // The terminal window with the double font holds whole double characters.
    let frame = wm::terminal_frame(2560, 1440, 2).ok_or("no window at 2560x1440")?;
    let inside = wm::Window { title: String::new(), frame }.content();
    check_eq!((inside.width % 16, inside.height % 32), (0, 0));
    check!(inside.width / 16 >= 100 && frame.x + frame.width < 2560);
    check!(wm::terminal_frame(400, 300, 2).is_some() && wm::terminal_frame(320, 200, 2).is_none());
    // Write-combining: which 2 MiB pages, and which bits.
    check_eq!(paging::pages_2m(0xFD00_0000, 1920 * 1080 * 4), Some((0xFD00_0000 / 0x20_0000, 4)));
    check_eq!(paging::pages_2m(0x20_0000, 1), Some((1, 1)));
    check_eq!(paging::pages_2m(0x3F_FFFF, 2), Some((1, 2)));
    check_eq!(paging::pages_2m(0xFFE0_0000, 0x20_0000), Some((2047, 1)));
    check_eq!(paging::pages_2m(0xFFE0_0000, 0x20_0001), None); // past 4 GiB
    check_eq!(paging::pages_2m(0x1_0000_0000, 4096), None);
    check_eq!(paging::pages_2m(0x1000, 0), None);
    check_eq!(paging::pages_2m(u64::MAX, 2), None);
    check_eq!(paging::pat_with_wc(0x0007_0406_0007_0406), 0x0007_0406_0007_0106);
    check_eq!(paging::entry_with_wc(0xFD00_0083), 0xFD00_008B); // a present 2 MiB page gets PWT
    check_eq!(paging::entry_with_wc(0xFD00_109B), 0xFD00_008B); // other caching bits go
    check_eq!(paging::entry_with_wc(0xFD00_0082), 0xFD00_0082); // not present: untouched
    check_eq!(paging::entry_with_wc(0x0010_0003), 0x0010_0003); // points to a page table: untouched
    Ok(())
}

fn usb_descriptors() -> TestResult {
    use crate::usb::{class_name, parse_config, HidInterface, Kind};
    // A keyboard as QEMU's describes itself: configuration, interface, HID, endpoint.
    let keyboard: &[u8] = &[
        9, 2, 34, 0, 1, 1, 0, 0xA0, 50, // configuration 1, 34 bytes in all
        9, 4, 0, 0, 1, 3, 1, 1, 0, // interface 0: HID, boot, keyboard
        9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0, // HID descriptor
        7, 5, 0x81, 3, 8, 0, 10, // endpoint 1 in, interrupt, 8 bytes, every 10 ms
    ];
    let config = parse_config(keyboard);
    check_eq!(config.value, 1);
    check_eq!(config.classes, vec![(3, 1, 1)]);
    let want = HidInterface { number: 0, kind: Kind::Keyboard, endpoint: 0x81, max_packet: 8, interval: 10 };
    check_eq!(config.interfaces, vec![want]);
    // A keyboard with a second interface (media keys, no boot protocol) and a mouse behind it.
    let mut combo = keyboard.to_vec();
    combo.extend_from_slice(&[9, 4, 1, 0, 1, 3, 0, 0, 0, 7, 5, 0x82, 3, 4, 0, 10]);
    combo.extend_from_slice(&[9, 4, 2, 0, 1, 3, 1, 2, 0, 7, 5, 0x03, 3, 4, 0, 10, 7, 5, 0x83, 2, 4, 0, 10, 7, 5, 0x84, 3, 4, 0, 7]);
    let config = parse_config(&combo);
    check_eq!(config.classes, vec![(3, 1, 1), (3, 0, 0), (3, 1, 2)]);
    check_eq!(config.interfaces.len(), 2);
    // Of the mouse's endpoints: not the outgoing one, not the bulk one.
    check_eq!(config.interfaces[1], HidInterface { number: 2, kind: Kind::Mouse, endpoint: 0x84, max_packet: 4, interval: 7 });
    // Cut short anywhere: no panic, and never more than the whole gives.
    for len in 0..combo.len() {
        let part = parse_config(&combo[..len]);
        check!(part.interfaces.len() <= 2 && part.classes.len() <= 3);
        check!(part.interfaces.iter().all(|i| config.interfaces.contains(i)));
    }
    check_eq!(parse_config(&combo[..33]).interfaces.len(), 0); // the endpoint is cut off
    // Lengths that lie.
    check_eq!(parse_config(&[9, 2, 9, 0, 1, 1, 0, 0, 0, 0, 4]).classes.len(), 0); // a descriptor of length 0
    check_eq!(parse_config(&[9, 2, 9, 0, 1, 1, 0, 0, 0, 1, 4]).classes.len(), 0); // length 1
    check_eq!(parse_config(&[9, 2, 9, 0, 1, 1, 0, 0, 0, 200, 4, 0, 0]).classes.len(), 0); // longer than the data
    check_eq!(parse_config(&[9, 4, 0, 0, 1, 3, 1, 1, 0]).value, 0); // not a configuration at all
    check_eq!(parse_config(&[2, 2]).value, 0);
    let mut zero_packet = keyboard.to_vec();
    zero_packet[31] = 0;
    check_eq!(parse_config(&zero_packet).interfaces.len(), 0); // an endpoint that carries nothing
    // A device that lists interfaces without end.
    let mut many = vec![9, 2, 9, 0, 1, 1, 0, 0, 0];
    for i in 0..200u8 {
        many.extend_from_slice(&[9, 4, i, 0, 1, 3, 1, 1, 0, 7, 5, 0x81, 3, 8, 0, 10]);
    }
    check!(parse_config(&many).classes.len() <= 16 && parse_config(&many).interfaces.len() <= 16);
    check_eq!((class_name(3, 1, 1), class_name(3, 1, 2), class_name(9, 0, 0), class_name(8, 6, 0x50)), ("keyboard", "mouse", "hub", "storage"));
    Ok(())
}

fn usb_reports() -> TestResult {
    use crate::usb::{keyboard_changes, mouse_report, repeat_key, usage_to_scancode};
    // USB key numbers -> the scancodes of the PS/2 keyboard.
    check_eq!(usage_to_scancode(0x04), Some((false, 0x1E))); // a
    check_eq!(usage_to_scancode(0x1D), Some((false, 0x2C))); // z
    check_eq!(usage_to_scancode(0x1E), Some((false, 0x02))); // 1
    check_eq!(usage_to_scancode(0x27), Some((false, 0x0B))); // 0
    check_eq!(usage_to_scancode(0x28), Some((false, 0x1C))); // Enter
    check_eq!(usage_to_scancode(0x2C), Some((false, 0x39))); // Space
    check_eq!(usage_to_scancode(0x38), Some((false, 0x35))); // /
    check_eq!(usage_to_scancode(0x3A), Some((false, 0x3B))); // F1
    check_eq!(usage_to_scancode(0x52), Some((true, 0x48))); // Up
    check_eq!(usage_to_scancode(0x4C), Some((true, 0x53))); // Delete
    check_eq!(usage_to_scancode(0x64), Some((false, 0x56))); // the key next to left Shift
    check_eq!((usage_to_scancode(0), usage_to_scancode(1), usage_to_scancode(0xFF)), (None, None, None));
    check!((0..=255u8).filter_map(usage_to_scancode).all(|(_, code)| code > 0 && code < 0x80));
    // Press a, add Shift, let go of a, let go of Shift.
    let none = [0u8; 8];
    let a = [0, 0, 0x04, 0, 0, 0, 0, 0];
    let shift_a = [0x02, 0, 0x04, 0, 0, 0, 0, 0];
    let shift = [0x02, 0, 0, 0, 0, 0, 0, 0];
    check_eq!(keyboard_changes(&none, &a), vec![0x1E]);
    check_eq!(keyboard_changes(&a, &shift_a), vec![0x2A]);
    check_eq!(keyboard_changes(&shift_a, &shift), vec![0x9E]);
    check_eq!(keyboard_changes(&shift, &none), vec![0xAA]);
    check_eq!(keyboard_changes(&a, &a), Vec::<u8>::new());
    // Two keys, in any place of the report; releases come before presses.
    let ab = [0, 0, 0x04, 0x05, 0, 0, 0, 0];
    let ba = [0, 0, 0x05, 0x04, 0, 0, 0, 0];
    let bc = [0, 0, 0x05, 0x06, 0, 0, 0, 0];
    check_eq!(keyboard_changes(&ab, &ba), Vec::<u8>::new());
    check_eq!(keyboard_changes(&ab, &bc), vec![0x9E, 0x2E]);
    // Keys with a prefix: right Alt (AltGr) and the arrow keys.
    check_eq!(keyboard_changes(&none, &[0x40, 0, 0x52, 0, 0, 0, 0, 0]), vec![0xE0, 0x38, 0xE0, 0x48]);
    check_eq!(keyboard_changes(&[0x40, 0, 0x52, 0, 0, 0, 0, 0], &none), vec![0xE0, 0xB8, 0xE0, 0xC8]);
    // "Too many keys" and unknown keys change nothing.
    check_eq!(keyboard_changes(&a, &[0, 0, 1, 1, 1, 1, 1, 1]), Vec::<u8>::new());
    check_eq!(keyboard_changes(&none, &[0, 0, 0xF0, 0, 0, 0, 0, 0]), Vec::<u8>::new());
    // The key that repeats: the newest one; modifiers alone do not repeat.
    check_eq!((repeat_key(&none), repeat_key(&shift), repeat_key(&a), repeat_key(&ab)), (None, None, Some(0x04), Some(0x05)));
    // Mouse: buttons, right, down; movement is signed.
    check_eq!(mouse_report(&[1, 5, 0xFB]), Some((1, 5, -5)));
    check_eq!(mouse_report(&[0xFF, 0x80, 0x7F, 9, 9]), Some((7, -128, 127)));
    check_eq!((mouse_report(&[]), mouse_report(&[1, 2])), (None, None));
    Ok(())
}

fn xhci_rings() -> TestResult {
    use crate::xhci::{control_trbs, default_max_packet, endpoint_interval, is_xhci, port_write, Ring, Trb, RING_TRBS};
    check!(is_xhci(0x0C, 0x03, 0x30) && !is_xhci(0x0C, 0x03, 0x20) && !is_xhci(0x0C, 0x05, 0x30));
    // A ring: each TRB gets the cycle bit; the last place links back and flips it.
    let mut ring = Ring::new().ok_or("no page for a ring")?;
    check_eq!(ring.addr() % 4096, 0);
    check!(ring.addr() < 1 << 32);
    let first = ring.push(Trb::new(9, 0x1122_3344_5566_7788, 0xABCD, 1 << 5));
    check_eq!(first, ring.addr());
    check_eq!(ring.peek(0), Trb { param: 0x1122_3344_5566_7788, status: 0xABCD, control: 9 << 10 | 1 << 5 | 1 });
    for i in 1..RING_TRBS - 1 {
        check_eq!(ring.push(Trb::new(1, i as u64, 0, 0)), ring.addr() + i as u64 * 16);
    }
    let link = ring.peek(RING_TRBS - 1);
    check_eq!((link.kind(), link.param, link.control & 3), (6, ring.addr(), 3)); // link, toggle, cycle 1
    check_eq!(ring.push(Trb::new(1, 0x77, 0, 1)), ring.addr()); // second lap: at the start again
    check_eq!(ring.peek(0).control & 1, 0); // with the cycle bit flipped
    check_eq!(ring.peek(0).param, 0x77);
    // GET_DESCRIPTOR(device, 18 bytes): setup, data in, status out.
    let trbs = control_trbs(0x80, 6, 0x0100, 0, 0x5000, 18);
    check_eq!(trbs.len(), 3);
    check_eq!((trbs[0].kind(), trbs[0].param, trbs[0].status), (2, 0x0012_0000_0100_0680, 8));
    check_eq!(trbs[0].control >> 16 & 3, 3); // data comes in
    check!(trbs[0].control & 1 << 6 != 0); // the request is in the TRB itself
    check_eq!((trbs[1].kind(), trbs[1].param, trbs[1].status, trbs[1].control >> 16 & 1), (3, 0x5000, 18, 1));
    check_eq!((trbs[2].kind(), trbs[2].control >> 16 & 1, trbs[2].control >> 5 & 1), (4, 0, 1));
    // SET_CONFIGURATION(1): no data, status in.
    let trbs = control_trbs(0x00, 9, 1, 0, 0x5000, 0);
    check_eq!(trbs.len(), 2);
    check_eq!((trbs[0].control >> 16 & 3, trbs[1].kind(), trbs[1].control >> 16 & 1), (0, 4, 1));
    // Data going out: status comes in.
    let trbs = control_trbs(0x21, 9, 0x0200, 0, 0x5000, 1);
    check_eq!((trbs[0].control >> 16 & 3, trbs[1].control >> 16 & 1, trbs[2].control >> 16 & 1), (2, 0, 1));
    // How often to ask: 10 ms at low speed is 2^6 * 125 µs; high speed counts differently.
    check_eq!((endpoint_interval(2, 10), endpoint_interval(1, 1), endpoint_interval(2, 255), endpoint_interval(2, 0)), (6, 3, 10, 3));
    check_eq!((endpoint_interval(3, 7), endpoint_interval(3, 0), endpoint_interval(4, 200)), (6, 0, 15));
    check_eq!((default_max_packet(2), default_max_packet(1), default_max_packet(3), default_max_packet(4)), (8, 8, 64, 512));
    // Writing a port register never repeats "enabled" (that would switch the
    // port off) or a "changed" flag (that would clear it) by accident.
    let status = 1 | 1 << 1 | 1 << 9 | 3 << 10 | 1 << 17 | 1 << 21;
    check_eq!(port_write(status, 1 << 4), 1 << 9 | 1 << 4);
    check_eq!(port_write(status, 1 << 21), 1 << 9 | 1 << 21);
    Ok(())
}

fn wm_mouse() -> TestResult {
    use crate::display::PixelFormat::*;
    use crate::fbcon::FbCon;
    use crate::gfx::{Canvas, Rect};
    use crate::wm::{self, Desktop, MouseEffect};
    let mut t = TestScreen::new(640, 480, 0, Xrgb8888);
    let canvas = Canvas::new(t.pixels.as_mut_ptr(), t.mode);
    let screen = canvas.bounds();
    let frame = wm::terminal_frame(640, 480, 1).ok_or("no window at 640x480")?;
    let mut desk = Desktop::new();
    desk.open("Terminal", frame);
    let inside = desk.window(0).ok_or("no window")?.content();
    let mut cells = vec![0u16; 8000];
    let mut term = FbCon::in_area(canvas, inside, &mut cells, 1);
    desk.draw(&canvas);
    term.clear();
    term.write_str("hello");
    term.sync_cursor();
    // No pointer until the mouse moves; then it starts in the middle.
    check!(desk.pointer().is_none());
    desk.draw_pointer(&canvas);
    check_eq!(desk.mouse(0, 0, 0, screen), MouseEffect::PointerMoved(Rect::new(320, 240, 12, 19)));
    check_eq!(desk.mouse(0, 0, 0, screen), MouseEffect::Nothing);
    check_eq!(desk.mouse(0, -20, 10, screen), MouseEffect::PointerMoved(Rect::new(320, 240, 12, 19)));
    check_eq!(desk.pointer().map(|p| (p.x, p.y)), Some((300, 250)));
    // It stops at the edges of the screen.
    desk.mouse(0, -5000, -5000, screen);
    check_eq!(desk.pointer().map(|p| (p.x, p.y)), Some((0, 0)));
    desk.mouse(0, i32::MAX, i32::MAX, screen);
    check_eq!(desk.pointer().map(|p| (p.x, p.y)), Some((639, 479)));
    // Drawn at the edge it is cut off, not written outside.
    desk.draw_pointer(&canvas);
    check_eq!(t.rgb(639, 479), 0x000000);
    check!(t.untouched_outside());
    // Over the terminal: black edge, white inside; moving away repairs the text.
    desk.mouse(0, -5000, -5000, screen);
    desk.mouse(0, inside.x as i32, inside.y as i32, screen);
    desk.draw_pointer(&canvas);
    check_eq!((t.rgb(inside.x, inside.y + 5), t.rgb(inside.x + 1, inside.y + 5)), (0x000000, 0xFFFFFF));
    check!(!t.shows_at(inside.x, inside.y, 'h', 0xAAAAAA, 0));
    let MouseEffect::PointerMoved(old) = desk.mouse(0, 200, 100, screen) else { return Err(String::from("the pointer did not move")) };
    desk.draw(&canvas.clipped(old));
    term.redraw_in(old);
    check!(t.shows_at(inside.x, inside.y, 'h', 0xAAAAAA, 0) && t.shows_at(inside.x + 8, inside.y, 'e', 0xAAAAAA, 0));
    check_eq!(t.rgb(0, 0), wm::BACKGROUND);
    // A repair only touches its rectangle.
    canvas.fill_rect(screen, 0x123456);
    desk.draw(&canvas.clipped(Rect::new(0, 0, 10, 10)));
    term.redraw_in(Rect::new(0, 0, 10, 10));
    check_eq!((t.rgb(9, 9), t.rgb(10, 9), t.rgb(9, 10), t.rgb(inside.x, inside.y)), (wm::BACKGROUND, 0x123456, 0x123456, 0x123456));
    canvas.glyph(100, 100, b'A', 0xFFFFFF, 0, 1);
    canvas.fill_rect(Rect::new(100, 100, 8, 16), 0x123456);
    canvas.clipped(Rect::new(104, 100, 100, 100)).glyph(100, 100, b'M', 0xFFFFFF, 0, 1); // only its right half
    check_eq!(t.rgb(103, 108), 0x123456);
    check!((104..108).any(|x| t.rgb(x, 108) != 0x123456));
    desk.draw(&canvas);
    term.redraw();
    // A click inside the window grabs nothing.
    desk.mouse(0, -5000, -5000, screen);
    desk.mouse(0, (inside.x + 50) as i32, (inside.y + 50) as i32, screen);
    desk.mouse(1, 0, 0, screen);
    check!(matches!(desk.mouse(1, 30, 30, screen), MouseEffect::PointerMoved(_)));
    check_eq!(desk.window(0).map(|w| w.frame), Some(frame));
    desk.mouse(0, 0, 0, screen);
    // Pressing on the title bar and moving drags the window along.
    desk.mouse(0, -5000, -5000, screen);
    desk.mouse(0, (frame.x + 100) as i32, (frame.y + 10) as i32, screen);
    check_eq!(desk.mouse(1, 0, 0, screen), MouseEffect::Nothing);
    check_eq!(desk.mouse(1, -7, 5, screen), MouseEffect::WindowMoved);
    let moved = desk.window(0).ok_or("no window")?;
    check_eq!(moved.frame, Rect::new(frame.x - 7, frame.y + 5, frame.width, frame.height));
    let area = moved.content();
    check!(term.move_to(area.x, area.y));
    desk.draw(&canvas);
    term.redraw();
    check!(t.shows_at(inside.x - 7, inside.y + 5, 'h', 0xAAAAAA, 0));
    check_eq!(t.rgb(frame.x - 7 + 100, frame.y + 5 + 3), wm::TITLE_BAR);
    // Dragged far away it stops at the edge of the screen, whole.
    check_eq!(desk.mouse(1, 5000, 5000, screen), MouseEffect::WindowMoved);
    check_eq!(desk.window(0).map(|w| w.frame), Some(Rect::new(640 - frame.width, 480 - frame.height, frame.width, frame.height)));
    check_eq!(desk.mouse(1, -5000, -5000, screen), MouseEffect::WindowMoved);
    check_eq!(desk.window(0).map(|w| (w.frame.x, w.frame.y)), Some((0, 0)));
    // Letting go ends it: the window stays where it is.
    desk.mouse(0, 0, 0, screen);
    check!(matches!(desk.mouse(0, 50, 50, screen), MouseEffect::PointerMoved(_)));
    check_eq!(desk.window(0).map(|w| (w.frame.x, w.frame.y)), Some((0, 0)));
    // The console refuses a place where it does not fit.
    check!(!term.move_to(600, 0) && !term.move_to(0, 470) && !term.move_to(usize::MAX, 5));
    check!(t.untouched_outside());
    // A new desktop takes the pointer over, inside its own screen.
    let old_pointer = desk.pointer();
    let mut small = Desktop::new();
    small.set_pointer(old_pointer, Rect::new(0, 0, 20, 20));
    check_eq!(small.pointer().map(|p| (p.x, p.y)), Some((19, 19)));
    Ok(())
}

fn hive_link() -> TestResult {
    use crate::aibridge::Settings;
    use beenet::hive;
    // The hash, inside the kernel as well (beenet's own tests run on the host).
    check_eq!(hive::hex(&beenet::sha256::sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    check_eq!(
        hive::hex(&beenet::sha256::hmac(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    let key = hive::key_bytes("abcde-fghij-kmnpq-rstuv").ok_or("not a key")?;
    let (a, b) = ("00112233445566778899aabbccddeeff", "ffeeddccbbaa99887766554433221100");
    check_eq!(hive::proof(&key, "os", a, b), "c6a12e0fb9bfc1849ceef735bb0e27218e7150f4366bd88dab638b76d0ccb894");
    // The settings file, there and back.
    let settings = Settings::new("192.168.1.10", 4560, "hp", key).ok_or("settings refused")?;
    let text = settings.to_text();
    check_eq!(text, "host 192.168.1.10\nport 4560\nname hp\nkey abcde-fghij-kmnpq-rstuv\n");
    check_eq!(Settings::parse(&text), Some(settings.clone()));
    check_eq!(Settings::parse("name hp\nkey ABCDE-FGHIJ-KMNPQ-RSTUV\n\nhost 192.168.1.10\nsomething else\n"), Some(settings));
    // Anything missing or wrong: no link at all, rather than a half one.
    for bad in [
        "",
        "host 192.168.1.10\nname hp\n",
        "host 192.168.1.10\nkey abcde-fghij-kmnpq-rstuv\n",
        "name hp\nkey abcde-fghij-kmnpq-rstuv\n",
        "host 192.168.1.10\nname hp\nkey abcde-fghij-klmno-pqrst\n",
        "host 192.168.1.10\nname HP\nkey abcde-fghij-kmnpq-rstuv\n",
        "host 192.168.1.10\nport 0\nname hp\nkey abcde-fghij-kmnpq-rstuv\n",
        "host 192.168.1.10\nport 99999\nname hp\nkey abcde-fghij-kmnpq-rstuv\n",
        "host bad/host\nname hp\nkey abcde-fghij-kmnpq-rstuv\n",
    ] {
        check_eq!((bad, Settings::parse(bad)), (bad, None));
    }
    check!(Settings::new("", 4560, "hp", key).is_none() && Settings::new("host name", 4560, "hp", key).is_none());
    check!(Settings::new("pc.lan", 1, "hp", key).is_some());
    Ok(())
}

fn install_layout() -> TestResult {
    use crate::install::{erase_sentence, first_sector, layout, sentence_matches, size_text, DATA_PARTITION_TYPE};
    const MIB: u64 = 2048;
    // Boot is always the same; os is half of the rest up to 256 MiB; data the rest up to 4 GiB.
    let parts = layout(96 * MIB).ok_or("no layout for 96 MiB")?;
    check_eq!(parts[0], (0xEF, 2048, 16 * MIB));
    check_eq!(parts[1], (0x7F, 17 * MIB, 39 * MIB));
    check_eq!(parts[2], (DATA_PARTITION_TYPE, 56 * MIB, 40 * MIB));
    let big = layout(500_000 * MIB).ok_or("no layout for a large disk")?;
    check_eq!((big[1].2, big[2].2), (256 * MIB, 4096 * MIB));
    check!(layout(u64::MAX).is_some() && layout(1 << 33).is_some()); // beyond 2 TiB: the start is used
    for sectors in [33 * MIB, 40 * MIB, 64 * MIB, 1000 * MIB, 5000 * MIB, u32::MAX as u64, u64::MAX] {
        let p = layout(sectors).ok_or(format!("no layout for {sectors} sectors"))?;
        check!(p[0].1 + p[0].2 == p[1].1 && p[1].1 + p[1].2 == p[2].1); // one after the other
        check!(p[2].1 + p[2].2 <= sectors.min(u32::MAX as u64)); // inside the disk and the table's reach
        check!(p.iter().all(|x| x.1 % MIB == 0 && x.2 % MIB == 0 && x.2 >= 8 * MIB));
    }
    for too_small in [0, 1, 17 * MIB, 32 * MIB, 33 * MIB - 1] {
        check_eq!((too_small, layout(too_small)), (too_small, None));
    }
    // The first sector: the image's boot code, our table, the signature.
    let mut image: beefs::Block = [0xAB; 512];
    image[446..510].fill(0x11);
    let sector = first_sector(&image, &parts);
    check!(sector[..446].iter().all(|b| *b == 0xAB));
    check_eq!((sector[510], sector[511]), (0x55, 0xAA));
    let table = beefs::mbr_partitions(&sector).ok_or("no table")?;
    check_eq!(&table[..3], &parts[..]);
    check_eq!(table[3], (0, 0, 0));
    check_eq!((sector[446], sector[462], sector[478]), (0x80, 0, 0)); // only boot is active
    // The sentence that erases a disk.
    check_eq!((size_text(96 * MIB), size_text(2047 * MIB), size_text(476_940 * MIB)), (String::from("96 MiB"), String::from("2047 MiB"), String::from("465 GiB")));
    let sentence = erase_sentence(476_940 * MIB, "WDC  WD5000AAKX-60U6AA0 ");
    check_eq!(sentence, "erase 465 GiB WDC WD5000AAKX-60U6AA0");
    check!(sentence_matches("erase 465 GiB WDC WD5000AAKX-60U6AA0", &sentence));
    check!(sentence_matches("  ERASE 465 gib  wdc wd5000aakx-60u6aa0 ", &sentence));
    for wrong in ["", "yes", "erase", "erase 465 GiB", "erase 466 GiB WDC WD5000AAKX-60U6AA0", "erase 465 GiB WDC WD5000AAKX-60U6AA", "erase465 GiB WDC WD5000AAKX-60U6AA0"] {
        check!(!sentence_matches(wrong, &sentence));
    }
    check_eq!(erase_sentence(64 * MIB, ""), "erase 64 MiB");
    // Not an install image: wrong size, or no boot partition where it belongs.
    check!(!crate::install::is_image(&[]) && !crate::install::is_image(&[0u8; 512]) && !crate::install::is_image(&vec![0u8; 4096]));
    check_eq!(crate::install::build_in(b"#slots good=a trial=- state=ok      build=000042\ndefault_entry: 1\n"), Some(42));
    check_eq!(crate::install::build_in(b"timeout: 0"), None);
    // A boot area that cannot be read as FAT is written whole.
    check_eq!(crate::install::used_ranges(&[0u8; 2048]), vec![(1, 17 * MIB - 1)]);
    Ok(())
}

fn install_inspect() -> TestResult {
    use crate::install::{first_sector, inspect, layout, Contents};
    let disk = |mib: usize| RamDisk::new(mib << 20);
    let unchanged = |a: &mut RamDisk, b: &mut RamDisk| -> bool {
        let (mut x, mut y) = ([0u8; 512], [0u8; 512]);
        (0..a.block_count()).all(|i| a.read_block(i, &mut x).is_ok() && b.read_block(i, &mut y).is_ok() && x == y)
    };
    // Empty.
    let mut d = disk(2);
    check_eq!(inspect(&mut d), Ok(Contents::Empty));
    // Something in the first sectors, no table.
    d.write_block(5, &[7; 512]).map_err(|e| format!("{e}"))?;
    check_eq!(inspect(&mut d), Ok(Contents::Other(String::from("data without a partition table"))));
    // An empty table with boot code is not empty either.
    let mut d = disk(2);
    let mut sector = [0u8; 512];
    sector[0] = 0xEB;
    sector[510..].copy_from_slice(&[0x55, 0xAA]);
    d.write_block(0, &sector).map_err(|e| format!("{e}"))?;
    check!(matches!(inspect(&mut d), Ok(Contents::Other(_))));
    // Another system: a Linux partition, a Windows one, a GPT disk.
    let entry = |kind: u8, start: u32, len: u32| {
        let mut e = [0u8; 16];
        e[4] = kind;
        e[8..12].copy_from_slice(&start.to_le_bytes());
        e[12..16].copy_from_slice(&len.to_le_bytes());
        e
    };
    let with_table = |entries: &[[u8; 16]]| {
        let mut s = [0u8; 512];
        for (i, e) in entries.iter().enumerate() {
            s[446 + i * 16..462 + i * 16].copy_from_slice(e);
        }
        s[510..].copy_from_slice(&[0x55, 0xAA]);
        s
    };
    let mut d = disk(2);
    d.write_block(0, &with_table(&[entry(0x83, 2048, 2000)])).map_err(|e| format!("{e}"))?;
    check_eq!(inspect(&mut d), Ok(Contents::Other(String::from("partitions of another system (1 partition, type 83)"))));
    d.write_block(0, &with_table(&[entry(0x07, 2048, 1000), entry(0x83, 3048, 1000)])).map_err(|e| format!("{e}"))?;
    check_eq!(inspect(&mut d), Ok(Contents::Other(String::from("partitions of another system (2 partitions, type 07 83)"))));
    d.write_block(0, &with_table(&[entry(0xEE, 1, 4000)])).map_err(|e| format!("{e}"))?;
    check_eq!(inspect(&mut d), Ok(Contents::Other(String::from("a GPT partition table (1 partition, type ee)"))));
    // Our boot partition next to somebody else's: still another system.
    d.write_block(0, &with_table(&[entry(0xEF, 2048, 32768), entry(0x83, 34816, 100)])).map_err(|e| format!("{e}"))?;
    check!(matches!(inspect(&mut d), Ok(Contents::Other(_))));
    // BeeFS over the whole disk.
    let mut d = FileSystem::format(disk(2)).map_err(|e| format!("{e}"))?.into_device();
    check_eq!(inspect(&mut d), Ok(Contents::BeeFsDisk));
    // Ours: the table install writes, with and without a data partition; the build from the boot settings.
    let mut d = disk(40);
    let parts = layout(d.block_count()).ok_or("no layout")?;
    d.write_block(0, &first_sector(&[0; 512], &parts)).map_err(|e| format!("{e}"))?;
    check_eq!(inspect(&mut d), Ok(Contents::Installed { build: None, data: true }));
    let mut conf = [b' '; 512];
    let state = b"#slots good=a trial=- state=ok      build=000042\ndefault_entry: 1\n";
    conf[..state.len()].copy_from_slice(state);
    d.write_block(2048 + 300, &conf).map_err(|e| format!("{e}"))?;
    check_eq!(inspect(&mut d), Ok(Contents::Installed { build: Some(42), data: true }));
    d.write_block(0, &with_table(&[entry(0xEF, 2048, 32768), entry(0x7F, 34816, 1000)])).map_err(|e| format!("{e}"))?;
    check_eq!(inspect(&mut d), Ok(Contents::Installed { build: Some(42), data: false }));
    // An install that stopped before the table was written: a FAT boot partition, no first sector.
    let mut d = disk(2);
    let mut fat = [0u8; 512];
    fat[54..59].copy_from_slice(b"FAT16");
    fat[510..].copy_from_slice(&[0x55, 0xAA]);
    d.write_block(2048, &fat).map_err(|e| format!("{e}"))?;
    d.write_block(3, &[1; 512]).map_err(|e| format!("{e}"))?;
    check_eq!(inspect(&mut d), Ok(Contents::Unfinished));
    // Looking never writes.
    let mut before = disk(2);
    let mut after = disk(2);
    for (i, fill) in [(0u64, 0x55u8), (1, 0xAA), (2048, 0x11), (4000, 0x77)] {
        before.write_block(i, &[fill; 512]).map_err(|e| format!("{e}"))?;
        after.write_block(i, &[fill; 512]).map_err(|e| format!("{e}"))?;
    }
    check!(inspect(&mut after).is_ok());
    check!(unchanged(&mut before, &mut after));
    // A disk too small to have the sectors we look at.
    let mut tiny = RamDisk::new(512);
    check_eq!(inspect(&mut tiny), Ok(Contents::Empty));
    Ok(())
}

fn install_fat() -> TestResult {
    use crate::fat::{Fat16, File, Memory, CONF, KERNEL_A, KERNEL_B};
    // A small FAT16 partition at sector 8: 1 reserved sector, 1 table of 2
    // sectors, 16 root entries (1 sector), 2 sectors per cluster.
    const START: u64 = 8;
    let mut d = RamDisk::new(256 * 512);
    let mut boot = [0u8; 512];
    boot[11..13].copy_from_slice(&512u16.to_le_bytes());
    boot[13] = 2;
    boot[14..16].copy_from_slice(&1u16.to_le_bytes());
    boot[16] = 1;
    boot[17..19].copy_from_slice(&16u16.to_le_bytes());
    boot[22..24].copy_from_slice(&2u16.to_le_bytes());
    d.write_block(START, &boot).map_err(|e| format!("{e}"))?;
    let entry = |name: &[u8; 11], cluster: u16, size: u32| {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name);
        e[26..28].copy_from_slice(&cluster.to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    };
    let mut root = [0u8; 512];
    root[0] = 0xE5; // a deleted file
    root[32 + 11] = 0x0F; // part of a long name
    root[32] = 0x41;
    root[64..96].copy_from_slice(&entry(CONF, 2, 100));
    root[96..128].copy_from_slice(&entry(KERNEL_A, 3, 2500)); // clusters 3 -> 5 -> 4
    root[128..160].copy_from_slice(&entry(b"CIRCLE  ELF", 6, 9000)); // 6 -> 7 -> 6 ...
    root[160..192].copy_from_slice(&entry(b"OUTSIDE ELF", 9000, 600));
    root[192..224].copy_from_slice(&entry(b"SHORT   ELF", 8, 5000)); // the chain ends too early
    d.write_block(START + 3, &root).map_err(|e| format!("{e}"))?;
    let mut table = [0u8; 512];
    let mut link = |from: usize, to: u16| table[from * 2..from * 2 + 2].copy_from_slice(&to.to_le_bytes());
    link(2, 0xFFFF);
    link(3, 5);
    link(5, 4);
    link(4, 0xFFFF);
    link(6, 7);
    link(7, 6);
    link(8, 0xFFFF);
    d.write_block(START + 1, &table).map_err(|e| format!("{e}"))?;

    let fat = Fat16::open(&mut d, START, 248).ok_or("the partition was not recognised")?;
    let data = START + 4; // after the boot sector, the table and the root directory
    check_eq!((fat.sector_of(2), fat.sector_of(3), fat.sector_of(1), fat.sector_of(0)), (Some(data), Some(data + 2), None, None));
    check_eq!(fat.sector_of(60_000), None);
    check_eq!(fat.find(&mut d, CONF), Some(File { first_cluster: 2, size: 100 }));
    check_eq!(fat.find(&mut d, KERNEL_B), None);
    let kernel = fat.find(&mut d, KERNEL_A).ok_or("kernel-a not found")?;
    // 2500 bytes: 5 sectors, following the chain 3, 5, 4.
    check_eq!(fat.sectors(&mut d, &kernel), Some(vec![data + 2, data + 3, data + 6, data + 7, data + 4]));
    check_eq!(fat.sectors(&mut d, &File { first_cluster: 2, size: 0 }), Some(vec![]));
    // Damaged chains give nothing rather than sectors that belong to something else.
    for name in [b"CIRCLE  ELF", b"OUTSIDE ELF", b"SHORT   ELF"] {
        let file = fat.find(&mut d, name).ok_or("a test file is missing")?;
        check_eq!((name, fat.sectors(&mut d, &file)), (name, None));
    }
    check_eq!(fat.sectors(&mut d, &File { first_cluster: 3, size: u64::MAX }), None);
    // Not FAT16: zero fields, a partition smaller than its own tables, one off the disk.
    check!(Fat16::open(&mut d, 100, 100).is_none());
    check!(Fat16::open(&mut d, START, 3).is_none());
    check!(Fat16::open(&mut d, 10_000, 100).is_none());
    check!(Fat16::open(&mut d, START, u64::MAX).is_none());
    // Memory as a read-only disk.
    let bytes = [9u8; 1024 + 100];
    let mut memory = Memory(&bytes);
    let mut block = [0u8; 512];
    check_eq!(memory.block_count(), 2);
    check!(memory.read_block(1, &mut block).is_ok() && block == [9u8; 512]);
    check!(memory.read_block(2, &mut block).is_err() && memory.read_block(u64::MAX, &mut block).is_err());
    check!(memory.write_block(0, &block).is_err());
    Ok(())
}

fn install_memory_and_paths() -> TestResult {
    use crate::storage::route;
    use crate::{allocator, bootmods};
    // The heap takes what is left of a free area around the bootloader's files.
    check_eq!(allocator::without(0x10_0000, 0x800_0000, None), [(0x10_0000, 0x800_0000), (0, 0)]);
    check_eq!(allocator::without(0x10_0000, 0x800_0000, Some((0x20_0000, 0x130_0123))), [(0x10_0000, 0x20_0000), (0x130_1000, 0x800_0000)]);
    check_eq!(allocator::without(0x10_0000, 0x800_0000, Some((0x10_0000, 0x200_0000))), [(0x10_0000, 0x10_0000), (0x200_0000, 0x800_0000)]);
    check_eq!(allocator::without(0x10_0000, 0x800_0000, Some((0x900_0000, 0xA00_0000))), [(0x10_0000, 0x800_0000), (0, 0)]);
    let [below, above] = allocator::without(0x10_0000, 0x800_0000, Some((0x0, 0xFFFF_0000)));
    check!(below.1 <= below.0 && above.1 <= above.0.max(above.1) && above.0 >= 0x800_0000); // nothing left
    let [below, above] = allocator::without(0x10_0000, 0x800_0000, Some((0x30_0800, 0x30_0801)));
    check!(below.1 <= 0x30_0800 && above.0 >= 0x30_0801);
    // The bootloader's list of files: entries that make no sense are left out.
    let list: [u32; 16] = [
        0x20_0000, 0x130_0000, 0, 0, // a good one
        0x500, 0x900, 0, 0, // in low memory
        0x200_0000, 0x200_0000, 0, 0, // empty
        0x300_0000, 0x200_0000, 0, 0, // end before start
    ];
    let read = |addr: u64| list.get(((addr - 0x1000) / 4) as usize).copied().unwrap_or(0xFFFF_FFFF);
    check_eq!(bootmods::parse(1 << 3, 4, 0x1000, read), ([(0x20_0000, 0x130_0000), (0, 0), (0, 0), (0, 0)], 1));
    check_eq!(bootmods::parse(0, 4, 0x1000, read).1, 0); // the flag is not set
    check_eq!(bootmods::parse(1 << 3, 0, 0x1000, read).1, 0);
    check_eq!(bootmods::parse(1 << 3, 4, 0, read).1, 0);
    check!(bootmods::parse(1 << 3, u32::MAX, 0x1000, read).1 <= 4); // a count that lies
    // Paths: /data and below go to the data partition, when there is one.
    check_eq!(route("/data", true), (true, String::from("/")));
    check_eq!(route("/data/notes.txt", true), (true, String::from("/notes.txt")));
    check_eq!(route("/data/a/../b", true), (true, String::from("/b")));
    check_eq!(route("/data/..", true), (false, String::from("/")));
    check_eq!(route("/data/../system/x", true), (false, String::from("/system/x")));
    check_eq!(route("/database", true), (false, String::from("/database")));
    check_eq!(route("/system/data", true), (false, String::from("/system/data")));
    check_eq!(route("/", true), (false, String::from("/")));
    check_eq!(route("/data/notes.txt", false), (false, String::from("/data/notes.txt")));
    Ok(())
}

fn remote_work() -> TestResult {
    use crate::bootslots::Phase;
    use crate::display::PixelFormat::*;
    use crate::hivectl::before_prompt;
    use crate::update::wanted;
    // A kernel on offer is fetched unless we know it already.
    check_eq!(wanted(48, 47, (Phase::Ok, 47)), Ok(()));
    check_eq!(wanted(46, 47, (Phase::Ok, 47)), Ok(())); // an older one too: the host decides
    check!(wanted(47, 47, (Phase::Ok, 40)).is_err()); // it is running
    check!(wanted(48, 47, (Phase::Ok, 48)).is_err()); // installed (the other slot runs for now)
    check!(wanted(48, 47, (Phase::Failed, 48)).is_err()); // tried and failed: not again
    check_eq!(wanted(49, 47, (Phase::Failed, 48)), Ok(())); // the next one is tried
    check!(wanted(49, 47, (Phase::Pending, 48)).is_err() && wanted(49, 47, (Phase::Armed, 48)).is_err());
    // A command is done when the output ends with the shell's prompt.
    check_eq!(before_prompt("hello\nbee:/$ "), Some("hello\n"));
    check_eq!(before_prompt("bee:/data/work$ "), Some(""));
    check_eq!(before_prompt("a\nb\nbee:/x$ "), Some("a\nb\n"));
    for not_yet in ["", "hello\n", "hello", "bee:/$", "bee:/$ x", "bee:/$ \n", "xbee:/$ ", "bee:/a b$ ", "see bee:/$ "] {
        check_eq!((not_yet, before_prompt(not_yet)), (not_yet, None));
    }
    // Reading pixels back: what was encoded comes out again (565 loses the low bits).
    for rgb in [0x000000, 0xFFFFFF, 0xA8D0F0, 0x123456, 0xFF0000, 0x00FF00, 0x0000FF] {
        check_eq!(Xrgb8888.decode(Xrgb8888.encode(rgb)), rgb);
        check_eq!(Xbgr8888.decode(Xbgr8888.encode(rgb)), rgb);
        check_eq!(Rgb888.decode(Rgb888.encode(rgb)), rgb);
        let back = Rgb565.decode(Rgb565.encode(rgb));
        check!((0..3).all(|i| ((back >> (i * 8)) & 0xFF).abs_diff((rgb >> (i * 8)) & 0xFF) <= 8));
    }
    check_eq!((Rgb565.decode(0xFFFF), Rgb565.decode(0)), (0xFFFFFF, 0));
    let mut t = TestScreen::new(16, 8, 3, Rgb888);
    let canvas = crate::gfx::Canvas::new(t.pixels.as_mut_ptr(), t.mode);
    canvas.fill_rect(crate::gfx::Rect::new(2, 1, 3, 2), 0xA8D0F0);
    check_eq!((canvas.pixel(2, 1), canvas.pixel(4, 2), canvas.pixel(5, 2)), (0xA8D0F0, 0xA8D0F0, 0xEEEEEE));
    check_eq!((canvas.pixel(16, 0), canvas.pixel(0, 8), canvas.pixel(usize::MAX, usize::MAX)), (0, 0, 0));
    // Characters of the screen font back to text.
    for c in ['a', 'Z', '~', ' ', 'é', 'ü', '§', '■', 'ñ'] {
        check_eq!(crate::vga::from_cp437(crate::vga::cp437(c)), c);
    }
    check_eq!((crate::vga::from_cp437(0), crate::vga::from_cp437(0x7F), crate::vga::from_cp437(0xB0)), (' ', ' ', ' '));
    Ok(())
}

fn crash_then() -> TestResult {
    use crate::crash::{then, Then};
    // Someone is at the machine: the message stays on the screen.
    check_eq!(then(false, false, 0), Then::Halt);
    check_eq!(then(false, false, 3_600_000), Then::Halt);
    // A kernel on trial restarts whenever it crashes: that is the rollback.
    check_eq!(then(true, false, 0), Then::Restart);
    check_eq!(then(true, true, 5_000), Then::Restart);
    // Driven from the host: restart, but not in a loop right after starting.
    check_eq!(then(false, true, 59_999), Then::Halt);
    check_eq!(then(false, true, 60_000), Then::Restart);
    Ok(())
}

fn gfx_fast_paths() -> TestResult {
    use crate::display::PixelFormat::{Rgb888, Xbgr8888, Xrgb8888};
    use crate::gfx::{Canvas, Rect};
    // 4-byte pixels are drawn a line at a time, 3-byte pixels one by one:
    // the same drawing must give the same picture on both, guard bytes
    // between the lines included.
    let draw = |canvas: &Canvas| {
        canvas.fill_rect(Rect::new(3, 2, 50, 30), 0x123456);
        canvas.fill_rect(Rect::new(60, 40, 100, 100), 0xA8D0F0); // partly outside
        canvas.fill_rect(Rect::new(0, 0, 0, 10), 0xFFFFFF); // empty
        for scale in 1..=4 {
            canvas.glyph(1 + scale * 9, 3 * scale, b'A' + scale as u8, 0xFFFF55, 0x0000AA, scale);
        }
        canvas.glyph(64 - 8, 48 - 16, b'Z', 0xFFFFFF, 0x000000, 1); // last place that fits
        canvas.glyph(64 - 7, 0, b'!', 0xFFFFFF, 0x000000, 1); // does not fit: not drawn
        canvas.clipped(Rect::new(10, 10, 5, 5)).glyph(8, 8, b'#', 0xFF0000, 0x00FF00, 1); // cut: slow path
        canvas.clipped(Rect::new(20, 20, 9, 9)).fill_rect(Rect::new(0, 0, 64, 48), 0x555555);
    };
    let mut reference = TestScreen::new(64, 48, 1, Rgb888);
    let slow = Canvas::new(reference.pixels.as_mut_ptr(), reference.mode);
    draw(&slow);
    for format in [Xrgb8888, Xbgr8888] {
        let mut t = TestScreen::new(64, 48, 5, format);
        let fast = Canvas::new(t.pixels.as_mut_ptr(), t.mode);
        draw(&fast);
        for y in 0..48 {
            for x in 0..64 {
                check_eq!(fast.pixel(x, y), slow.pixel(x, y));
            }
            // The 5 spare bytes after each line are untouched.
            let end = y * t.mode.pitch as usize + 64 * 4;
            check!(t.pixels[end..end + 5].iter().all(|b| *b == 0xEE));
        }
        let size = t.mode.bytes() as usize;
        check!(t.pixels[size..].iter().all(|b| *b == 0xEE));
    }
    Ok(())
}

fn screen_in_memory() -> TestResult {
    use crate::console::show_due;
    use crate::display::PixelFormat::{Rgb565, Xrgb8888};
    use crate::gfx::{Canvas, Rect};
    // Rectangles: what must be copied is the union of what was drawn.
    let (a, b, none) = (Rect::new(10, 20, 5, 5), Rect::new(12, 2, 30, 20), Rect::new(7, 7, 0, 9));
    check_eq!(a.union(&b), Rect::new(10, 2, 32, 23));
    check_eq!((a.union(&none), none.union(&a), none.union(&none).is_empty()), (a, a, true));
    check!(a.overlaps(&b) && !a.overlaps(&Rect::new(15, 20, 5, 5)) && !a.overlaps(&none));
    // The console says which part of its canvas it drew on.
    let mut cells = [0u16; 80 * 30];
    let mut back = TestScreen::new(640, 480, 0, Xrgb8888);
    let mut con = crate::fbcon::FbCon::new(back.pixels.as_mut_ptr(), back.mode, &mut cells);
    con.clear();
    check_eq!(con.take_dirty(), Rect::new(0, 0, 640, 480));
    check!(con.take_dirty().is_empty());
    con.set_pos(3, 10);
    con.write_str("hi");
    con.sync_cursor();
    check_eq!(con.take_dirty(), Rect::new(0, 0, 8, 16).union(&Rect::new(80, 48, 24, 16))); // old cursor, text, new cursor
    // Copying that part to the screen: exactly those pixels arrive.
    let mut front = TestScreen::new(640, 480, 0, Xrgb8888);
    let (from, to) = (Canvas::new(back.pixels.as_mut_ptr(), back.mode), Canvas::new(front.pixels.as_mut_ptr(), front.mode));
    to.copy_from(&from, Rect::new(80, 48, 16, 16));
    check!(front.shows(3, 10, 'h', Color::LightGray, Color::Black) && front.shows(3, 11, 'i', Color::LightGray, Color::Black));
    check_eq!((front.pixel(79, 48), front.pixel(96, 48), front.pixel(80, 47), front.pixel(80, 64)), (0xEEEEEEEE, 0xEEEEEEEE, 0xEEEEEEEE, 0xEEEEEEEE));
    // A part that sticks out is cut off; another pixel format is refused.
    to.copy_from(&from, Rect::new(632, 470, 100, 100));
    check_eq!((front.pixel(639, 479), front.pixel(631, 479)), (0, 0xEEEEEEEE));
    check!(front.pixels[front.mode.bytes() as usize..].iter().all(|b| *b == 0xEE));
    let mut other = TestScreen::new(640, 480, 0, Rgb565);
    Canvas::new(other.pixels.as_mut_ptr(), other.mode).copy_from(&from, Rect::new(0, 0, 640, 480));
    check!(other.pixels.iter().all(|b| *b == 0xEE));
    // When a batch is due: at once if forced or during boot or a crash,
    // else 20 ms after the last one.
    check!(show_due(true, false, 100, 100) && show_due(false, true, 100, 100));
    check!(!show_due(false, false, 119, 100) && show_due(false, false, 120, 100));
    check!(!show_due(false, false, 5, 100)); // a clock that went back: wait
    Ok(())
}

fn wm_windows() -> TestResult {
    use crate::display::PixelFormat::{Rgb565, Xrgb8888};
    use crate::gfx::{Canvas, Rect, Surface};
    use crate::wm::{self, Desktop, MouseEffect};
    let screen = Rect::new(0, 0, 640, 480);
    let mut desk = Desktop::new();
    let (a, b, c) = (
        desk.open("A", Rect::new(20, 20, 300, 200)),
        desk.open("B", Rect::new(100, 100, 300, 200)),
        desk.open("C", Rect::new(400, 300, 200, 150)),
    );
    // Numbers stay with their windows while the order changes.
    check_eq!((a, b, c, desk.top()), (0, 1, 2, Some(c)));
    check!(desk.raise(a) && !desk.raise(a) && !desk.raise(77));
    check_eq!((desk.top(), desk.window(b).map(|w| w.title.as_str())), (Some(a), Some("B")));
    // Alt+Tab: the window at the back comes up, so every window gets a turn.
    let mut seen = Vec::new();
    for _ in 0..3 {
        check!(desk.cycle());
        seen.push(desk.top());
    }
    check_eq!(seen, [Some(b), Some(c), Some(a)]);
    // A click on a window that lies lower brings it up; on the top window
    // the order stays. (Inside a window the click is also for its program:
    // see the test of mouse clicks.)
    let in_b = desk.window(b).ok_or("b")?.content();
    check_eq!(desk.mouse(0, 0, 0, screen), MouseEffect::PointerMoved(Rect::new(320, 240, wm::POINTER_WIDTH, wm::POINTER_HEIGHT)));
    // 350,180: only B is there
    check_eq!(desk.mouse(1, 30, -60, screen), MouseEffect::Click { id: b, x: 350 - in_b.x, y: 180 - in_b.y, raised: true });
    check_eq!(desk.top(), Some(b));
    check_eq!(desk.mouse(0, 0, 0, screen), MouseEffect::Nothing);
    check_eq!(desk.mouse(1, 0, 0, screen), MouseEffect::Click { id: b, x: 350 - in_b.x, y: 180 - in_b.y, raised: false });
    check_eq!(desk.top(), Some(b));
    desk.mouse(0, 0, 0, screen);
    // On its title bar: it comes up, and that is all.
    check!(desk.raise(a));
    desk.mouse(0, 0, -75, screen); // 350,105
    check_eq!((desk.mouse(1, 0, 0, screen), desk.top()), (MouseEffect::Raised, Some(b)));
    desk.mouse(0, 0, 0, screen);
    desk.mouse(0, 0, 75, screen);
    // Where A and B overlap, the click goes to the one on top (B) ...
    desk.mouse(0, -150, -30, screen); // 200,150
    check_eq!(desk.mouse(1, 0, 0, screen), MouseEffect::Click { id: b, x: 200 - in_b.x, y: 150 - in_b.y, raised: false });
    check_eq!(desk.top(), Some(b));
    desk.mouse(0, 0, 0, screen);
    // ... and on the desktop itself to nobody.
    desk.mouse(0, 300, 100, screen); // 500,250
    check_eq!((desk.mouse(1, 0, 0, screen), desk.top()), (MouseEffect::Nothing, Some(b)));
    desk.mouse(0, 0, 0, screen);
    // The close box of a lower window: it comes up and asks to be closed.
    let close = desk.window(c).and_then(|w| w.close_box()).ok_or("no close box")?;
    let p = desk.pointer().ok_or("no pointer")?;
    desk.mouse(0, close.x as i32 + 2 - p.x as i32, close.y as i32 + 2 - p.y as i32, screen);
    check_eq!((desk.mouse(1, 0, 0, screen), desk.top()), (MouseEffect::Close(c), Some(c)));
    // Moving with the button still down does not drag it: the box is no handle.
    check_eq!(desk.mouse(1, 5, 5, screen), MouseEffect::PointerMoved(Rect::new(close.x + 2, close.y + 2, wm::POINTER_WIDTH, wm::POINTER_HEIGHT)));
    desk.mouse(0, 0, 0, screen);
    check!(desk.close(c) && !desk.close(c) && desk.window(c).is_none());
    check_eq!(desk.top(), Some(b));
    check_eq!(desk.open("D", Rect::new(0, 0, 50, 50)), 3); // numbers are not used again
    check!(desk.close(3));
    // A window too narrow for a close box has none.
    check!(wm::Window { title: String::new(), frame: Rect::new(0, 0, 20, 60) }.close_box().is_none());
    check_eq!(wm::frame_size(640, 400), (640 + 12, 400 + 12 + 24));

    // Contents: each window's picture is put in after its frame, back to
    // front, so a window on top covers what lies below.
    let mut t = TestScreen::new(640, 480, 0, Xrgb8888);
    let canvas = Canvas::new(t.pixels.as_mut_ptr(), t.mode);
    let (red, green) = (Surface::new(40, 30, Xrgb8888).ok_or("surface")?, Surface::new(40, 30, Xrgb8888).ok_or("surface")?);
    red.canvas().fill_rect(Rect::new(0, 0, 40, 30), 0xFF0000);
    green.canvas().fill_rect(Rect::new(0, 0, 40, 30), 0x00FF00);
    let mut order = Vec::new();
    desk.draw_with(&canvas, None, |id, window| {
        order.push(id);
        let at = window.content();
        canvas.blit(&(if id == a { red.canvas() } else { green.canvas() }), at.x, at.y);
    });
    check_eq!(order, [a, b]);
    let (in_a, in_b) = (desk.window(a).ok_or("a")?.content(), desk.window(b).ok_or("b")?.content());
    check_eq!((t.rgb(in_a.x, in_a.y), t.rgb(in_a.x + 39, in_a.y + 29), t.rgb(in_a.x + 40, in_a.y + 29)), (0xFF0000, 0xFF0000, wm::CONTENT));
    check_eq!((t.rgb(in_b.x, in_b.y), t.rgb(in_b.x + 39, in_b.y + 29)), (0x00FF00, 0x00FF00));
    check_eq!(t.rgb(150, 101), 0x1F3A5F); // B's border lies over A's content
    check_eq!((t.rgb(0, 0), t.rgb(639, 479)), (wm::BACKGROUND, wm::BACKGROUND));
    // A picture that sticks out of the canvas is cut off; one in another
    // pixel format is refused; one without pixels cannot be made.
    canvas.blit(&red.canvas(), 620, 465);
    check_eq!((t.rgb(639, 479), t.rgb(619, 479), t.rgb(620, 464)), (0xFF0000, wm::BACKGROUND, wm::BACKGROUND));
    check!(t.pixels[t.mode.bytes() as usize..].iter().all(|b| *b == 0xEE));
    canvas.clipped(Rect::new(0, 0, 5, 5)).blit(&green.canvas(), 3, 3);
    check_eq!((t.rgb(3, 3), t.rgb(4, 4), t.rgb(5, 4), t.rgb(4, 5)), (0x00FF00, 0x00FF00, wm::BACKGROUND, wm::BACKGROUND));
    let other = Surface::new(8, 8, Rgb565).ok_or("surface")?;
    other.canvas().fill_rect(Rect::new(0, 0, 8, 8), 0xFFFFFF);
    canvas.blit(&other.canvas(), 0, 0);
    check_eq!(t.rgb(0, 0), wm::BACKGROUND);
    check!(Surface::new(0, 10, Xrgb8888).is_none() && Surface::new(10, 0, Xrgb8888).is_none());
    check!(Surface::new(usize::MAX, 2, Xrgb8888).is_none());
    Ok(())
}

fn text_editor() -> TestResult {
    use crate::input::Key;
    use crate::textedit::{Action, Editor};
    let keys = |e: &mut Editor, keys: &[Key]| keys.iter().for_each(|k| check_none(e.key(*k, 10)));
    fn check_none(action: Action) {
        assert!(action == Action::None);
    }
    let typed = |e: &mut Editor, text: &str| text.chars().for_each(|c| check_none(e.key(Key::Char(c), 10)));
    let row_text = |cells: &[u16]| cells.iter().map(|c| crate::vga::from_cp437(*c as u8)).collect::<String>();

    // A file is shown as it is and written back as it was.
    let mut e = Editor::new("one\ntwo\n");
    check_eq!((e.text().as_str(), e.modified(), e.position()), ("one\ntwo\n", false, (1, 1)));
    check_eq!(Editor::new("").text(), "\n"); // a new file: one empty line
    check_eq!(Editor::new("a\r\nb").text(), "a\nb\n"); // DOS line ends go, a missing last one comes

    // Typing, Enter in the middle of a line, Backspace and Delete across lines.
    keys(&mut e, &[Key::End]);
    typed(&mut e, "!!");
    check_eq!((e.text().as_str(), e.modified(), e.position()), ("one!!\ntwo\n", true, (1, 6)));
    keys(&mut e, &[Key::Left, Key::Left, Key::Enter]);
    check_eq!((e.text().as_str(), e.position()), ("one\n!!\ntwo\n", (2, 1)));
    keys(&mut e, &[Key::Backspace]);
    check_eq!((e.text().as_str(), e.position()), ("one!!\ntwo\n", (1, 4)));
    keys(&mut e, &[Key::Delete, Key::Delete, Key::Delete]);
    check_eq!((e.text().as_str(), e.position()), ("onetwo\n", (1, 4)));
    keys(&mut e, &[Key::Delete, Key::Delete, Key::Delete, Key::Delete, Key::Backspace, Key::Backspace, Key::Backspace, Key::Backspace]);
    check_eq!((e.text().as_str(), e.position()), ("\n", (1, 1))); // nothing left to remove: no harm

    // Moving: line ends wrap, a shorter line pulls the cursor in, pages.
    let mut e = Editor::new("long line\nab\n\nlast");
    keys(&mut e, &[Key::End, Key::Down]);
    check_eq!(e.position(), (2, 3));
    keys(&mut e, &[Key::Right]);
    check_eq!(e.position(), (3, 1));
    keys(&mut e, &[Key::Left, Key::Home, Key::Left]);
    check_eq!(e.position(), (1, 10));
    keys(&mut e, &[Key::Up, Key::Up, Key::PageDown]);
    check_eq!(e.position(), (4, 5));
    keys(&mut e, &[Key::Down, Key::Right, Key::PageUp]);
    check_eq!((e.position(), e.modified()), ((1, 5), false));

    // Tab goes to the next multiple of four; Enter keeps the indent; Ctrl+K cuts.
    let mut e = Editor::new("");
    typed(&mut e, "a");
    keys(&mut e, &[Key::Tab]);
    typed(&mut e, "b");
    check_eq!(e.text(), "a   b\n");
    let mut e = Editor::new("    if x {");
    keys(&mut e, &[Key::End, Key::Enter]);
    typed(&mut e, "y");
    check_eq!((e.text().as_str(), e.position()), ("    if x {\n    y\n", (2, 6)));
    keys(&mut e, &[Key::Up, Key::Home, Key::Right, Key::Right, Key::Ctrl('k')]);
    check_eq!(e.text(), "  \n    y\n");
    keys(&mut e, &[Key::Ctrl('k')]);
    check_eq!(e.text(), "      y\n");

    // Saving and leaving: Ctrl+Q with changes asks first; any other key takes the question back.
    let mut e = Editor::new("x");
    check_eq!(e.key(Key::Ctrl('q'), 10), Action::Quit);
    typed(&mut e, "y");
    check_eq!(e.key(Key::Ctrl('q'), 10), Action::None);
    let (lines, _) = e.render("f.txt", 80, 5);
    check!(row_text(&lines[0]).starts_with(" changes not saved: Ctrl+Q again"));
    check_eq!(lines[0][0] >> 8, 0x4F);
    keys(&mut e, &[Key::Right]);
    check_eq!(e.key(Key::Ctrl('q'), 10), Action::None);
    check_eq!(e.key(Key::Ctrl('q'), 10), Action::Quit);
    check_eq!(e.key(Key::Ctrl('s'), 10), Action::Save);
    e.saved(Err("the file could not be written"));
    check!(e.modified() && row_text(&e.render("f.txt", 80, 5).0[0]).starts_with(" NOT saved: the file could not be written"));
    e.saved(Ok(3));
    check!(!e.modified() && row_text(&e.render("f.txt", 80, 5).0[0]).starts_with(" saved, 3 bytes"));
    check_eq!(e.key(Key::Ctrl('q'), 10), Action::Quit);

    // What the window shows: the status line, then the text, scrolled so
    // that the cursor is in view, every line exactly as wide as the window.
    let text: String = (1..=30).map(|n| alloc::format!("line {n} is here\n")).collect();
    let mut e = Editor::new(&text);
    let (lines, cursor) = e.render("notes.txt", 20, 6);
    check_eq!((lines.len(), cursor), (6, (1, 0)));
    check!(lines.iter().all(|l| l.len() == 20));
    check_eq!((row_text(&lines[0]).as_str(), lines[0][19] >> 8), (" notes.txt  line 1 o", 0x70));
    check_eq!((row_text(&lines[1]).as_str(), row_text(&lines[5]).as_str(), lines[1][0] >> 8), ("line 1 is here      ", "line 5 is here      ", 0x07));
    keys(&mut e, &[Key::PageDown, Key::PageDown]);
    let (lines, cursor) = e.render("notes.txt", 20, 6);
    check_eq!((e.position(), cursor, row_text(&lines[5]).trim_end()), ((21, 1), (5, 0), "line 21 is here"));
    keys(&mut e, &[Key::Up, Key::Up, Key::Up, Key::Up, Key::Up]);
    let (lines, cursor) = e.render("notes.txt", 20, 6);
    check_eq!((cursor, row_text(&lines[1]).trim_end()), ((1, 0), "line 16 is here"));
    // A long line scrolls sideways.
    let mut e = Editor::new("0123456789abcdefghijklmnopqrstuvwxyz");
    keys(&mut e, &[Key::End]);
    let (lines, cursor) = e.render("f", 10, 3);
    check_eq!((row_text(&lines[1]).as_str(), cursor), ("rstuvwxyz ", (1, 9)));
    keys(&mut e, &[Key::Home]);
    check_eq!(row_text(&e.render("f", 10, 3).0[1]), "0123456789");
    // Past the end of the text the window is empty; a window of nothing does not break it.
    let mut e = Editor::new("only");
    let (lines, _) = e.render("f", 8, 4);
    check_eq!((row_text(&lines[1]).as_str(), row_text(&lines[2]).as_str(), row_text(&lines[3]).as_str()), ("only    ", "        ", "        "));
    check_eq!(e.render("f", 0, 0).0.len(), 2);
    Ok(())
}

fn remote_keys() -> TestResult {
    use crate::hivectl::{parse_keys, MAX_KEYS};
    use crate::input::Key::*;
    check_eq!(parse_keys(""), Some(vec![]));
    check_eq!(parse_keys("ls é\n"), Some(vec![Char('l'), Char('s'), Char(' '), Char('é'), Enter]));
    check_eq!(
        parse_keys("a{enter}{ctrl+s}{alt+tab}{up}{pagedown}{lbrace}}{esc}"),
        Some(vec![Char('a'), Enter, Ctrl('s'), NextWindow, Up, PageDown, Char('{'), Char('}'), Escape])
    );
    check_eq!(parse_keys("{tab}{backspace}{delete}{down}{left}{right}{home}{end}{pageup}"), Some(vec![Tab, Backspace, Delete, Down, Left, Right, Home, End, PageUp]));
    // Not keys: unknown names, Ctrl with anything but one small letter, a brace left open.
    for bad in ["{nosuchkey}", "{}", "{ctrl+}", "{ctrl+S}", "{ctrl+ab}", "{ctrl+1}", "abc{enter", "{", "{Enter}"] {
        check_eq!((bad, parse_keys(bad)), (bad, None));
    }
    // Bounded.
    check_eq!(parse_keys(&"x".repeat(MAX_KEYS)).map(|k| k.len()), Some(MAX_KEYS));
    check_eq!(parse_keys(&"x".repeat(MAX_KEYS + 1)), None);
    Ok(())
}

fn file_manager() -> TestResult {
    use crate::filemgr::{join, parent, valid_name, Action, Browser, Entry};
    use crate::input::Key;
    let entry = |name: &str, folder: bool, size: u64| Entry { name: String::from(name), folder, size };
    let row_text = |cells: &[u16]| cells.iter().map(|c| crate::vga::from_cp437(*c as u8)).collect::<String>();
    let typed = |b: &mut Browser, text: &str| text.chars().for_each(|c| assert!(b.key(Key::Char(c), 5) == Action::None));

    // Paths.
    check_eq!((join("/", "a").as_str(), join("/data", "a").as_str(), join("/data/", "a").as_str()), ("/a", "/data/a", "/data/a"));
    check_eq!((parent("/").as_str(), parent("/a").as_str(), parent("/a/b").as_str(), parent("/a/b/").as_str()), ("/", "/", "/a", "/a"));
    check!(valid_name("notes.txt") && valid_name("a b") && valid_name("..."));
    check!(!valid_name("") && !valid_name(".") && !valid_name("..") && !valid_name("a/b") && !valid_name(&"x".repeat(61)));

    // Folders first, then by name whatever the case.
    let mut b = Browser::new();
    let files = || vec![entry("zeta.txt", false, 10), entry("Beta", true, 0), entry("alpha.txt", false, 1234), entry("apps", true, 0)];
    b.show("/data", files(), None);
    let (lines, cursor) = b.render(40, 8);
    check_eq!((lines.len(), cursor), (8, (0, 39)));
    check!(lines.iter().all(|l| l.len() == 40));
    check_eq!(row_text(&lines[0]).trim_end(), " /data  (1 of 4)");
    check_eq!(row_text(&lines[1]).trim_end(), "   <folder>  apps/");
    check_eq!(row_text(&lines[2]).trim_end(), "   <folder>  Beta/");
    check_eq!(row_text(&lines[3]).trim_end(), "       1234  alpha.txt");
    check_eq!(row_text(&lines[4]).trim_end(), "         10  zeta.txt");
    check_eq!((row_text(&lines[5]).trim_end(), row_text(&lines[7]).trim_end()), ("", " Enter open  e edit  Backspace up  v ico"));
    // The chosen line stands out; folders have their own color.
    check_eq!((lines[1][0] >> 8, lines[2][0] >> 8, lines[3][0] >> 8, lines[0][0] >> 8), (0x1F, 0x09, 0x07, 0x70));

    // Choosing and opening.
    check_eq!(b.key(Key::Enter, 5), Action::Go(String::from("/data/apps")));
    check_eq!(b.key(Key::Down, 5), Action::None);
    check_eq!(b.key(Key::Down, 5), Action::None);
    check_eq!(b.chosen().map(|e| e.name.as_str()), Some("alpha.txt"));
    check_eq!(b.key(Key::Enter, 5), Action::Open(String::from("/data/alpha.txt")));
    check_eq!(b.key(Key::Char('e'), 5), Action::Edit(String::from("/data/alpha.txt")));
    b.key(Key::PageDown, 5);
    b.key(Key::Down, 5);
    check_eq!(b.chosen().map(|e| e.name.as_str()), Some("zeta.txt"));
    b.key(Key::Home, 5);
    b.key(Key::Up, 5);
    check_eq!(b.chosen().map(|e| e.name.as_str()), Some("apps"));
    check_eq!(b.key(Key::Backspace, 5), Action::Go(String::from("/")));
    // Coming out of a folder the bar is on it; in the root there is no way up.
    b.show("/", vec![entry("system", true, 0), entry("data", true, 0), entry("a.txt", false, 1)], Some("data"));
    check_eq!((b.folder(), b.chosen().map(|e| e.name.as_str())), ("/", Some("data")));
    check_eq!(b.key(Key::Backspace, 5), Action::None);
    check_eq!(b.key(Key::Enter, 5), Action::Go(String::from("/data")));
    check_eq!((b.key(Key::Char('u'), 5), b.key(Key::Char('q'), 5)), (Action::Go(String::from("/")), Action::Quit));

    // Deleting asks first; anything but y leaves it.
    b.show("/data", files(), Some("zeta.txt"));
    check_eq!(b.key(Key::Char('d'), 5), Action::None);
    let (lines, _) = b.render(60, 8);
    check_eq!((row_text(&lines[0]).trim_end(), lines[0][0] >> 8), (" Delete zeta.txt? y: yes, any other key: no", 0x4F));
    check_eq!(b.key(Key::Enter, 5), Action::None);
    check_eq!(row_text(&b.render(60, 8).0[0]).trim_end(), " nothing was deleted");
    check_eq!(b.key(Key::Delete, 5), Action::None);
    check_eq!(b.key(Key::Char('y'), 5), Action::Delete(String::from("/data/zeta.txt")));
    // The list is one shorter: the bar stays on the last line.
    b.show("/data", files().into_iter().filter(|e| e.name != "zeta.txt").collect(), None);
    check_eq!(b.chosen().map(|e| e.name.as_str()), Some("alpha.txt"));

    // A new folder: the name is typed in the status line, with the cursor after it.
    check_eq!(b.key(Key::Char('n'), 5), Action::None);
    typed(&mut b, "pix/");
    check_eq!(b.key(Key::Backspace, 5), Action::None);
    typed(&mut b, "cs");
    let (lines, cursor) = b.render(60, 8);
    check_eq!((row_text(&lines[0]).trim_end(), cursor), (" Name of the new folder: pics", (0, 29)));
    check_eq!(b.key(Key::Enter, 5), Action::MakeFolder(String::from("/data/pics")));
    // Escape drops the question; an empty name too; a bad one is refused.
    b.key(Key::Char('n'), 5);
    typed(&mut b, "x");
    check_eq!((b.key(Key::Escape, 5), b.key(Key::Char('q'), 5)), (Action::None, Action::Quit));
    b.key(Key::Char('n'), 5);
    check_eq!((b.key(Key::Enter, 5), b.key(Key::Char('q'), 5)), (Action::None, Action::Quit));
    b.key(Key::Char('n'), 5);
    typed(&mut b, "..");
    check_eq!(b.key(Key::Enter, 5), Action::None);
    check_eq!(row_text(&b.render(60, 8).0[0]).trim_end(), " not a usable name (no '/', not '.' or '..')");
    // Renaming starts from the old name.
    check_eq!(b.key(Key::Char('r'), 5), Action::None);
    check_eq!(row_text(&b.render(60, 8).0[0]).trim_end(), " New name for alpha.txt: alpha.txt");
    for _ in 0..9 {
        b.key(Key::Backspace, 5);
    }
    typed(&mut b, "beta.md");
    check_eq!(b.key(Key::Enter, 5), Action::Rename(String::from("/data/alpha.txt"), String::from("/data/beta.md")));

    // An empty folder: nothing to open, rename or delete; a long list scrolls with the bar.
    b.show("/empty", Vec::new(), None);
    check_eq!(row_text(&b.render(40, 6).0[0]).trim_end(), " /empty  (empty)");
    for key in [Key::Enter, Key::Char('r'), Key::Char('d'), Key::Down, Key::End, Key::PageDown] {
        check_eq!(b.key(key, 5), Action::None);
    }
    check_eq!(b.key(Key::Char('q'), 5), Action::Quit);
    b.show("/many", (0..50).map(|n| entry(&alloc::format!("f{n:02}"), false, n)).collect(), None);
    b.key(Key::PageDown, 4);
    b.key(Key::PageDown, 4);
    let (lines, _) = b.render(30, 6); // 4 entries fit
    check_eq!((row_text(&lines[1]).trim_end(), row_text(&lines[4]).trim_end(), lines[4][0] >> 8), ("          5  f05", "          8  f08", 0x1F));
    b.key(Key::End, 4);
    check_eq!(row_text(&b.render(30, 6).0[4]).trim_end(), "         49  f49");
    check_eq!(b.render(0, 0).0.len(), 3);
    Ok(())
}

fn mouse_clicks() -> TestResult {
    use crate::display::PixelFormat::Xrgb8888;
    use crate::filemgr::{Action, Browser, Entry};
    use crate::gfx::{Canvas, Rect};
    use crate::input::Key;
    use crate::wm::{self, Desktop, MouseEffect};
    let screen = Rect::new(0, 0, 640, 480);
    let mut desk = Desktop::new();
    // Buttons in a row at the top; one that does not fit is left out.
    let places = desk.set_buttons(&["Terminal", "Files", "Notes"], 640);
    check_eq!(places, [Rect::new(100, 6, 80, 24), Rect::new(188, 6, 56, 24), Rect::new(252, 6, 56, 24)]);
    check_eq!(Desktop::new().set_buttons(&["Terminal", "Files", "Notes"], 250).len(), 2);
    let (a, b) = (desk.open("A", Rect::new(20, 60, 300, 200)), desk.open("B", Rect::new(200, 100, 300, 200)));
    let mut t = TestScreen::new(640, 480, 0, Xrgb8888);
    desk.draw(&Canvas::new(t.pixels.as_mut_ptr(), t.mode));
    check_eq!((t.rgb(100, 6), t.rgb(102, 8), t.rgb(99, 6), t.rgb(180, 6), t.rgb(184, 10)), (0x1F3A5F, wm::TITLE_BAR, wm::BACKGROUND, wm::BACKGROUND, wm::BACKGROUND));
    check!(t.shows_at(108, 10, 'T', 0xFFFFFF, wm::TITLE_BAR) && t.shows_at(196, 10, 'F', 0xFFFFFF, wm::TITLE_BAR));

    // Move the pointer to a place and press and let go there.
    let click = |desk: &mut Desktop, x: usize, y: usize| {
        desk.mouse(0, 0, 0, screen);
        let p = desk.pointer().unwrap_or_else(|| panic!("no pointer"));
        desk.mouse(0, x as i32 - p.x as i32, y as i32 - p.y as i32, screen);
        let effect = desk.mouse(1, 0, 0, screen);
        desk.mouse(0, 0, 0, screen);
        effect
    };
    // On a button: start what it stands for. Next to it, on the desktop: nothing.
    check_eq!(click(&mut desk, 110, 10), MouseEffect::Launch(0));
    check_eq!(click(&mut desk, 300, 29), MouseEffect::Launch(2));
    check_eq!(click(&mut desk, 184, 10), MouseEffect::Nothing);
    check_eq!(click(&mut desk, 600, 400), MouseEffect::Nothing);
    // Inside the top window: where, counted from the top left of its content.
    let inside = desk.window(b).ok_or("b")?.content();
    check_eq!(click(&mut desk, inside.x, inside.y), MouseEffect::Click { id: b, x: 0, y: 0, raised: false });
    check_eq!(click(&mut desk, inside.x + 17, inside.y + 40), MouseEffect::Click { id: b, x: 17, y: 40, raised: false });
    // In a window that lies lower: it comes up, and the click counts.
    let inside_a = desk.window(a).ok_or("a")?.content();
    check_eq!(click(&mut desk, inside_a.x + 5, inside_a.y + 100), MouseEffect::Click { id: a, x: 5, y: 100, raised: true });
    check_eq!(desk.top(), Some(a));
    // On the frame around the content (the padding, the title bar): no click for the program.
    check_eq!(click(&mut desk, inside_a.x - 2, inside_a.y + 5), MouseEffect::Nothing);
    check_eq!(click(&mut desk, 30, 65), MouseEffect::Nothing);
    // A window over a button gets the click, not the button.
    let c = desk.open("C", Rect::new(90, 0, 200, 100));
    check!(matches!(click(&mut desk, 110, 10), MouseEffect::Nothing | MouseEffect::Click { .. }));
    check_eq!(desk.top(), Some(c));

    // The file manager: a click on an entry puts the bar there, a second one opens it.
    let entry = |name: &str, folder: bool| Entry { name: String::from(name), folder, size: 1 };
    let mut files = Browser::new();
    files.show("/", vec![entry("docs", true), entry("a.txt", false), entry("b.txt", false)], None);
    files.render(40, 8);
    check_eq!(files.key(Key::Click(3, 3), 6), Action::None);
    check_eq!(files.chosen().map(|e| e.name.as_str()), Some("b.txt"));
    check_eq!(files.key(Key::Click(30, 3), 6), Action::Open(String::from("/b.txt")));
    // What opens what: by the ending, in any case.
    use crate::filemgr::{opener, Opener};
    check_eq!(opener("/data/Photo.PNG"), Opener::Picture);
    check_eq!(opener("clip.gif"), Opener::Picture);
    check_eq!(opener("/song.wav"), Opener::Sound);
    check_eq!(opener("/notes.txt"), Opener::Text);
    check_eq!(opener("/wav"), Opener::Text);
    check_eq!(opener("/a.wav.txt"), Opener::Text);
    check_eq!(files.key(Key::Click(0, 1), 6), Action::None);
    check_eq!(files.key(Key::Click(0, 1), 6), Action::Go(String::from("/docs")));
    // On the status line, below the list, or on the line of keys: nothing.
    for (col, row) in [(0, 0), (5, 4), (5, 7), (0, 200)] {
        check_eq!(files.key(Key::Click(col, row), 6), Action::None);
        check_eq!(files.chosen().map(|e| e.name.as_str()), Some("docs"));
    }
    // The editor: a click puts the cursor there, within the text.
    let mut edit = crate::textedit::Editor::new("first\nsecond line\nthird");
    edit.render("f", 40, 10);
    edit.key(Key::Click(3, 2), 9);
    check_eq!(edit.position(), (2, 4));
    edit.key(Key::Click(30, 1), 9);
    check_eq!(edit.position(), (1, 6)); // past the end of the line: at its end
    edit.key(Key::Click(2, 9), 9);
    check_eq!(edit.position(), (3, 3)); // below the text: on the last line
    edit.key(Key::Click(0, 0), 9);
    check_eq!((edit.position(), edit.modified()), ((3, 3), false)); // the status line is not text
    Ok(())
}

fn cpu_facts() -> TestResult {
    use crate::cpu::{clean_name, features, madt_processors, Raw, MAX_PROCESSORS};
    check_eq!(clean_name(b"  Intel(R) Core(TM)   i5-4590 CPU @ 3.30GHz\0\0junk"), "Intel(R) Core(TM) i5-4590 CPU @ 3.30GHz");
    check_eq!((clean_name(b"").as_str(), clean_name(&[0xFF, b'A', 7, b'B']).as_str()), ("", "AB"));
    // This very processor: it has a name, and sse2 (every 64-bit one does).
    check!(!crate::cpu::name().is_empty());
    check!(features(crate::cpu::raw()).contains(&"sse2"));
    check_eq!(features(Raw { one: (0, 0), seven_ebx: 0 }), Vec::<&str>::new());
    check_eq!(features(Raw { one: (1 << 28 | 1 << 31, 1 << 26), seven_ebx: 1 << 5 }), ["sse2", "avx", "avx2", "vm"]);

    // A table of processors: header (44 bytes), then entries.
    let mut table = vec![0u8; 44];
    let mut entry = |kind: u8, body: &[u8]| {
        table.push(kind);
        table.push(body.len() as u8 + 2);
        table.extend_from_slice(body);
    };
    entry(0, &[0, 0, 1, 0, 0, 0]); // processor 0, APIC 0, enabled
    entry(1, &[0; 10]); // an I/O APIC: not a processor
    entry(0, &[1, 2, 1, 0, 0, 0]); // APIC 2, enabled
    entry(0, &[2, 4, 0, 0, 0, 0]); // APIC 4: not usable
    entry(0, &[3, 6, 2, 0, 0, 0]); // APIC 6: can be switched on
    entry(9, &[0, 0, 0x00, 0x01, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0]); // x2APIC 256, enabled
    entry(9, &[0, 0, 0x01, 0x01, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0]); // x2APIC 257: not usable
    check_eq!(madt_processors(&table), [0, 2, 6, 256]);
    // Cut off anywhere, it gives the entries that are whole and reads nothing beyond.
    for len in 0..table.len() {
        let found = madt_processors(&table[..len]);
        check!(found.len() <= 4 && [0u32, 2, 6, 256].starts_with(&found));
    }
    // An entry with a length that cannot be ends the list; a huge table is cut off.
    let mut bad = table.clone();
    bad[45] = 0;
    check_eq!(madt_processors(&bad), Vec::<u32>::new());
    bad[45] = 1;
    check_eq!(madt_processors(&bad), Vec::<u32>::new());
    let mut many = vec![0u8; 44];
    for i in 0..1000u32 {
        many.extend_from_slice(&[0, 8, i as u8, i as u8, 1, 0, 0, 0]);
    }
    check_eq!(madt_processors(&many).len(), MAX_PROCESSORS);
    check!(!crate::cpu::processors().is_empty());
    Ok(())
}

fn smp_jobs() -> TestResult {
    use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};
    // On however many processors are in use (one, unless `cpu start` was given).
    check!(crate::smp::running() >= 1);
    let counts: Vec<AtomicU32> = (0..1000).map(|_| AtomicU32::new(0)).collect();
    let sum = AtomicUsize::new(0);
    for round in 0..20 {
        crate::smp::run(counts.len(), &|n| {
            counts[n].fetch_add(1, SeqCst);
            sum.fetch_add(n, SeqCst);
        });
        check!(counts.iter().all(|c| c.load(SeqCst) == round + 1));
    }
    check_eq!(sum.load(SeqCst), 20 * (999 * 1000 / 2));
    // No pieces: nothing runs. One piece: it gets number 0.
    crate::smp::run(0, &|_| panic!("a job without pieces ran"));
    let only = AtomicUsize::new(usize::MAX);
    crate::smp::run(1, &|n| only.store(n, SeqCst));
    check_eq!(only.load(SeqCst), 0);
    // A job inside a piece (as a copy from the timer interrupt may be)
    // runs on that processor alone, and every piece of both runs once.
    let inner = AtomicUsize::new(0);
    crate::smp::run(8, &|_| crate::smp::run(10, &|n| { inner.fetch_add(n + 1, SeqCst); }));
    check_eq!(inner.load(SeqCst), 8 * 55);
    Ok(())
}

fn smp_choice() -> TestResult {
    let choice = crate::smp::choice;
    // Nothing said: they start.
    check!(choice("", None) && choice("test slot=a", None));
    // What was saved: only "auto off" keeps them waiting.
    check!(!choice("test", Some("auto off")) && !choice("", Some("auto off\n")));
    check!(choice("", Some("auto on\n")) && choice("", Some("")) && choice("", Some("off")));
    // A boot flag wins over what was saved.
    check!(choice("test smp=on", Some("auto off")) && !choice("smp=off", Some("auto on")) && !choice("smp=off", None));
    check!(choice("nosmp=off smps=off", None));
    Ok(())
}

fn web_viewer() -> TestResult {
    use crate::web::{resolve, Action, Viewer};
    use alloc::string::ToString;
    // Where links lead: on the disk, and on the web.
    check_eq!(resolve("/site/a/index.html", "b.html"), Ok("/site/a/b.html".to_string()));
    check_eq!(resolve("/site/a/index.html", "../top.html#x"), Ok("/site/top.html".to_string()));
    check_eq!(resolve("/site/a/index.html", "/../../etc"), Ok("/etc".to_string()));
    check_eq!(resolve("http://h:8080/a/b/c.html", "../d.html?q=1"), Ok("http://h:8080/a/d.html?q=1".to_string()));
    check_eq!(resolve("http://h/a/", "/x"), Ok("http://h/x".to_string()));
    check_eq!(resolve("http://h/a/", "http://other.org"), Ok("http://other.org/".to_string()));
    check_eq!(resolve("http://h/a/", "//cdn.org/s"), Ok("http://cdn.org/s".to_string()));
    check_eq!(resolve("http://h/", "https://secure.org/"), Ok("https://secure.org/".to_string()));
    check_eq!(resolve("https://h/a/b.html", "c.html"), Ok("https://h/a/c.html".to_string()));
    check_eq!(resolve("https://h/a/", "//cdn.org/s"), Ok("https://cdn.org/s".to_string()));
    check!(resolve("http://h/", "mailto:someone@example.org").is_err());
    check!(resolve("/a.html", "javascript:void(0)").is_err());
    // What is typed: a path, an address, or words to search for.
    let typed = crate::web::typed_address;
    check_eq!(typed("/site/a.html"), Ok("/site/a.html".to_string()));
    check_eq!(typed("example.org/x"), Ok("http://example.org/x".to_string()));
    check_eq!(typed("10.0.2.2:8080"), Ok("http://10.0.2.2:8080/".to_string()));
    check_eq!(typed("localhost"), Ok("http://localhost/".to_string()));
    check_eq!(typed("honey bees"), Ok("http://frogfind.com/?q=honey%20bees".to_string()));
    check_eq!(typed("rust"), Ok("http://frogfind.com/?q=rust".to_string()));
    check_eq!(typed("what is 2+2? "), Ok("http://frogfind.com/?q=what%20is%202%2B2%3F".to_string()));
    check_eq!(typed("https://en.wikipedia.org/wiki/Bee"), Ok("https://en.wikipedia.org/wiki/Bee".to_string()));
    check!(crate::web::looks_like_html("http://h/", "<!DOCTYPE html><p>x"));
    check!(!crate::web::looks_like_html("http://h/", "just text, 1 < 2"));
    check!(!crate::web::looks_like_html("/notes.txt", "<p>shown as it is</p>"));

    let mut v = Viewer::new();
    let mut body = String::from("<title>Start</title><h1>Top</h1><p><a href=one.html>One</a> and <a href='https://x/'>Secure</a>");
    for i in 0..40 {
        body.push_str(&alloc::format!("<p>line {i}</p>"));
    }
    body.push_str("<p><a href=/last.html>Last</a>");
    v.show("/site/index.html", &body, false, false);
    let (w, h) = (40, 12);
    let lines = v.lines(w);
    check_eq!(v.title(), "Start");
    check_eq!(v.address(), "/site/index.html");
    check_eq!(lines[0].as_str(), "Top");
    check_eq!(lines[2].as_str(), "One and Secure");
    check_eq!(v.link(0), Some("one.html"));
    // Enter without a link chosen does nothing; Tab picks the first one.
    check_eq!(v.key(Key::Enter, w, h), Action::None);
    check_eq!(v.key(Key::Tab, w, h), Action::None);
    check_eq!(v.key(Key::Enter, w, h), Action::Open("/site/one.html".to_string()));
    // The second link is a secure page: it opens like any other.
    check_eq!(v.key(Key::Tab, w, h), Action::None);
    check_eq!(v.key(Key::Enter, w, h), Action::Open("https://x/".to_string()));
    // `f` reads a page from the web through the reader; this one is on the disk.
    check_eq!(v.key(Key::Char('f'), w, h), Action::None);
    let (cells, _) = v.render(w, h);
    let status: String = cells[0].iter().map(|c| (*c & 0xFF) as u8 as char).collect();
    check!(status.starts_with(" f reads a page from the web"));
    // The third link is far down: Tab scrolls to it.
    check_eq!(v.key(Key::Tab, w, h), Action::None);
    let (cells, _) = v.render(w, h);
    let shown: Vec<String> = cells.iter().map(|l| l.iter().map(|c| (*c & 0xFF) as u8 as char).collect::<String>()).collect();
    check!(shown.iter().any(|l| l.starts_with("Last")));
    check_eq!(v.key(Key::Enter, w, h), Action::Open("/last.html".to_string()));
    // Scrolling stays within the page.
    check_eq!(v.key(Key::Home, w, h), Action::None);
    check_eq!(v.key(Key::Up, w, h), Action::None);
    let (cells, _) = v.render(w, h);
    check_eq!(cells[1][0] & 0xFF, b'T' as u16);
    // A click on a link follows it (row 3 of the window is line 2 of the page).
    check_eq!(v.key(Key::Click(1, 3), w, h), Action::Open("/site/one.html".to_string()));
    check_eq!(v.key(Key::Click(5, 3), w, h), Action::None); // " and " is no link
    // Going on and coming back.
    check_eq!(v.key(Key::Backspace, w, h), Action::None); // nothing to go back to
    v.show("/site/one.html", "<p>Page one <a href=index.html>home</a>", false, false);
    check_eq!(v.key(Key::Backspace, w, h), Action::Back("/site/index.html".to_string()));
    v.show("/site/index.html", &body, true, false);
    check_eq!(v.key(Key::Backspace, w, h), Action::None); // the way back was used up
    // `g` asks for an address; one without http:// is a web address.
    check_eq!(v.key(Key::Char('g'), w, h), Action::None);
    for c in "example.org/x".chars() {
        check_eq!(v.key(Key::Char(c), w, h), Action::None);
    }
    let (_, cursor) = v.render(w, h);
    check_eq!(cursor, (0, 8 + 13));
    check_eq!(v.key(Key::Enter, w, h), Action::Open("http://example.org/x".to_string()));
    check_eq!(v.key(Key::Char('r'), w, h), Action::Reload("/site/index.html".to_string()));
    check_eq!(v.key(Key::Char('q'), w, h), Action::Quit);
    // Plain text is shown as it is, and a narrow window gets the page anew.
    v.show("/notes.txt", "a <b>  c\nnext", false, false);
    check_eq!(v.lines(40), alloc::vec!["a <b>  c".to_string(), "next".to_string()]);
    check_eq!(v.lines(10).len(), 2);
    Ok(())
}

fn text_tools() -> TestResult {
    use crate::textops::*;
    let text = "one\ntwo\nthree\nfour\n";
    check_eq!(head(text, 2), "one\ntwo\n");
    check_eq!(head(text, 9), text);
    check_eq!(head(text, 0), "");
    check_eq!(tail(text, 2), "three\nfour\n");
    check_eq!(tail(text, 4), text);
    check_eq!(tail(text, 9), text);
    check_eq!(tail("a\nb", 1), "b");
    check_eq!(tail("", 3), "");
    check_eq!(count(text), (4, 4, 19));
    check_eq!(count("no end"), (1, 2, 6));
    check_eq!(count(""), (0, 0, 0));
    check_eq!(count("\n\n"), (2, 0, 2));
    let found: Vec<(usize, &str)> = grep("Bee\nbee hive\nwasp\n", "bee", false).collect();
    check_eq!(found, alloc::vec![(2, "bee hive")]);
    let found: Vec<(usize, &str)> = grep("Bee\nbee hive\nwasp\n", "BEE", true).collect();
    check_eq!(found, alloc::vec![(1, "Bee"), (2, "bee hive")]);
    check!(contains("anything", "", true) && !contains("", "x", true));
    check!(is_binary(b"ab\0c") && !is_binary(b"text\n"));
    check_eq!(parse_count(&["f"]), Some((10, "f")));
    check_eq!(parse_count(&["-n", "3", "f"]), Some((3, "f")));
    check_eq!(parse_count(&["-5", "f"]), Some((5, "f")));
    check_eq!(parse_count(&["-n", "x", "f"]), None);
    check_eq!(parse_count(&[]), None);
    Ok(())
}

fn do_plan() -> TestResult {
    use crate::doplan::{parse, refused, MAX_COMMANDS};
    let lines = |l: &[&str]| l.iter().map(|s| String::from(*s)).collect::<Vec<String>>();
    let (cmds, notes) = parse(&lines(&["I will make the folder.", "$ mkdir /photos", "  $ echo hi > /photos/a.txt", "$ ", "$x", "", "Done."]));
    check_eq!(cmds, lines(&["mkdir /photos", "echo hi > /photos/a.txt"]));
    check_eq!(notes, lines(&["I will make the folder.", "$x", "", "Done."]));
    // Bounded: at most MAX_COMMANDS, no control characters, no endless lines.
    let many: Vec<String> = (0..30).map(|i| alloc::format!("$ echo {i}")).collect();
    check_eq!(parse(&many).0.len(), MAX_COMMANDS);
    check!(parse(&lines(&["$ echo a\u{1b}[2J"])).0.is_empty());
    check!(parse(&[alloc::format!("$ echo {}", "x".repeat(300))]).0.is_empty());
    // What a plan may never run.
    for cmd in ["install sata0 --erase", "update", "reboot", "shutdown", "crash", "ai off", "remote on", "httpd on", "net off", "do it again", "cpu auto off", "desktop off", "layout us"] {
        check!(refused(cmd).is_some());
    }
    for cmd in ["mkdir /a", "echo hi > /a/b.txt", "ls", "cpu", "cpu test", "desktop", "rm /a/b.txt", "grep -i x /"] {
        check!(refused(cmd).is_none());
    }
    Ok(())
}

fn monitor_render() -> TestResult {
    use crate::monitor::{bar, bytes, render, Sample};
    check_eq!(bar(0, 100, 10), "[..........]");
    check_eq!(bar(50, 100, 10), "[#####.....]");
    check_eq!(bar(100, 100, 10), "[##########]");
    check_eq!(bar(500, 100, 10), "[##########]");
    check_eq!(bar(5, 0, 4), "[....]");
    check_eq!(bytes(512), "512 B");
    check_eq!(bytes(1536), "1.5 KiB");
    check_eq!(bytes(3 << 20), "3.0 MiB");
    let before = Sample { micros: 1_000_000, tsc: 1000, busy: alloc::vec![0, 100], heap_size: 1 << 20, ..Sample::default() };
    let now = Sample {
        micros: 2_000_000,
        tsc: 2000,
        busy: alloc::vec![1000, 350],
        heap_used: 256 << 10,
        heap_size: 1 << 20,
        rx_bytes: 2048,
        tx_bytes: 0,
        threads: alloc::vec![(String::from("main"), String::from("running")), (String::from("net"), String::from("waiting"))],
        ..Sample::default()
    };
    let lines: Vec<String> = render(&before, &now, 60).into_iter().map(|(text, _)| text).collect();
    check!(lines.iter().all(|l| l.chars().count() <= 60));
    check!(lines.iter().any(|l| l == " Processors: 3 in use"));
    check!(lines.iter().any(|l| l.starts_with("  cpu 1  [") && l.ends_with("100% on jobs")));
    check!(lines.iter().any(|l| l.starts_with("  cpu 2  [") && l.ends_with(" 25% on jobs")));
    check!(lines.iter().any(|l| l.contains("256.0 KiB of 1.0 MiB")));
    check!(lines.iter().any(|l| l.starts_with("  in  2.0 KiB/s  (2.0 KiB so far)   out 0 B/s")));
    check!(lines.iter().any(|l| l == " Threads: 2"));
    check!(lines.iter().any(|l| l.starts_with("  net ") && l.ends_with("waiting")));
    // A narrow window: nothing wider than it.
    check!(render(&before, &now, 20).iter().all(|(l, _)| l.chars().count() <= 30));
    Ok(())
}

fn time_zones() -> TestResult {
    use crate::time::{local, offset, parse_zone, zone_name, Zone};
    check_eq!(parse_zone("utc"), Some(Zone::Utc));
    check_eq!(parse_zone(" CET\n"), Some(Zone::Cet));
    check_eq!(parse_zone("+05:30"), Some(Zone::Fixed(330)));
    check_eq!(parse_zone("-3"), Some(Zone::Fixed(-180)));
    check_eq!(parse_zone("+00:00"), Some(Zone::Utc));
    for bad in ["", "+", "+15", "+14:30", "+1:60", "5", "CETT", "+a:00", "+123"] {
        check_eq!(parse_zone(bad), None);
    }
    check_eq!(zone_name(Zone::Fixed(-570)), "UTC-09:30");
    check_eq!(zone_name(Zone::Cet), "CET");
    // What is saved comes back.
    for zone in [Zone::Utc, Zone::Cet, Zone::Fixed(330), Zone::Fixed(-570)] {
        check_eq!(parse_zone(&zone_name(zone)), Some(zone));
    }
    // EU summer time 2026: from 29 March 01:00 UTC to 25 October 01:00 UTC.
    let spring = crate::time::days_from_civil(2026, 3, 29) as u64 * 86400 + 3600;
    let autumn = crate::time::days_from_civil(2026, 10, 25) as u64 * 86400 + 3600;
    check_eq!(offset(Zone::Cet, spring - 1).0, 60);
    check_eq!(offset(Zone::Cet, spring).0, 120);
    check_eq!(offset(Zone::Cet, autumn - 1), (120, String::from("CEST")));
    check_eq!(offset(Zone::Cet, autumn), (60, String::from("CET")));
    // 2026-11-18 19:05:31 UTC
    let t = crate::time::days_from_civil(2026, 11, 18) as u64 * 86400 + 19 * 3600 + 5 * 60 + 31;
    check_eq!(local(Zone::Cet, t), "2026-11-18 20:05:31 CET");
    check_eq!(local(Zone::Utc, t), "2026-11-18 19:05:31 UTC");
    check_eq!(local(Zone::Fixed(330), t), "2026-11-19 00:35:31 UTC+05:30");
    check_eq!(crate::clock::text(Zone::Cet, t), "Wed 18 Nov 20:05");
    check_eq!(crate::clock::text(Zone::Utc, 0), "Thu 1 Jan 00:00");
    // On the desktop: right of the buttons, and only where it fits.
    let mut desk = crate::wm::Desktop::new();
    desk.set_buttons(&["Terminal"], 640);
    let first = desk.set_clock("Wed 18 Nov 20:05");
    check!(first.is_some_and(|r| r.x + r.width <= 640 && r.x > 180));
    check_eq!(desk.set_clock("Wed 18 Nov 20:05"), None); // nothing changed
    check!(desk.set_clock("Wed 18 Nov 20:06").is_some());
    let mut narrow = crate::wm::Desktop::new();
    narrow.set_buttons(&["Terminal", "Files"], 300);
    check_eq!(narrow.set_clock("Wed 18 Nov 20:05"), None); // does not fit
    Ok(())
}

fn pictures() -> TestResult {
    use crate::image::{decode, encode_bmp, fit, Image};
    // A BMP written here comes back the same (3 pixels wide: the lines need padding).
    let colors = [0xFF0000, 0x00FF00, 0x0000FF, 0x123456, 0xFFFFFF, 0x000000];
    let bmp = encode_bmp(3, 2, |x, y| colors[y * 3 + x]);
    check_eq!(bmp.len(), 54 + 12 * 2);
    check_eq!(decode(&bmp), Ok(Image { width: 3, height: 2, pixels: colors.to_vec() }));
    // Stored from the bottom (positive height), as most programs write it.
    let mut bottom_up = bmp.clone();
    bottom_up[22..26].copy_from_slice(&2i32.to_le_bytes());
    check_eq!(decode(&bottom_up).map(|i| i.pixels), Ok(alloc::vec![0x123456, 0xFFFFFF, 0x000000, 0xFF0000, 0x00FF00, 0x0000FF]));
    // PPM with a comment.
    let ppm = b"P6\n# made by hand\n2 1\n255\n\xff\x00\x00\x00\x00\xff";
    check_eq!(decode(ppm), Ok(Image { width: 2, height: 1, pixels: alloc::vec![0xFF0000, 0x0000FF] }));
    // Damaged or cut off anywhere: an error, never a crash.
    for end in 0..bmp.len() {
        check!(decode(&bmp[..end]).is_err());
    }
    for end in 0..ppm.len() {
        check!(decode(&ppm[..end]).is_err());
    }
    check!(decode(b"GIF89a").is_err());
    check!(decode(b"P6 5000 5000 255 ").is_err()); // too large
    check!(decode(b"P6 2 1 65535 ").is_err());
    let mut huge = bmp.clone();
    huge[18..22].copy_from_slice(&100_000i32.to_le_bytes());
    check!(decode(&huge).is_err());
    let mut negative = bmp.clone();
    negative[18..22].copy_from_slice(&(-3i32).to_le_bytes());
    check!(decode(&negative).is_err());
    // Fitting: as large as fits, at most twice the size.
    check_eq!(fit(1920, 1080, 640, 480), (640, 360, 33));
    check_eq!(fit(100, 50, 640, 480), (200, 100, 200));
    check_eq!(fit(640, 480, 640, 480), (640, 480, 100));
    check_eq!(fit(10_000, 1, 100, 100), (100, 1, 1));
    check_eq!(fit(0, 5, 100, 100), (0, 0, 0));
    // A PNG (palette, 4 bits) made by ImageMagick, through the same door as the others.
    // A JPEG made on every processor (rows of blocks) is the same as one made in order.
    let jpg: &[u8] = include_bytes!("../../beenet/testdata/jphoto.jpg");
    let parallel = decode(jpg).map_err(String::from)?;
    let (w, h, serial) = beenet::jpeg::decode(jpg).map_err(String::from)?;
    check_eq!((parallel.width, parallel.height), (w, h));
    check!(parallel.pixels == serial);
    check_eq!(crate::image::dimensions(jpg), Some((160, 120)));
    check_eq!(crate::filemgr::opener("/photos/Bee.JPG"), crate::filemgr::Opener::Picture);
    // An MJPEG video (ffmpeg): its frames are pictures for the same decoder.
    let clip: &[u8] = include_bytes!("../../beenet/testdata/clip.avi");
    let avi = beenet::avi::parse(clip).map_err(String::from)?;
    check!(avi.is_mjpeg() && avi.frames.len() == 10 && avi.sound.is_some());
    let frame = decode(avi.frame(clip, 9).unwrap_or(&[])).map_err(String::from)?;
    check_eq!((frame.width, frame.height), (64, 48));
    check_eq!(crate::filemgr::opener("/films/Bees.AVI"), crate::filemgr::Opener::Video);
    let png = decode(include_bytes!("../../beenet/testdata/pal.png")).map_err(String::from)?;
    check_eq!((png.width, png.height), (37, 23));
    check!(decode(&include_bytes!("../../beenet/testdata/pal.png")[..100]).is_err());
    // A PNG made on every processor (pieces compressed apart) reads back the same.
    let colors = |x: usize, y: usize| if (x / 9 + y / 7) % 3 == 0 { 0x203040 } else { ((x * 7) as u32 & 0xFF) << 16 | ((y * 3) as u32 & 0xFF) << 8 | (x ^ y) as u32 & 0xFF };
    let png = crate::image::encode_png(173, 61, &colors);
    let back = decode(&png).map_err(String::from)?;
    check_eq!((back.width, back.height), (173, 61));
    check!(back.pixels.iter().enumerate().all(|(i, p)| *p == colors(i % 173, i / 173)));
    check!(png.len() < 173 * 61 * 3 / 2);
    // An animated GIF (ImageMagick): the first frame, through the same door.
    let gif = decode(include_bytes!("../../beenet/testdata/anim.gif")).map_err(String::from)?;
    check_eq!((gif.width, gif.height), (40, 30));
    check_eq!(gif.pixels[0], 0x000080); // navy
    check_eq!(beenet::gif::decode(include_bytes!("../../beenet/testdata/anim.gif")).map(|g| g.frames.len()), Ok(5));
    Ok(())
}

fn wallpaper() -> TestResult {
    use crate::image::cover_pixel;
    // Same shape: one to one, corners to corners.
    check_eq!(cover_pixel(100, 50, 200, 100, 0, 0), (0, 0));
    check_eq!(cover_pixel(100, 50, 200, 100, 199, 99), (99, 49));
    // A square picture on a wide screen: scaled to the width, the top and
    // bottom cut off evenly.
    check_eq!(cover_pixel(100, 100, 200, 100, 0, 0), (0, 25));
    check_eq!(cover_pixel(100, 100, 200, 100, 199, 99), (99, 74));
    // A tall screen: the sides are cut off.
    check_eq!(cover_pixel(100, 100, 100, 200, 0, 0), (25, 0));
    // Every pixel of the screen comes from inside the picture.
    for (w, h, rw, rh) in [(3, 7, 640, 480), (1920, 1080, 800, 600), (1, 1, 5, 5), (640, 480, 1, 1)] {
        for (x, y) in [(0, 0), (rw - 1, 0), (0, rh - 1), (rw - 1, rh - 1)] {
            let (sx, sy) = cover_pixel(w, h, rw, rh, x, y);
            check!(sx < w && sy < h);
        }
    }
    // Drawn behind the windows.
    let format = crate::display::PixelFormat::Xrgb8888;
    let (Some(screen), Some(paper)) = (crate::gfx::Surface::new(320, 240, format), crate::gfx::Surface::new(320, 240, format)) else {
        return Err(String::from("no memory for the test"));
    };
    paper.canvas().fill_rect(paper.canvas().bounds(), 0x336699);
    let mut desk = crate::wm::Desktop::new();
    desk.open("Window", crate::gfx::Rect::new(100, 100, 100, 80));
    desk.draw_with(&screen.canvas(), Some(&paper.canvas()), |_, _| {});
    check_eq!(screen.canvas().pixel(5, 5), 0x336699);
    check!(screen.canvas().pixel(150, 150) != 0x336699); // the window covers it
    desk.draw_with(&screen.canvas(), None, |_, _| {});
    check_eq!(screen.canvas().pixel(5, 5), crate::wm::BACKGROUND);
    Ok(())
}

fn vnc_parts() -> TestResult {
    use crate::vnc::{changed_tiles, Keys, TILE};
    let mut keys = Keys::default();
    check_eq!(keys.event(true, b'a' as u32), Some(Key::Char('a')));
    check_eq!(keys.event(false, b'a' as u32), None); // letting go types nothing
    check_eq!(keys.event(true, 0xE9), Some(Key::Char('é'))); // no layout applies
    check_eq!(keys.event(true, 0xFF0D), Some(Key::Enter));
    // Control held: letters become Ctrl+letter, until it is let go.
    check_eq!(keys.event(true, 0xFFE3), None);
    check_eq!(keys.event(true, b'C' as u32), Some(Key::Ctrl('c')));
    check_eq!(keys.event(true, b'1' as u32), Some(Key::Char('1')));
    check_eq!(keys.event(false, 0xFFE3), None);
    check_eq!(keys.event(true, b'c' as u32), Some(Key::Char('c')));
    // Alt+Tab: the next window.
    check_eq!(keys.event(true, 0xFFE9), None);
    check_eq!(keys.event(true, 0xFF09), Some(Key::NextWindow));
    check_eq!(keys.event(false, 0xFFE9), None);
    check_eq!(keys.event(true, 0xFF09), Some(Key::Tab));
    check_eq!(keys.event(true, 0xFFBE), None); // F1: nothing yet
    // Tiles: everything the first time, then only what changed.
    let (w, h) = (150, 70); // 3 x 2 tiles, the last ones narrower
    let old = alloc::vec![0u32; w * h];
    check_eq!(changed_tiles(&[], &old, w, h).len(), 6);
    check!(changed_tiles(&old, &old, w, h).is_empty());
    let mut new = old.clone();
    new[66 * w + 149] = 0xFFFFFF; // bottom right corner
    check_eq!(changed_tiles(&old, &new, w, h), alloc::vec![(2 * TILE, TILE, w - 2 * TILE, h - TILE)]);
    new[0] = 1;
    check_eq!(changed_tiles(&old, &new, w, h).len(), 2);
    Ok(())
}

fn accounts() -> TestResult {
    use crate::users::{format_all, hash_password, needs_admin, parse, valid_name, User};
    let alice = User::new("alice", &["admin"], "0123456789abcdef", "s3cret-bee");
    let bob = User::new("bob", &[], "fedcba9876543210", "hunter22");
    check!(alice.password_ok("s3cret-bee") && !alice.password_ok("s3cret-bef") && !alice.password_ok(""));
    check!(alice.is_admin() && !bob.is_admin());
    // The same password with another salt gives another hash; the password itself is never in the file.
    check!(hash_password("0123456789abcdef", "x") != hash_password("0123456789abcdee", "x"));
    let text = format_all(&[alice.clone(), bob.clone()]);
    check!(!text.contains("s3cret") && !text.contains("hunter"));
    check_eq!(parse(&text), alloc::vec![alice.clone(), bob.clone()]);
    // Damaged or doubled lines give nobody anything.
    let damaged = alloc::format!("{text}eve:admin:nothex!!nothex!!:00\nalice:admin:0123456789abcdef:{}\nBad:admin:0123456789abcdef:{}\njunk\n", "0".repeat(64), "0".repeat(64));
    check_eq!(parse(&damaged).len(), 2);
    check!(valid_name("bart") && valid_name("a-b_9") && !valid_name("Bart") && !valid_name("9lives") && !valid_name("") && !valid_name("a:b"));
    check!(!valid_name("seventeen-letters"));
    // What needs an admin: changing the system; looking does not.
    for (cmd, args) in [("reboot", &[][..]), ("install", &[]), ("useradd", &["x"]), ("vnc", &["on"]), ("net", &["off"]), ("passwd", &["bob"]), ("timezone", &["CET"]), ("cpu", &["auto", "off"])] {
        check!(needs_admin(cmd, args));
    }
    for (cmd, args) in [("vnc", &[][..]), ("net", &[]), ("ls", &["/"]), ("passwd", &[]), ("cpu", &["test"]), ("display", &["speed"]), ("whoami", &[]), ("timezone", &[])] {
        check!(!needs_admin(cmd, args));
    }
    // Who may change what: your home, the shared rest; /system and others'
    // homes only for admins; reading is not checked here at all.
    use crate::users::owner_blocking;
    let some = |u: &User| Some(u.clone());
    check_eq!(owner_blocking(some(&bob).as_ref(), false, "/home/bob/notes.txt"), None);
    check_eq!(owner_blocking(some(&bob).as_ref(), false, "/home/bob"), None);
    check_eq!(owner_blocking(some(&bob).as_ref(), false, "/data/shared.txt"), None);
    check_eq!(owner_blocking(some(&bob).as_ref(), false, "/home/alice/x"), Some(String::from("alice")));
    check_eq!(owner_blocking(some(&bob).as_ref(), false, "/home/bobby/x"), Some(String::from("bobby")));
    check_eq!(owner_blocking(some(&bob).as_ref(), false, "/system/users"), Some(String::from("the admins")));
    check_eq!(owner_blocking(some(&bob).as_ref(), false, "/home"), Some(String::from("the admins")));
    check_eq!(owner_blocking(None, false, "/home/bob/x"), Some(String::from("bob")));
    check_eq!(owner_blocking(some(&alice).as_ref(), true, "/home/bob/x"), None);
    check_eq!(owner_blocking(some(&alice).as_ref(), true, "/system/users"), None);
    Ok(())
}

/// A WAV file as a test makes it: `fmt ` then other chunks then `data`.
fn wav_file(channels: u16, rate: u32, bits: u16, data: &[u8], extra: &[u8]) -> Vec<u8> {
    let mut f = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * channels as u32 * bits as u32 / 8).to_le_bytes());
    f.extend_from_slice(&(channels * bits / 8).to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    f.extend_from_slice(extra);
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(data);
    let size = (f.len() - 8) as u32;
    f[4..8].copy_from_slice(&size.to_le_bytes());
    f
}

fn sound_files() -> TestResult {
    use crate::sound::{frequency, parse_wav, sin_turns, tone};
    // 16-bit mono at 24 kHz, after a LIST chunk of odd size (padded).
    let samples: Vec<u8> = [0i16, 1000, -1000, 2000].iter().flat_map(|v| v.to_le_bytes()).collect();
    let file = wav_file(1, 24_000, 16, &samples, b"LIST\x03\0\0\0abc\0");
    let wav = parse_wav(&file).map_err(String::from)?;
    check_eq!((wav.rate, wav.channels, wav.bits, wav.frames()), (24_000, 1, 16, 4));
    // Twice the rate: every other frame lies halfway; mono goes to both sides.
    let out = wav.to_48k();
    check_eq!(out.len(), 8 * 4);
    let left = |i: usize| i16::from_le_bytes([out[i * 4], out[i * 4 + 1]]);
    let right = |i: usize| i16::from_le_bytes([out[i * 4 + 2], out[i * 4 + 3]]);
    check_eq!((left(0), left(1), left(2), left(3), left(4)), (0, 500, 1000, 0, -1000));
    check_eq!(right(2), 1000);
    // 8-bit stereo: 128 is silence, 255 nearly full.
    let wav = parse_wav(&wav_file(2, 48_000, 8, &[128, 255, 0, 128], &[])).map_err(String::from)?;
    let out = wav.to_48k();
    check_eq!(out.len(), 8);
    check_eq!(i16::from_le_bytes([out[0], out[1]]), 0);
    check_eq!(i16::from_le_bytes([out[2], out[3]]), 127 << 8);
    check_eq!(i16::from_le_bytes([out[4], out[5]]), -32768);
    // Refused: not a WAV, float samples, 24 bits; cut anywhere: never a panic.
    check!(parse_wav(b"hello").is_err());
    let mut float = wav_file(1, 8000, 16, &samples, &[]);
    float[20] = 3;
    check!(parse_wav(&float).is_err());
    check!(parse_wav(&wav_file(1, 8000, 24, &samples, &[])).is_err());
    let file = wav_file(2, 44_100, 16, &samples, &[]);
    for end in 0..file.len() {
        if let Ok(w) = parse_wav(&file[..end]) {
            check!(w.frames() <= 2);
            let _ = w.to_48k();
        }
    }
    // Sine: right at the quarter turns, close in between.
    for (t, want) in [(0.0f32, 0.0f32), (0.25, 1.0), (0.5, 0.0), (0.75, -1.0), (1.0 / 12.0, 0.5), (-0.25, -1.0), (2.125, 0.707_107)] {
        check!((sin_turns(t) - want).abs() < 0.002);
    }
    // A tone: the right length, silent at both ends, as many waves as asked.
    let t = tone(1000, 100, 50);
    check_eq!(t.len(), 4800 * 4);
    let sample = |i: usize| i16::from_le_bytes([t[i * 4], t[i * 4 + 1]]);
    check!(sample(0).abs() < 100 && sample(4799).abs() < 300);
    let peak = (0..4800).map(|i| sample(i).unsigned_abs()).max().unwrap_or(0);
    check!((16_000..=16_400).contains(&peak));
    let crossings = (1..4800).filter(|&i| (sample(i - 1) < 0) != (sample(i) < 0)).count();
    check!((198..=202).contains(&crossings)); // 100 waves, two crossings each
    // Notes.
    check_eq!(frequency("a4"), Some(440));
    check_eq!(frequency("A5"), Some(880));
    check_eq!(frequency("c5"), Some(523));
    check_eq!(frequency("a#4"), frequency("bb4"));
    check_eq!(frequency("a#4"), Some(466));
    check_eq!(frequency("c0"), Some(16));
    check_eq!(frequency("1000"), Some(1000));
    check_eq!(frequency("5"), None);
    check_eq!(frequency("h4"), None);
    check_eq!(frequency("a9"), None);
    check_eq!(frequency("b"), None);
    // Tunes: notes with and without lengths, rests; saved and read back the same.
    use crate::sound::{encode_wav, parse_tune, render_tune};
    let tune = parse_tune(&["c5", "r:100", "a4:500", "440:10"]).map_err(String::from)?;
    check_eq!(tune, alloc::vec![(Some(523), 250), (None, 100), (Some(440), 500), (Some(440), 10)]);
    let frames = render_tune(&tune, 50);
    check_eq!(frames.len(), 48 * (250 + 100 + 500 + 10) * 4);
    check!(frames[48 * 250 * 4..48 * 350 * 4].iter().all(|b| *b == 0)); // the rest is silent
    let back = parse_wav(&encode_wav(&frames)).map_err(String::from)?;
    check_eq!((back.rate, back.channels, back.bits), (48_000, 2, 16));
    check!(back.to_48k() == frames);
    check!(parse_tune(&[]).is_err());
    check!(parse_tune(&["c5:5"]).is_err());
    check!(parse_tune(&["x5"]).is_err());
    check!(parse_tune(&["c5:10000"; 7]).is_err()); // over 60 s
    Ok(())
}

fn sound_routes() -> TestResult {
    use crate::sound::{connections, output_use, route, verb, verb16, Kind, Node};
    let node = |nid: u8, kind: Kind, caps: u32, inputs: &[u8], config: u32| Node { nid, kind, caps, inputs: inputs.to_vec(), config, pin_caps: 1 << 4 };
    // Like a laptop codec: the speaker through a mixer, the headphones
    // through a selector that offers a digital converter first, and a
    // mixer that feeds itself through another.
    let nodes = [
        node(0x02, Kind::Output, 0, &[], 0),
        node(0x03, Kind::Output, 0, &[], 0),
        node(0x06, Kind::Output, 1 << 9, &[], 0), // digital
        node(0x0c, Kind::Mixer, 0, &[0x0b, 0x02], 0),
        node(0x0b, Kind::Mixer, 0, &[0x0c], 0),
        node(0x0d, Kind::Selector, 0, &[0x06, 0x0e], 0),
        node(0x0e, Kind::Mixer, 0, &[0x03], 0),
        node(0x14, Kind::Pin, 0, &[0x0c], 0x9017_0110),
        node(0x21, Kind::Pin, 0, &[0x0d], 0x0221_1020),
        node(0x1e, Kind::Pin, 0, &[0x06], 0x4000_0000),
        node(0x1f, Kind::Pin, 0, &[0x0b], 0x0000_0000),
    ];
    check_eq!(route(&nodes, 0x14), Some(alloc::vec![(0x14, 0), (0x0c, 1), (0x02, 0)]));
    check_eq!(route(&nodes, 0x21), Some(alloc::vec![(0x21, 0), (0x0d, 1), (0x0e, 0), (0x03, 0)]));
    check_eq!(route(&nodes, 0x1e), None); // only a digital converter
    check_eq!(route(&nodes, 0x1f), Some(alloc::vec![(0x1f, 0), (0x0b, 0), (0x0c, 1), (0x02, 0)]));
    check_eq!(output_use(0x9017_0110), Some("speaker"));
    check_eq!(output_use(0x0221_1020), Some("headphones"));
    check_eq!(output_use(0x0000_4010), Some("line out"));
    check_eq!(output_use(0x4000_0000), None); // no connection
    check_eq!(output_use(0x02a1_1030), None); // a microphone
    check_eq!(Kind::from_caps(0x0040_0000), Kind::Pin);
    // Connection lists: short form with a range (2, 5..7, 10, 9), long form.
    check_eq!(connections(5, |o| if o == 0 { 0x0a87_0502 } else { 0x0000_0009 }), alloc::vec![2, 5, 6, 7, 10, 9]);
    check_eq!(connections(0x80 | 3, |o| if o == 0 { 0x0014_0013 } else { 0x0000_0015 }), alloc::vec![0x13, 0x14, 0x15]);
    check_eq!(connections(0x7F, |_| 0x8181_8181).len() <= 32, true);
    // Verbs as the specification lays them out.
    check_eq!(verb(0, 0x14, 0x707, 0x40), 0x0147_0740);
    check_eq!(verb16(2, 0x02, 0x3, 0xB04A), 0x2023_B04A);
    Ok(())
}

fn file_icons() -> TestResult {
    use crate::icons::{step, thumbnail, Grid, CELL_H, CELL_W, THUMB};
    use crate::image::Image;
    // 640 x 400 pixels: 5 columns of 112 (80 left over, 40 on each side), 3 rows.
    let grid = Grid::new(640, 400);
    check_eq!(grid, Grid { cols: 5, rows: 3, left: 40 });
    check_eq!(Grid::new(50, 50), Grid { cols: 1, rows: 1, left: 0 });
    // Arrows in a grid of 5 columns, 12 entries (the last row has 2).
    check_eq!(step(0, Key::Left, 5, 3, 12), Some(0));
    check_eq!(step(0, Key::Right, 5, 3, 12), Some(1));
    check_eq!(step(11, Key::Right, 5, 3, 12), Some(11));
    check_eq!(step(7, Key::Up, 5, 3, 12), Some(2));
    check_eq!(step(2, Key::Up, 5, 3, 12), Some(2));
    check_eq!(step(2, Key::Down, 5, 3, 12), Some(7));
    check_eq!(step(8, Key::Down, 5, 3, 12), Some(11)); // the row below is short
    check_eq!(step(11, Key::Down, 5, 3, 12), Some(11));
    check_eq!(step(3, Key::End, 5, 3, 12), Some(11));
    check_eq!(step(3, Key::Enter, 5, 3, 12), None);
    check_eq!(step(0, Key::Down, 5, 3, 0), Some(0));
    // Scrolling by whole rows; clicks to entries.
    check_eq!(grid.top_for(4, 0), 0);
    check_eq!(grid.top_for(15, 0), 5);
    check_eq!(grid.top_for(2, 10), 0);
    check_eq!(grid.cell_at(40, 22, 0, 12), Some(0));
    check_eq!(grid.cell_at(40 + CELL_W + 5, 22 + CELL_H + 5, 0, 12), Some(6));
    check_eq!(grid.cell_at(40 + 2 * CELL_W, 22 + 2 * CELL_H, 0, 12), None); // past the last
    check_eq!(grid.cell_at(30, 50, 0, 12), None);
    check_eq!(grid.cell_at(40, 22, 5, 12), Some(5));
    // Thumbnails: shrunk to fit, the shape kept, colors averaged; small ones kept.
    let wide = Image { width: 256, height: 128, pixels: (0..256 * 128).map(|i| if (i % 256) < 128 { 0xFF0000 } else { 0x0000FF }).collect() };
    let t = thumbnail(&wide);
    check_eq!((t.width, t.height), (THUMB, THUMB / 2));
    check_eq!((t.pixels[0], t.pixels[THUMB - 1]), (0xFF0000, 0x0000FF));
    let grey = Image { width: 2, height: 2, pixels: alloc::vec![0x000000, 0xFFFFFF, 0xFFFFFF, 0x000000] };
    check_eq!(thumbnail(&grey).pixels, grey.pixels);
    let tall = Image { width: 10, height: 1000, pixels: alloc::vec![0x101010; 10_000] };
    check_eq!((thumbnail(&tall).width, thumbnail(&tall).height), (1, THUMB));
    let checker = Image { width: 128, height: 128, pixels: (0..128 * 128).map(|i| if (i % 128 + i / 128) % 2 == 0 { 0xFFFFFF } else { 0 }).collect() };
    check!(thumbnail(&checker).pixels.iter().all(|p| *p == 0x7F7F7F));
    // Picture sizes from the file header only.
    check_eq!(crate::image::dimensions(include_bytes!("../../beenet/testdata/pal.png")), Some((37, 23)));
    check_eq!(crate::image::dimensions(include_bytes!("../../beenet/testdata/anim.gif")), Some((40, 30)));
    check_eq!(crate::image::dimensions(b"hello"), None);
    Ok(())
}

fn tls_crypto() -> TestResult {
    use beenet::crypto::{hkdf_expand, hkdf_extract, x25519, x25519_public, Gcm};
    let hex = |s: &str| -> Vec<u8> { (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0)).collect() };
    let arr = |s: &str| -> [u8; 32] { hex(s).try_into().unwrap_or([0; 32]) };
    // RFC 7748 6.1: both sides reach the same secret.
    let alice = arr("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    let bob = arr("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
    let shared = hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
    check_eq!(x25519(&alice, &x25519_public(&bob)).to_vec(), shared);
    check_eq!(x25519(&bob, &x25519_public(&alice)).to_vec(), shared);
    // RFC 5869 test 1.
    let prk = hkdf_extract(&hex("000102030405060708090a0b0c"), &[0x0b; 22]);
    check_eq!(hkdf_expand(&prk, &hex("f0f1f2f3f4f5f6f7f8f9"), 42), hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"));
    // GCM test case 2, and a changed byte refused.
    let gcm = Gcm::new(&[0; 16]);
    let sealed = gcm.seal(&[0; 12], &[], &[0; 16]);
    check_eq!(sealed, hex("0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf"));
    let mut bad = sealed.clone();
    bad[3] ^= 1;
    check_eq!(gcm.open(&[0; 12], &[], &bad), None);
    Ok(())
}

fn certificates() -> TestResult {
    use beenet::x509::{split, verify, CertError, Trust, ROOTS};
    let chain: Vec<Vec<u8>> = split(include_bytes!("../../beenet/testdata/certs/www.google.com.der")).into_iter().map(|c| c.to_vec()).collect();
    let at: i64 = include_str!("../../beenet/testdata/certs/saved-at.txt").trim().parse().unwrap_or(0);
    let start = crate::time::micros();
    check_eq!(verify(&chain, "www.google.com", at, ROOTS), Ok(Trust::Verified { root: "GTS Root R1".into() }));
    let ms = (crate::time::micros() - start) / 1000;
    check!(ms < 2000); // the RSA checks of a chain are quick
    check!(matches!(verify(&chain, "www.g00gle.com", at, ROOTS), Err(CertError::WrongName(_))));
    check_eq!(verify(&chain, "www.google.com", at + 3 * 365 * 86400, ROOTS), Err(CertError::Expired));
    // An ECDSA chain (P-256 under a P-384 root).
    let chain: Vec<Vec<u8>> = split(include_bytes!("../../beenet/testdata/certs/en.wikipedia.org.der")).into_iter().map(|c| c.to_vec()).collect();
    let at: i64 = include_str!("../../beenet/testdata/certs/saved-at-ec.txt").trim().parse().unwrap_or(0);
    let start = crate::time::micros();
    check!(matches!(verify(&chain, "en.wikipedia.org", at, ROOTS), Ok(Trust::Verified { .. })));
    check!((crate::time::micros() - start) / 1000 < 2000);
    Ok(())
}

fn web_pictures() -> TestResult {
    use crate::web::Viewer;
    use beenet::html::{render, render_with, PageImage};
    let html = "<p>before<img src=a.png alt=Bee><p>after<img src='data:x'><img src=b.jpg>";
    // Not loaded yet: the alt text stands there, the picture is noted.
    let page = render(html, 40);
    check_eq!(page.text(0), "before[Bee]");
    check_eq!(page.images.iter().map(|i| (i.src.as_str(), i.rows)).collect::<Vec<_>>(), alloc::vec![("a.png", 0), ("b.jpg", 0)]);
    // Loaded (3 lines for a.png): empty lines kept for it, the text goes on below.
    let page = render_with(html, 40, &[(String::from("a.png"), 3)], false);
    check_eq!(page.images[0], PageImage { src: String::from("a.png"), line: 1, rows: 3 });
    check!((1..4).all(|l| page.text(l).is_empty()));
    check!((0..page.lines.len()).position(|l| page.text(l).starts_with("after")).is_some_and(|l| l >= 4));
    // A picture at the very end keeps its lines.
    let page = render_with("<p>x<img src=a.png>", 40, &[(String::from("a.png"), 2)], false);
    check_eq!(page.lines.len(), 3);
    // The viewer: the pictures not loaded yet, then where it draws them.
    let mut v = Viewer::new();
    v.show("/site/p.html", html, false, false);
    check_eq!(v.missing_images(40), alloc::vec![String::from("a.png"), String::from("b.jpg")]);
    v.add_image("a.png", crate::image::Image { width: 10, height: 40, pixels: alloc::vec![0; 400] }); // 40 px: 3 lines + 1
    v.image_failed("b.jpg");
    check!(v.missing_images(40).is_empty());
    let _ = v.render(40, 12);
    let spots = v.image_spots(12);
    check_eq!(spots.len(), 1);
    check_eq!(spots[0].1, 2 * 16); // line 1 of the page, below the status line
    Ok(())
}

