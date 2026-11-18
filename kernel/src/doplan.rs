//! `do TASK`: the AI turns a task in plain words into shell commands, and
//! the user decides whether they run. This is the part without the AI or
//! the shell: reading the plan out of the answer, and the commands a plan
//! may never contain. The answer is untrusted data: nothing in it runs
//! without the user's `y`, and some commands not even then.

use alloc::string::String;
use alloc::vec::Vec;

/// A plan has at most this many commands; the rest are dropped.
pub const MAX_COMMANDS: usize = 10;
/// A command line longer than this is not taken.
pub const MAX_LINE: usize = 200;

/// The plan in an answer: the commands (lines that start with "$ "), and
/// the other lines (what the AI says about them).
pub fn parse(answer: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut commands, mut notes) = (Vec::new(), Vec::new());
    for line in answer {
        match line.trim_start().strip_prefix("$ ") {
            Some(cmd) if !cmd.trim().is_empty() && cmd.len() <= MAX_LINE && !cmd.chars().any(char::is_control) => {
                if commands.len() < MAX_COMMANDS {
                    commands.push(String::from(cmd.trim()));
                }
            }
            Some(_) => {}
            None => notes.push(line.clone()),
        }
    }
    (commands, notes)
}

/// Commands an AI's plan may not run, even when the user says yes: they
/// change the system itself (kernel, disks, power, who may reach the
/// machine) or start another plan. Gives the reason, `None` if allowed.
pub fn refused(cmd: &str) -> Option<&'static str> {
    let name = cmd.split_whitespace().next().unwrap_or("");
    match name {
        "install" | "update" | "slots" => Some("changes the installed system"),
        "reboot" | "shutdown" | "crash" => Some("stops the machine"),
        "ai" | "remote" | "httpd" | "net" => Some("changes who can reach this machine"),
        "do" | "ask" | "explain" => Some("asks the AI again"),
        "cpu" if cmd.split_whitespace().nth(1) == Some("auto") => Some("changes how the machine starts"),
        "desktop" | "display" | "layout" if cmd.split_whitespace().nth(1).is_some() => Some("changes a saved setting"),
        _ => None,
    }
}
