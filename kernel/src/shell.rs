//! BeeShell: the interactive terminal started at boot.

use crate::editor::{Editor, Line};
use crate::layouts::Layout;
use crate::storage::Storage;
use crate::vga::Color;
use crate::aibridge::{self, Outcome};
use crate::json::Obj;
use crate::ring::Global;
use crate::{allocator, console, crash, eventlog, keyboard, power, selftest, time};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use beefs::{normalize, FileKind, FsError};

const LAYOUT_FILE: &str = "/system/keyboard";
/// Contains "off" when the AI features are switched off (`ai off`).
const AI_SWITCH: &str = "/system/ai";
const INBOX: &str = "/system/inbox/signals.jsonl";
const OLD_INBOX: &str = "/system/inbox/signals.old.jsonl";

/// Last error message printed by `error()`, and whether the current command printed one.
static LAST_ERROR: Global<Option<String>> = Global::new(None);

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
        *LAST_ERROR.get() = None;
        self.status = Status::Ok;
        let start = time::uptime_ms();
        self.execute(line);
        if self.status == Status::Ok && LAST_ERROR.get().is_some() {
            self.status = Status::Error;
        }
        if self.status != Status::Ok {
            let msg = LAST_ERROR.get().clone().unwrap_or_default();
            self.last_failure = Some((String::from(line), msg));
        }
        let status = match self.status {
            Status::Ok => "ok",
            Status::Error => "error",
            Status::NotFound => "notfound",
        };
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
            if cmd != "echo" {
                return error("redirection is only supported for echo");
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
        aibridge::send(
            &Obj::new()
                .str("type", "signal")
                .str("kind", kind)
                .str("text", text)
                .str("build", crate::OS_BUILD)
                .str("t", &time::timestamp())
                .finish(),
        );
        aibridge::peer().is_some()
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
    ("date, uptime", "current date and time (UTC), time since boot"),
    ("log [N]", "last N events from the event log"),
    ("wish TEXT", "tell the OS what you'd like it to do"),
    ("feedback TEXT", "tell the OS what could be better"),
    ("ask QUESTION", "ask the AI (needs beehive on the host)"),
    ("explain", "ask the AI about the last error or crash"),
    ("ai [on|off]", "AI bridge status; switch AI features on/off"),
    ("selftest", "run the kernel self-tests"),
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

fn report(r: Result<(), FsError>) {
    if let Err(e) = r {
        error(&e.to_string());
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
    *LAST_ERROR.get() = Some(String::from(msg));
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
