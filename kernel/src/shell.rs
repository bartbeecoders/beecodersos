//! BeeShell: the interactive terminal started at boot.

use crate::editor::{Editor, Line};
use crate::layouts::Layout;
use crate::disks::{self, DiskRef};
use crate::storage::{self, Storage};
use crate::vga::Color;
use crate::aibridge::{self, Outcome};
use crate::json::Obj;
use crate::ring::Global;
use crate::{allocator, bootslots, display, hivectl, httpd, install, update, remote, session, console, crash, eventlog, keyboard, net, pci, power, selftest, textops, doplan, thread, time, users};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use beefs::{normalize, FileKind, FsError};

const LAYOUT_FILE: &str = "/system/keyboard";
/// Lines "mode WIDTHxHEIGHT" and "font 1|2|auto" (`display mode`, `display font`).
const DISPLAY_FILE: &str = "/system/display";
/// "on" or "off": the desktop (`desktop on` / `desktop off`). No file: on.
const DESKTOP_FILE: &str = "/system/desktop";
/// Shown at boot when it exists (at most `BANNER_LINES` lines).
const BANNER_FILE: &str = "/system/banner";
const BANNER_LINES: usize = 20;
/// The path of the desktop's wallpaper picture.
const WALLPAPER_FILE: &str = "/system/wallpaper";
/// The time zone for local time ("UTC", "CET", "+05:30").
const TIMEZONE_FILE: &str = "/system/timezone";
/// "auto off": the other processors are not started at boot.
const CPU_FILE: &str = "/system/cpu";
/// Contains "off" when the AI features are switched off (`ai off`).
const AI_SWITCH: &str = "/system/ai";
/// The network link to beehive (`ai connect`): host, port, this machine's
/// name and its pairing key. See aibridge::Settings.
const HIVE_FILE: &str = "/system/hive";
/// "on": the host may run commands here and look at the screen (`ai control on`).
const CONTROL_FILE: &str = "/system/hive-control";
/// "auto": fetch new kernels from the host and restart to try them (`update auto on`).
const UPDATE_FILE: &str = "/system/update";
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
    /// In a console session: show what is typed (the remote shell; a test
    /// that types whole lines does not need it).
    echo: bool,
    /// In a console session: `exit` was typed.
    leave: bool,
    /// Who runs the commands, once the machine has accounts (users.rs).
    user: Option<users::User>,
    /// The AI host's control shell: admin rights (`ai control on` at the console allowed it).
    host: bool,
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
            echo: false,
            leave: false,
            user: None,
            host: false,
        }
    }

    /// The shell of a remote session (remote.rs): it shows what is typed.
    pub fn remote(storage: Storage) -> Self {
        let mut shell = Shell { echo: true, ..Shell::new(storage) };
        shell.go_home();
        shell
    }

    /// The shell remote control runs its commands in (hivectl.rs): nothing
    /// typed is shown back, the output is all the host wants.
    pub fn control(storage: Storage) -> Self {
        let mut shell = Shell { host: true, ..Shell::new(storage) };
        shell.go_home();
        shell
    }

    /// The shell of a terminal window on the desktop: it runs as whoever is
    /// logged in at the console.
    pub fn terminal(storage: Storage) -> Self {
        let mut shell = Shell { echo: true, user: users::console_user(), ..Shell::new(storage) };
        shell.go_home();
        shell
    }

    /// A shell whose commands are not recorded in the event log.
    pub fn unlogged(storage: Storage) -> Self {
        Shell { logging: false, ..Shell::new(storage) }
    }

    /// Run in a console session (see session.rs): read command lines from
    /// its input until it is closed or `exit` is typed. Line editing is
    /// minimal: Backspace, and Ctrl+C to drop the line.
    pub fn run_session(&mut self) {
        use crate::input::Key;
        while !self.leave {
            self.prompt();
            let mut line = String::new();
            loop {
                match crate::input::read_key() {
                    Key::Enter => break,
                    Key::Char(c) => {
                        line.push(c);
                        if self.echo {
                            print!("{c}");
                        }
                    }
                    Key::Backspace => {
                        if line.pop().is_some() && self.echo {
                            print!("\x08 \x08");
                        }
                    }
                    Key::Ctrl('c') => {
                        line.clear();
                        if self.echo {
                            print!("^C");
                        }
                        break;
                    }
                    Key::Ctrl('d') => {
                        set_last_error(None);
                        return;
                    }
                    _ => {}
                }
            }
            println!();
            self.run_line(&line);
        }
        set_last_error(None);
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

    /// Apply the resolution and font size saved on disk, if any. A `mode=`
    /// boot flag wins over the saved resolution; a driver that cannot change
    /// the mode keeps the one it has.
    pub fn load_display(&mut self) {
        let Ok(data) = self.storage.fs().read_file(DISPLAY_FILE) else { return };
        let (mode, font) = display::parse_settings(&String::from_utf8_lossy(&data));
        if let Some(font) = font {
            console::set_font(font);
        }
        let now = display::current().map(|(_, m)| (m.width, m.height));
        if let Some((width, height)) = mode.filter(|m| Some(*m) != now && crate::bootargs::value("mode").is_none()) {
            let _ = console::set_display_mode(width, height);
        }
    }

    /// Remember one display setting ("mode" or "font"); the other one stays.
    fn save_display(&mut self, key: &str, value: &str) -> &'static str {
        let mut fs = self.storage.fs();
        let old = fs.read_file(DISPLAY_FILE).map(|d| String::from_utf8_lossy(&d).into_owned()).unwrap_or_default();
        let mut text: String = old.lines().filter(|l| l.split_whitespace().next() != Some(key)).map(|l| format!("{l}\n")).collect();
        text += &format!("{key} {value}\n");
        let saved = fs.mkdir_all("/system").and_then(|_| fs.write_file(DISPLAY_FILE, text.as_bytes()));
        drop(fs);
        match (saved, self.storage.persistent) {
            (Ok(()), true) => "saved",
            (Ok(()), false) => "RAM disk: not kept after reboot",
            (Err(_), _) => "could not save",
        }
    }

    /// Start where the user's files are: on an installed disk that is the
    /// data partition. (For the shells people type in; a new `Shell` starts in /.)
    pub fn go_home(&mut self) {
        if storage::has_data() {
            self.cwd = String::from(storage::DATA_DIR);
        }
    }

    /// `cpu test`: the same work (hashing 64 MiB) on one processor and
    /// on all that are in use, with the time each took.
    fn cpu_test(&mut self) {
        use core::sync::atomic::{AtomicU64, Ordering};
        const PIECES: usize = 64;
        let data: Vec<u8> = (0..1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        // Each piece hashes the megabyte with its own number in front; the
        // result of the job is all those hashes folded together.
        let piece = |n: usize| {
            let mut hash = beenet::sha256::Sha256::new();
            hash.update(&[n as u8]);
            hash.update(&data);
            u64::from_le_bytes(hash.finish()[..8].try_into().unwrap_or([0; 8]))
        };
        let start = time::micros();
        let alone = (0..PIECES).fold(0u64, |sum, n| sum ^ piece(n).rotate_left(n as u32));
        let alone_ms = (time::micros() - start) / 1000;
        let sum = AtomicU64::new(0);
        let start = time::micros();
        crate::smp::run(PIECES, &|n| {
            sum.fetch_xor(piece(n).rotate_left(n as u32), Ordering::SeqCst);
        });
        let together_ms = (time::micros() - start) / 1000;
        let running = crate::smp::running();
        println!("hashing 64 MiB on 1 processor: {alone_ms} ms");
        println!("hashing 64 MiB on {running}: {together_ms} ms");
        if sum.load(Ordering::SeqCst) == alone {
            let tenths = alone_ms * 10 / together_ms.max(1);
            println!("same result; {}.{} times as fast", tenths / 10, tenths % 10);
        } else {
            error("cpu test: the results differ");
        }
    }

    /// Follow the desktop choice saved on disk, if any (default: on).
    pub fn load_desktop(&mut self) {
        let data = self.storage.fs().read_file(DESKTOP_FILE).ok();
        console::apply_desktop_setting(data.as_deref().and_then(|d| core::str::from_utf8(d).ok()));
    }

    /// At boot: the text of `/system/banner`, if there is one (the first
    /// lines; control characters left out).
    pub fn show_banner(&mut self) {
        let data = self.storage.fs().read_file(BANNER_FILE).ok();
        let Some(data) = data else { return };
        let text = String::from_utf8_lossy(&data);
        println!();
        for line in text.lines().take(BANNER_LINES) {
            let line: String = line.chars().filter(|c| !c.is_control() || *c == '\t').take(200).collect();
            println!("{line}");
        }
    }

    // ----- accounts (users.rs) ---------------------------------------------------

    /// May this shell change the system? Without accounts everyone may.
    fn is_admin(&mut self) -> bool {
        self.host || self.user.as_ref().is_some_and(users::User::is_admin) || users::load(&mut self.storage).is_empty()
    }

    /// May this shell change what is at `path`? Says whose it is if not.
    fn may_change(&mut self, path: &str) -> bool {
        let admin = self.is_admin();
        match users::owner_blocking(self.user.as_ref(), admin, path) {
            None => true,
            Some(owner) => {
                error(&format!("{path}: belongs to {owner} (anyone may read it, only they may change it)"));
                false
            }
        }
    }

    fn whoami(&mut self) {
        match &self.user {
            Some(user) => println!("{} ({})", user.name, if user.groups.is_empty() { String::from("no groups") } else { user.groups.join(", ") }),
            None if self.host => println!("the AI host's control shell (admin rights)"),
            None if users::load(&mut self.storage).is_empty() => {
                println!("nobody: this machine has no accounts, so everyone may do everything ('useradd NAME' makes the first, an admin)")
            }
            None => println!("nobody: not logged in (a remote session has no admin rights)"),
        }
    }

    /// A password typed without showing it; `None` after Ctrl+C or Escape.
    fn read_secret(&mut self, prompt: &str) -> Option<String> {
        use crate::input::Key;
        print!("{prompt}");
        console::flush();
        let mut text = String::new();
        loop {
            match crate::input::read_key() {
                Key::Enter => break,
                Key::Char(c) if text.len() < users::MAX_PASSWORD => text.push(c),
                Key::Backspace => {
                    text.pop();
                }
                Key::Ctrl('c' | 'd') | Key::Escape => {
                    println!();
                    return None;
                }
                _ => {}
            }
        }
        println!();
        Some(text)
    }

    /// A new password, typed twice; `None` if they differ or it is too short.
    fn new_password(&mut self, prompt: &str) -> Option<String> {
        let first = self.read_secret(prompt)?;
        if first.chars().count() < users::MIN_PASSWORD {
            error(&format!("a password needs at least {} characters", users::MIN_PASSWORD));
            return None;
        }
        let again = self.read_secret("the same again: ")?;
        if again != first {
            error("the two passwords differ");
            return None;
        }
        Some(first)
    }

    /// Ask for a name and password until they match an account (the console
    /// at boot, and after `logout`). A pause after every wrong try.
    fn login_at_console(&mut self) {
        loop {
            let accounts = users::load(&mut self.storage);
            if accounts.is_empty() {
                return;
            }
            print!("login: ");
            console::flush();
            let name = match self.editor.read_line() {
                Line::Text(line) => String::from(line.trim()),
                _ => continue,
            };
            let Some(password) = self.read_secret("password: ") else { continue };
            match accounts.into_iter().find(|u| u.name == name).filter(|u| u.password_ok(&password)) {
                Some(user) => {
                    println!("welcome, {}", user.name);
                    let home = format!("/home/{}", user.name);
                    if self.storage.fs().is_dir(&home) {
                        self.cwd = home;
                    }
                    users::set_console_user(Some(user.clone()));
                    self.user = Some(user);
                    let ev = eventlog::event("login").str("result", "ok").finish();
                    eventlog::write(&mut self.storage.fs(), ev);
                    return;
                }
                None => {
                    // Names are never logged, only that a try failed.
                    let ev = eventlog::event("login").str("result", "wrong").finish();
                    eventlog::write(&mut self.storage.fs(), ev);
                    thread::sleep_ms(2_000);
                    println!("wrong name or password");
                }
            }
        }
    }

    /// `useradd NAME [admin]`. The first account is always an admin, and the
    /// console is logged in as it (so nobody is locked out).
    fn useradd(&mut self, name: &str, admin: bool) {
        let mut accounts = users::load(&mut self.storage);
        if !users::valid_name(name) {
            return error("useradd: a name is a lowercase letter, then up to 15 letters, digits, - or _");
        }
        if accounts.iter().any(|u| u.name == name) {
            return error(&format!("useradd: {name} exists already"));
        }
        if accounts.len() >= users::MAX_USERS {
            return error("useradd: too many accounts");
        }
        let first = accounts.is_empty();
        let Some(password) = self.new_password(&format!("password for {name}: ")) else { return println!("no account made") };
        let groups: &[&str] = if admin || first { &[users::ADMIN] } else { &[] };
        let user = users::User::new(name, groups, &users::new_salt(), &password);
        accounts.push(user.clone());
        if let Err(e) = users::save(&mut self.storage, &accounts) {
            return error(&format!("useradd: {e}"));
        }
        let _ = self.storage.fs().mkdir_all(&format!("/home/{name}"));
        println!("account {name} made{}, home folder /home/{name}", if groups.is_empty() { "" } else { " (admin)" });
        if first {
            println!("the first account: from now on the console asks for a name and password at boot");
            if session::current().is_none() && !self.host {
                users::set_console_user(Some(user.clone()));
                self.user = Some(user);
                println!("logged in as {name}");
            }
        }
    }

    fn userdel(&mut self, name: &str) {
        let mut accounts = users::load(&mut self.storage);
        let Some(at) = accounts.iter().position(|u| u.name == name) else { return error(&format!("userdel: no account {name}")) };
        let removed = accounts.remove(at);
        if removed.is_admin() && !accounts.is_empty() && !accounts.iter().any(users::User::is_admin) {
            return error("userdel: that is the last admin; make another admin first");
        }
        if let Err(e) = users::save(&mut self.storage, &accounts) {
            return error(&format!("userdel: {e}"));
        }
        if self.user.as_ref().is_some_and(|u| u.name == name) {
            self.user = None;
            users::set_console_user(None);
        }
        println!("account {name} removed (its home folder stays){}", if accounts.is_empty() { "; no accounts are left: no login any more" } else { "" });
    }

    /// `passwd`: a new password for yourself (after the old one), or, for an
    /// admin, for someone else.
    fn passwd(&mut self, other: Option<&str>) {
        let mut accounts = users::load(&mut self.storage);
        let name = match (other, &self.user) {
            (Some(name), _) => String::from(name),
            (None, Some(me)) => me.name.clone(),
            (None, None) => return error("passwd: nobody is logged in here (passwd NAME, as an admin)"),
        };
        let Some(at) = accounts.iter().position(|u| u.name == name) else { return error(&format!("passwd: no account {name}")) };
        if other.is_none() {
            let Some(old) = self.read_secret("current password: ") else { return };
            if !accounts[at].password_ok(&old) {
                thread::sleep_ms(2_000);
                return error("passwd: that is not the current password");
            }
        }
        let Some(password) = self.new_password(&format!("new password for {name}: ")) else { return println!("password not changed") };
        accounts[at].set_password(&users::new_salt(), &password);
        match users::save(&mut self.storage, &accounts) {
            Ok(()) => println!("password of {name} changed"),
            Err(e) => error(&format!("passwd: {e}")),
        }
    }

    /// Play 48k frames on the sound card; Ctrl+C stops.
    fn play_frames(&mut self, frames: &[u8], name: &str) {
        let stop = || crate::input::pending() && crate::input::poll_key() == Some(crate::input::Key::Ctrl('c'));
        match crate::hda::play(frames, stop) {
            Ok((ms, moved)) => println!("played {ms} ms ({} KiB taken by the sound card)", moved / 1024),
            Err(why) => error(&format!("{name}: {why}")),
        }
    }

    /// `record`: five pictures of the screen a second into an animated
    /// GIF, each turned into the GIF's colors on all processors; a
    /// picture like the one before only makes that one last longer.
    fn record(&mut self, path: &str, file: &str, secs: u32) {
        const FRAME_MS: u64 = 200;
        /// A recording stops growing here (a still desktop takes far less).
        const MAX_BYTES: usize = 32 * 1024 * 1024;
        let mut writer: Option<beenet::gif::Writer> = None;
        let start = crate::time::uptime_ms();
        let (mut taken, mut stopped) = (0u32, false);
        for i in 0..secs as u64 * 1000 / FRAME_MS {
            let shot = console::with_screen(|w, h, pixel| {
                let mut indices = alloc::vec![0u8; w * h];
                let rows = crate::smp::Disjoint(indices.as_mut_ptr());
                let pixel = crate::smp::Disjoint(pixel);
                crate::smp::run(h, &|y| {
                    // SAFETY: each job writes its own line of `indices`.
                    let line = unsafe { core::slice::from_raw_parts_mut(rows.get().add(y * w), w) };
                    for (x, out) in line.iter_mut().enumerate() {
                        *out = beenet::gif::color_index(pixel.get()(x, y));
                    }
                });
                (w, h, indices)
            });
            let Some((w, h, indices)) = shot else {
                error("record: needs a graphics screen");
                return;
            };
            writer.get_or_insert_with(|| beenet::gif::Writer::new(w, h)).frame_indices(FRAME_MS as u32, indices);
            taken += 1;
            if writer.as_ref().is_some_and(|g| g.len() > MAX_BYTES) {
                break;
            }
            // Until the next picture: Ctrl+C stops (other keys are ignored).
            let due = start + (i + 1) * FRAME_MS;
            while crate::time::uptime_ms() < due && !stopped {
                crate::thread::pause(|| crate::input::pending() || crate::time::uptime_ms() >= due);
                stopped = crate::input::poll_key() == Some(crate::input::Key::Ctrl('c'));
            }
            if stopped {
                break;
            }
        }
        let Some(writer) = writer else { return };
        let frames = writer.frames;
        let data = writer.finish();
        let seconds = (crate::time::uptime_ms() - start).div_ceil(1000);
        match self.storage.fs().write_file(path, &data) {
            Ok(()) => println!("recorded {taken} pictures in {seconds} s ({frames} different) to {path} ({} KiB)", data.len().div_ceil(1024)),
            Err(e) => error(&format!("record {file}: {e}")),
        }
    }

    /// `record FILE.avi`: ten pictures of the screen a second, each a JPEG
    /// made on every processor, into a Motion-JPEG video. The time per frame
    /// in the file is the one really measured, so it plays at the right pace
    /// even if a picture took longer than a tenth of a second.
    /// Pictures from the camera: `each` gets every new one until it says
    /// stop (false), Ctrl+C is pressed, the camera fails, or no picture
    /// comes for 5 seconds. The camera is switched off afterwards.
    fn camera_pictures(&mut self, size: (u16, u16), mut each: impl FnMut(crate::usb::Picture) -> bool) -> Result<(), String> {
        use crate::usb::{CameraState, CAMERAS, CAMERA_PICTURE, CAMERA_STATE, CAMERA_WANT};
        use crate::{input, thread, time};
        if CAMERAS.get().is_empty() {
            return Err(String::from("no camera plugged in ('lsusb' lists the devices)"));
        }
        *CAMERA_STATE.get() = CameraState::Off;
        *CAMERA_PICTURE.get() = None;
        *CAMERA_WANT.get() = Some(size);
        let mut last = 0;
        let mut deadline = time::uptime_ms() + 5000;
        let result = loop {
            thread::pause(|| {
                input::pending()
                    || time::uptime_ms() >= deadline
                    || CAMERA_PICTURE.get().as_ref().is_some_and(|p| p.seq != last)
                    || matches!(*CAMERA_STATE.get(), CameraState::Failed(_))
            });
            if input::poll_key() == Some(input::Key::Ctrl('c')) {
                break Err(String::from("stopped"));
            }
            if let CameraState::Failed(why) = CAMERA_STATE.get().clone() {
                break Err(why);
            }
            let picture = CAMERA_PICTURE.get().as_ref().filter(|p| p.seq != last).cloned();
            match picture {
                Some(picture) => {
                    last = picture.seq;
                    deadline = time::uptime_ms() + 5000;
                    if !each(picture) {
                        break Ok(());
                    }
                }
                None if time::uptime_ms() >= deadline => break Err(String::from("the camera sent no picture for 5 seconds")),
                None => {}
            }
        };
        *CAMERA_WANT.get() = None;
        result
    }

    /// `camera on` / `camera off`: leave the camera sending (to watch what
    /// arrives with `camera`), or stop it.
    fn camera_switch(&mut self, size: Option<(u16, u16)>) {
        use crate::usb::{CameraState, CAMERAS, CAMERA_STATE, CAMERA_WANT};
        if size.is_some() && CAMERAS.get().is_empty() {
            error("camera: no camera plugged in ('lsusb' lists the devices)");
            return;
        }
        if size.is_some() {
            *CAMERA_STATE.get() = CameraState::Off;
        }
        *CAMERA_WANT.get() = size;
        let until = crate::time::uptime_ms() + 3000;
        crate::thread::pause(|| {
            let state = CAMERA_STATE.get();
            crate::time::uptime_ms() >= until
                || match size {
                    Some(_) => *state != CameraState::Off,
                    None => *state == CameraState::Off,
                }
        });
        match CAMERA_STATE.get().clone() {
            CameraState::On(what) => println!("camera on: {what}"),
            CameraState::Failed(why) => error(&format!("camera: {why}")),
            CameraState::Off if size.is_none() => println!("camera off"),
            CameraState::Off => error("camera: the usb thread did not answer"),
        }
    }

    /// `camera watch`: a picture saved in `folder` whenever something moves
    /// (at most one a second), named after the local time; until Ctrl+C or
    /// the minutes are over.
    fn camera_watch(&mut self, folder: &str, minutes: Option<u64>) {
        let dir = self.abs(folder);
        if !self.storage.fs().is_dir(&dir) {
            error(&format!("camera watch: {folder}: no such folder"));
            return;
        }
        let probe = format!("{}/.watch", dir.trim_end_matches('/'));
        if !self.may_change(&probe) {
            return;
        }
        let start = crate::time::uptime_ms();
        let end = minutes.map(|m| start + m * 60_000);
        println!("watching: a picture goes to {dir} when something moves (Ctrl+C stops)");
        let mut detector = beenet::motion::Detector::new();
        let (mut pictures, mut saved, mut last_save) = (0u32, 0u32, 0u64);
        let Some(mut storage) = crate::storage::handle() else { return };
        let result = self.camera_pictures((640, 480), |picture| {
            let now = crate::time::uptime_ms();
            pictures += 1;
            if let Some(img) = crate::webcam::pixels(&picture) {
                let moved = detector.feed(&beenet::motion::cells(&img.pixels, img.width, img.height));
                if beenet::motion::is_movement(moved) && now >= last_save + 1000 {
                    last_save = now;
                    let stamp = match crate::time::now_unix() {
                        Some(unix) => crate::time::local(crate::time::zone(), unix)[..19].replace(' ', "_").replace(':', "-"),
                        None => format!("up-{}", now / 1000),
                    };
                    let path = format!("{}/{stamp}.jpg", dir.trim_end_matches('/'));
                    match storage.fs().write_file(&path, &crate::webcam::as_jpeg(&picture, &img)) {
                        Ok(()) => {
                            saved += 1;
                            println!("{} movement ({}.{}% of the picture): {path}", &stamp[stamp.len().saturating_sub(8)..], moved / 10, moved % 10);
                        }
                        Err(e) => {
                            error(&format!("camera watch: {path}: {e}"));
                            return false;
                        }
                    }
                }
            }
            end.is_none_or(|end| now < end)
        });
        if let Err(why) = result {
            if why != "stopped" {
                error(&format!("camera: {why}"));
            }
        }
        let secs = (crate::time::uptime_ms() - start) / 1000;
        println!("watched for {} ({pictures} pictures): {saved} saved", crate::time::format_duration(secs * 1000));
    }

    fn camera_snap(&mut self, file: &str, size: (u16, u16)) {
        let path = self.abs(file);
        if !self.may_change(&path) {
            return;
        }
        // The first pictures are dark while the camera finds its exposure.
        const SKIP: u32 = 10;
        let mut got: Option<(crate::usb::Picture, crate::image::Image)> = None;
        let mut seen = 0;
        let result = self.camera_pictures(size, |picture| {
            seen += 1;
            if seen <= SKIP {
                return true;
            }
            match crate::webcam::pixels(&picture) {
                Some(image) => {
                    got = Some((picture, image));
                    false
                }
                None => seen < SKIP + 30, // damaged: try the next one
            }
        });
        if let Err(why) = result {
            error(&format!("camera: {why}"));
            return;
        }
        let Some((picture, image)) = got else {
            error("camera: no undamaged picture came");
            return;
        };
        let name = path.to_ascii_lowercase();
        let (w, h) = (image.width, image.height);
        let pixel = |x: usize, y: usize| image.pixels[y * w + x];
        let data = if name.ends_with(".png") {
            crate::image::encode_png(w, h, &pixel)
        } else if name.ends_with(".bmp") {
            crate::image::encode_bmp(w, h, pixel)
        } else {
            crate::webcam::as_jpeg(&picture, &image)
        };
        match self.storage.fs().write_file(&path, &data) {
            Ok(()) => println!("saved a {w} x {h} picture from the camera to {path} ({} KiB)", data.len().div_ceil(1024)),
            Err(e) => error(&format!("camera snap {file}: {e}")),
        }
    }

    fn camera_record(&mut self, file: &str, secs: u32) {
        const MAX_BYTES: usize = 256 * 1024 * 1024;
        let path = self.abs(file);
        if !self.may_change(&path) {
            return;
        }
        let mut writer: Option<beenet::avi::Writer> = None;
        let mut start = 0;
        let mut end = 0;
        let result = self.camera_pictures((640, 480), |picture| {
            let now = crate::time::uptime_ms();
            let jpeg = match picture.kind {
                beenet::uvc::Kind::Mjpeg => beenet::jpeg::with_tables(&picture.data),
                _ => match crate::webcam::pixels(&picture) {
                    Some(image) => {
                        let w = image.width;
                        crate::image::encode_jpeg(image.width, image.height, 80, &|x, y| image.pixels[y * w + x])
                    }
                    None => return true,
                },
            };
            let video = writer.get_or_insert_with(|| {
                start = now;
                beenet::avi::Writer::new(picture.width as usize, picture.height as usize, 33_333)
            });
            video.frame(&jpeg);
            end = now;
            video.len() < MAX_BYTES && now < start + secs as u64 * 1000
        });
        if let Err(why) = &result {
            if why != "stopped" || writer.is_none() {
                error(&format!("camera: {why}"));
                if writer.is_none() {
                    return;
                }
            }
        }
        let Some(mut video) = writer else { return };
        let frames = video.frames();
        let ms = (end - start).max(1);
        if frames > 1 {
            video.set_frame_us((ms * 1000 / (frames as u64 - 1)) as u32);
        }
        let data = video.finish();
        match self.storage.fs().write_file(&path, &data) {
            Ok(()) => println!(
                "filmed {frames} pictures in {}.{} s to {path} ({} KiB)",
                ms / 1000,
                ms % 1000 / 100,
                data.len().div_ceil(1024)
            ),
            Err(e) => error(&format!("camera record {file}: {e}")),
        }
    }

    fn record_avi(&mut self, path: &str, file: &str, secs: u32) {
        const FRAME_MS: u64 = 100;
        const MAX_BYTES: usize = 128 * 1024 * 1024;
        let mut writer: Option<beenet::avi::Writer> = None;
        let start = crate::time::uptime_ms();
        let mut stopped = false;
        for i in 0..secs as u64 * 1000 / FRAME_MS {
            let shot = console::with_screen(|w, h, pixel| {
                let pixel = crate::smp::Disjoint(pixel);
                (w, h, crate::image::encode_jpeg(w, h, 75, &|x, y| pixel.get()(x, y)))
            });
            let Some((w, h, jpeg)) = shot else {
                error("record: needs a graphics screen");
                return;
            };
            let video = writer.get_or_insert_with(|| beenet::avi::Writer::new(w, h, FRAME_MS as u32 * 1000));
            video.frame(&jpeg);
            if video.len() > MAX_BYTES {
                break;
            }
            // Until the next picture (Ctrl+C stops; other keys are ignored).
            let due = start + (i + 1) * FRAME_MS;
            while crate::time::uptime_ms() < due && !stopped {
                crate::thread::pause(|| crate::input::pending() || crate::time::uptime_ms() >= due);
                stopped = crate::input::poll_key() == Some(crate::input::Key::Ctrl('c'));
            }
            if stopped {
                break;
            }
        }
        let Some(mut video) = writer else { return };
        let ms = crate::time::uptime_ms() - start;
        let frames = video.frames();
        video.set_frame_us((ms * 1000 / frames.max(1) as u64) as u32);
        let data = video.finish();
        match self.storage.fs().write_file(path, &data) {
            Ok(()) => println!(
                "recorded {frames} frames in {}.{} s ({} a second) to {path} ({} KiB)",
                ms / 1000,
                ms % 1000 / 100,
                frames as u64 * 1000 / ms.max(1),
                data.len().div_ceil(1024)
            ),
            Err(e) => error(&format!("record {file}: {e}")),
        }
    }

    /// At boot: the saved wallpaper, if its picture is still there.
    pub fn load_wallpaper(&mut self) {
        let data = self.storage.fs().read_file(WALLPAPER_FILE).ok();
        let Some(path) = data.as_deref().and_then(|d| core::str::from_utf8(d).ok()).map(str::trim).map(String::from) else { return };
        match crate::view::load(&mut self.storage, &path) {
            Ok(picture) => {
                let _ = console::set_wallpaper(Some(picture));
            }
            Err(why) => println!("[desk] wallpaper {path}: {why}"),
        }
    }

    /// At boot: the saved time zone (UTC without one).
    pub fn load_timezone(&mut self) {
        let data = self.storage.fs().read_file(TIMEZONE_FILE).ok();
        let zone = data.as_deref().and_then(|d| core::str::from_utf8(d).ok()).and_then(time::parse_zone);
        time::set_zone(zone.unwrap_or(time::Zone::Utc));
    }

    /// At boot: start the other processors, unless that was switched off.
    pub fn load_cpus(&mut self) {
        let data = self.storage.fs().read_file(CPU_FILE).ok();
        let saved = data.as_deref().and_then(|d| core::str::from_utf8(d).ok());
        if crate::cpu::processors().len() < 2 {
            return;
        }
        if !crate::smp::choice(crate::bootargs::cmdline(), saved) {
            println!("[cpu]  1 of {} processors in use ('cpu auto on' starts the others at boot)", crate::cpu::processors().len());
            return;
        }
        match crate::smp::start() {
            Ok(running) => println!("[cpu]  {running} of {} processors in use ('cpu auto off' to use one)", crate::cpu::processors().len()),
            Err(why) => println!("[cpu]  only 1 processor in use: {why}"),
        }
    }

    /// `cpu auto on|off`: start the other processors at boot or not (saved).
    fn cpu_auto(&mut self, state: &str) {
        let mut fs = self.storage.fs();
        let saved = fs.mkdir_all("/system").and_then(|_| fs.write_file(CPU_FILE, format!("auto {state}
").as_bytes()));
        drop(fs);
        let note = match (saved, self.storage.persistent) {
            (Ok(()), true) => "saved",
            (Ok(()), false) => "RAM disk: not kept after reboot",
            (Err(_), _) => "could not save",
        };
        match state {
            "on" => println!("the other processors start at boot ({note})"),
            _ => println!("only one processor at the next boot ({note})"),
        }
    }

    /// `desktop on` / `desktop off`: switch, and remember it for the next boot.
    fn desktop(&mut self, state: &str) {
        if let Err(e) = console::set_desktop(state == "on") {
            return error(&format!("desktop {state}: {e}"));
        }
        let mut fs = self.storage.fs();
        let saved = fs.mkdir_all("/system").and_then(|_| fs.write_file(DESKTOP_FILE, format!("{state}\n").as_bytes()));
        drop(fs);
        let note = match (saved, self.storage.persistent) {
            (Ok(()), true) => " (saved)",
            (Ok(()), false) => " (RAM disk: not kept after reboot)",
            (Err(_), _) => " (could not save)",
        };
        println!("desktop {state}{note}");
    }

    pub fn run(&mut self) -> ! {
        if !users::load(&mut self.storage).is_empty() {
            self.login_at_console();
        }
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
        for (state, why) in aibridge::take_news() {
            // Never the address: the event log and the screen get the state only.
            let text = match (state, why.is_empty()) {
                ("connected", _) => String::from("connected to beehive over the network"),
                (_, true) => String::from("the connection to beehive ended"),
                (_, false) => format!("the connection to beehive ended: {why}"),
            };
            println!("[hive] {text}");
            let ev = eventlog::event("hive").str("state", state).finish();
            eventlog::write(&mut self.storage.fs(), ev);
        }
        let (logins, failures) = remote::take_news();
        for (count, state, text) in
            [(logins, "login", "login to the remote shell"), (failures, "refused", "wrong password on the remote shell")]
        {
            if count > 0 {
                let times = if count == 1 { String::new() } else { format!(" ({count} times)") };
                println!("[remote] {text}{times}");
                let ev = eventlog::event("remote").str("state", state).num("count", count as i64).finish();
                eventlog::write(&mut self.storage.fs(), ev);
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
            if !self.may_change(&path) {
                return;
            }
            let mut fs = self.storage.fs();
            let r = if append { fs.append(&path, text.as_bytes()) } else { fs.write_file(&path, text.as_bytes()) };
            return report(r);
        }

        if users::needs_admin(&cmd, &a) && !self.is_admin() {
            let who = self.user.as_ref().map_or(String::from("a remote session"), |u| u.name.clone());
            return error(&format!("{cmd}: only for admins ({who} is not one; see whoami)"));
        }

        match (cmd.as_str(), a.as_slice()) {
            ("help", _) => help(),
            ("whoami", []) => self.whoami(),
            ("users", []) => {
                for user in users::load(&mut self.storage) {
                    println!("{:<16} {}", user.name, user.groups.join(","));
                }
            }
            ("useradd", [name]) => self.useradd(name, false),
            ("useradd", [name, "admin"]) => self.useradd(name, true),
            ("useradd", _) => error("usage: useradd NAME [admin]"),
            ("userdel", [name]) => self.userdel(name),
            ("userdel", _) => error("usage: userdel NAME"),
            ("passwd", []) => self.passwd(None),
            ("passwd", [name]) => self.passwd(Some(name)),
            ("passwd", _) => error("usage: passwd [NAME]"),
            ("login", []) if session::current().is_none() => self.login_at_console(),
            ("logout", []) if session::current().is_none() => {
                if users::load(&mut self.storage).is_empty() {
                    return error("logout: this machine has no accounts (see useradd)");
                }
                println!("logged out");
                self.user = None;
                users::set_console_user(None);
                self.login_at_console();
            }
            ("login" | "logout", []) => error("login and logout are for the machine's own console; here: exit"),
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
            ("head" | "tail", args) => match textops::parse_count(args) {
                Some((n, file)) => self.head_tail(file, n, cmd == "tail"),
                None => error(&format!("usage: {cmd} [-n N] FILE   (the {} 10 lines unless N is given)", if cmd == "tail" { "last" } else { "first" })),
            },
            ("wc", [file]) => {
                let Some(text) = self.read_text(file) else { return };
                let (lines, words, bytes) = textops::count(&text);
                println!("{lines} lines, {words} words, {bytes} bytes  {file}");
            }
            ("wc", _) => error("usage: wc FILE"),
            ("grep", [pattern, path]) => self.grep(pattern, path, false),
            ("grep", ["-i", pattern, path]) => self.grep(pattern, path, true),
            ("grep", _) => error("usage: grep [-i] TEXT FILE|FOLDER   (-i: upper and lower case alike)"),
            ("touch", [file]) => self.touch(file),
            ("mkdir", [dir]) => {
                let dir = self.abs(dir);
                if self.may_change(&dir) {
                    let r = self.storage.fs().mkdir_all(&dir);
                    report(r)
                }
            }
            ("rm" | "del" | "rmdir", [path]) => {
                let path = self.abs(path);
                if self.may_change(&path) {
                    let r = self.storage.fs().remove(&path);
                    report(r)
                }
            }
            ("mv" | "move", [from, to]) => {
                let (from, to) = (self.abs(from), self.abs_target(from, to));
                if self.may_change(&from) && self.may_change(&to) {
                    let r = self.storage.fs().rename(&from, &to);
                    report(r)
                }
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
            ("cpu", []) => {
                let processors = crate::cpu::processors();
                println!("processor: {}", crate::cpu::name());
                match crate::smp::running() {
                    1 if processors.len() > 1 => println!("count: {}, 1 in use ('cpu start' starts the others)", processors.len()),
                    running => println!("count: {}, {running} in use", processors.len()),
                }
                println!("clock: {} MHz", time::tsc_mhz());
                println!("can do: {}", crate::cpu::features(crate::cpu::raw()).join(" "));
            }
            ("cpu", ["start"]) => match crate::smp::start() {
                Ok(running) => println!("{running} of {} processors in use", crate::cpu::processors().len()),
                Err(why) => error(&format!("cpu start: {why}")),
            },
            ("cpu", ["test"]) => self.cpu_test(),
            ("cpu", ["auto", state @ ("on" | "off")]) => self.cpu_auto(state),
            ("cpu", _) => error("usage: cpu [start | test | auto on|off]"),
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
                Some(t) if time::zone() == time::Zone::Utc => println!("{}", time::iso8601(t)),
                Some(t) => println!("{}   ({})", time::local(time::zone(), t), time::iso8601(t)),
                None => error("no real-time clock"),
            },
            ("timezone", []) => {
                let zone = time::zone();
                println!("time zone: {} ('timezone UTC|CET|+HH:MM' to change)", time::zone_name(zone));
            }
            ("timezone", [name]) => match time::parse_zone(name) {
                Some(zone) => {
                    time::set_zone(zone);
                    let mut fs = self.storage.fs();
                    let saved = fs.mkdir_all("/system").and_then(|_| fs.write_file(TIMEZONE_FILE, format!("{}\n", time::zone_name(zone)).as_bytes()));
                    drop(fs);
                    let note = match (saved, self.storage.persistent) {
                        (Ok(()), true) => "saved",
                        (Ok(()), false) => "RAM disk: not kept after reboot",
                        (Err(_), _) => "could not save",
                    };
                    println!("time zone: {} ({note})", time::zone_name(zone));
                    crate::clock::refresh();
                }
                None => error("usage: timezone UTC | CET (Central Europe, with summer time) | +HH:MM | -HH:MM"),
            },
            ("timezone", _) => error("usage: timezone UTC | CET (Central Europe, with summer time) | +HH:MM | -HH:MM"),
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
            ("do", []) => error("usage: do TASK   (e.g. do make a folder photos with a file readme.txt in it)"),
            ("do", words) => self.do_task(&words.join(" ")),
            ("explain", []) => self.explain(),
            ("ai", []) => self.ai_status(),
            ("ai", [s @ ("on" | "off")]) => self.ai_switch(s),
            ("ai", ["connect", target, name, key]) => self.ai_connect(target, name, key),
            ("ai", ["disconnect"]) => self.ai_disconnect(),
            ("ai", ["control"]) => println!("remote control by the AI host: {}", if aibridge::control() { "on" } else { "off ('ai control on' allows it)" }),
            ("ai", ["control", state @ ("on" | "off")]) => self.switch("ai control", CONTROL_FILE, state),
            ("update", []) => self.update(),
            ("update", ["auto"]) => println!("automatic updates: {}", if update::auto() { "on" } else { "off ('update auto on' allows them)" }),
            ("update", ["auto", state @ ("on" | "off")]) => self.switch("update auto", UPDATE_FILE, state),
            ("update", _) => error("usage: update [auto [on | off]]"),
            ("ai", ["connect", ..]) => error("usage: ai connect HOST[:PORT] NAME KEY   (as printed by: beehive.py pair NAME)"),
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
            ("lspci", options @ ([] | ["-v"])) => {
                for d in pci::scan() {
                    let irq = if d.irq != 0 && d.irq != 0xFF { format!(" (IRQ {})", d.irq) } else { String::new() };
                    println!("{:02x}:{:02x}.{} {:04x}:{:04x} {}{irq}", d.bus, d.dev, d.func, d.vendor, d.device, d.class_name());
                    if options.is_empty() {
                        continue;
                    }
                    // What the device can do, for the drivers still to write.
                    let names: Vec<String> = d
                        .capabilities()
                        .iter()
                        .map(|&(id, _)| pci::capability_name(id).map(String::from).unwrap_or_else(|| format!("0x{id:02x}")))
                        .collect();
                    if !names.is_empty() {
                        println!("        {}", names.join(", "));
                    }
                }
            }
            ("net" | "ifconfig", []) => match net::describe() {
                Some(text) => println!("{text}"),
                None => println!("no network card (supported: Intel e1000, e1000e, I217; see lspci)"),
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
            ("remote", []) => self.remote_status(),
            ("remote", ["on"]) => self.remote_command(Some(Some(remote::DEFAULT_PORT))),
            ("remote", ["on", port]) => match port.parse::<u16>() {
                Ok(p) if p != 0 => self.remote_command(Some(Some(p))),
                _ => error("usage: remote on [PORT]   (port 1-65535)"),
            },
            ("remote", ["off"]) => self.remote_command(Some(None)),
            ("remote", ["password"]) => self.remote_command(None),
            ("remote", _) => error("usage: remote [on [PORT] | off | password]"),
            ("vnc", []) => self.vnc_status(),
            ("vnc", ["on"]) => self.vnc_command(Some(Some(crate::vnc::DEFAULT_PORT))),
            ("vnc", ["on", port]) => match port.parse::<u16>() {
                Ok(p) if p != 0 => self.vnc_command(Some(Some(p))),
                _ => error("usage: vnc [on [PORT] | off | password]"),
            },
            ("vnc", ["off"]) => self.vnc_command(Some(None)),
            ("vnc", ["password"]) => self.vnc_command(None),
            ("vnc", _) => error("usage: vnc [on [PORT] | off | password]"),
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
            ("display", []) => println!("{}", console::describe_display()),
            ("display", ["mode", size]) => match display::parse_mode(size) {
                Some((width, height)) => match console::set_display_mode(width, height) {
                    Ok(()) => {
                        let note = self.save_display("mode", size);
                        println!("{}\n({note})", console::describe_display())
                    }
                    Err(e) => error(&format!("display mode {size}: {}", e.message())),
                },
                None => error(&format!("display mode {size}: {}", display::DisplayError::BadSize.message())),
            },
            ("display", ["test"]) if display::current().is_none() => {
                error("display test: needs a graphics mode, and this is the text screen")
            }
            ("display", ["test"]) => {
                // The text goes out first: the bars then cover the whole screen.
                println!("color bars on the screen: press Enter to go on");
                console::show_color_bars();
                // Ctrl+D is also what a closed remote session gives: never wait for more.
                use crate::input::Key;
                while !matches!(crate::input::read_key(), Key::Enter | Key::Ctrl('c' | 'd')) {}
                let (micros, _) = console::redraw();
                println!("drawing the whole screen again took {}.{} ms", micros / 1000, micros % 1000 / 100);
            }
            ("display", ["speed"]) if display::current().is_none() => {
                error("display speed: needs a graphics mode, and this is the text screen")
            }
            ("display", ["speed"]) => {
                // Unlike `display test` this waits for no key: it can be asked from afar.
                let (best, copy) = (0..5).map(|_| console::redraw()).min().unwrap_or((0, 0));
                println!("drawing the whole screen again takes {}.{} ms (best of 5)", best / 1000, best % 1000 / 100);
                println!("of which copying to video memory: {}.{} ms", copy / 1000, copy % 1000 / 100);
            }
            ("display", ["font", size]) => match display::parse_font(size) {
                Some(_) if display::current().is_none() => error("display font: needs a graphics mode, and this is the text screen"),
                Some(font) => {
                    console::set_font(font);
                    let note = self.save_display("font", size);
                    println!("{}\n({note})", console::describe_display())
                }
                None => error("usage: display font 1|2|auto   (1: 8x16 pixels per character, 2: 16x32)"),
            },
            ("display", _) => error("usage: display [mode WIDTHxHEIGHT | font 1|2|auto | test | speed]"),
            ("desktop", []) if console::desktop_on() => {
                let (cols, rows) = console::size();
                println!("desktop: on, the terminal window has {cols}x{rows} characters ('desktop off' ends it)");
                if let Some((x, y)) = console::pointer() {
                    println!("mouse pointer at {x},{y}");
                }
            }
            ("desktop", []) => println!("desktop: off ('desktop on' starts it)"),
            ("sleep", [seconds]) => match seconds.parse::<u64>() {
                Ok(seconds @ 0..=3600) => {
                    // Ctrl+C ends it early.
                    let end = time::uptime_ms() + seconds * 1000;
                    while time::uptime_ms() < end && !net::cancelled() {
                        thread::sleep_ms(50);
                    }
                }
                _ => error("usage: sleep SECONDS   (0 to 3600)"),
            },
            ("sleep", _) => error("usage: sleep SECONDS   (0 to 3600)"),
            ("edit", [file]) => {
                let path = self.abs(file);
                match crate::desk::open_editor(&path) {
                    Ok(_) => println!("editing {path} in a new window: Ctrl+S saves, Ctrl+Q leaves"),
                    Err(why) => error(&format!("edit {file}: {why}")),
                }
            }
            ("edit", _) => error("usage: edit FILE"),
            ("files", args @ ([] | [_])) => {
                let folder = self.abs(args.first().copied().unwrap_or("."));
                match crate::desk::open_files(&folder) {
                    Ok(_) => println!("{folder} in a new window: arrows choose, Enter opens, q leaves"),
                    Err(why) => error(&format!("files {folder}: {why}")),
                }
            }
            ("files", _) => error("usage: files [FOLDER]"),
            ("browse", [address]) if { let path = self.abs(address); self.storage.fs().exists(&path) } => {
                // A file that is there is shown from the disk.
                let path = self.abs(address);
                match crate::desk::open_web(&path) {
                    Ok(_) => println!("{path} in a new window: Tab picks a link, Enter follows it, q leaves"),
                    Err(why) => error(&format!("browse {path}: {why}")),
                }
            }
            ("browse", words @ [_, ..]) => {
                // A web address, or words to search for.
                let typed = words.join(" ");
                match crate::web::typed_address(&typed).map(|t| (crate::desk::open_web(&t), t)) {
                    Ok((Ok(_), t)) => println!("{t} in a new window: Tab picks a link, Enter follows it, q leaves"),
                    Ok((Err(why), t)) => error(&format!("browse {t}: {why}")),
                    Err(why) => error(&format!("browse {typed}: {why}")),
                }
            }
            ("browse", _) => error("usage: browse URL|FILE|WORDS   (words: a search on FrogFind)"),
            ("view", [file]) => {
                let path = self.abs(file);
                match crate::desk::open_picture(&path) {
                    Ok((_, w, h, 1)) => println!("{path} ({w} x {h} pixels) in a new window: q leaves"),
                    Ok((_, w, h, n)) => println!("{path} ({w} x {h} pixels, {n} frames) in a new window: space pauses, n steps, q leaves"),
                    Err(why) => error(&format!("view {file}: {why}")),
                }
            }
            ("view", _) => error("usage: view FILE   (a JPEG, PNG, GIF, BMP or PPM picture; an animated GIF plays)"),
            ("screenshot", [file]) => {
                let path = self.abs(file);
                if !self.may_change(&path) {
                    return;
                }
                // PNG or GIF when the name says so, BMP otherwise.
                let name = path.to_ascii_lowercase();
                let picture = console::with_screen(|w, h, pixel| {
                    let data = if name.ends_with(".jpg") || name.ends_with(".jpeg") {
                        let pixel = crate::smp::Disjoint(pixel);
                        crate::image::encode_jpeg(w, h, 90, &|x, y| pixel.get()(x, y))
                    } else if name.ends_with(".png") {
                        let pixel = crate::smp::Disjoint(pixel);
                        crate::image::encode_png(w, h, &|x, y| pixel.get()(x, y))
                    } else if name.ends_with(".gif") {
                        let mut gif = beenet::gif::Writer::new(w, h);
                        gif.frame(100, pixel);
                        gif.finish()
                    } else {
                        crate::image::encode_bmp(w, h, pixel)
                    };
                    (w, h, data)
                });
                match picture {
                    Some((w, h, bmp)) => {
                        let saved = self.storage.fs().write_file(&path, &bmp);
                        match saved {
                            Ok(()) => println!("saved the screen ({w} x {h}) to {path} ({} KiB)", bmp.len() / 1024),
                            Err(e) => error(&format!("screenshot {file}: {e}")),
                        }
                    }
                    None => error("screenshot: needs a graphics screen"),
                }
            }
            ("screenshot", _) => error("usage: screenshot FILE   (saves the screen as a picture: JPEG, PNG or GIF by the ending, BMP otherwise)"),
            ("record", [file, secs]) => {
                let path = self.abs(file);
                match secs.parse::<u32>() {
                    Ok(secs @ 1..=60) if path.to_ascii_lowercase().ends_with(".gif") => {
                        if self.may_change(&path) {
                            self.record(&path, file, secs);
                        }
                    }
                    Ok(secs @ 1..=60) if path.to_ascii_lowercase().ends_with(".avi") => {
                        if self.may_change(&path) {
                            self.record_avi(&path, file, secs);
                        }
                    }
                    _ => error("usage: record FILE.gif|FILE.avi SECONDS   (1 to 60; Ctrl+C stops early)"),
                }
            }
            ("sound", []) => match crate::hda::describe() {
                Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
                Err(why) => error(&format!("sound: {why}")),
            },
            ("sound", ["volume", n]) => match n.trim_end_matches('%').parse::<u32>() {
                Ok(v @ 0..=100) => match crate::hda::set_volume(v) {
                    Ok(()) => println!("volume {v} %"),
                    Err(why) => error(&format!("sound: {why}")),
                },
                _ => error("usage: sound volume 0-100"),
            },
            ("sound", _) => error("usage: sound [volume 0-100]"),
            ("beep", args) if args.len() <= 2 => {
                let hz = args.first().map_or(Some(880), |w| crate::sound::frequency(w));
                let ms = args.get(1).map_or(Some(300), |w| w.parse::<u32>().ok().filter(|ms| (1..=10_000).contains(ms)));
                match (hz, ms) {
                    (Some(hz), Some(ms)) => self.play_frames(&crate::sound::tone(hz, ms, 50), "beep"),
                    _ => error("usage: beep [HZ|NOTE [MS]]   (e.g. beep 440 500, beep c5; 20-20000 Hz, at most 10000 ms)"),
                }
            }
            ("beep", _) => error("usage: beep [HZ|NOTE [MS]]   (e.g. beep 440 500, beep c5; 20-20000 Hz, at most 10000 ms)"),
            ("tune", ["save", file, notes @ ..]) if !notes.is_empty() => {
                let path = self.abs(file);
                if !path.to_ascii_lowercase().ends_with(".wav") {
                    error("usage: tune save FILE.wav NOTE[:MS]...");
                    return;
                }
                if !self.may_change(&path) {
                    return;
                }
                match crate::sound::parse_tune(notes) {
                    Ok(tune) => {
                        let wav = crate::sound::encode_wav(&crate::sound::render_tune(&tune, 50));
                        match self.storage.fs().write_file(&path, &wav) {
                            Ok(()) => println!("saved {} notes to {path} ({} KiB)", tune.len(), wav.len().div_ceil(1024)),
                            Err(e) => error(&format!("tune: {e}")),
                        }
                    }
                    Err(why) => error(&format!("tune: {why}")),
                }
            }
            ("tune", notes) if !notes.is_empty() && notes[0] != "save" => match crate::sound::parse_tune(notes) {
                Ok(tune) => self.play_frames(&crate::sound::render_tune(&tune, 50), "tune"),
                Err(why) => error(&format!("tune: {why}")),
            },
            ("tune", _) => error("usage: tune NOTE[:MS]...  or  tune save FILE.wav NOTE[:MS]...   (e.g. tune c5 e5 g5 c6:500; r is a rest)"),
            ("play", [file]) if file.to_ascii_lowercase().ends_with(".avi") => {
                let path = self.abs(file);
                match crate::desk::open_video(&path) {
                    Ok((_, w, h, n, sound)) => println!("{path} ({w} x {h}, {n} frames{}) in a new window: space pauses, q leaves", if sound { ", with sound" } else { "" }),
                    Err(why) => error(&format!("play {file}: {why}")),
                }
            }
            ("play", [file]) => {
                let path = self.abs(file);
                let data = self.storage.fs().read_file(&path);
                match data.map_err(|e| format!("{e}")).and_then(|d| crate::sound::parse_wav(&d).map_err(String::from)) {
                    Ok(wav) => {
                        println!("{path}: {} Hz, {} bits, {}, {} (Ctrl+C stops)", wav.rate, wav.bits, if wav.channels == 1 { "mono" } else { "stereo" }, wav.duration_text());
                        self.play_frames(&wav.to_48k(), "play");
                    }
                    Err(why) => error(&format!("play {file}: {why}")),
                }
            }
            ("play", _) => error("usage: play FILE.wav|FILE.avi"),
            ("record", _) => error("usage: record FILE.gif|FILE.avi SECONDS   (1 to 60; Ctrl+C stops early)"),
            ("monitor", []) => match crate::desk::open_monitor() {
                Ok(_) => println!("the system monitor in a new window: q leaves"),
                Err(why) => error(&format!("monitor: {why}")),
            },
            ("monitor", _) => error("usage: monitor"),
            ("terminal", []) => match crate::desk::open_terminal() {
                Ok(_) => println!("a new terminal window ({} open): Alt+Tab or a click changes window, 'exit' or its close box ends it", crate::desk::count()),
                Err(why) => error(&format!("terminal: {why}")),
            },
            ("terminal", _) => error("usage: terminal"),
            ("desktop", [state @ ("on" | "off")]) => self.desktop(state),
            ("desktop", ["wallpaper", "none"]) => {
                let _ = console::set_wallpaper(None);
                // No saved wallpaper is fine too.
                let _ = self.storage.fs().remove(WALLPAPER_FILE);
                println!("wallpaper: none, the plain color");
            }
            ("desktop", ["wallpaper", file]) => {
                let path = self.abs(file);
                let picture = crate::view::load(&mut self.storage, &path);
                match picture.map(|p| (p.width, p.height, console::set_wallpaper(Some(p)))) {
                    Ok((w, h, Ok(()))) => {
                        let mut fs = self.storage.fs();
                        let saved = fs.mkdir_all("/system").and_then(|_| fs.write_file(WALLPAPER_FILE, format!("{path}\n").as_bytes()));
                        drop(fs);
                        let note = match (saved, self.storage.persistent) {
                            (Ok(()), true) => "saved",
                            (Ok(()), false) => "RAM disk: not kept after reboot",
                            (Err(_), _) => "could not save",
                        };
                        println!("wallpaper: {path} ({w} x {h}, scaled to cover the screen; {note})");
                    }
                    Ok((_, _, Err(why))) => error(&format!("desktop wallpaper: {why}")),
                    Err(why) => error(&format!("desktop wallpaper {file}: {why}")),
                }
            }
            ("desktop", _) => error("usage: desktop [on | off | wallpaper FILE|none]"),
            ("install", []) => self.install_list(),
            ("install", [disk]) => self.install(disk, ""),
            ("install", [disk, option @ ("--erase" | "--reinstall")]) => self.install(disk, option),
            ("install", _) => error("usage: install [DISK [--reinstall | --erase]]   (install alone lists the disks)"),
            ("lsusb", []) => println!("{}", crate::usb::describe()),
            ("camera", []) => {
                println!("{}", crate::usb::describe_cameras());
                match crate::usb::CAMERA_STATE.get().clone() {
                    crate::usb::CameraState::On(what) => println!("sending: {what}"),
                    crate::usb::CameraState::Failed(why) => println!("last attempt failed: {why}"),
                    crate::usb::CameraState::Off => {}
                }
                let stats = *crate::usb::CAMERA_STATS.get();
                if stats != crate::usb::CameraStats::default() {
                    println!("{}", stats.describe());
                }
            }
            ("camera", ["on"]) => self.camera_switch(Some((640, 480))),
            ("camera", ["on", size]) => match parse_size(size) {
                Some(size) => self.camera_switch(Some(size)),
                None => error("usage: camera on [WIDTHxHEIGHT]"),
            },
            ("camera", ["off"]) => self.camera_switch(None),
            ("camera", ["watch", folder]) => self.camera_watch(folder, None),
            ("camera", ["watch", folder, minutes]) => match minutes.parse::<u64>() {
                Ok(m @ 1..=1440) => self.camera_watch(folder, Some(m)),
                _ => error("usage: camera watch FOLDER [MINUTES]   (1 to 1440; Ctrl+C stops)"),
            },
            ("camera", ["view"]) => match crate::desk::open_camera() {
                Ok(_) => println!("the camera in a new window: s saves a picture, m mirrors, q leaves"),
                Err(why) => error(&format!("camera: {why}")),
            },
            ("camera", ["log"]) => {
                let lines: Vec<String> = crate::usb::LOG.get().iter().cloned().collect();
                if lines.is_empty() {
                    println!("nothing noted yet");
                }
                for line in lines {
                    println!("{line}");
                }
            }
            ("camera", ["snap", file]) => self.camera_snap(file, (640, 480)),
            ("camera", ["snap", file, size]) => match parse_size(size) {
                Some(size) => self.camera_snap(file, size),
                None => error("usage: camera snap FILE [WIDTHxHEIGHT]"),
            },
            ("camera", ["record", file, secs]) => match secs.parse::<u32>() {
                Ok(secs @ 1..=600) if file.to_ascii_lowercase().ends_with(".avi") => self.camera_record(file, secs),
                _ => error("usage: camera record FILE.avi SECONDS   (1 to 600; Ctrl+C stops early)"),
            },
            ("camera", ["raw"]) => {
                let raw = crate::usb::CAMERAS.get().first().map(|c| c.raw.clone());
                match raw {
                    Some(raw) => {
                        for line in raw.chunks(32) {
                            let hex: String = line.iter().map(|b| alloc::format!("{b:02x}")).collect();
                            println!("{hex}");
                        }
                    }
                    None => error("no camera plugged in"),
                }
            }
            ("slots", []) => match bootslots::describe() {
                Some(text) => println!("{text}"),
                None => println!("not booted from a boot disk with A/B slots (see tools/bootdisk.py)"),
            },
            // A bare `crash` only explains itself: typed by accident (or from
            // afar) it must not stop a machine nobody is sitting at.
            ("crash", ["panic"]) => panic!("'crash' command: deliberate kernel panic (tests crash reports)"),
            // An instruction the CPU refuses: the other way a kernel dies.
            ("crash", ["exception"]) => unsafe { core::arch::asm!("ud2", options(nomem, nostack)) },
            // A thread that never lets the others run (as the first video
            // player did): the watchdog must restart the machine.
            ("crash", ["hang"]) => loop {
                crate::thread::pause(|| true);
            },
            ("crash", _) => error("usage: crash panic|exception|hang (deliberately stops the machine)"),
            ("reboot", []) => {
                self.log_power("reboot");
                power::reboot()
            }
            ("shutdown" | "halt" | "poweroff", []) => {
                self.log_power("shutdown");
                power::shutdown()
            }
            ("exit" | "logout" | "quit", _) if session::current().is_some() => self.leave = true,
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

    fn remote_status(&self) {
        let Some(port) = remote::port() else {
            return println!("remote shell: off (remote on [PORT] starts it)");
        };
        let (logins, failures, connected) = remote::counters();
        let how = match net::address() {
            Some(ip) if port == remote::DEFAULT_PORT => format!("telnet {ip}"),
            Some(ip) => format!("telnet {ip} {port}"),
            None => format!("port {port}, but the network has no address yet (see net)"),
        };
        println!("remote shell: on, {how}");
        println!(
            "since boot: {logins} logins, {failures} wrong passwords; {}",
            if connected { "someone is connected now" } else { "nobody is connected" }
        );
    }

    fn vnc_status(&self) {
        let Some(port) = crate::vnc::port() else {
            return println!("remote desktop (VNC): off (vnc on [PORT] starts it)");
        };
        let (logins, failures, connected) = crate::vnc::counters();
        let how = match net::address() {
            // VNC viewers take "host::port", or "host:display" for port 5900 + display.
            Some(ip) if port == crate::vnc::DEFAULT_PORT => format!("connect a VNC viewer to {ip}"),
            Some(ip) => format!("connect a VNC viewer to {ip}::{port}"),
            None => format!("port {port}, but the network has no address yet (see net)"),
        };
        println!("remote desktop (VNC): on, {how}");
        println!(
            "since boot: {logins} logins, {failures} wrong passwords; {}",
            if connected { "a viewer is connected now" } else { "no viewer is connected" }
        );
    }

    /// `vnc on [PORT]` / `vnc off` (Some) or `vnc password` (None); like
    /// `remote`, only from the machine's own console.
    fn vnc_command(&mut self, switch: Option<Option<u16>>) {
        if session::current().is_some() {
            return error("vnc: this can only be changed on the machine's own console");
        }
        let saved = if self.storage.persistent { " (saved)" } else { " (not saved: RAM disk)" };
        let password = match switch {
            Some(port) => match crate::vnc::set(port) {
                Ok(password) => {
                    match port {
                        Some(port) => println!("remote desktop (VNC) on, port {port}{saved}"),
                        None => return println!("remote desktop (VNC) off{saved}"),
                    }
                    password
                }
                Err(e) => return error(&format!("vnc: {e}")),
            },
            None => match crate::vnc::change_password() {
                Ok(password) => Some(password),
                Err(e) => return error(&format!("vnc: {e}")),
            },
        };
        match password {
            Some(p) => println!("password: {p}   (shown only now; 'vnc password' makes a new one)"),
            None => println!("password: as before ('vnc password' makes a new one)"),
        }
        if switch.is_some() {
            println!("not encrypted: the screen and all that is typed can be seen on the network; use it on your own network only");
        }
    }

    /// `remote on [PORT]` / `remote off` (Some) or `remote password` (None).
    /// Only from the machine's own console: a remote session can't open the
    /// door wider or change the lock.
    fn remote_command(&mut self, switch: Option<Option<u16>>) {
        if session::current().is_some() {
            return error("remote: this can only be changed on the machine's own console");
        }
        let saved = if self.storage.persistent { " (saved)" } else { " (not saved: RAM disk)" };
        let password = match switch {
            Some(port) => match remote::set(port) {
                Ok(password) => {
                    match port {
                        Some(port) => println!("remote shell on, port {port}{saved}"),
                        None => return println!("remote shell off{saved}"),
                    }
                    password
                }
                Err(e) => return error(&format!("remote: {e}")),
            },
            None => match remote::change_password() {
                Ok(password) => Some(password),
                Err(e) => return error(&format!("remote: {e}")),
            },
        };
        match password {
            Some(p) => println!("password: {p}   (shown only now; 'remote password' makes a new one)"),
            None => println!("password: as before ('remote password' makes a new one)"),
        }
        if switch.is_some() {
            println!("not encrypted: the password and all that is typed can be read on the network; use it on your own network only");
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
                Ok(r) => {
                    if let Some(note) = r.trust_note() {
                        println!("({note})");
                    }
                }
                Err(e) if e == "too large" => {
                    error(&format!("{url}: too large to show ({} KiB at most): save it with wget URL FILE", MAX_SHOWN / 1024))
                }
                Err(e) => error(&format!("{url}: {e}")),
            };
        };
        let path = self.abs_target(url.split(['?', '#']).next().unwrap_or(""), file);
        if !self.may_change(&path) {
            return;
        }
        let mut download = Download::start(&mut self.storage, &path);
        let result = net::http_get(url, beefs::MAX_FILE_SIZE, &mut |data| download.write(data));
        match result.and_then(|r| download.finish().map(|()| r)) {
            Ok(r) => {
                if let Some(to) = &r.redirected_to {
                    println!("(redirected to {to})");
                }
                println!("saved {} bytes to {path}", r.bytes);
                if let Some(note) = r.trust_note() {
                    println!("({note})");
                }
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
        if !self.may_change(&path) {
            return;
        }
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
            error("no AI bridge: this machine has no second serial port (COM2); over the network: see 'ai connect'");
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

    /// `do TASK`: the AI suggests commands for the task; they run, one
    /// after the other, only when the user says yes. Some are never run
    /// (doplan::refused); the first one that fails stops the rest.
    fn do_task(&mut self, task: &str) {
        if !self.ai_ready() {
            return;
        }
        let mut answer = Vec::new();
        let fields = Obj::new().str("text", task).str("cwd", &self.cwd);
        let outcome = aibridge::request("do", fields, |l| answer.push(String::from(l)));
        let result = match outcome {
            Outcome::Done => "done",
            Outcome::Cancelled => "cancelled",
            Outcome::Offline => "offline",
            Outcome::Error(_) => "error",
        };
        // Only the outcome is logged, not the task or the plan.
        let ev = eventlog::event("ai").str("kind", "do").str("result", result).num("lines", answer.len() as i64).finish();
        eventlog::write(&mut self.storage.fs(), ev);
        match outcome {
            Outcome::Done => {}
            Outcome::Cancelled => return println!("(cancelled)"),
            Outcome::Offline => return error("AI offline: beehive is not running on the host (start it with: make beehive)"),
            Outcome::Error(e) => return error(&format!("AI error: {e}")),
        }
        let (commands, notes) = doplan::parse(&answer);
        console::set_color(Color::LightCyan, Color::Black);
        for note in notes.iter().filter(|n| !n.trim().is_empty()) {
            println!("{note}");
        }
        console::set_color(Color::LightGray, Color::Black);
        if commands.is_empty() {
            return println!("(no commands to run)");
        }
        for (i, cmd) in commands.iter().enumerate() {
            match doplan::refused(cmd) {
                Some(why) => println!("  {}. {cmd}   (will not run: {why})", i + 1),
                None => println!("  {}. {cmd}", i + 1),
            }
        }
        print!("Run {}? y: yes, any other key: no ", if commands.len() == 1 { "it" } else { "them" });
        console::flush();
        let yes = matches!(crate::input::read_key(), crate::input::Key::Char('y' | 'Y'));
        println!();
        if !yes {
            return println!("nothing was run");
        }
        let (mut ran, total) = (0, commands.len());
        for cmd in &commands {
            if let Some(why) = doplan::refused(cmd) {
                println!("skipped: {cmd} ({why})");
                continue;
            }
            console::set_color(Color::DarkGray, Color::Black);
            println!("$ {cmd}");
            console::set_color(Color::LightGray, Color::Black);
            self.run_line(cmd);
            if self.status != Status::Ok {
                return error(&format!("stopped: '{cmd}' did not work ({ran} of {total} ran)"));
            }
            ran += 1;
        }
        println!("done: {ran} of {total} ran");
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
        Self::wait_for_link(3000); // a connection that is just being made
        if let Some((name, connected, problem, ms)) = aibridge::link_status() {
            let since = time::format_duration(ms);
            return match (connected, aibridge::peer(), problem) {
                (true, Some(peer), _) => println!("bridge:      network, as machine '{name}': beehive online ({peer}) for {since}, not encrypted"),
                (_, _, Some(why)) => println!("bridge:      network, as machine '{name}': not connected: {why}"),
                _ => println!("bridge:      network, as machine '{name}': connecting"),
            };
        }
        if !aibridge::present() {
            return println!("bridge:      none (no COM2 port in this machine; over the network: ai connect HOST NAME KEY)");
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
        // The kill switch also closes (or allows) the network link to beehive.
        aibridge::set_enabled(state == "on");
        match r {
            Ok(()) => println!("AI features switched {state}."),
            Err(e) => error(&format!("could not save: {e}")),
        }
    }

    /// At boot: what the host may do on its own, as saved: remote control
    /// and automatic updates. Both are off unless switched on at the console.
    pub fn load_remote_work(&mut self) {
        let is = |fs: &mut storage::FsGuard, file: &str, word: &[u8]| fs.read_file(file).is_ok_and(|d| d.trim_ascii() == word);
        let mut fs = self.storage.fs();
        let (control, auto) = (is(&mut fs, CONTROL_FILE, b"on"), is(&mut fs, UPDATE_FILE, b"auto"));
        drop(fs);
        aibridge::set_control(control);
        if control {
            hivectl::start();
        }
        update::set_auto(auto);
    }

    /// `ai control on|off` and `update auto on|off`: switches that let the
    /// host act on this machine. Only from the machine's own console.
    fn switch(&mut self, what: &str, file: &str, state: &str) {
        if session::current().is_some() {
            return error(&format!("{what}: only from the machine's own console"));
        }
        let on = state == "on";
        let word = match (file, on) {
            (UPDATE_FILE, true) => "auto",
            (_, true) => "on",
            _ => "off",
        };
        let mut fs = self.storage.fs();
        let saved = fs.mkdir_all("/system").and_then(|_| fs.write_file(file, format!("{word}\n").as_bytes()));
        drop(fs);
        let note = match (saved, self.storage.persistent) {
            (Ok(()), true) => "saved",
            (Ok(()), false) => "RAM disk: not kept after reboot",
            (Err(_), _) => "could not save",
        };
        if file == UPDATE_FILE {
            update::set_auto(on);
            println!("automatic updates {state} ({note})");
            if on {
                println!("this machine now fetches kernels the AI host offers and restarts by itself to try them");
            }
        } else {
            aibridge::set_control(on);
            if on {
                hivectl::start();
            }
            println!("remote control by the AI host {state} ({note})");
            if on {
                println!("the AI host can now run commands on this machine and look at its screen");
            }
        }
    }

    /// `update`: fetch the kernel the host offers, if it is new, and put it on trial.
    fn update(&mut self) {
        if !self.ai_ready() {
            return;
        }
        match update::fetch(|step| println!("  {step}")) {
            Ok(update::Found::Nothing) => println!("no newer kernel on offer (this is b{})", crate::OS_BUILD),
            Ok(update::Found::Known(build, why)) => println!("kernel b{build} is on offer, but {why}"),
            Ok(update::Found::Ready(build, slot)) => {
                println!("kernel b{build} is in slot {}, on trial at the next start: reboot", slot as char);
                let ev = eventlog::event("update").num("build", build as i64).finish();
                eventlog::write(&mut self.storage.fs(), ev);
            }
            Err(why) => error(&format!("update: {why}")),
        }
    }

    /// At boot: the saved network link to beehive, and the kill switch.
    pub fn load_hive(&mut self) {
        aibridge::set_enabled(self.ai_enabled());
        let data = self.storage.fs().read_file(HIVE_FILE).ok();
        if let Some(settings) = data.and_then(|d| aibridge::Settings::parse(&String::from_utf8_lossy(&d))) {
            aibridge::configure(Some(settings));
        }
    }

    /// `ai connect HOST[:PORT] NAME KEY`: reach beehive over the network.
    fn ai_connect(&mut self, target: &str, name: &str, key: &str) {
        if session::current().is_some() {
            return error("ai connect: only from the machine's own console");
        }
        let Some(key) = beenet::hive::key_bytes(key) else {
            return error("ai connect: that is not a pairing key (four groups of five characters, as beehive.py pair prints it)");
        };
        let settings = net::split_host_port(target, beenet::hive::DEFAULT_PORT)
            .and_then(|(host, port)| aibridge::Settings::new(host, port, name, key));
        let Some(settings) = settings else {
            return error("usage: ai connect HOST[:PORT] NAME KEY   (as printed by: beehive.py pair NAME)");
        };
        let mut fs = self.storage.fs();
        let saved = fs.mkdir_all("/system").and_then(|_| fs.write_file(HIVE_FILE, settings.to_text().as_bytes()));
        drop(fs);
        let note = match (saved, self.storage.persistent) {
            (Ok(()), true) => "saved",
            (Ok(()), false) => "RAM disk: not kept after reboot",
            (Err(_), _) => "could not save",
        };
        aibridge::configure(Some(settings));
        println!("connecting to beehive as machine '{name}' ({note})");
        println!("not encrypted: people on your network can read questions and answers, but not use or change them");
        Self::wait_for_link(5000);
        match aibridge::link_status() {
            Some((_, true, _, _)) => println!("connected ({})", aibridge::peer().unwrap_or_default()),
            Some((_, _, Some(why), _)) => error(&format!("not connected: {why}\nit keeps trying; 'ai' shows how it goes")),
            _ => println!("still connecting; 'ai' shows how it goes"),
        }
    }

    /// Give the hive thread a moment to connect or to find out why it cannot.
    fn wait_for_link(ms: u64) {
        let until = time::uptime_ms() + ms;
        thread::wait_until(|| {
            time::uptime_ms() >= until
                || match aibridge::link_status() {
                    None | Some((_, true, _, _)) => true,
                    // "No address yet" is over as soon as the network has one: the link then connects.
                    Some((_, _, Some(why), _)) => why != aibridge::NO_ADDRESS || net::address().is_none(),
                    _ => false,
                }
        });
    }

    fn ai_disconnect(&mut self) {
        if session::current().is_some() {
            return error("ai disconnect: only from the machine's own console");
        }
        if aibridge::link_status().is_none() {
            return println!("there is no network link to beehive");
        }
        aibridge::configure(None);
        let removed = self.storage.fs().remove(HIVE_FILE);
        match removed {
            Ok(()) => println!("network link to beehive removed, and its key with it"),
            Err(e) => error(&format!("link closed, but {HIVE_FILE} could not be removed: {e}")),
        }
    }

    // ----- install ---------------------------------------------------------------

    /// One line about what is on a disk, and what `install` would do with it.
    fn disk_summary(contents: &install::Contents) -> (String, &'static str) {
        use install::Contents::*;
        match contents {
            Empty => (String::from("empty"), "install"),
            Unfinished => (String::from("an install that was not finished"), "install again"),
            Installed { build, data } => (
                format!(
                    "BeeCodersOs{}{}",
                    build.map(|b| format!(" b{b}")).unwrap_or_default(),
                    if *data { " (boot, os, data)" } else { " (boot disk without a data partition)" }
                ),
                "upgrade: new kernel on trial, os and data kept (--reinstall: boot and os again; --erase: everything)",
            ),
            BeeFsDisk => (String::from("a BeeFS data disk, without the system"), "refuse; with --erase: erase it and install"),
            Other(what) => (what.clone(), "refuse; with --erase: erase it and install"),
        }
    }

    /// `install`: the disks, what is on them, and what would happen. Changes nothing.
    fn install_list(&mut self) {
        match install::image() {
            Some(image) => {
                let build = install::build_in(image).map(|b| format!("b{b}")).unwrap_or_else(|| String::from("an unknown build"));
                println!("This install medium carries BeeCodersOs {build}.");
            }
            None => println!("Not started from an install medium: this can only show the disks."),
        }
        if disks::count() == 0 {
            return println!("No disk found (IDE or SATA).");
        }
        for index in 0..disks::count() {
            let Some(disk) = disks::get(index) else { continue };
            let size = install::size_text(disk.sectors);
            match install::inspect(&mut DiskRef(index)) {
                Ok(contents) => {
                    let (what, action) = Self::disk_summary(&contents);
                    println!("  {:<6} {:>8}  \"{}\": {what}", disk.name, size, disk.model);
                    println!("         install {}  would {action}", disk.name);
                }
                Err(e) => println!("  {:<6} {:>8}  \"{}\": cannot be read: {e}", disk.name, size, disk.model),
            }
        }
        println!("Nothing was changed.");
    }

    /// Ask a question and read one line from the keyboard. `None`: Ctrl+C or Esc.
    fn question(question: &str) -> Option<String> {
        use crate::input::Key;
        print!("{question} ");
        let mut line = String::new();
        loop {
            match crate::input::read_key() {
                Key::Enter => {
                    println!();
                    return Some(line);
                }
                Key::Char(c) => {
                    line.push(c);
                    print!("{c}");
                }
                Key::Backspace => {
                    if line.pop().is_some() {
                        print!("\x08 \x08");
                    }
                }
                Key::Ctrl('c' | 'd') | Key::Escape => {
                    println!();
                    return None;
                }
                _ => {}
            }
        }
    }

    /// `install DISK [--erase | --reinstall]`: after a clear question, put
    /// the OS on that disk, or upgrade the BeeCodersOs that is on it.
    fn install(&mut self, name: &str, option: &str) {
        use install::Contents::*;
        if session::current().is_some() {
            return error("install: only from the machine's own console");
        }
        let Some(image) = install::image() else {
            return error("install: this system was not started from an install medium (the ISO or the USB stick)");
        };
        let Some(index) = disks::find(name) else {
            return error(&format!("install: no disk called {name} ('install' lists the disks)"));
        };
        let Some(disk) = disks::get(index) else { return };
        let (sectors, model) = (disk.sectors, disk.model.clone());
        let contents = match install::inspect(&mut DiskRef(index)) {
            Ok(contents) => contents,
            Err(e) => return error(&format!("install: {name} cannot be read: {e}")),
        };
        let Some(parts) = install::layout(sectors) else {
            return error(&format!("install: {name} is too small ({}; at least 33 MiB)", install::size_text(sectors)));
        };
        let (what, _) = Self::disk_summary(&contents);
        let build = install::build_in(image).map(|b| format!(" b{b}")).unwrap_or_default();
        println!("Disk {name}: {}, \"{model}\"", install::size_text(sectors));
        println!("  now:   {what}");

        // What to do: an upgrade, boot and os again, or everything new.
        #[derive(PartialEq)]
        enum Plan {
            Fresh,
            Upgrade,
            Reinstall,
        }
        let plan = match (&contents, option) {
            (Empty | Unfinished, "" | "--erase") | (_, "--erase") => Plan::Fresh,
            (Installed { .. }, "") => Plan::Upgrade,
            (Installed { data: true, .. }, "--reinstall") => Plan::Reinstall,
            (_, "--reinstall") => return error(&format!("install: --reinstall is for a disk with BeeCodersOs and a data partition. To erase everything: install {name} --erase")),
            _ => return error(&format!("install: {name} is not empty. To erase everything on it: install {name} --erase")),
        };
        match plan {
            Plan::Fresh => println!(
                "  after: BeeCodersOs{build}: boot {}, os {}, data {}",
                install::size_text(parts[0].2),
                install::size_text(parts[1].2),
                install::size_text(parts[2].2)
            ),
            Plan::Upgrade => println!("  after: the same, with kernel{build} on trial in the spare slot (the old kernel boots again if it fails)"),
            Plan::Reinstall => println!("  after: BeeCodersOs{build}: boot and os written again (settings and logs are lost), data kept"),
        }
        let erases_something = plan == Plan::Fresh && !matches!(contents, Empty | Unfinished);
        if erases_something {
            println!("EVERYTHING on this disk will be erased. There is no undo.");
            let sentence = install::erase_sentence(sectors, &model);
            let typed = Self::question(&format!("To go on, type exactly:  {sentence}\n>"));
            if !typed.is_some_and(|t| install::sentence_matches(&t, &sentence)) {
                return println!("That is not the sentence: nothing was changed.");
            }
        } else {
            let ask = match plan {
                Plan::Upgrade => "Upgrade? Type yes to go on:",
                Plan::Reinstall => "Write boot and os again? Type yes to go on:",
                Plan::Fresh => "Install? Type yes to go on:",
            };
            if Self::question(ask).map(|t| t.trim().to_lowercase()) != Some(String::from("yes")) {
                return println!("Nothing was changed.");
            }
        }
        if plan != Plan::Upgrade && storage::mounted_disk() == Some(index) {
            // The system runs from this disk: let go of it first.
            self.storage = storage::detach();
            self.cwd = String::from("/");
        }
        let mut disk = DiskRef(index);
        let show = |step: &str| println!("  {step}");
        let result = match plan {
            Plan::Fresh => {
                let stop = crate::bootargs::value("installstop").and_then(|n| n.parse().ok());
                install::install(&mut disk, image, stop, show).map(|_| format!("Installed on {name}."))
            }
            Plan::Reinstall => install::reinstall(&mut disk, image, show).map(|_| format!("Boot and os written again on {name}; data kept.")),
            Plan::Upgrade => install::upgrade(&mut disk, image, show)
                .map(|(slot, build)| format!("Kernel b{build} is in slot {} of {name}, on trial at the next start from the disk.", slot as char)),
        };
        match result {
            Ok(done) => {
                println!("{done} Take out the install medium and type: reboot");
                let ev = eventlog::event("install").str("result", "done").finish();
                eventlog::write(&mut self.storage.fs(), ev);
            }
            Err(why) if plan == Plan::Fresh => {
                error(&format!("install failed: {why}"));
                println!("The disk has no partition table now; 'install {name}' starts over.");
            }
            Err(why) => error(&format!("install: {why}")),
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

    /// A file's content as text; prints why not and gives `None` otherwise.
    fn read_text(&mut self, file: &str) -> Option<String> {
        let path = self.abs(file);
        let data = self.storage.fs().read_file(&path);
        match data {
            Ok(data) => Some(String::from_utf8_lossy(&data).into_owned()),
            Err(e) => {
                error(&format!("{file}: {e}"));
                None
            }
        }
    }

    /// `head` / `tail`: the first or last `n` lines of a file.
    fn head_tail(&mut self, file: &str, n: usize, last: bool) {
        let Some(text) = self.read_text(file) else { return };
        let part = if last { textops::tail(&text, n) } else { textops::head(&text, n) };
        print!("{part}");
        if !part.is_empty() && !part.ends_with('\n') {
            println!();
        }
    }

    /// `grep [-i] TEXT FILE|FOLDER`: the lines that contain TEXT, with their
    /// numbers; in a folder, every file in it and below (with its path).
    fn grep(&mut self, pattern: &str, path: &str, ignore_case: bool) {
        const MAX_MATCHES: usize = 1000;
        let start = self.abs(path);
        let folder = self.storage.fs().is_dir(&start);
        let mut files = Vec::new();
        if folder {
            let mut todo = alloc::vec![start.clone()];
            while let Some(dir) = todo.pop() {
                let Ok(entries) = self.storage.fs().read_dir(&dir) else { continue };
                for e in entries.iter().rev() {
                    let full = format!("{}/{}", dir.trim_end_matches('/'), e.name);
                    match e.kind {
                        FileKind::Directory => todo.push(full),
                        FileKind::File => files.push(full),
                    }
                }
            }
            files.sort();
        } else {
            files.push(start);
        }
        let mut found = 0;
        for file in &files {
            let data = match self.storage.fs().read_file(file) {
                Ok(data) => data,
                Err(e) if !folder => return error(&format!("{path}: {e}")),
                Err(_) => continue,
            };
            if textops::is_binary(&data) {
                if String::from_utf8_lossy(&data).contains(pattern) {
                    println!("{file}: a binary file, it matches");
                }
                continue;
            }
            let text = String::from_utf8_lossy(&data);
            for (number, line) in textops::grep(&text, pattern, ignore_case) {
                if found == MAX_MATCHES {
                    println!("... (stopped after {MAX_MATCHES} lines)");
                    return;
                }
                found += 1;
                if folder {
                    println!("{file}:{number}: {line}");
                } else {
                    println!("{number}: {line}");
                }
            }
        }
        if found == 0 {
            println!("no line contains {pattern:?}");
        }
    }

    fn touch(&mut self, file: &str) {
        let path = self.abs(file);
        if !self.may_change(&path) {
            return;
        }
        let exists = self.storage.fs().exists(&path);
        if !exists {
            let r = self.storage.fs().create(&path);
            report(r);
        }
    }

    fn cp(&mut self, from: &str, to: &str) {
        let src = self.abs(from);
        let dst = self.abs_target(from, to);
        if !self.may_change(&dst) {
            return;
        }
        let mut fs = self.storage.fs();
        let r = fs.read_file(&src).and_then(|d| fs.write_file(&dst, &d));
        drop(fs);
        report(r);
    }

    fn df(&mut self) {
        let fs = self.storage.fs();
        let (root, data) = (fs.stats(), fs.data_stats());
        drop(fs);
        let line = |name: &str, s: beefs::FsStats| {
            let kib = |b: u32| b as u64 * s.block_size as u64 / 1024;
            println!(
                "{name}: {} KiB used, {} KiB free, {} KiB total; {} of {} inodes free",
                kib(s.total_blocks - s.free_blocks),
                kib(s.free_blocks),
                kib(s.total_blocks),
                s.free_inodes,
                s.total_inodes
            );
        };
        line(if self.storage.persistent { "disk" } else { "ramdisk" }, root);
        if let Some(data) = data {
            line("data (/data)", data);
        }
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
    ("whoami, users", "who is logged in; the accounts (none: no login, everyone may do everything)"),
    ("useradd NAME [admin]", "make an account (the first is an admin); userdel NAME; passwd [NAME]"),
    ("login, logout", "log in at the console / log out (with accounts, the console asks at boot)"),
    ("clear", "clear the screen (or Ctrl+L)"),
    ("echo TEXT [> FILE]", "print text, or write (>) / append (>>) to a file"),
    ("ls [DIR]", "list a directory"),
    ("cd [DIR], pwd", "change / show the current directory"),
    ("cat FILE", "show a file"),
    ("head|tail [-n N] FILE", "the first / last N lines of a file (10 unless N is given)"),
    ("wc FILE", "count the lines, words and bytes of a file"),
    ("grep [-i] TEXT PATH", "lines that contain TEXT, in a file or every file of a folder"),
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
    ("date, uptime", "current date and time (in the time zone, and UTC), time since boot"),
    ("timezone [ZONE]", "the time zone for date and the desktop clock: UTC, CET, +HH:MM (saved)"),
    ("log [N]", "last N events from the event log"),
    ("wish TEXT", "tell the OS what you'd like it to do"),
    ("feedback TEXT", "tell the OS what could be better"),
    ("ask QUESTION", "ask the AI (needs beehive on the host)"),
    ("explain", "ask the AI about the last error or crash"),
    ("do TASK", "the AI suggests commands for a task in plain words; they run if you say y"),
    ("update [auto on|off]", "fetch a new kernel from the AI host and put it on trial"),
    ("ai control [on|off]", "let the AI host run commands here and see the screen"),
    ("ai connect HOST NAME KEY", "reach beehive over the network (ai disconnect: stop)"),
    ("ai [on|off]", "AI bridge status; switch AI features on/off"),
    ("selftest", "run the kernel self-tests"),
    ("net", "network status; net dhcp | static IP/PREFIX [GW] | dns ADDR | off"),
    ("ping [-c N] HOST", "check that a machine answers"),
    ("nslookup NAME", "look up the address of a host name"),
    ("nc HOST PORT [TEXT]", "open a TCP connection: type lines, see the answers"),
    ("httpd [on [PORT]|off]", "status page for the local network (off by default)"),
    ("vnc [on|off|password]", "remote desktop: see and use this screen from a VNC viewer (off by default; not encrypted)"),
    ("remote [on|off]", "remote shell with a password, not encrypted (off by default)"),
    ("wget URL [FILE]", "fetch a web address (http://): show it or save it"),
    ("ntp [SERVER]", "set the clock from a time server"),
    ("tftp get|put HOST FILE", "copy a file from / to a TFTP server"),
    ("arp", "neighbours learned on the local network"),
    ("lspci [-v]", "list PCI devices (-v: what each one can do)"),
    ("install [DISK]", "put the OS on a disk, or upgrade it (alone: list the disks)"),
    ("lsusb", "USB controller and devices"),
    ("camera [raw|log]", "USB cameras: the picture formats and sizes they offer ('raw': the description as the camera gave it; 'log': what the driver noted)"),
    ("camera watch FOLDER [MIN]", "a security camera: saves a picture in FOLDER whenever something moves (Ctrl+C stops)"),
    ("camera view", "the camera live in a window, as in a mirror: s saves a picture, m mirrors, q leaves"),
    ("camera on [WxH] | off", "leave the camera sending (then 'camera' counts what arrives), or stop it"),
    ("camera snap FILE [WxH]", "take a picture with the USB camera (640x480 unless asked): JPEG, PNG or BMP by the name"),
    ("camera record FILE.avi SECS", "film with the USB camera into a Motion JPEG video (play it with 'play')"),
    ("threads", "what runs side by side inside the kernel"),
    ("monitor", "system monitor in a window: work per processor, memory, network, threads"),
    ("view FILE", "show a picture (JPEG, PNG, GIF, BMP or PPM) in a window; animated GIFs play"),
    ("sound [volume N]", "the sound card, its outputs and volume (0-100)"),
    ("beep [HZ|NOTE [MS]]", "play a tone (default 880 Hz, 300 ms; notes like a4, c#5)"),
    ("play FILE.wav|.avi", "play a WAV file, or a Motion-JPEG AVI video in a window"),
    ("tune [save FILE.wav] NOTES", "play notes like c5 e5 g5 c6:500 (r rests), or save them as a WAV file"),
    ("record FILE.gif|.avi SECS", "film the screen for SECS seconds (1-60): an animated GIF, or an MJPEG video"),
    ("screenshot FILE", "save the screen as a picture (FILE.jpg: JPEG, .png: PNG, .gif: GIF, otherwise BMP)"),
    ("display [mode WxH]", "screen: driver, resolution; 'display font 1|2|auto'; 'display test'; 'display speed'"),
    ("edit FILE", "text editor in a window on the desktop (Ctrl+S saves, Ctrl+Q leaves)"),
    ("browse URL|FILE|WORDS", "web page viewer in a window: http:// pages, HTML files, or a search; Tab/Enter/click follow links"),
    ("files [FOLDER]", "file manager in a window: browse, open in the editor, new folder, rename, delete"),
    ("cpu [start|test|auto on|off]", "the processor: name, how many, clock, features; start the others; measure; at boot or not (saved)"),
    ("sleep SECONDS", "wait (Ctrl+C ends it)"),
    ("terminal", "one more terminal window on the desktop, with a shell of its own"),
    ("desktop [on|off]", "graphical desktop: the terminal in a window (on by default; saved); 'desktop wallpaper FILE|none'"),
    ("slots", "A/B kernel slots: which one runs, last trial"),
    ("crash panic|exception|hang", "deliberately crash the kernel: a panic, a CPU exception, or a hang the watchdog ends (tests crash reports)"),
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

/// "640x480" -> (640, 480).
fn parse_size(text: &str) -> Option<(u16, u16)> {
    let (w, h) = text.split_once(['x', 'X'])?;
    let (w, h) = (w.parse::<u16>().ok()?, h.parse::<u16>().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}
