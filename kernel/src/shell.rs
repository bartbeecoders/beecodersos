//! BeeShell: the interactive terminal started at boot.

use crate::editor::{Editor, Line};
use crate::layouts::Layout;
use crate::storage::Storage;
use crate::vga::Color;
use crate::aibridge::{self, Outcome};
use crate::json::Obj;
use crate::ring::Global;
use crate::{allocator, bootslots, httpd, console, crash, eventlog, keyboard, net, pci, power, selftest, thread, time};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use beefs::{normalize, FileKind, FsError};

const LAYOUT_FILE: &str = "/system/keyboard";
/// Contains "off" when the AI features are switched off (`ai off`).
const AI_SWITCH: &str = "/system/ai";
/// Network settings: "dhcp", "none" or "static IP/PREFIX [GATEWAY]", and
/// optionally a line "dns ADDRESS[:PORT]". No file: DHCP.
const NET_FILE: &str = "/system/net";
const INBOX: &str = "/system/inbox/signals.jsonl";
const OLD_INBOX: &str = "/system/inbox/signals.old.jsonl";

/// The error message the current command printed with `error()`, per thread
/// (every shell runs in a thread of its own).
static LAST_ERROR: Global<Vec<(thread::ThreadId, String)>> = Global::new(Vec::new());

fn set_last_error(msg: Option<&str>) {
    let me = thread::current();
    let errors = LAST_ERROR.get();
    errors.retain(|(id, _)| *id != me);
    if let Some(msg) = msg {
        errors.push((me, String::from(msg)));
    }
}

fn last_error() -> Option<String> {
    let me = thread::current();
    LAST_ERROR.get().iter().find(|(id, _)| *id == me).map(|(_, msg)| msg.clone())
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Ok,
    Error,
    NotFound,
}

pub struct Shell {
    storage: Storage,
    cwd: String,
    editor: Editor,
    status: Status,
    /// The last command line that failed, with its error (for `explain`).
    last_failure: Option<(String, String)>,
    /// A crash saved by the previous boot (for `explain`).
    last_crash: Option<crash::Report>,
    /// Record commands in the event log (off for the self-tests' shell).
    logging: bool,
}

impl Shell {
    pub fn new(storage: Storage) -> Self {
        Shell {
            storage,
            cwd: String::from("/"),
            editor: Editor::new(),
            status: Status::Ok,
            last_failure: None,
            last_crash: None,
            logging: true,
        }
    }

    /// A shell whose commands are not recorded in the event log.
    pub fn unlogged(storage: Storage) -> Self {
        Shell { logging: false, ..Shell::new(storage) }
    }

    /// Run in a console session (see session.rs): read command lines from
    /// its input until it is closed. No line editing: the other end does that.
    pub fn run_session(&mut self) {
        loop {
            self.prompt();
            let mut line = String::new();
            loop {
                match crate::input::read_key() {
                    crate::input::Key::Enter => break,
                    crate::input::Key::Char(c) => line.push(c),
                    crate::input::Key::Ctrl('d') => {
                        set_last_error(None);
                        return;
                    }
                    _ => {}
                }
            }
            println!();
            self.run_line(&line);
        }
    }

    /// At boot: report a crash from the previous boot and log this boot.
    pub fn boot_report(&mut self) {
        let report = crash::take_pending(&mut self.storage.fs());
        if let Some(r) = &report {
            console::set_color(Color::LightRed, Color::Black);
            println!("[crash] the previous boot crashed (b{}, {}): {}", r.build, r.kind, r.msg);
            console::set_color(Color::LightGray, Color::Black);
            println!("        type 'explain' to ask the AI what happened");
            self.forward_signal("crash", &r.msg);
        }
        self.last_crash = report;
        let ev = eventlog::event("boot")
            .str("version", crate::OS_VERSION)
            .str("build", crate::OS_BUILD)
            .num("boot", self.storage.boot as i64)
            .str("disk", if self.storage.persistent { "ata" } else { "ram" })
            .num("heap_kib", (allocator::heap_size() / 1024) as i64)
            .num("boot_ms", time::uptime_ms() as i64)
            .bool("ai_bridge", aibridge::present())
            .finish();
        eventlog::write(&mut self.storage.fs(), ev);
    }

    /// Apply the keyboard layout saved on disk, if any.
    pub fn load_layout(&mut self) {
        if let Ok(data) = self.storage.fs().read_file(LAYOUT_FILE) {
            if let Some(l) = core::str::from_utf8(&data).ok().and_then(|s| Layout::from_id(s.trim())) {
                keyboard::set_layout(l);
            }
        }
    }

    pub fn run(&mut self) -> ! {
        loop {
            self.show_notices();
            self.prompt();
            match self.editor.read_line() {
                Line::Text(line) => self.run_line(&line),
                Line::Cancelled => {}
                Line::ClearScreen => console::clear(),
                Line::PowerButton => {
                    println!("Power button pressed.");
                    power::shutdown()
                }
            }
        }
    }

    /// Things that happened in the background (the network got an address, ...).
    fn show_notices(&mut self) {
        while let Some((kind, text)) = net::take_notice() {
            println!("[net]  {text}");
            let ev = eventlog::event("net").str("state", kind).finish();
            eventlog::write(&mut self.storage.fs(), ev);
        }
    }

    fn prompt(&self) {
        console::set_color(Color::LightGreen, Color::Black);
        print!("bee");
        console::set_color(Color::LightGray, Color::Black);
        print!(":");
        console::set_color(Color::LightBlue, Color::Black);
        print!("{}", self.cwd);
        console::set_color(Color::LightGray, Color::Black);
        print!("$ ");
    }

    /// Run one command line and log it (by command name only).
    fn run_line(&mut self, line: &str) {
        let Some(name) = line.split_whitespace().next().map(String::from) else { return };
        crash::set_command(&name);
        set_last_error(None);
        self.status = Status::Ok;
        let start = time::uptime_ms();
        self.execute(line);
        if self.status == Status::Ok && last_error().is_some() {
            self.status = Status::Error;
        }
        if self.status != Status::Ok {
            let msg = last_error().unwrap_or_default();
            self.last_failure = Some((String::from(line), msg));
        }
        let status = match self.status {
            Status::Ok => "ok",
            Status::Error => "error",
            Status::NotFound => "notfound",
        };
        if !self.logging {
            return;
        }
        let ev = eventlog::event("cmd")
            .str("cmd", &name)
            .str("status", status)
            .num("ms", (time::uptime_ms() - start) as i64)
            .finish();
        eventlog::write(&mut self.storage.fs(), ev);
    }

