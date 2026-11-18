//! User accounts: who is at the console, and what they may do.
//!
//! Accounts are lines in /system/users: `name:groups:salt:hash`, groups
//! separated by commas, the password never stored: `hash` is SHA-256 run
//! `ROUNDS` times over the salt and the password (slow on purpose, so a
//! copied file is slow to guess from). A machine without the file has no
//! accounts and works as before: no login, everything allowed.
//!
//! With accounts, the console asks for a name and password at boot (and
//! after `logout`), and commands that change the system itself need the
//! group `admin` (`needs_admin`). The AI host's control shell keeps admin
//! rights (it is switched on with `ai control on` at the console); the
//! remote shell keeps its own password and has no admin rights.
//!
//! What may be changed follows the folder (`owner_blocking`): your home
//! folder is yours, /system is the admins', the rest is shared; reading is
//! open to all. (Owners per file would need room in BeeFS's inodes.)

use crate::storage::Storage;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use beenet::sha256;

pub const USERS_FILE: &str = "/system/users";
pub const ADMIN: &str = "admin";
/// Rounds of SHA-256 for a password (about 10 ms on the HP).
const ROUNDS: u32 = 20_000;
pub const MAX_USERS: usize = 64;
/// Longest password that is accepted (and shortest).
pub const MAX_PASSWORD: usize = 64;
pub const MIN_PASSWORD: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub groups: Vec<String>,
    salt: String,
    hash: String,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.groups.iter().any(|g| g == ADMIN)
    }

    /// A new account; `salt` must be fresh for every password.
    pub fn new(name: &str, groups: &[&str], salt: &str, password: &str) -> User {
        User {
            name: String::from(name),
            groups: groups.iter().map(|g| String::from(*g)).collect(),
            salt: String::from(salt),
            hash: hash_password(salt, password),
        }
    }

    pub fn set_password(&mut self, salt: &str, password: &str) {
        self.salt = String::from(salt);
        self.hash = hash_password(salt, password);
    }

    /// Is this the password? Compares every byte, whatever differs first.
    pub fn password_ok(&self, password: &str) -> bool {
        if password.len() > MAX_PASSWORD {
            return false;
        }
        let right = hash_password(&self.salt, password);
        right.len() == self.hash.len() && right.bytes().zip(self.hash.bytes()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
    }

    fn line(&self) -> String {
        format!("{}:{}:{}:{}", self.name, self.groups.join(","), self.salt, self.hash)
    }
}

pub fn hash_password(salt: &str, password: &str) -> String {
    let mut h = sha256::sha256(format!("{salt}:{password}").as_bytes());
    for _ in 0..ROUNDS {
        let mut next = sha256::Sha256::new();
        next.update(&h);
        next.update(salt.as_bytes());
        next.update(password.as_bytes());
        h = next.finish();
    }
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// A name for an account or a group: a lowercase letter, then up to 15
/// lowercase letters, digits, '-' or '_'.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=16).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

/// The accounts in the text of /system/users. Lines that do not make sense
/// are left out (a damaged line never gives anyone rights); at most MAX_USERS.
pub fn parse(text: &str) -> Vec<User> {
    let mut users: Vec<User> = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.trim().split(':').collect();
        let [name, groups, salt, hash] = parts[..] else { continue };
        let groups: Vec<String> = groups.split(',').filter(|g| !g.is_empty()).map(String::from).collect();
        let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid_name(name) || !groups.iter().all(|g| valid_name(g)) || !hex(salt, 16) || !hex(hash, 64) {
            continue;
        }
        if users.iter().any(|u| u.name == name) || users.len() == MAX_USERS {
            continue;
        }
        users.push(User { name: String::from(name), groups, salt: String::from(salt), hash: String::from(hash) });
    }
    users
}

pub fn format_all(users: &[User]) -> String {
    users.iter().map(|u| u.line() + "\n").collect()
}

pub fn load(storage: &mut Storage) -> Vec<User> {
    let data = storage.fs().read_file(USERS_FILE).ok();
    data.map(|d| parse(&String::from_utf8_lossy(&d))).unwrap_or_default()
}

pub fn save(storage: &mut Storage, users: &[User]) -> Result<(), String> {
    let mut fs = storage.fs();
    let _ = fs.mkdir_all("/system");
    if users.is_empty() {
        return fs.remove(USERS_FILE).map_err(|e| e.to_string());
    }
    fs.write_file(USERS_FILE, format_all(users).as_bytes()).map_err(|e| e.to_string())
}

/// A fresh salt: 16 hex digits.
pub fn new_salt() -> String {
    let n = crate::rand::next_u64() ^ crate::time::rdtsc().rotate_left(23);
    format!("{n:016x}")
}

/// Does this command line change the system itself (so only an admin may
/// run it once there are accounts)? Looking only is allowed to everyone.
pub fn needs_admin(cmd: &str, args: &[&str]) -> bool {
    match cmd {
        // Always: they change the installed system, the accounts or the power.
        "install" | "update" | "crash" | "useradd" | "userdel" | "reboot" | "shutdown" => true,
        // Showing is for everyone, changing for admins.
        "slots" | "layout" | "timezone" | "vnc" | "remote" | "httpd" | "net" | "ai" | "desktop" => !args.is_empty(),
        "cpu" | "display" => !matches!(args, [] | ["test" | "speed"]),
        // Someone else's password.
        "passwd" => !args.is_empty(),
        _ => false,
    }
}

/// Who may change what is at `path` (an absolute, normalized path), once
/// the machine has accounts: `None` if `user` may, otherwise who it belongs
/// to. Ownership follows the folder: `/home/NAME` and everything in it
/// belongs to NAME, `/system` to the admins, the rest is shared. Admins may
/// change everything. Reading is open to all.
pub fn owner_blocking(user: Option<&User>, admin: bool, path: &str) -> Option<String> {
    if admin {
        return None;
    }
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    match (parts.next(), parts.next()) {
        (Some("system"), _) => Some(String::from("the admins")),
        (Some("home"), Some(name)) if user.is_none_or(|u| u.name != name) => Some(String::from(name)),
        // `/home` itself: making or removing someone's home is for admins.
        (Some("home"), None) => Some(String::from("the admins")),
        _ => None,
    }
}

/// For the programs in windows (editor, file manager): may whoever is
/// logged in at the console change `path`? `Err` says whose it is.
pub fn console_may_change(storage: &mut Storage, path: &str) -> Result<(), String> {
    if load(storage).is_empty() {
        return Ok(());
    }
    let user = console_user();
    let admin = user.as_ref().is_some_and(User::is_admin);
    match owner_blocking(user.as_ref(), admin, path) {
        None => Ok(()),
        Some(owner) => Err(format!("belongs to {owner}")),
    }
}

/// Who is logged in at the console (`None`: nobody, or no accounts).
static CONSOLE_USER: crate::ring::Global<Option<User>> = crate::ring::Global::new(None);

pub fn console_user() -> Option<User> {
    CONSOLE_USER.get().clone()
}

pub fn set_console_user(user: Option<User>) {
    *CONSOLE_USER.get() = user;
}
