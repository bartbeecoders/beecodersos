//! BeeShell: the interactive terminal started at boot.

use crate::editor::{Editor, Line};
use crate::layouts::Layout;
use crate::storage::Storage;
use crate::vga::Color;
use crate::{allocator, console, keyboard, power};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use beefs::{normalize, FileKind, FsError};

const LAYOUT_FILE: &str = "/system/keyboard";

pub struct Shell {
    storage: Storage,
    cwd: String,
    editor: Editor,
}

impl Shell {
    pub fn new(storage: Storage) -> Self {
        Shell { storage, cwd: String::from("/"), editor: Editor::new() }
    }

    /// Apply the keyboard layout saved on disk, if any.
    pub fn load_layout(&mut self) {
        if let Ok(data) = self.storage.fs.read_file(LAYOUT_FILE) {
            if let Some(l) = core::str::from_utf8(&data).ok().and_then(|s| Layout::from_id(s.trim())) {
                keyboard::set_layout(l);
            }
        }
    }

    pub fn run(&mut self) -> ! {
        loop {
            self.prompt();
            match self.editor.read_line() {
                Line::Text(line) => self.execute(&line),
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
            let fs = &mut self.storage.fs;
            let r = if append { fs.append(&path, text.as_bytes()) } else { fs.write_file(&path, text.as_bytes()) };
            return self.report(r);
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
                let r = self.storage.fs.mkdir_all(&self.abs(dir));
                self.report(r)
            }
            ("rm" | "del" | "rmdir", [path]) => {
                let r = self.storage.fs.remove(&self.abs(path));
                self.report(r)
            }
            ("mv" | "move", [from, to]) => {
                let (from, to) = (self.abs(from), self.abs_target(from, to));
                let r = self.storage.fs.rename(&from, &to);
                self.report(r)
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
            ("reboot", []) => power::reboot(),
            ("shutdown" | "halt" | "poweroff", []) => power::shutdown(),
            ("exit" | "logout" | "quit", _) => {
                println!("BeeShell is the system console; use 'shutdown' or 'reboot'.")
            }
            (c, _) if is_command(c) => error(&format!("{c}: wrong arguments (see 'help')")),
            (c, _) => error(&format!("{c}: command not found (type 'help')")),
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
        if self.storage.fs.is_dir(&to) {
            let name = self.abs(from).rsplit('/').next().unwrap_or("").to_string();
            return normalize(&format!("{to}/{name}"));
        }
        to
    }

    fn report(&self, r: Result<(), FsError>) {
        if let Err(e) = r {
            error(&e.to_string());
        }
    }

    // ----- commands ------------------------------------------------------------

    fn cd(&mut self, dir: &str) {
        let path = self.abs(dir);
        match self.storage.fs.metadata(&path) {
            Ok(m) if m.kind == FileKind::Directory => self.cwd = path,
            Ok(_) => error(&format!("{dir}: not a directory")),
            Err(e) => error(&format!("{dir}: {e}")),
        }
    }

    fn ls(&mut self, path: &str) {
        match self.storage.fs.read_dir(path) {
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
            Err(FsError::NotADirectory) => match self.storage.fs.metadata(path) {
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
        for e in self.storage.fs.read_dir(path)? {
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
        match self.storage.fs.read_file(&self.abs(file)) {
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
        if !self.storage.fs.exists(&path) {
            let r = self.storage.fs.create(&path);
            self.report(r);
        }
    }

    fn cp(&mut self, from: &str, to: &str) {
        let src = self.abs(from);
        let dst = self.abs_target(from, to);
        let r = self.storage.fs.read_file(&src).and_then(|d| self.storage.fs.write_file(&dst, &d));
        self.report(r);
    }

    fn df(&self) {
        let s = self.storage.fs.stats();
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
        let saved = self.storage.fs.mkdir_all("/system").and_then(|_| {
            self.storage.fs.write_file(LAYOUT_FILE, format!("{}\n", layout.id()).as_bytes())
        });
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
    ("reboot, shutdown", "restart / power off"),
];

fn is_command(c: &str) -> bool {
    const ALIASES: &[&str] = &[
        "cls", "dir", "type", "del", "rmdir", "move", "copy", "keyboard", "version", "halt", "poweroff", "pwd",
        "about", "reboot", "mem",
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

fn error(msg: &str) {
    console::set_color(Color::LightRed, Color::Black);
    println!("{msg}");
    console::set_color(Color::LightGray, Color::Black);
}

/// Split a command line into words; "double quotes" group words.
fn tokenize(line: &str) -> Result<Vec<String>, &'static str> {
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