    fn execute(&mut self, line: &str) {
        let mut args = match tokenize(line) {
            Ok(a) => a,
            Err(e) => return error(e),
        };
        if args.is_empty() {
            return;
        }
        // Output redirection: `cmd ... > file` or `cmd ... >> file`.
        let mut redirect = None;
        if let Some(i) = args.iter().position(|a| a == ">" || a == ">>") {
            if i + 2 != args.len() {
                return error("usage: COMMAND ... > FILE");
            }
            redirect = Some((args[i] == ">>", args[i + 1].clone()));
            args.truncate(i);
        }
        let cmd = args.remove(0);
        let a: Vec<&str> = args.iter().map(String::as_str).collect();

        if let Some((append, file)) = redirect {
            if cmd == "wget" && !append && a.len() == 1 {
                return self.wget(a[0], Some(&file));
            }
            if cmd != "echo" {
                return error("redirection is only supported for echo and wget");
            }
            let mut text = a.join(" ");
            text.push('\n');
            let path = self.abs(&file);
            let mut fs = self.storage.fs();
            let r = if append { fs.append(&path, text.as_bytes()) } else { fs.write_file(&path, text.as_bytes()) };
            return report(r);
        }

        match (cmd.as_str(), a.as_slice()) {
            ("help", _) => help(),
            ("clear" | "cls", _) => console::clear(),
            ("echo", rest) => println!("{}", rest.join(" ")),
            ("pwd", []) => println!("{}", self.cwd),
            ("cd", []) => self.cwd = String::from("/"),
            ("cd", [dir]) => self.cd(dir),
            ("ls" | "dir", []) => self.ls(&self.cwd.clone()),
            ("ls" | "dir", [dir]) => self.ls(&self.abs(dir)),
            ("tree", []) => self.tree(&self.cwd.clone()),
            ("tree", [dir]) => self.tree(&self.abs(dir)),
            ("cat" | "type", [file]) => self.cat(file),
            ("touch", [file]) => self.touch(file),
            ("mkdir", [dir]) => {
                let dir = self.abs(dir);
                let r = self.storage.fs().mkdir_all(&dir);
                report(r)
            }
            ("rm" | "del" | "rmdir", [path]) => {
                let path = self.abs(path);
                let r = self.storage.fs().remove(&path);
                report(r)
            }
            ("mv" | "move", [from, to]) => {
                let (from, to) = (self.abs(from), self.abs_target(from, to));
                let r = self.storage.fs().rename(&from, &to);
                report(r)
            }
            ("cp" | "copy", [from, to]) => self.cp(from, to),
            ("df", []) => self.df(),
            ("layout" | "keyboard", []) => show_layouts(),
            ("layout" | "keyboard", [id]) => self.set_layout(id),
            ("history", []) => {
                for (i, h) in self.editor.history().iter().enumerate() {
                    println!("{:>4}  {}", i + 1, h);
                }
            }
            ("mem", []) => println!(
                "heap: {} KiB used of {} KiB",
                allocator::used() / 1024,
                allocator::heap_size() / 1024
            ),
            ("about" | "version", []) => {
                println!("BeeCodersOs {} b{} - x86_64 kernel in Rust", crate::OS_VERSION, crate::OS_BUILD);
                println!("filesystem: BeeFS on {}", if self.storage.persistent { "ATA disk" } else { "RAM disk" });
            }
            ("selftest", []) => {
                selftest::run();
            }
            ("calc", []) => error("usage: calc EXPR (e.g. calc (2 + 3) * 4)"),
            ("calc", words) => match calc(&words.join(" ")) {
                Ok(v) => println!("{v}"),
                Err(e) => error(&format!("calc: {e}")),
            },
            ("date", []) => match time::now_unix() {
                Some(t) => println!("{}", time::iso8601(t)),
                None => error("no real-time clock"),
            },
            ("uptime", []) => println!("up {}", time::format_duration(time::uptime_ms())),
            ("log", []) => self.show_log(10),
            ("log", [n]) => match n.parse() {
                Ok(n) => self.show_log(n),
                Err(_) => error("usage: log [LINES]"),
            },
            ("wish" | "feedback", []) => error(&format!("usage: {cmd} TEXT")),
            ("wish" | "feedback", words) => self.signal(&cmd, &words.join(" ")),
            ("ask", []) => error("usage: ask QUESTION"),
            ("ask", words) => self.ask(&words.join(" ")),
            ("explain", []) => self.explain(),
            ("ai", []) => self.ai_status(),
            ("ai", [s @ ("on" | "off")]) => self.ai_switch(s),
            ("threads" | "ps", []) => {
                println!("{:>3}  {:<10} {:<8} {:>9}  STACK USED", "ID", "NAME", "STATE", "RUNS");
                for t in thread::list() {
                    let stack = match t.stack {
                        Some((used, size)) => format!("{} of {} KiB", used.div_ceil(1024), size / 1024),
                        None => String::from("(boot stack)"),
                    };
                    println!("{:>3}  {:<10} {:<8} {:>9}  {stack}", t.id, t.name, t.state, t.runs);
                }
            }
            ("lspci", []) => {
                for d in pci::scan() {
                    let irq = if d.irq != 0 && d.irq != 0xFF { format!(" (IRQ {})", d.irq) } else { String::new() };
                    println!("{:02x}:{:02x}.{} {:04x}:{:04x} {}{irq}", d.bus, d.dev, d.func, d.vendor, d.device, d.class_name());
                }
            }
            ("net" | "ifconfig", []) => match net::describe() {
                Some(text) => println!("{text}"),
                None => println!("no network card (supported: Intel e1000; see lspci)"),
            },
            ("net", ["static", config @ ..]) if !config.is_empty() => self.net_static(&config.join(" ")),
            ("net", ["off"]) => self.net_off(),
            ("net", ["dhcp"]) => self.net_dhcp(),
            ("net", ["dns", value]) => self.net_dns(value),
            ("nslookup" | "host", [name]) => self.nslookup(name),
            ("nc", [host, port]) => self.nc(host, port, None),
            ("nc", [host, port, text @ ..]) => self.nc(host, port, Some(&text.join(" "))),
            ("httpd", []) => match (httpd::port(), net::address()) {
                (Some(port), Some(ip)) => {
                    let shown = if port == httpd::DEFAULT_PORT { format!("{ip}") } else { format!("{ip}:{port}") };
                    println!("status page: on, http://{shown}/ ({} requests answered)", httpd::answered());
                }
                (Some(port), None) => println!("status page: on, port {port}, but the network has no address yet (see net)"),
                (None, _) => println!("status page: off (httpd on [PORT] starts it)"),
            },
            ("httpd", ["on"]) => self.httpd(Some(httpd::DEFAULT_PORT)),
            ("httpd", ["on", port]) => match port.parse::<u16>() {
                Ok(p) if p != 0 => self.httpd(Some(p)),
                _ => error("usage: httpd on [PORT]   (port 1-65535)"),
            },
            ("httpd", ["off"]) => self.httpd(None),
            ("httpd", _) => error("usage: httpd [on [PORT] | off]"),
            ("wget", [url]) => self.wget(url, None),
            ("wget", [url, file]) => self.wget(url, Some(file)),
            ("wget", _) => error("usage: wget URL [FILE]   (or: wget URL > FILE)"),
            ("ntp", []) => self.ntp("pool.ntp.org"),
            ("ntp", [server]) => self.ntp(server),
            ("tftp", ["get", host, remote]) => self.tftp_get(host, remote, None),
            ("tftp", ["get", host, remote, local]) => self.tftp_get(host, remote, Some(local)),
            ("tftp", ["put", host, local]) => self.tftp_put(host, local, None),
            ("tftp", ["put", host, local, remote]) => self.tftp_put(host, local, Some(remote)),
            ("tftp", _) => error("usage: tftp get HOST[:PORT] FILE [LOCAL-NAME]   or: tftp put HOST[:PORT] FILE [REMOTE-NAME]"),
            ("arp", []) => {
                let table = net::arp_table();
                if table.is_empty() {
                    println!("no neighbours known yet (addresses are learned when packets are exchanged)");
                }
                for (ip, mac, age) in table {
                    println!("{:<16} {}  confirmed {age} s ago", format!("{ip}"), net::format_mac(&mac));
                }
            }
            ("ping", [host]) => self.ping(host, 4),
            ("ping", ["-c", n, host]) => match n.parse() {
                Ok(n @ 1..=1000) => self.ping(host, n),
                _ => error("usage: ping [-c COUNT] HOST   (count 1-1000)"),
            },
            ("net", ["test"]) => match net::send_test_frame() {
                Ok(len) => println!("sent a {len}-byte broadcast test frame (EtherType 0x{:04x})", net::ETHERTYPE_TEST),
                Err(e) => error(e.message()),
            },
            ("slots", []) => match bootslots::describe() {
                Some(text) => println!("{text}"),
                None => println!("not booted from a boot disk with A/B slots (see tools/bootdisk.py)"),
            },
            ("crash", []) => panic!("'crash' command: deliberate kernel panic (tests crash reports)"),
            ("reboot", []) => {
                self.log_power("reboot");
                power::reboot()
            }
            ("shutdown" | "halt" | "poweroff", []) => {
                self.log_power("shutdown");
                power::shutdown()
            }
            ("exit" | "logout" | "quit", _) => {
                println!("BeeShell is the system console; use 'shutdown' or 'reboot'.")
            }
            (c, _) if is_command(c) => error(&format!("{c}: wrong arguments (see 'help')")),
            (c, _) => self.not_found(c),
        }
    }

