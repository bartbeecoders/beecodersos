//! The server side of Telnet (RFC 854), as little as a remote shell needs
//! (network phase N6). No sockets here: the caller feeds received bytes in
//! and sends what comes out.
//!
//! The server offers to echo and to work a character at a time
//! (`GREETING`); every other option a client proposes is refused. Nothing a
//! client sends is stored: the decoder is a handful of states, so no input
//! can make it grow.

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;
const INTERRUPT_PROCESS: u8 = 244;
const BREAK: u8 = 243;
const END_OF_FILE: u8 = 236;

const OPT_ECHO: u8 = 1;
const OPT_SUPPRESS_GO_AHEAD: u8 = 3;

/// Sent once when a client connects: "I will echo, and no line buffering".
pub const GREETING: [u8; 6] = [IAC, WILL, OPT_ECHO, IAC, WILL, OPT_SUPPRESS_GO_AHEAD];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// A byte the user typed. The end of a line is one `\r`.
    Byte(u8),
    /// The client's "interrupt" (Ctrl+C in line mode).
    Interrupt,
    /// The client's "end of input" (Ctrl+D in line mode).
    EndOfFile,
    /// Send these three bytes back: an option we refuse.
    Reply([u8; 3]),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Data,
    /// After a carriage return: the line feed or NUL that follows belongs to it.
    AfterCr,
    Command,
    /// After WILL / WONT / DO / DONT: the option byte comes next.
    Option(u8),
    Subnegotiation,
    SubnegotiationCommand,
}

pub struct Decoder {
    state: State,
}

impl Default for Decoder {
    fn default() -> Self {
        Decoder::new()
    }
}

impl Decoder {
    pub const fn new() -> Self {
        Decoder { state: State::Data }
    }

    /// Take one received byte; at most one event comes out.
    pub fn feed(&mut self, b: u8) -> Option<Event> {
        match self.state {
            State::Data | State::AfterCr => {
                let after_cr = self.state == State::AfterCr;
                self.state = State::Data;
                match b {
                    IAC => {
                        self.state = State::Command;
                        None
                    }
                    b'\n' | 0 if after_cr => None,
                    b'\r' => {
                        self.state = State::AfterCr;
                        Some(Event::Byte(b'\r'))
                    }
                    b'\n' => Some(Event::Byte(b'\r')), // a bare line feed also ends a line
                    0 => None,
                    _ => Some(Event::Byte(b)),
                }
            }
            State::Command => {
                self.state = State::Data;
                match b {
                    IAC => Some(Event::Byte(IAC)), // a doubled IAC is the byte 255 itself
                    WILL | WONT | DO | DONT => {
                        self.state = State::Option(b);
                        None
                    }
                    SB => {
                        self.state = State::Subnegotiation;
                        None
                    }
                    INTERRUPT_PROCESS | BREAK => Some(Event::Interrupt),
                    END_OF_FILE => Some(Event::EndOfFile),
                    _ => None,
                }
            }
            State::Option(command) => {
                self.state = State::Data;
                let ours = b == OPT_ECHO || b == OPT_SUPPRESS_GO_AHEAD;
                match command {
                    // The client wants to switch something on at its side: no.
                    WILL => Some(Event::Reply([IAC, DONT, b])),
                    // It asks us to: only what we offered (that is its answer to GREETING).
                    DO if !ours => Some(Event::Reply([IAC, WONT, b])),
                    _ => None,
                }
            }
            State::Subnegotiation => {
                if b == IAC {
                    self.state = State::SubnegotiationCommand;
                }
                None
            }
            State::SubnegotiationCommand => {
                self.state = if b == SE { State::Data } else { State::Subnegotiation };
                None
            }
        }
    }
}

