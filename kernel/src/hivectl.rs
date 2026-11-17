//! Remote control through beehive: the host (the developer's PC) runs shell
//! commands on this machine and looks at its screen, over the link the
//! machine already has to beehive (tools/beectl.py on the host).
//!
//! Off unless `ai control on` was typed on the machine's own console. The
//! commands run in a shell of their own, like a remote shell session:
//! everything the local console may not do from a session (install, ai
//! connect) stays refused here too.
//!
//! Host -> machine                                   Machine -> host
//!   {"type":"run","rid":R,"cmd":..,"timeout":S}       {"type":"ran","rid":R,"text":..}  (pieces)
//!                                                     {"type":"ran","rid":R,"done":true[,"timeout":true]}
//!   {"type":"screenshot","rid":R,"scale":N}           {"type":"shot","rid":R,"w":..,"h":..,"scale":N}
//!       (scale 0: the text on the screen)             {"type":"shot","rid":R,"data":base64}  (pieces)
//!                                                     {"type":"shot","rid":R,"text":..} / {"done":true}

use crate::aibridge;
use crate::input::Key;
use crate::json::{Obj, Object};
use crate::ring::Global;
use crate::shell::Shell;
use crate::{console, session, storage, thread, time};
use alloc::string::String;
use beenet::base64;

/// Most output one command may send back.
const MAX_OUTPUT: usize = 256 * 1024;
/// Text per message, and picture bytes per message (small enough for a serial link).
const TEXT_PIECE: usize = 1500;
const DATA_PIECE: usize = 1536;

static STARTED: Global<bool> = Global::new(false);

/// Start the control thread (once, when control is first switched on). It
/// idles while control is switched off again.
pub fn start() {
    if !*STARTED.get() {
        *STARTED.get() = thread::spawn("control", None, serve).is_some();
    }
}

/// Does `output` end with the shell's prompt ("bee:/path$ ")? Then the
/// command is done. Returns the output without that last line.
pub fn before_prompt(output: &str) -> Option<&str> {
    let start = output.rfind('\n').map_or(0, |i| i + 1);
    let last = &output[start..];
    (last.starts_with("bee:/") && last.ends_with("$ ") && !last[..last.len() - 2].contains(' ')).then(|| &output[..start])
}

fn serve() {
    // The shell the commands run in, made when the first one arrives.
    let mut shell: Option<(u32, thread::ThreadId)> = None;
    loop {
        let Some(request) = aibridge::take_request() else {
            let tick = time::uptime_ms();
            thread::wait_until(|| time::uptime_ms() != tick);
            continue;
        };
        let rid = String::from(request.str("rid").unwrap_or(""));
        match request.str("type") {
            Some("run") => run(&mut shell, &rid, &request),
            Some("screenshot") => screenshot(&rid, request.num("scale").unwrap_or(4)),
            _ => {}
        }
    }
}

fn reply(kind: &str, rid: &str) -> Obj {
    Obj::new().str("type", kind).str("rid", rid)
}

fn run(shell: &mut Option<(u32, thread::ThreadId)>, rid: &str, request: &Object) {
    let refuse = |why: &str| aibridge::send(&reply("ran", rid).bool("done", true).str("error", why).finish());
    if shell.is_some_and(|(_, id)| !thread::is_alive(id)) {
        if let Some((session, _)) = shell.take() {
            session::remove(session); // its shell left (`exit`): a new one next
        }
    }
    if shell.is_none() {
        let (Some(session), Some(storage)) = (session::open(), storage::handle()) else {
            return refuse("no room for another shell on the machine");
        };
        let Some(id) = thread::spawn("control-shell", Some(session), move || Shell::control(storage).run_session()) else {
            session::remove(session);
            return refuse("no room for another shell on the machine");
        };
        *shell = Some((session, id));
        // Its first prompt is not part of any command's output.
        let give_up = time::uptime_ms() + 2000;
        let mut seen = String::new();
        while before_prompt(&seen).is_none() && time::uptime_ms() < give_up {
            thread::pause(|| false);
            seen += &session::take_output(session);
        }
    }
    let Some((session, id)) = *shell else { return };
    let timeout = request.num("timeout").unwrap_or(30).clamp(1, 600) as u64 * 1000;
    session::take_output(session);
    session::type_text(session, request.str("cmd").unwrap_or(""));
    session::type_text(session, "\n");
    let give_up = time::uptime_ms() + timeout;
    let mut output = String::new();
    let (mut sent, mut timed_out) = (0, false);
    loop {
        thread::pause(|| false);
        output += &session::take_output(session);
        let done = before_prompt(&output).map(str::len);
        // Send what is certain to be output (not a prompt that may still come).
        let upto = done.unwrap_or_else(|| output.rfind('\n').map_or(0, |i| i + 1)).min(MAX_OUTPUT);
        while sent < upto {
            let mut end = (sent + TEXT_PIECE).min(upto);
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            aibridge::send_all(&reply("ran", rid).str("text", &output[sent..end]).finish());
            sent = end;
        }
        if done.is_some() || !thread::is_alive(id) {
            break;
        }
        if time::uptime_ms() >= give_up || output.len() > MAX_OUTPUT {
            session::push_key(session, Key::Ctrl('c'));
            timed_out = true;
            break;
        }
    }
    let mut last = reply("ran", rid).bool("done", true);
    if timed_out {
        last = last.bool("timeout", true);
    }
    aibridge::send_all(&last.finish());
}

fn screenshot(rid: &str, scale: i64) {
    if scale <= 0 {
        let text = console::screen_text();
        let mut at = 0;
        while at < text.len() {
            let mut end = (at + TEXT_PIECE).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            aibridge::send_all(&reply("shot", rid).str("text", &text[at..end]).finish());
            at = end;
        }
    } else if let Some((width, height, pixels)) = console::capture(scale.clamp(1, 16) as usize) {
        aibridge::send_all(&reply("shot", rid).num("w", width as i64).num("h", height as i64).num("scale", scale.clamp(1, 16)).finish());
        for piece in pixels.chunks(DATA_PIECE) {
            aibridge::send_all(&reply("shot", rid).str("data", &base64::encode(piece)).finish());
        }
    } else {
        return aibridge::send(&reply("shot", rid).bool("done", true).str("error", "this machine shows a text screen: ask for its text").finish());
    }
    aibridge::send_all(&reply("shot", rid).bool("done", true).finish());
}