    /// Log an event and send it to beehive as a signal (A/B slot results).
    pub fn note(&mut self, event: &str, signal: &str, text: &str) {
        let ev = eventlog::event(event).str("text", text).finish();
        eventlog::write(&mut self.storage.fs(), ev);
        self.forward_signal(signal, text);
    }

    fn not_found(&mut self, c: &str) {
        self.status = Status::NotFound;
        error(&format!("{c}: command not found (type 'help')"));
        if let Some(s) = suggest(c) {
            println!("did you mean '{s}'?");
        }
        self.forward_signal("missed", c);
    }

    fn log_power(&mut self, what: &str) {
        let ev = eventlog::event(what).finish();
        eventlog::write(&mut self.storage.fs(), ev);
    }

    // ----- phase 5: observing the OS ------------------------------------------

    fn show_log(&mut self, n: usize) {
        let data = self.storage.fs().read_file(eventlog::LOG);
        match data {
            Ok(d) => {
                let text = String::from_utf8_lossy(&d);
                let lines: Vec<&str> = text.lines().collect();
                for l in &lines[lines.len().saturating_sub(n)..] {
                    println!("{l}");
                }
            }
            Err(e) => error(&format!("{}: {e}", eventlog::LOG)),
        }
    }

    /// `wish` / `feedback`: save to the inbox and send to beehive if it runs.
    fn signal(&mut self, kind: &str, text: &str) {
        let line = Obj::new()
            .str("t", &time::timestamp())
            .str("kind", kind)
            .str("text", text)
            .str("build", crate::OS_BUILD)
            .finish();
        let saved = eventlog::append(&mut self.storage.fs(), INBOX, OLD_INBOX, &line);
        let ev = eventlog::event(kind).str("text", text).finish();
        eventlog::write(&mut self.storage.fs(), ev);
        let sent = self.forward_signal(kind, text);
        match (saved, sent) {
            (Err(e), _) => error(&format!("could not save: {e}")),
            (Ok(()), true) => println!("Thanks! Saved to {INBOX} and sent to the AI host."),
            (Ok(()), false) => println!("Thanks! Saved to {INBOX}."),
        }
    }

    /// Send a signal (wish, feedback, missed command, crash) to beehive.
    /// Returns true when beehive is connected.
    fn forward_signal(&mut self, kind: &str, text: &str) -> bool {
        if !aibridge::present() || !self.ai_enabled() {
            return false;
        }
        let msg = Obj::new()
            .str("type", "signal")
            .str("kind", kind)
            .str("text", text)
            .str("build", crate::OS_BUILD)
            .str("t", &time::timestamp())
            .finish();
        if kind == "health" {
            aibridge::set_status(msg); // must reach the deploy watchdog
        } else {
            aibridge::send(&msg);
        }
        aibridge::peer().is_some()
    }

    // ----- network -------------------------------------------------------------------

    /// Apply the network settings saved on disk. Without saved settings the
    /// address is requested automatically (DHCP), in the background.
    pub fn load_network(&mut self) {
        if !net::present() {
            return;
        }
        let saved = self.storage.fs().read_file(NET_FILE);
        let settings = match saved.as_deref().map(String::from_utf8_lossy) {
            Ok(text) => match net::parse_settings(&text) {
                Ok(s) => s,
                Err(e) => {
                    println!("[net]  ignoring {NET_FILE}: {e}");
                    return;
                }
            },
            // `nodhcp` on the kernel command line: tests that need a silent card.
            Err(_) if crate::bootargs::has("nodhcp") => return,
            Err(_) => net::Settings { mode: net::Mode::Dhcp, dns: None },
        };
        let _ = net::apply(settings);
        match settings.mode {
            net::Mode::Static(c) => {
                let gateway = c.gateway.map(|g| format!(", gateway {g}")).unwrap_or_default();
                println!("[net]  address {}/{} (static){gateway}", c.ip, c.prefix);
            }
            net::Mode::Dhcp => println!("[net]  asking the network for an address (DHCP)"),
            net::Mode::Off => {}
        }
    }