/// Text for the client: every line end becomes CR LF, and a byte 255 is doubled.
pub fn encode(text: &[u8], out: &mut alloc::vec::Vec<u8>) {
    for &b in text {
        match b {
            b'\n' => out.extend_from_slice(b"\r\n"),
            IAC => out.extend_from_slice(&[IAC, IAC]),
            _ => out.push(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn decode(bytes: &[u8]) -> Vec<Event> {
        let mut d = Decoder::new();
        bytes.iter().filter_map(|&b| d.feed(b)).collect()
    }

    fn text(bytes: &[u8]) -> Vec<u8> {
        decode(bytes)
            .into_iter()
            .filter_map(|e| match e {
                Event::Byte(b) => Some(b),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_text_and_line_ends() {
        assert_eq!(text(b"ls\r\n"), b"ls\r");
        assert_eq!(text(b"ls\r\0pwd\r\n"), b"ls\rpwd\r");
        assert_eq!(text(b"ls\npwd\n"), b"ls\rpwd\r");
        assert_eq!(text(b"a\r\rb"), b"a\r\rb"); // two Enters
        assert_eq!(text(b"a\0b"), b"ab");
        assert_eq!(text(&[b'a', 3, 4, 0x7F]), [b'a', 3, 4, 0x7F]); // control keys pass
    }

    #[test]
    fn options_we_offered_are_accepted_silently() {
        assert_eq!(decode(&[IAC, DO, OPT_ECHO, IAC, DO, OPT_SUPPRESS_GO_AHEAD, b'x']), vec![Event::Byte(b'x')]);
        // A client that declines them gets no argument either.
        assert_eq!(decode(&[IAC, DONT, OPT_ECHO, IAC, WONT, 31]), vec![]);
    }

    #[test]
    fn other_options_are_refused() {
        assert_eq!(decode(&[IAC, WILL, 31]), vec![Event::Reply([IAC, DONT, 31])]); // window size
        assert_eq!(decode(&[IAC, DO, 5]), vec![Event::Reply([IAC, WONT, 5])]); // status
        assert_eq!(decode(&[IAC, WILL, OPT_ECHO]), vec![Event::Reply([IAC, DONT, OPT_ECHO])]);
    }

    #[test]
    fn subnegotiations_are_skipped() {
        let mut bytes = vec![b'a', IAC, SB, 31, 0, 80, 0, 24, IAC, SE, b'b'];
        assert_eq!(text(&bytes), b"ab");
        // An IAC IAC inside is data of the subnegotiation, not its end.
        bytes = vec![IAC, SB, 24, IAC, IAC, b'x', IAC, SE, b'c'];
        assert_eq!(text(&bytes), b"c");
        // One that never ends swallows everything, and stores nothing.
        let mut d = Decoder::new();
        d.feed(IAC);
        d.feed(SB);
        for _ in 0..100_000 {
            assert_eq!(d.feed(b'x'), None);
        }
    }

    #[test]
    fn commands() {
        assert_eq!(decode(&[IAC, INTERRUPT_PROCESS]), vec![Event::Interrupt]);
        assert_eq!(decode(&[IAC, BREAK]), vec![Event::Interrupt]);
        assert_eq!(decode(&[IAC, END_OF_FILE]), vec![Event::EndOfFile]);
        assert_eq!(decode(&[IAC, 241, IAC, 249, b'z']), vec![Event::Byte(b'z')]); // NOP, go ahead
        assert_eq!(decode(&[IAC, IAC]), vec![Event::Byte(255)]);
    }

    #[test]
    fn truncated_input_gives_nothing_and_recovers() {
        // Every sequence cut off at every point: no event from the cut part, no panic.
        let sequences: [&[u8]; 4] = [&[IAC, DO, 5], &[IAC, WILL, 31], &[IAC, SB, 31, 0, 80, IAC, SE], &[IAC, IAC]];
        for seq in sequences {
            for cut in 1..seq.len() {
                assert_eq!(decode(&seq[..cut]), vec![], "{seq:?} cut at {cut}");
            }
        }
        // The rest may arrive in a later packet.
        let mut d = Decoder::new();
        assert_eq!(d.feed(IAC), None);
        assert_eq!(d.feed(DO), None);
        assert_eq!(d.feed(5), Some(Event::Reply([IAC, WONT, 5])));
        assert_eq!(d.feed(b'k'), Some(Event::Byte(b'k')));
    }

    #[test]
    fn every_byte_pair_is_safe() {
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                let mut d = Decoder::new();
                d.feed(a);
                d.feed(b);
                d.feed(b'x');
            }
        }
    }

    #[test]
    fn encoding() {
        let mut out = Vec::new();
        encode(b"one\ntwo\n", &mut out);
        assert_eq!(out, b"one\r\ntwo\r\n");
        out.clear();
        encode(&[b'a', 255], &mut out);
        assert_eq!(out, [b'a', 255, 255]);
    }
}