    /// Apply new settings and save them. Returns a note about saving.
    fn set_network(&mut self, settings: net::Settings) -> Result<&'static str, &'static str> {
        net::apply(settings).map_err(|e| e.message())?;
        let text = net::format_settings(&settings);
        let mut fs = self.storage.fs();
        let saved = fs.mkdir_all("/system").and_then(|_| fs.write_file(NET_FILE, format!("{text}\n").as_bytes()));
        drop(fs);
        Ok(match (saved, self.storage.persistent) {
            (Ok(()), true) => " (saved)",
            (Ok(()), false) => " (RAM disk: not kept after reboot)",
            (Err(_), _) => " (could not save)",
        })
    }

    fn current_settings(&self) -> Option<net::Settings> {
        let s = net::settings();
        if s.is_none() {
            error("no network card");
        }
        s
    }

    fn net_static(&mut self, text: &str) {
        let Some(old) = self.current_settings() else { return };
        let config = match net::parse_config(text) {
            Ok(c) => c,
            Err(e) => return error(&format!("{e}\nusage: net static IP/PREFIX [GATEWAY]   e.g. net static 192.168.1.50/24 192.168.1.1")),
        };
        match self.set_network(net::Settings { mode: net::Mode::Static(config), dns: old.dns }) {
            Ok(note) => println!("address {}/{}, gateway {}{note}", config.ip, config.prefix, config.gateway.map(|g| format!("{g}")).unwrap_or_else(|| "none".into())),
            Err(e) => error(e),
        }
    }

    fn net_off(&mut self) {
        let Some(old) = self.current_settings() else { return };
        match self.set_network(net::Settings { mode: net::Mode::Off, dns: old.dns }) {
            Ok(note) => println!("IP address removed{note}"),
            Err(e) => error(e),
        }
    }

    /// `net dhcp`: automatic setup; waits for the result.
    fn net_dhcp(&mut self) {
        let Some(old) = self.current_settings() else { return };
        let note = match self.set_network(net::Settings { mode: net::Mode::Dhcp, dns: old.dns }) {
            Ok(note) => note,
            Err(e) => return error(e),
        };
        println!("asking the network for an address (DHCP){note}...");
        match net::dhcp_wait(15_000) {
            net::DhcpWait::Bound(text) => {
                println!("{text}");
                let ev = eventlog::event("net").str("state", "dhcp-bound").finish();
                eventlog::write(&mut self.storage.fs(), ev);
            }
            net::DhcpWait::Failed => error("no DHCP server answered; still trying in the background (see: net)"),
            net::DhcpWait::Cancelled => println!("(still trying in the background)"),
        }
    }

    /// `net dns ADDRESS[:PORT]` or `net dns auto`.
    fn net_dns(&mut self, value: &str) {
        let Some(old) = self.current_settings() else { return };
        let dns = match value {
            "auto" => None,
            v => match net::parse_dns_server(v) {
                Some(server) => Some(server),
                None => return error("usage: net dns ADDRESS[:PORT]   or: net dns auto (use the one DHCP gives)"),
            },
        };
        match self.set_network(net::Settings { mode: old.mode, dns }) {
            Ok(note) if dns.is_some() => println!("DNS server {value}{note}"),
            Ok(note) => println!("DNS server: the one DHCP gives{note}"),
            Err(e) => error(e),
        }
    }

    /// `nc HOST PORT`: a raw TCP connection. Lines you type are sent; what
    /// arrives is shown. With TEXT: send that line, show the answer, finish.
    fn nc(&mut self, host: &str, port: &str, text: Option<&str>) {
        let port = match port.parse::<u16>() {
            Ok(p) if p != 0 => p,
            _ => return error("usage: nc HOST PORT [TEXT]   (port 1-65535)"),
        };
        let ip = match net::resolve(host) {
            Ok(ip) => ip,
            Err(e) => return error(&e),
        };
        let stream = match net::tcp_connect(ip, port) {
            Ok(s) => s,
            Err(e) => return error(&format!("{host} port {port}: {e}")),
        };
        let interactive = text.is_none();
        if interactive {
            println!("connected to {host} port {port} (type lines; Ctrl+D ends your side, Ctrl+C quits)");
        } else {
            println!("connected to {host} port {port}");
        }
        if let Some(t) = text {
            if let Err(e) = stream.send(format!("{t}\r\n").as_bytes()) {
                return error(e);
            }
        }
        let mut line = String::new();
        let mut buf = [0u8; 512];
        let mut our_side_open = interactive;
        let mut at_line_start = true;
        let mut last_data = time::uptime_ms();
        // Start messages of our own on a fresh line.
        let fresh_line = |at_start: &mut bool| {
            if !*at_start {
                println!();
                *at_start = true;
            }
        };
        loop {
            match stream.receive(&mut buf) {
                net::Received::Data(n) => {
                    for c in String::from_utf8_lossy(&buf[..n]).chars().filter(|c| *c != '\r') {
                        let shown = if c == '\n' || c == '\t' || !c.is_control() { c } else { '.' };
                        print!("{shown}");
                        at_line_start = shown == '\n';
                    }
                    last_data = time::uptime_ms();
                    continue;
                }
                net::Received::End => {
                    fresh_line(&mut at_line_start);
                    println!("(the other side closed the connection)");
                    return;
                }
                net::Received::Failed(e) => {
                    fresh_line(&mut at_line_start);
                    return error(e);
                }
                net::Received::Nothing => {}
            }
            if !interactive && time::uptime_ms() - last_data > 5_000 {
                fresh_line(&mut at_line_start);
                println!("(nothing more for 5 seconds: closed)");
                return;
            }
            while let Some(key) = crate::input::poll_key() {
                match key {
                    crate::input::Key::Ctrl('c') | crate::input::Key::Escape => {
                        fresh_line(&mut at_line_start);
                        println!("(closed)");
                        return;
                    }
                    crate::input::Key::Ctrl('d') if our_side_open => {
                        stream.finish();
                        our_side_open = false;
                        fresh_line(&mut at_line_start);
                        println!("(your side is closed; waiting for the other side, Ctrl+C quits)");
                    }
                    crate::input::Key::Enter if our_side_open => {
                        println!();
                        at_line_start = true;
                        let mut data = core::mem::take(&mut line);
                        data.push_str("\r\n");
                        if let Err(e) = stream.send(data.as_bytes()) {
                            return error(e);
                        }
                    }
                    crate::input::Key::Char(c) if our_side_open && line.len() < 500 => {
                        line.push(c);
                        print!("{c}");
                        at_line_start = false;
                    }
                    _ => {}
                }
            }
            net::idle(10); // one timer tick: data and keys are handled promptly
        }
    }

    /// `httpd on [PORT]` / `httpd off`: the status page for the local network.
    fn httpd(&mut self, port: Option<u16>) {
        if let Err(e) = httpd::set(port, true) {
            return error(&format!("httpd: {e}"));
        }
        let saved = if self.storage.persistent { " (saved)" } else { " (not saved: RAM disk)" };
        match port {
            Some(port) => {
                println!("status page on, port {port}{saved}");
                println!(
                    "anyone who can reach this machine can read it, and the files in {}, without a password",
                    httpd::PUBLIC_DIR
                );
            }
            None => println!("status page off{saved}"),
        }
    }

    /// `wget URL [FILE]`: fetch over HTTP, to the screen or into a file.
    fn wget(&mut self, url: &str, file: Option<&str>) {
        let Some(file) = file else {
            let mut at_line_start = true;
            let result = net::http_get(url, MAX_SHOWN, &mut |data| {
                for c in String::from_utf8_lossy(data).chars().filter(|c| *c != '\r') {
                    let shown = if c == '\n' || c == '\t' || !c.is_control() { c } else { '.' };
                    print!("{shown}");
                    at_line_start = shown == '\n';
                }
                Ok(())
            });
            if !at_line_start {
                println!();
            }
            return match result {
                Ok(_) => {}
                Err(e) if e == "too large" => {
                    error(&format!("{url}: too large to show ({} KiB at most): save it with wget URL FILE", MAX_SHOWN / 1024))
                }
                Err(e) => error(&format!("{url}: {e}")),
            };
        };
        let path = self.abs_target(url.split(['?', '#']).next().unwrap_or(""), file);
        let mut download = Download::start(&mut self.storage, &path);
        let result = net::http_get(url, beefs::MAX_FILE_SIZE, &mut |data| download.write(data));
        match result.and_then(|r| download.finish().map(|()| r)) {
            Ok(r) => {
                if let Some(to) = r.redirected_to {
                    println!("(redirected to {to})");
                }
                println!("saved {} bytes to {path}", r.bytes);
            }
            Err(e) if e == "too large" => {
                error(&format!("{url}: too large (files can be {} MiB at most)", beefs::MAX_FILE_SIZE >> 20))
            }
            Err(e) => error(&format!("{url}: {e}")),
        }
    }

    /// `ntp [SERVER[:PORT]]`: set the clock from a time server.
    fn ntp(&mut self, server: &str) {
        let Some((host, port)) = net::split_host_port(server, beenet::sntp::PORT) else {
            return error("usage: ntp [SERVER[:PORT]]");
        };
        let ip = match net::resolve(host) {
            Ok(ip) => ip,
            Err(e) => return error(&e),
        };
        let before = time::now_unix_ms();
        let (secs, millis, stratum) = match net::sntp_query(ip, port) {
            Ok(t) => t,
            Err(e) => return error(&format!("{host}: {e}")),
        };
        time::set_unix(secs, millis);
        let offset = before.map(|old| (secs * 1000 + millis as u64) as i64 - old as i64);
        let was = match offset {
            None => String::from("there was no clock before"),
            Some(ms) if ms.abs() < 1000 => String::from("it was right to within a second"),
            Some(ms) => format!(
                "it was {} {}",
                time::format_duration(ms.unsigned_abs()),
                if ms > 0 { "behind" } else { "ahead" }
            ),
        };
        println!("clock set to {} ({was}; server stratum {stratum})", time::iso8601(secs));
        let ev = eventlog::event("ntp").num("offset_s", offset.unwrap_or(0) / 1000).finish();
        eventlog::write(&mut self.storage.fs(), ev);
    }

    /// `tftp get HOST[:PORT] FILE [LOCAL]`: fetch a file from a TFTP server.
    fn tftp_get(&mut self, host: &str, remote: &str, local: Option<&str>) {
        let Some((ip, port)) = self.tftp_server(host) else { return };
        let name = remote.rsplit('/').next().unwrap_or(remote);
        let path = match local {
            Some(l) => self.abs_target(name, l),
            None => self.abs(name),
        };
        let started = time::uptime_ms();
        let mut download = Download::start(&mut self.storage, &path);
        let result = net::tftp_get(ip, port, remote, beefs::MAX_FILE_SIZE, &mut |data| download.write(data));
        match result.and_then(|n| download.finish().map(|()| n)) {
            Ok(n) => println!("received {n} bytes into {path}{}", rate(n, started)),
            Err(e) => error(&format!("{remote}: {e}")),
        }
    }

    /// `tftp put HOST[:PORT] FILE [REMOTE]`: store a file on a TFTP server.
    fn tftp_put(&mut self, host: &str, local: &str, remote: Option<&str>) {
        let path = self.abs(local);
        let data = match self.storage.fs().read_file(&path) {
            Ok(d) => d,
            Err(e) => return error(&format!("{local}: {e}")),
        };
        let Some((ip, port)) = self.tftp_server(host) else { return };
        let name = remote.unwrap_or_else(|| path.rsplit('/').next().unwrap_or(local));
        let started = time::uptime_ms();
        match net::tftp_put(ip, port, name, data) {
            Ok(n) => println!("sent {n} bytes as {name}{}", rate(n, started)),
            Err(e) => error(&format!("{name}: {e}")),
        }
    }

    fn tftp_server(&mut self, host: &str) -> Option<(beenet::Ipv4, u16)> {
        let Some((name, port)) = net::split_host_port(host, beenet::tftp::PORT) else {
            error("the server must look like HOST or HOST:PORT");
            return None;
        };
        match net::resolve(name) {
            Ok(ip) => Some((ip, port)),
            Err(e) => {
                error(&e);
                None
            }
        }
    }

    fn nslookup(&mut self, name: &str) {
        match net::resolve(name) {
            Ok(ip) => println!("{name} has address {ip}"),
            Err(e) => error(&e),
        }
    }

    fn ping(&mut self, host: &str, count: u32) {
        let dst = match net::resolve(host) {
            Ok(ip) => ip,
            Err(e) => return error(&e),
        };
        let id = crate::rand::next_u32() as u16;
        let (mut received, mut min, mut max, mut total) = (0u32, u64::MAX, 0u64, 0u64);
        let mut sent = 0;
        if beenet::Ipv4::parse(host).is_some() {
            println!("PING {dst}: 32 data bytes");
        } else {
            println!("PING {host} ({dst}): 32 data bytes");
        }
        for seq in 1..=count {
            let started = time::uptime_ms();
            sent += 1;
            match net::ping(dst, id, seq as u16, 1000) {
                net::Ping::Reply { ttl, micros, len } => {
                    received += 1;
                    (min, max, total) = (min.min(micros), max.max(micros), total + micros);
                    println!("{len} bytes from {dst}: seq={seq} ttl={ttl} time={}", millis(micros));
                }
                net::Ping::Timeout => println!("no answer from {dst}: seq={seq} (timeout)"),
                net::Ping::Unreachable(beenet::Reason::NoArpReply) => {
                    error(&format!("{dst} is unreachable: no machine with that address answered on the local network"));
                    break;
                }
                net::Ping::Unreachable(beenet::Reason::Icmp(code)) => {
                    error(&format!("{dst} is unreachable (a router said so, code {code})"));
                    break;
                }
                net::Ping::Cancelled => {
                    sent -= 1;
                    break;
                }
                net::Ping::Failed(why) => return error(why),
            }
            // One ping per second.
            if seq < count && !net::sleep(1000u64.saturating_sub(time::uptime_ms() - started)) {
                break;
            }
        }
        let loss = if sent > 0 { 100 - received * 100 / sent } else { 0 };
        println!("{sent} sent, {received} received, {loss}% loss");
        if received > 0 {
            println!("round trip min/avg/max = {}/{}/{}", millis(min), millis(total / received as u64), millis(max));
        }
    }

    // ----- phase 6: AI bridge ---------------------------------------------------

    fn ai_enabled(&mut self) -> bool {
        let data = self.storage.fs().read_file(AI_SWITCH);
        !matches!(data, Ok(d) if d.trim_ascii() == b"off")
    }

    /// Check the AI can be used; prints why not.
    fn ai_ready(&mut self) -> bool {
        if !aibridge::present() {
            error("no AI bridge: this machine has no second serial port (COM2)");
            false
        } else if !self.ai_enabled() {
            error("AI is switched off ('ai on' to enable)");
            false
        } else {
            true
        }
    }

    fn ask(&mut self, question: &str) {
        if self.ai_ready() {
            let fields = Obj::new().str("text", question).str("cwd", &self.cwd);
            self.ai_request("ask", fields);
        }
    }

    fn explain(&mut self) {
        if !self.ai_ready() {
            return;
        }
        let fields = if let Some((line, err)) = self.last_failure.clone() {
            println!("Explaining: {line}");
            Obj::new().str("cmd", &line).str("error", &err)
        } else if let Some(r) = &self.last_crash {
            println!("Explaining the crash of the previous boot.");
            Obj::new().str("cmd", &r.cmd).str("crash", &r.msg).str("kind", &r.kind).str("build", &r.build)
        } else {
            return println!("Nothing to explain: no failed command or crash yet.");
        };
        self.ai_request("explain", fields.str("cwd", &self.cwd));
    }

    fn ai_request(&mut self, kind: &str, fields: Obj) {
        console::set_color(Color::LightCyan, Color::Black);
        let mut lines = 0;
        let outcome = aibridge::request(kind, fields, |l| {
            println!("{l}");
            lines += 1;
        });
        console::set_color(Color::LightGray, Color::Black);
        let result = match outcome {
            Outcome::Done => "done",
            Outcome::Cancelled => {
                println!("(cancelled)");
                "cancelled"
            }
            Outcome::Offline => {
                error("AI offline: beehive is not running on the host (start it with: make beehive)");
                "offline"
            }
            Outcome::Error(e) => {
                error(&format!("AI error: {e}"));
                "error"
            }
        };
        // Only the outcome is logged, not the question or the answer.
        let ev = eventlog::event("ai").str("kind", kind).str("result", result).num("lines", lines).finish();
        eventlog::write(&mut self.storage.fs(), ev);
    }

    fn ai_status(&mut self) {
        let switch = if self.ai_enabled() { "on" } else { "off ('ai on' to enable)" };
        println!("AI features: {switch}");
        if !aibridge::present() {
            return println!("bridge:      none (no COM2 port in this machine)");
        }
        match aibridge::ping(1500) {
            Some(peer) => println!("bridge:      COM2, beehive online ({peer})"),
            None => println!("bridge:      COM2, beehive offline (start it with: make beehive)"),
        }
    }

    fn ai_switch(&mut self, state: &str) {
        let mut fs = self.storage.fs();
        let r = fs.mkdir_all("/system").and_then(|_| fs.write_file(AI_SWITCH, format!("{state}\n").as_bytes()));
        drop(fs);
        match r {
            Ok(()) => println!("AI features switched {state}."),
            Err(e) => error(&format!("could not save: {e}")),
        }
    }

    // ----- helpers -----------------------------------------------------------

    /// Absolute, normalized path for an argument relative to the cwd.
    fn abs(&self, p: &str) -> String {
        if p.starts_with('/') {
            normalize(p)
        } else {
            normalize(&format!("{}/{}", self.cwd, p))
        }
    }

    /// Destination for mv/cp: if `to` is a directory, put the item inside it.
    fn abs_target(&mut self, from: &str, to: &str) -> String {
        let to = self.abs(to);
        if self.storage.fs().is_dir(&to) {
            let name = self.abs(from).rsplit('/').next().unwrap_or("").to_string();
            return normalize(&format!("{to}/{name}"));
        }
        to
    }


    // ----- commands ------------------------------------------------------------

    fn cd(&mut self, dir: &str) {
        let path = self.abs(dir);
        match self.storage.fs().metadata(&path) {
            Ok(m) if m.kind == FileKind::Directory => self.cwd = path,
            Ok(_) => error(&format!("{dir}: not a directory")),
            Err(e) => error(&format!("{dir}: {e}")),
        }
    }

    fn ls(&mut self, path: &str) {
        let listing = self.storage.fs().read_dir(path);
        match listing {
            Ok(entries) => {
                for e in entries {
                    match e.kind {
                        FileKind::Directory => {
                            print!("  {:>8}  ", "<dir>");
                            console::set_color(Color::LightBlue, Color::Black);
                            println!("{}/", e.name);
                            console::set_color(Color::LightGray, Color::Black);
                        }
                        FileKind::File => println!("  {:>8}  {}", e.size, e.name),
                    }
                }
            }
            Err(FsError::NotADirectory) => match self.storage.fs().metadata(path) {
                Ok(m) => println!("  {:>8}  {}", m.size, path),
                Err(e) => error(&e.to_string()),
            },
            Err(e) => error(&format!("{path}: {e}")),
        }
    }

    fn tree(&mut self, path: &str) {
        println!("{path}");
        if let Err(e) = self.tree_walk(path, 1) {
            error(&e.to_string());
        }
    }

    fn tree_walk(&mut self, path: &str, depth: usize) -> Result<(), FsError> {
        let entries = self.storage.fs().read_dir(path)?;
        for e in entries {
            let indent = "  ".repeat(depth);
            match e.kind {
                FileKind::Directory => {
                    println!("{indent}{}/", e.name);
                    self.tree_walk(&format!("{}/{}", path.trim_end_matches('/'), e.name), depth + 1)?;
                }
                FileKind::File => println!("{indent}{} ({} B)", e.name, e.size),
            }
        }
        Ok(())
    }

    fn cat(&mut self, file: &str) {
        let path = self.abs(file);
        let data = self.storage.fs().read_file(&path);
        match data {
            Ok(data) => {
                let text = String::from_utf8_lossy(&data);
                print!("{text}");
                if !text.is_empty() && !text.ends_with('\n') {
                    println!();
                }
            }
            Err(e) => error(&format!("{file}: {e}")),
        }
    }

    fn touch(&mut self, file: &str) {
        let path = self.abs(file);
        let exists = self.storage.fs().exists(&path);
        if !exists {
            let r = self.storage.fs().create(&path);
            report(r);
        }
    }

    fn cp(&mut self, from: &str, to: &str) {
        let src = self.abs(from);
        let dst = self.abs_target(from, to);
        let mut fs = self.storage.fs();
        let r = fs.read_file(&src).and_then(|d| fs.write_file(&dst, &d));
        drop(fs);
        report(r);
    }

    fn df(&mut self) {
        let s = self.storage.fs().stats();
        let kib = |b: u32| b as u64 * s.block_size as u64 / 1024;
        println!(
            "{}: {} KiB used, {} KiB free, {} KiB total; {} of {} inodes free",
            if self.storage.persistent { "disk" } else { "ramdisk" },
            kib(s.total_blocks - s.free_blocks),
            kib(s.free_blocks),
            kib(s.total_blocks),
            s.free_inodes,
            s.total_inodes
        );
    }

    fn set_layout(&mut self, id: &str) {
        let Some(layout) = Layout::from_id(id) else {
            return error(&format!("unknown layout '{id}'; available: us, be"));
        };
        keyboard::set_layout(layout);
        let mut fs = self.storage.fs();
        let saved = fs.mkdir_all("/system").and_then(|_| fs.write_file(LAYOUT_FILE, format!("{}\n", layout.id()).as_bytes()));
        drop(fs);
        let note = match (saved, self.storage.persistent) {
            (Ok(()), true) => " (saved)",
            (Ok(()), false) => " (RAM disk: not kept after reboot)",
            (Err(_), _) => " (could not save)",
        };
        println!("keyboard layout: {}{}", layout.name(), note);
    }
}

fn show_layouts() {
    let current = keyboard::layout();
    for l in Layout::ALL {
        let mark = if l == current { "*" } else { " " };
        println!(" {mark} {:<4} {}", l.id(), l.name());
    }
    println!("change with: layout <id>   (serial terminals use the host's layout)");
}

const COMMANDS: &[(&str, &str)] = &[
    ("help", "show this help"),
    ("clear", "clear the screen (or Ctrl+L)"),
    ("echo TEXT [> FILE]", "print text, or write (>) / append (>>) to a file"),
    ("ls [DIR]", "list a directory"),
    ("cd [DIR], pwd", "change / show the current directory"),
    ("cat FILE", "show a file"),
    ("touch FILE", "create an empty file"),
    ("mkdir DIR", "create a directory (with parents)"),
    ("rm PATH", "remove a file or empty directory"),
    ("mv FROM TO", "move / rename"),
    ("cp FROM TO", "copy a file"),
    ("tree [DIR]", "show a directory tree"),
    ("df", "filesystem usage"),
    ("layout [us|be]", "show / change the keyboard layout"),
    ("history", "previous commands (Up/Down to recall)"),
    ("mem, about", "memory usage, system info"),
    ("calc EXPR", "integer arithmetic: + - * / % ( )"),
    ("date, uptime", "current date and time (UTC), time since boot"),
    ("log [N]", "last N events from the event log"),
    ("wish TEXT", "tell the OS what you'd like it to do"),
    ("feedback TEXT", "tell the OS what could be better"),
    ("ask QUESTION", "ask the AI (needs beehive on the host)"),
    ("explain", "ask the AI about the last error or crash"),
    ("ai [on|off]", "AI bridge status; switch AI features on/off"),
    ("selftest", "run the kernel self-tests"),
    ("net", "network status; net dhcp | static IP/PREFIX [GW] | dns ADDR | off"),
    ("ping [-c N] HOST", "check that a machine answers"),
    ("nslookup NAME", "look up the address of a host name"),
    ("nc HOST PORT [TEXT]", "open a TCP connection: type lines, see the answers"),
    ("httpd [on [PORT]|off]", "status page for the local network (off by default)"),
    ("wget URL [FILE]", "fetch a web address (http://): show it or save it"),
    ("ntp [SERVER]", "set the clock from a time server"),
    ("tftp get|put HOST FILE", "copy a file from / to a TFTP server"),
    ("arp", "neighbours learned on the local network"),
    ("lspci", "list PCI devices"),
    ("threads", "what runs side by side inside the kernel"),
    ("slots", "A/B kernel slots: which one runs, last trial"),
    ("crash", "deliberately crash the kernel (tests crash reports)"),
    ("reboot, shutdown", "restart / power off"),
];

fn is_command(c: &str) -> bool {
    const ALIASES: &[&str] = &[
        "cls", "dir", "type", "del", "rmdir", "move", "copy", "keyboard", "version", "halt", "poweroff", "pwd",
        "about", "reboot", "mem", "uptime",
    ];
    ALIASES.contains(&c) || COMMANDS.iter().any(|(u, _)| u.split([' ', ',']).next() == Some(c))
}

fn help() {
    for (usage, text) in COMMANDS {
        console::set_color(Color::Yellow, Color::Black);
        print!("  {usage:<20}");
        console::set_color(Color::LightGray, Color::Black);
        println!("{text}");
    }
}

/// `wget URL` without a file shows at most this much.
const MAX_SHOWN: u64 = 256 * 1024;

/// " in 1.2 s (340 KiB/s)" for transfers that took a noticeable time.
fn rate(bytes: u64, started_ms: u64) -> String {
    let ms = time::uptime_ms() - started_ms;
    if ms < 500 {
        return String::new();
    }
    format!(" in {}.{} s ({} KiB/s)", ms / 1000, ms % 1000 / 100, bytes * 1000 / ms / 1024)
}

/// A file being received from the network. The data goes to `PATH.part`
/// first; only a complete transfer replaces the file at PATH.
struct Download<'a> {
    storage: &'a mut Storage,
    path: &'a str,
    part: String,
    /// Received, not yet written.
    buffer: Vec<u8>,
    /// `part` exists on the disk.
    started: bool,
    done: bool,
}

impl<'a> Download<'a> {
    const FLUSH: usize = 64 * 1024;

    fn start(storage: &'a mut Storage, path: &'a str) -> Self {
        Download { storage, path, part: format!("{path}.part"), buffer: Vec::new(), started: false, done: false }
    }

    fn flush(&mut self) -> Result<(), String> {
        let mut fs = self.storage.fs();
        let r = if self.started { fs.append(&self.part, &self.buffer) } else { fs.write_file(&self.part, &self.buffer) };
        drop(fs);
        self.started = true;
        self.buffer.clear();
        r.map_err(|e| format!("can't write {}: {e}", self.path))
    }

    fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.buffer.extend_from_slice(data);
        if self.buffer.len() >= Self::FLUSH {
            return self.flush();
        }
        Ok(())
    }

    /// The transfer is complete: put the file in place.
    fn finish(mut self) -> Result<(), String> {
        self.flush()?;
        let mut fs = self.storage.fs();
        if fs.exists(self.path) {
            fs.remove(self.path).map_err(|e| format!("can't replace {}: {e}", self.path))?;
        }
        let r = fs.rename(&self.part, self.path).map_err(|e| format!("can't write {}: {e}", self.path));
        drop(fs);
        self.done = r.is_ok();
        r
    }
}

impl Drop for Download<'_> {
    /// An unfinished transfer leaves no partial file behind.
    fn drop(&mut self) {
        if !self.done && self.started {
            let _ = self.storage.fs().remove(&self.part);
        }
    }
}

fn report(r: Result<(), FsError>) {
    if let Err(e) = r {
        error(&e.to_string());
    }
}

/// Microseconds as "0.42 ms" / "12.3 ms" / "150 ms".
fn millis(micros: u64) -> String {
    match micros {
        0..=9_999 => format!("{}.{:02} ms", micros / 1000, micros % 1000 / 10),
        10_000..=99_999 => format!("{}.{} ms", micros / 1000, micros % 1000 / 100),
        _ => format!("{} ms", micros / 1000),
    }
}

/// Closest known command (edit distance at most 2), for "did you mean".
fn suggest(c: &str) -> Option<&'static str> {
    let names = COMMANDS.iter().flat_map(|(u, _)| u.split(", ")).filter_map(|u| u.split(' ').next());
    names
        .map(|n| (edit_distance(c, n), n))
        .filter(|&(d, n)| d <= 2 && d < n.len())
        .min_by_key(|&(d, _)| d)
        .map(|(_, n)| n)
}

pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, &cb) in b.iter().enumerate() {
            cur.push((prev[j] + (ca != cb) as usize).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn error(msg: &str) {
    set_last_error(Some(msg));
    console::set_color(Color::LightRed, Color::Black);
    println!("{msg}");
    console::set_color(Color::LightGray, Color::Black);
}

/// Split a command line into words; "double quotes" group words.
pub(crate) fn tokenize(line: &str) -> Result<Vec<String>, &'static str> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_word {
                    out.push(core::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quoted {
        return Err("missing closing quote");
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

/// Evaluate an integer expression: + - * / % with the usual precedence,
/// parentheses and unary minus. i64 arithmetic; `/` and `%` truncate toward
/// zero (so -7 / 2 = -3 and -7 % 2 = -1). Overflow and division by zero are
/// errors, never panics.
pub(crate) fn calc(expr: &str) -> Result<i64, &'static str> {
    let mut p = Calc { s: expr.as_bytes(), pos: 0, depth: 0 };
    let v = p.expr()?;
    if p.peek().is_some() {
        return Err("parse error");
    }
    Ok(v)
}

struct Calc<'a> {
    s: &'a [u8],
    pos: usize,
    depth: u32,
}

impl Calc<'_> {
    fn peek(&mut self) -> Option<u8> {
        while self.s.get(self.pos).is_some_and(|c| c.is_ascii_whitespace()) {
            self.pos += 1;
        }
        self.s.get(self.pos).copied()
    }

    fn expr(&mut self) -> Result<i64, &'static str> {
        let mut v = self.term()?;
        while let Some(op @ (b'+' | b'-')) = self.peek() {
            self.pos += 1;
            let r = self.term()?;
            v = if op == b'+' { v.checked_add(r) } else { v.checked_sub(r) }.ok_or("overflow")?;
        }
        Ok(v)
    }

    fn term(&mut self) -> Result<i64, &'static str> {
        let mut v = self.unary()?;
        while let Some(op @ (b'*' | b'/' | b'%')) = self.peek() {
            self.pos += 1;
            let r = self.unary()?;
            if op != b'*' && r == 0 {
                return Err("division by zero");
            }
            v = match op {
                b'*' => v.checked_mul(r),
                b'/' => v.checked_div(r),
                _ => v.checked_rem(r),
            }
            .ok_or("overflow")?;
        }
        Ok(v)
    }

    fn unary(&mut self) -> Result<i64, &'static str> {
        // Bounded recursion: the kernel stack is small.
        if self.depth >= 64 {
            return Err("expression too deeply nested");
        }
        self.depth += 1;
        let r = self.unary_inner();
        self.depth -= 1;
        r
    }

    fn unary_inner(&mut self) -> Result<i64, &'static str> {
        match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                // A negative literal can reach i64::MIN: -9223372036854775808.
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    return self.number(true);
                }
                self.unary()?.checked_neg().ok_or("overflow")
            }
            Some(b'(') => {
                self.pos += 1;
                let v = self.expr()?;
                if self.peek() != Some(b')') {
                    return Err("missing ')'");
                }
                self.pos += 1;
                Ok(v)
            }
            Some(c) if c.is_ascii_digit() => self.number(false),
            _ => Err("parse error"),
        }
    }

    fn number(&mut self, negative: bool) -> Result<i64, &'static str> {
        let mut v: i64 = 0;
        while let Some(&c) = self.s.get(self.pos).filter(|c| c.is_ascii_digit()) {
            self.pos += 1;
            let d = (c - b'0') as i64;
            v = v
                .checked_mul(10)
                .and_then(|v| if negative { v.checked_sub(d) } else { v.checked_add(d) })
                .ok_or("overflow")?;
        }
        Ok(v)
    }
}
