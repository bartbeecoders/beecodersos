//! TCP: reliable, ordered byte streams (RFC 793 / 9293, kept small).
//!
//! What it does: the three-way handshake (connecting, and listening for the
//! tests and later for servers), sequence numbers and acknowledgements,
//! retransmission with a measured timeout and backoff, fast retransmit after
//! three duplicate ACKs, the receive window (flow control) with zero-window
//! probes, orderly close in both directions, and resets.
//!
//! What it leaves out: out-of-order segments are dropped and re-sent by the
//! peer (no reassembly queue), no selective ACK, no window scaling, no
//! timestamps, and only a simple congestion rule (the send window grows by one
//! segment per ACK and collapses to one segment on a timeout).
//!
//! Sequence space of one direction:   SYN | data bytes ... | FIN
//! `send_buf[0]` is the byte with sequence number `snd_una` (once the SYN is
//! acknowledged); a FIN, when queued, sits right after the last byte.

use crate::stack::{Error, Stack};
use crate::wire::{self, Tcp, TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN};
use crate::Ipv4;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

const MAX_CONNECTIONS: usize = 8;
/// Connections on a listening port that nobody accepted yet.
const BACKLOG: usize = 4;
pub const SEND_BUFFER: usize = 16 * 1024;
pub const RECV_BUFFER: usize = 16 * 1024;
const OUR_MSS: u16 = 1460;
const DEFAULT_MSS: u16 = 536;
const RTO_INITIAL_MS: u64 = 1_000;
const RTO_MIN_MS: u64 = 200;
const RTO_MAX_MS: u64 = 16_000;
const SYN_RETRIES: u8 = 5;
const DATA_RETRIES: u8 = 8;
/// How long a closed connection lingers to absorb late segments.
const TIME_WAIT_MS: u64 = 2_000;
const MAX_CWND_SEGMENTS: usize = 32;
/// A released connection gets this long to finish closing before it is reset.
const LINGER_MS: u64 = 10_000;

/// Identifies a connection of this stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpHandle(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Both sides closed in an orderly way.
    Normal,
    /// The other side refused the connection (nothing listens on that port).
    Refused,
    /// The other side reset an established connection.
    Reset,
    /// No answer, after several retransmissions.
    TimedOut,
    /// The destination can't be reached (no ARP answer, or a router said so).
    Unreachable,
    /// We gave up the connection ourselves.
    Aborted,
}

impl CloseReason {
    pub fn message(self) -> &'static str {
        match self {
            CloseReason::Normal => "connection closed",
            CloseReason::Refused => "connection refused (nothing is listening on that port)",
            CloseReason::Reset => "connection reset by the other side",
            CloseReason::TimedOut => "connection timed out (no answer)",
            CloseReason::Unreachable => "host unreachable",
            CloseReason::Aborted => "connection aborted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpStatus {
    Connecting,
    /// Data can flow both ways.
    Open,
    /// The other side finished sending; we can still send.
    PeerClosed,
    /// We finished sending; the close is being completed (data may still arrive).
    Closing,
    Closed(CloseReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpError {
    BadHandle,
    /// Not (or no longer) possible in the connection's state.
    NotOpen,
    /// The connection is over; for `tcp_recv`, `Closed(Normal)` is the end of the data.
    Closed(CloseReason),
    Stack(Error),
}

impl TcpError {
    pub fn message(&self) -> &'static str {
        match self {
            TcpError::BadHandle => "no such connection",
            TcpError::NotOpen => "the connection is not open",
            TcpError::Closed(r) => r.message(),
            TcpError::Stack(e) => e.message(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    SynSent,
    SynReceived,
    Established,
    /// We sent FIN, waiting for its ACK (and their FIN).
    FinWait1,
    /// Our FIN is acknowledged, waiting for their FIN.
    FinWait2,
    /// They sent FIN; we can still send.
    CloseWait,
    /// Both sent FIN at the same time; waiting for the ACK of ours.
    Closing,
    /// They closed first, then we sent FIN; waiting for its ACK.
    LastAck,
    TimeWait,
    Closed(CloseReason),
}

trait ClearIf {
    fn clear_if(&mut self, condition: bool);
}

impl ClearIf for VecDeque<u8> {
    /// Throw away data nobody will read, so the window stays open while closing.
    fn clear_if(&mut self, condition: bool) {
        if condition {
            self.clear();
        }
    }
}

/// `a` is before `b` in sequence space (which wraps around).
fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// A segment to transmit, before addresses and checksum are added.
struct Segment {
    flags: u8,
    seq: u32,
    ack: u32,
    window: u16,
    mss: Option<u16>,
    data: Vec<u8>,
}

struct Connection {
    id: u32,
    local_ip: Ipv4,
    local_port: u16,
    remote_ip: Ipv4,
    remote_port: u16,
    state: State,
    /// Created by a listener and not handed out by `tcp_accept` yet.
    unaccepted: bool,
    /// Released by its user while closing: the stack finishes the close and
    /// frees it, at this time at the latest.
    released_until: Option<u64>,

    // Sending.
    iss: u32,
    /// Oldest unacknowledged sequence number.
    snd_una: u32,
    /// Next sequence number to send (moves back to `snd_una` to retransmit).
    snd_nxt: u32,
    /// Highest sequence number ever sent, plus one.
    snd_max: u32,
    /// The peer's receive window.
    snd_wnd: usize,
    /// Congestion window, in bytes.
    cwnd: usize,
    mss: usize,
    send_buf: VecDeque<u8>,
    /// The user closed: a FIN follows the buffered data.
    fin_queued: bool,
    dup_acks: u8,

    // Receiving.
    rcv_nxt: u32,
    recv_buf: VecDeque<u8>,
    peer_fin: bool,
    need_ack: bool,
    /// The window we last told the peer.
    advertised: usize,

    // Timers (milliseconds).
    rto: u64,
    timer: Option<u64>,
    retries: u8,
    /// Smoothed round-trip time and its variation.
    srtt: Option<(u64, u64)>,
    /// A segment being timed: (sequence number after it, when it was sent).
    rtt_probe: Option<(u32, u64)>,
    time_wait_until: u64,
}

impl Connection {
    fn new(id: u32, local: (Ipv4, u16), remote: (Ipv4, u16), state: State, iss: u32) -> Self {
        Connection {
            id,
            local_ip: local.0,
            local_port: local.1,
            remote_ip: remote.0,
            remote_port: remote.1,
            state,
            unaccepted: false,
            released_until: None,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_max: iss,
            snd_wnd: 0,
            cwnd: 4 * DEFAULT_MSS as usize,
            mss: DEFAULT_MSS as usize,
            send_buf: VecDeque::new(),
            fin_queued: false,
            dup_acks: 0,
            rcv_nxt: 0,
            recv_buf: VecDeque::new(),
            peer_fin: false,
            need_ack: false,
            advertised: RECV_BUFFER,
            rto: RTO_INITIAL_MS,
            timer: None,
            retries: 0,
            srtt: None,
            rtt_probe: None,
            time_wait_until: 0,
        }
    }

    fn status(&self) -> TcpStatus {
        match self.state {
            State::SynSent | State::SynReceived => TcpStatus::Connecting,
            State::Established => TcpStatus::Open,
            State::CloseWait => TcpStatus::PeerClosed,
            State::FinWait1 | State::FinWait2 | State::Closing | State::LastAck | State::TimeWait => TcpStatus::Closing,
            State::Closed(r) => TcpStatus::Closed(r),
        }
    }

    fn window(&mut self) -> u16 {
        self.advertised = (RECV_BUFFER - self.recv_buf.len()).min(u16::MAX as usize);
        self.advertised as u16
    }

    fn segment(&mut self, flags: u8, seq: u32, data: Vec<u8>) -> Segment {
        let handshake = flags & TCP_SYN != 0;
        Segment { flags, seq, ack: self.rcv_nxt, window: self.window(), mss: handshake.then_some(OUR_MSS), data }
    }

    fn close(&mut self, reason: CloseReason) {
        self.state = State::Closed(reason);
        self.timer = None;
        self.send_buf.clear();
        self.need_ack = false;
    }

    fn enter_time_wait(&mut self, now: u64) {
        self.state = State::TimeWait;
        self.timer = None;
        self.time_wait_until = now + TIME_WAIT_MS;
    }

    /// Our FIN (if sent) has been acknowledged.
    fn fin_acked(&mut self, now: u64) {
        match self.state {
            State::FinWait1 => self.state = State::FinWait2,
            State::Closing => self.enter_time_wait(now),
            State::LastAck => self.close(CloseReason::Normal),
            _ => {}
        }
    }

    /// Send whatever is due: SYN, data within the windows, FIN, a pending ACK.
    fn output(&mut self, now: u64, out: &mut Vec<Segment>) {
        match self.state {
            State::Closed(_) => return,
            State::SynSent | State::SynReceived => {
                if self.snd_nxt == self.iss {
                    let flags = if self.state == State::SynSent { TCP_SYN } else { TCP_SYN | TCP_ACK };
                    let seg = self.segment(flags, self.iss, Vec::new());
                    out.push(seg);
                    self.snd_nxt = self.iss.wrapping_add(1);
                    self.snd_max = self.snd_nxt;
                    if self.retries == 0 {
                        self.rtt_probe = Some((self.snd_nxt, now));
                    }
                    self.timer = Some(now + self.rto);
                }
                self.need_ack = false;
                return;
            }
            State::TimeWait => {}
            _ => {
                let total = self.send_buf.len();
                let mut offset = (self.snd_nxt.wrapping_sub(self.snd_una) as usize).min(total);
                let window = self.snd_wnd.min(self.cwnd);
                while offset < total && offset < window {
                    let n = (total - offset).min(window - offset).min(self.mss);
                    let data: Vec<u8> = self.send_buf.range(offset..offset + n).copied().collect();
                    let seq = self.snd_una.wrapping_add(offset as u32);
                    let push = if offset + n == total { TCP_PSH } else { 0 };
                    let seg = self.segment(TCP_ACK | push, seq, data);
                    out.push(seg);
                    offset += n;
                    let end = self.snd_una.wrapping_add(offset as u32);
                    if self.rtt_probe.is_none() && self.retries == 0 && !seq_lt(end, self.snd_max.wrapping_add(1)) {
                        self.rtt_probe = Some((end, now)); // time segments that are sent for the first time
                    }
                    self.snd_nxt = end;
                    self.need_ack = false;
                }
                // FIN, once every byte before it has been sent.
                let fin_seq = self.snd_una.wrapping_add(total as u32);
                if self.fin_queued && offset == total && self.snd_nxt == fin_seq {
                    let seg = self.segment(TCP_FIN | TCP_ACK, fin_seq, Vec::new());
                    out.push(seg);
                    self.snd_nxt = fin_seq.wrapping_add(1);
                    self.need_ack = false;
                }
                if seq_lt(self.snd_max, self.snd_nxt) {
                    self.snd_max = self.snd_nxt;
                }
                // Something is in flight, or data waits behind a closed window: keep the timer running.
                let waiting_on_window = self.snd_wnd == 0 && total > 0;
                if self.timer.is_none() && (self.snd_una != self.snd_max || waiting_on_window) {
                    self.timer = Some(now + self.rto);
                }
            }
        }
        if self.need_ack {
            let seg = self.segment(TCP_ACK, self.snd_nxt, Vec::new());
            out.push(seg);
            self.need_ack = false;
        }
    }

    fn update_rto(&mut self, sample: u64) {
        // RFC 6298, in milliseconds.
        let (srtt, rttvar) = match self.srtt {
            None => (sample, sample / 2),
            Some((srtt, rttvar)) => {
                let rttvar = (3 * rttvar + srtt.abs_diff(sample)) / 4;
                ((7 * srtt + sample) / 8, rttvar)
            }
        };
        self.srtt = Some((srtt, rttvar));
        self.rto = (srtt + (4 * rttvar).max(10)).clamp(RTO_MIN_MS, RTO_MAX_MS);
    }

    /// The retransmission timer (or the TIME-WAIT timer) ran out.
    fn on_timer(&mut self, now: u64, out: &mut Vec<Segment>) {
        if self.state == State::TimeWait {
            if now >= self.time_wait_until {
                self.close(CloseReason::Normal);
            }
            return;
        }
        if !self.timer.is_some_and(|t| now >= t) {
            return;
        }
        self.timer = None;
        self.rtt_probe = None; // never time a retransmitted segment
        self.dup_acks = 0;
        match self.state {
            State::Closed(_) => return,
            State::SynSent | State::SynReceived => {
                self.retries += 1;
                if self.retries > SYN_RETRIES {
                    return self.close(CloseReason::TimedOut);
                }
                self.rto = (self.rto * 2).min(RTO_MAX_MS);
                self.snd_nxt = self.iss; // send the SYN again
            }
            _ if self.snd_wnd == 0 && !self.send_buf.is_empty() && self.snd_max.wrapping_sub(self.snd_una) <= 1 => {
                // The peer's window is closed: probe it with one byte, so a
                // lost "window open" message can't stall the connection.
                let seg = self.segment(TCP_ACK, self.snd_una, alloc::vec![self.send_buf[0]]);
                out.push(seg);
                self.snd_nxt = self.snd_una.wrapping_add(1);
                self.snd_max = self.snd_nxt;
                self.rto = (self.rto * 2).min(RTO_MAX_MS);
                self.timer = Some(now + self.rto);
                return;
            }
            _ => {
                self.retries += 1;
                if self.retries > DATA_RETRIES {
                    let seg = self.segment(TCP_RST | TCP_ACK, self.snd_nxt, Vec::new());
                    out.push(seg);
                    return self.close(CloseReason::TimedOut);
                }
                self.rto = (self.rto * 2).min(RTO_MAX_MS);
                self.cwnd = self.mss; // assume congestion: start slowly again
                self.snd_nxt = self.snd_una; // go back and resend from the first unacknowledged byte
            }
        }
        self.output(now, out);
    }

    /// Handle the acknowledgement number and window of a segment. False: the
    /// segment acknowledges data we never sent and must be ignored entirely.
    fn on_ack(&mut self, seg: &Tcp, now: u64) -> bool {
        if seq_lt(self.snd_max, seg.ack) {
            self.need_ack = true;
            return false;
        }
        let acked = seg.ack.wrapping_sub(self.snd_una);
        if acked == 0 || seq_lt(seg.ack, self.snd_una) {
            let duplicate = acked == 0
                && seg.data.is_empty()
                && !seg.has(TCP_FIN)
                && seg.window as usize == self.snd_wnd
                && seg.window != 0 // answers to window probes are not a sign of loss
                && self.snd_una != self.snd_max;
            if duplicate {
                // The peer got something out of order: a segment was probably lost.
                self.dup_acks = self.dup_acks.saturating_add(1);
                if self.dup_acks == 3 {
                    self.cwnd = (self.cwnd / 2).max(2 * self.mss);
                    self.snd_nxt = self.snd_una; // fast retransmit
                    self.rtt_probe = None;
                }
            } else if acked == 0 {
                self.snd_wnd = seg.window as usize;
            }
            return true;
        }
        // New data is acknowledged.
        let data_acked = (acked as usize).min(self.send_buf.len());
        self.send_buf.drain(..data_acked);
        let fin_acked = acked as usize > data_acked && self.fin_queued;
        self.snd_una = seg.ack;
        if seq_lt(self.snd_nxt, self.snd_una) {
            self.snd_nxt = self.snd_una;
        }
        self.snd_wnd = seg.window as usize;
        self.retries = 0;
        self.dup_acks = 0;
        match self.rtt_probe {
            Some((end, sent)) if !seq_lt(seg.ack, end) => {
                self.rtt_probe = None;
                self.update_rto(now.saturating_sub(sent).max(1));
            }
            _ => {
                if let Some((srtt, rttvar)) = self.srtt {
                    self.rto = (srtt + (4 * rttvar).max(10)).clamp(RTO_MIN_MS, RTO_MAX_MS); // undo the backoff
                }
            }
        }
        self.cwnd = (self.cwnd + self.mss).min(MAX_CWND_SEGMENTS * self.mss);
        self.timer = (self.snd_una != self.snd_max).then_some(now + self.rto);
        if fin_acked {
            self.fin_queued = false; // done: it must not be sent again
            self.fin_acked(now);
        }
        true
    }

    /// A segment for this connection arrived.
    fn receive(&mut self, seg: &Tcp, now: u64, out: &mut Vec<Segment>) {
        match self.state {
            State::Closed(_) => return,
            State::SynSent => {
                if seg.has(TCP_ACK) && seg.ack != self.iss.wrapping_add(1) {
                    if !seg.has(TCP_RST) {
                        // An ACK for something we never sent: reset it.
                        out.push(Segment { flags: TCP_RST, seq: seg.ack, ack: 0, window: 0, mss: None, data: Vec::new() });
                    }
                    return;
                }
                if seg.has(TCP_RST) {
                    if seg.has(TCP_ACK) {
                        self.close(CloseReason::Refused);
                    }
                    return;
                }
                if seg.has(TCP_SYN) && seg.has(TCP_ACK) {
                    self.rcv_nxt = seg.seq.wrapping_add(1);
                    self.snd_una = seg.ack;
                    self.snd_wnd = seg.window as usize;
                    self.mss = seg.mss.unwrap_or(DEFAULT_MSS).clamp(64, OUR_MSS) as usize;
                    self.cwnd = 4 * self.mss;
                    self.state = State::Established;
                    self.timer = None;
                    self.retries = 0;
                    if let Some((_, sent)) = self.rtt_probe.take() {
                        self.update_rto(now.saturating_sub(sent).max(1));
                    }
                    self.need_ack = true;
                }
                return;
            }
            State::SynReceived => {
                if seg.has(TCP_RST) {
                    if seg.seq == self.rcv_nxt {
                        self.close(CloseReason::Reset);
                    }
                    return;
                }
                if seg.has(TCP_SYN) {
                    self.snd_nxt = self.iss; // our SYN+ACK was lost: send it again
                    return;
                }
                if !seg.has(TCP_ACK) || seg.ack != self.snd_max {
                    return;
                }
                self.snd_una = seg.ack;
                self.snd_wnd = seg.window as usize;
                self.state = State::Established;
                self.timer = None;
                self.retries = 0;
                if let Some((_, sent)) = self.rtt_probe.take() {
                    self.update_rto(now.saturating_sub(sent).max(1));
                }
                // fall through: the ACK may carry data
            }
            _ => {}
        }

        if seg.has(TCP_RST) {
            // Only a reset at exactly the expected sequence number counts
            // (a guessed one from an attacker almost never matches).
            if seg.seq == self.rcv_nxt {
                self.close(CloseReason::Reset);
            }
            return;
        }
        if seg.has(TCP_SYN) {
            self.need_ack = true; // a stray SYN: tell the peer where we are
            return;
        }
        if !seg.has(TCP_ACK) {
            return;
        }

        // Line the segment up with what we expect next.
        let mut data = seg.data;
        let mut fin = seg.has(TCP_FIN);
        let behind = self.rcv_nxt.wrapping_sub(seg.seq) as i32;
        let in_order = if behind > 0 {
            // Starts with bytes we already have (a retransmission).
            let behind = behind as usize;
            if behind >= data.len() + fin as usize {
                // Nothing new (a retransmission whose ACK was lost, or a
                // keep-alive probe): repeat our ACK.
                self.need_ack = true;
                data = &[];
                fin = false;
                false
            } else {
                data = &data[behind.min(data.len())..];
                true
            }
        } else if behind < 0 {
            self.need_ack = true; // a gap: this (duplicate) ACK asks for the missing part
            false
        } else {
            true
        };

        if !self.on_ack(seg, now) || matches!(self.state, State::Closed(_)) {
            return;
        }

        let can_receive = matches!(self.state, State::Established | State::FinWait1 | State::FinWait2);
        if in_order && can_receive {
            let accepted = data.len().min(RECV_BUFFER - self.recv_buf.len());
            self.recv_buf.extend(&data[..accepted]);
            self.rcv_nxt = self.rcv_nxt.wrapping_add(accepted as u32);
            if !data.is_empty() {
                self.need_ack = true;
            }
            if fin && accepted == data.len() {
                self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                self.peer_fin = true;
                self.need_ack = true;
                match self.state {
                    State::Established => self.state = State::CloseWait,
                    State::FinWait1 => self.state = State::Closing, // our FIN is not acknowledged yet
                    State::FinWait2 => self.enter_time_wait(now),
                    _ => {}
                }
            }
        } else if in_order && (fin || !data.is_empty()) {
            self.need_ack = true; // e.g. their FIN again while we linger in TIME-WAIT
        }
    }
}

#[derive(Default)]
pub(crate) struct TcpLayer {
    connections: Vec<Connection>,
    listeners: Vec<u16>,
    next_id: u32,
}

impl Stack {
    fn tcp_index(&self, h: TcpHandle) -> Result<usize, TcpError> {
        self.tcp.connections.iter().position(|c| c.id == h.0).ok_or(TcpError::BadHandle)
    }

    fn tcp_emit(&mut self, index: usize, segments: Vec<Segment>, now_ms: u64) {
        let c = &self.tcp.connections[index];
        let (local_ip, local_port, remote_ip, remote_port) = (c.local_ip, c.local_port, c.remote_ip, c.remote_port);
        for s in segments {
            let t = Tcp { src_port: local_port, dst_port: remote_port, seq: s.seq, ack: s.ack, flags: s.flags, window: s.window, mss: s.mss, data: &s.data };
            // A segment that can't be sent now (e.g. the ARP queue is full) is
            // simply lost: the retransmission timer covers it.
            let _ = self.send_ipv4(remote_ip, wire::PROTO_TCP, &wire::build_tcp(local_ip, remote_ip, &t), now_ms);
        }
    }

    /// Run `f` on a connection, then send what it produced.
    fn tcp_with<R>(&mut self, index: usize, now_ms: u64, f: impl FnOnce(&mut Connection, &mut Vec<Segment>) -> R) -> R {
        let mut out = Vec::new();
        let c = &mut self.tcp.connections[index];
        let result = f(c, &mut out);
        c.output(now_ms, &mut out);
        self.tcp_emit(index, out, now_ms);
        result
    }

    fn tcp_new_id(&mut self) -> u32 {
        self.tcp.next_id = self.tcp.next_id.wrapping_add(1);
        self.tcp.next_id
    }

    /// Start connecting to `dst:port`. The status is `Connecting` until the
    /// handshake completes or fails.
    pub fn tcp_connect(&mut self, dst: Ipv4, port: u16, now_ms: u64) -> Result<TcpHandle, Error> {
        let local_ip = self.config.ok_or(Error::NoAddress)?.ip;
        if self.tcp.connections.len() >= MAX_CONNECTIONS {
            return Err(Error::NoSockets);
        }
        let local_port = loop {
            let p = 49152 + (self.random() % 16384) as u16;
            if !self.tcp.connections.iter().any(|c| c.local_port == p) {
                break p;
            }
        };
        let (id, iss) = (self.tcp_new_id(), self.random());
        self.tcp.connections.push(Connection::new(id, (local_ip, local_port), (dst, port), State::SynSent, iss));
        let index = self.tcp.connections.len() - 1;
        self.tcp_with(index, now_ms, |_, _| ());
        Ok(TcpHandle(id))
    }

    /// Accept connections on a port (they are collected with `tcp_accept`).
    pub fn tcp_listen(&mut self, port: u16) -> Result<(), Error> {
        if self.tcp.listeners.contains(&port) {
            return Err(Error::PortInUse);
        }
        self.tcp.listeners.push(port);
        Ok(())
    }

    pub fn tcp_unlisten(&mut self, port: u16) {
        self.tcp.listeners.retain(|p| *p != port);
    }

    /// The next established connection on a listening port.
    pub fn tcp_accept(&mut self, port: u16) -> Option<TcpHandle> {
        let c = self.tcp.connections.iter_mut().find(|c| {
            c.unaccepted && c.local_port == port && !matches!(c.state, State::SynReceived | State::Closed(_))
        })?;
        c.unaccepted = false;
        Some(TcpHandle(c.id))
    }

    pub fn tcp_status(&self, h: TcpHandle) -> Result<TcpStatus, TcpError> {
        Ok(self.tcp.connections[self.tcp_index(h)?].status())
    }

    /// The other end of a connection.
    pub fn tcp_peer(&self, h: TcpHandle) -> Result<(Ipv4, u16), TcpError> {
        let c = &self.tcp.connections[self.tcp_index(h)?];
        Ok((c.remote_ip, c.remote_port))
    }

    /// Queue data for sending. Returns how many bytes were taken (less than
    /// `data.len()` when the send buffer is full: try the rest later).
    pub fn tcp_send(&mut self, h: TcpHandle, data: &[u8], now_ms: u64) -> Result<usize, TcpError> {
        let index = self.tcp_index(h)?;
        let c = &self.tcp.connections[index];
        match c.state {
            State::Established | State::CloseWait => {}
            State::Closed(r) => return Err(TcpError::Closed(r)),
            _ => return Err(TcpError::NotOpen),
        }
        let n = data.len().min(SEND_BUFFER - c.send_buf.len());
        Ok(self.tcp_with(index, now_ms, |c, _| {
            c.send_buf.extend(&data[..n]);
            n
        }))
    }

    /// Bytes queued by `tcp_send` that the other side has not acknowledged yet.
    pub fn tcp_unsent(&self, h: TcpHandle) -> Result<usize, TcpError> {
        Ok(self.tcp.connections[self.tcp_index(h)?].send_buf.len())
    }

    /// Read received data. `Ok(0)`: nothing yet. `Err(Closed(Normal))`: the
    /// other side finished and everything was read. Other errors: the
    /// connection failed.
    pub fn tcp_recv(&mut self, h: TcpHandle, buf: &mut [u8], now_ms: u64) -> Result<usize, TcpError> {
        let index = self.tcp_index(h)?;
        let c = &mut self.tcp.connections[index];
        if c.recv_buf.is_empty() {
            return match c.state {
                State::Closed(reason) => Err(TcpError::Closed(reason)),
                _ if c.peer_fin => Err(TcpError::Closed(CloseReason::Normal)),
                _ => Ok(0),
            };
        }
        let n = buf.len().min(c.recv_buf.len());
        for (dst, src) in buf.iter_mut().zip(c.recv_buf.drain(..n)) {
            *dst = src;
        }
        // Tell the peer when a nearly closed window has opened up again.
        let free = RECV_BUFFER - c.recv_buf.len();
        if c.advertised < 2 * c.mss && free >= 2 * c.mss && !matches!(c.state, State::Closed(_)) {
            self.tcp_with(index, now_ms, |c, _| c.need_ack = true);
        }
        Ok(n)
    }

    /// Finish sending: buffered data goes out, then a FIN. Data can still be received.
    pub fn tcp_close(&mut self, h: TcpHandle, now_ms: u64) -> Result<(), TcpError> {
        let index = self.tcp_index(h)?;
        self.tcp_with(index, now_ms, |c, _| match c.state {
            State::Established => {
                c.state = State::FinWait1;
                c.fin_queued = true;
            }
            State::CloseWait => {
                c.state = State::LastAck;
                c.fin_queued = true;
            }
            State::SynSent | State::SynReceived => c.close(CloseReason::Aborted),
            _ => {}
        });
        Ok(())
    }

    /// Drop the connection at once, telling the other side with a reset.
    pub fn tcp_abort(&mut self, h: TcpHandle, now_ms: u64) -> Result<(), TcpError> {
        let index = self.tcp_index(h)?;
        self.tcp_with(index, now_ms, |c, out| {
            if !matches!(c.state, State::Closed(_) | State::SynSent | State::TimeWait) {
                let seg = c.segment(TCP_RST | TCP_ACK, c.snd_nxt, Vec::new());
                out.push(seg);
            }
            if !matches!(c.state, State::Closed(_)) {
                c.close(CloseReason::Aborted);
            }
        });
        Ok(())
    }

    /// Give up a handle. Every handle from `tcp_connect` / `tcp_accept` must
    /// be released to free its slot. A connection that was closed with
    /// `tcp_close` finishes in the background (buffered data is still
    /// delivered); one that is still open is reset.
    pub fn tcp_release(&mut self, h: TcpHandle, now_ms: u64) {
        let Ok(index) = self.tcp_index(h) else { return };
        let c = &mut self.tcp.connections[index];
        match c.state {
            State::Closed(_) | State::TimeWait => {
                self.tcp.connections.remove(index);
            }
            State::FinWait1 | State::FinWait2 | State::Closing | State::LastAck => {
                c.released_until = Some(now_ms + LINGER_MS);
                c.recv_buf.clear(); // nobody will read it
            }
            _ => {
                let _ = self.tcp_abort(h, now_ms);
                self.tcp.connections.remove(index);
            }
        }
    }

    pub fn tcp_connection_count(&self) -> usize {
        self.tcp.connections.len()
    }

    pub(crate) fn tcp_poll(&mut self, now_ms: u64) {
        for index in 0..self.tcp.connections.len() {
            self.tcp_with(index, now_ms, |c, out| c.on_timer(now_ms, out));
        }
        // Released connections that finished closing, or ran out of time.
        let mut index = 0;
        while index < self.tcp.connections.len() {
            let c = &mut self.tcp.connections[index];
            c.recv_buf.clear_if(c.released_until.is_some());
            match (c.released_until, c.state) {
                (Some(_), State::Closed(_) | State::TimeWait) => {
                    self.tcp.connections.remove(index);
                }
                (Some(deadline), _) if now_ms >= deadline => {
                    self.tcp_with(index, now_ms, |c, out| {
                        let seg = c.segment(TCP_RST | TCP_ACK, c.snd_nxt, Vec::new());
                        out.push(seg);
                    });
                    self.tcp.connections.remove(index);
                }
                _ => index += 1,
            }
        }
        // Connections from a listener that failed before anyone accepted them.
        self.tcp.connections.retain(|c| !(c.unaccepted && matches!(c.state, State::Closed(_))));
    }

    /// `dst` can't be reached: fail connections that are still being set up.
    pub(crate) fn tcp_unreachable(&mut self, dst: Ipv4) {
        for c in &mut self.tcp.connections {
            if c.remote_ip == dst && c.state == State::SynSent {
                c.close(CloseReason::Unreachable);
            }
        }
    }

    /// A TCP segment arrived for our address.
    pub(crate) fn tcp_receive(&mut self, src: Ipv4, dst: Ipv4, data: &[u8], now_ms: u64) {
        let Some(seg) = wire::parse_tcp(data, src, dst) else {
            self.stats.dropped += 1;
            return;
        };
        let found = self.tcp.connections.iter().position(|c| {
            c.local_port == seg.dst_port && c.remote_port == seg.src_port && c.remote_ip == src && c.local_ip == dst
        });
        if let Some(index) = found {
            return self.tcp_with(index, now_ms, |c, out| c.receive(&seg, now_ms, out));
        }
        // A new connection to a port we listen on?
        let syn_only = seg.has(TCP_SYN) && !seg.has(TCP_ACK) && !seg.has(TCP_RST);
        if syn_only && self.tcp.listeners.contains(&seg.dst_port) {
            let waiting = self.tcp.connections.iter().filter(|c| c.unaccepted && c.local_port == seg.dst_port).count();
            if self.tcp.connections.len() >= MAX_CONNECTIONS || waiting >= BACKLOG {
                return; // full: stay silent, the client will try again
            }
            let (id, iss) = (self.tcp_new_id(), self.random());
            let mut c = Connection::new(id, (dst, seg.dst_port), (src, seg.src_port), State::SynReceived, iss);
            c.unaccepted = true;
            c.rcv_nxt = seg.seq.wrapping_add(1);
            c.snd_wnd = seg.window as usize;
            c.mss = seg.mss.unwrap_or(DEFAULT_MSS).clamp(64, OUR_MSS) as usize;
            c.cwnd = 4 * c.mss;
            self.tcp.connections.push(c);
            let index = self.tcp.connections.len() - 1;
            return self.tcp_with(index, now_ms, |_, _| ());
        }
        // Nobody wants this segment: answer with a reset (never to a reset).
        if seg.has(TCP_RST) {
            return;
        }
        let reply = if seg.has(TCP_ACK) {
            Tcp { src_port: seg.dst_port, dst_port: seg.src_port, seq: seg.ack, ack: 0, flags: TCP_RST, window: 0, mss: None, data: &[] }
        } else {
            let len = seg.data.len() as u32 + seg.has(TCP_SYN) as u32 + seg.has(TCP_FIN) as u32;
            Tcp { src_port: seg.dst_port, dst_port: seg.src_port, seq: 0, ack: seg.seq.wrapping_add(len), flags: TCP_RST | TCP_ACK, window: 0, mss: None, data: &[] }
        };
        self.stats.tcp_resets_sent += 1;
        let _ = self.send_ipv4(src, wire::PROTO_TCP, &wire::build_tcp(dst, src, &reply), now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::Config;
    use crate::wire::*;
    use crate::Mac;

    const MAC_A: Mac = [0x52, 0x54, 0x00, 0xbe, 0xe0, 0x01];
    const MAC_B: Mac = [0x02, 0, 0, 0, 0, 0x0b];
    const IP_A: Ipv4 = Ipv4([10, 0, 0, 2]);
    const IP_B: Ipv4 = Ipv4([10, 0, 0, 3]);

    /// Two stacks joined by a link that can lose, duplicate, delay and reorder frames.
    struct Link {
        a: Stack,
        b: Stack,
        now: u64,
        rng: u64,
        /// Percent of TCP frames dropped / duplicated.
        loss: u64,
        duplicate: u64,
        /// Extra random delay per frame (reorders them).
        jitter_ms: u64,
        /// Frames from a to b that pass this test are delivered; others vanish.
        a_to_b_filter: fn(&[u8]) -> bool,
        in_flight: Vec<(u64, bool, Vec<u8>)>,
        /// Every TCP segment sent by a: (time, flags, seq, ack, data length).
        sent_by_a: Vec<(u64, u8, u32, u32, usize)>,
        a_port: u16,
    }

    fn tcp_of(frame: &[u8]) -> Option<(Ipv4, Ipv4, Vec<u8>)> {
        let (eth, payload) = parse_eth(frame)?;
        if eth.ethertype != ETHERTYPE_IPV4 {
            return None;
        }
        let (ip, data) = parse_ipv4(payload).ok()?;
        (ip.protocol == PROTO_TCP).then(|| (ip.src, ip.dst, data.to_vec()))
    }

    impl Link {
        fn new() -> Link {
            let mut a = Stack::new(MAC_A, 0xA11CE);
            let mut b = Stack::new(MAC_B, 0xB0B);
            a.set_config(Some(Config { ip: IP_A, prefix: 24, gateway: None }));
            b.set_config(Some(Config { ip: IP_B, prefix: 24, gateway: None }));
            Link { a, b, now: 0, rng: 0x9E37_79B9_7F4A_7C15, loss: 0, duplicate: 0, jitter_ms: 0,
                   a_to_b_filter: |_| true, in_flight: Vec::new(), sent_by_a: Vec::new(), a_port: 0 }
        }

        fn random(&mut self, below: u64) -> u64 {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            self.rng % below.max(1)
        }

        /// Advance time by 10 ms: move frames, run timers.
        fn step(&mut self) {
            self.now += 10;
            let mut new = Vec::new();
            while let Some(f) = self.a.transmit() {
                if let Some((src, dst, seg)) = tcp_of(&f) {
                    let t = parse_tcp(&seg, src, dst).expect("a sends valid TCP");
                    self.a_port = t.src_port;
                    self.sent_by_a.push((self.now, t.flags, t.seq, t.ack, t.data.len()));
                }
                if (self.a_to_b_filter)(&f) {
                    new.push((true, f));
                }
            }
            while let Some(f) = self.b.transmit() {
                new.push((false, f));
            }
            for (to_b, f) in new {
                let is_tcp = tcp_of(&f).is_some();
                if is_tcp && self.random(100) < self.loss {
                    continue;
                }
                let copies = if is_tcp && self.random(100) < self.duplicate { 2 } else { 1 };
                for _ in 0..copies {
                    let at = self.now + self.random(self.jitter_ms + 1);
                    self.in_flight.push((at, to_b, f.clone()));
                }
            }
            let (due, later): (Vec<_>, Vec<_>) = core::mem::take(&mut self.in_flight).into_iter().partition(|(at, ..)| *at <= self.now);
            self.in_flight = later;
            for (_, to_b, f) in due {
                if to_b { self.b.receive(&f, self.now) } else { self.a.receive(&f, self.now) }
            }
            self.a.poll(self.now);
            self.b.poll(self.now);
        }

        /// Step until `done` holds; panics after `max_ms` of simulated time.
        fn run(&mut self, max_ms: u64, what: &str, mut done: impl FnMut(&mut Link) -> bool) {
            let end = self.now + max_ms;
            while !done(self) {
                assert!(self.now < end, "timed out waiting for: {what}");
                self.step();
            }
        }

        fn settle(&mut self, ms: u64) {
            let end = self.now + ms;
            while self.now < end {
                self.step();
            }
        }

        /// Connect a (client) to b (server, port 80); returns both handles.
        fn connect(&mut self) -> (TcpHandle, TcpHandle) {
            let _ = self.b.tcp_listen(80);
            let client = self.a.tcp_connect(IP_B, 80, self.now).unwrap();
            let mut server = None;
            self.run(60_000, "the connection", |l| {
                server = server.or_else(|| l.b.tcp_accept(80));
                server.is_some() && l.a.tcp_status(client) == Ok(TcpStatus::Open)
            });
            (client, server.unwrap())
        }
    }

    fn read_all(s: &mut Stack, h: TcpHandle, into: &mut Vec<u8>, now: u64) -> bool {
        let mut buf = [0u8; 2048];
        loop {
            match s.tcp_recv(h, &mut buf, now) {
                Ok(0) => return false,
                Ok(n) => into.extend_from_slice(&buf[..n]),
                Err(TcpError::Closed(CloseReason::Normal)) => return true,
                Err(e) => panic!("receive failed: {e:?}"),
            }
        }
    }

    fn pattern(len: usize, salt: u32) -> Vec<u8> {
        (0..len as u32).map(|i| (i.wrapping_mul(2654435761).wrapping_add(salt) >> 13) as u8).collect()
    }

    #[test]
    fn connect_exchange_close() {
        let mut l = Link::new();
        let (c, s) = l.connect();
        // The handshake on the wire: SYN (with our MSS), then an ACK of the server's SYN.
        let (_, flags, seq, _, len) = l.sent_by_a[0];
        assert_eq!((flags, len), (TCP_SYN, 0));
        assert_eq!((l.sent_by_a[1].1, l.sent_by_a[1].2), (TCP_ACK, seq.wrapping_add(1)));
        assert_eq!(l.b.tcp_status(s), Ok(TcpStatus::Open));
        assert_eq!(l.b.tcp_peer(s).unwrap().0, IP_A);

        assert_eq!(l.a.tcp_send(c, b"hello", l.now), Ok(5));
        let mut got = Vec::new();
        l.run(1_000, "data at the server", |l| { read_all(&mut l.b, s, &mut got, l.now); got == b"hello" });
        assert_eq!(l.b.tcp_send(s, b"world!", l.now), Ok(6));
        let mut got = Vec::new();
        l.run(1_000, "data at the client", |l| { read_all(&mut l.a, c, &mut got, l.now); got == b"world!" });
        assert_eq!(l.a.tcp_unsent(c), Ok(0));

        // The client closes; the server sees the end of the data, then closes too.
        l.a.tcp_close(c, l.now).unwrap();
        assert_eq!(l.a.tcp_send(c, b"x", l.now), Err(TcpError::NotOpen));
        l.run(1_000, "the server noticing", |l| l.b.tcp_status(s) == Ok(TcpStatus::PeerClosed));
        let mut buf = [0u8; 8];
        assert_eq!(l.b.tcp_recv(s, &mut buf, l.now), Err(TcpError::Closed(CloseReason::Normal)));
        assert_eq!(l.a.tcp_status(c), Ok(TcpStatus::Closing));
        l.b.tcp_close(s, l.now).unwrap();
        l.run(1_000, "the server closed", |l| l.b.tcp_status(s) == Ok(TcpStatus::Closed(CloseReason::Normal)));
        // The side that closed first lingers briefly, then is closed too.
        l.run(TIME_WAIT_MS + 1_000, "the client closed", |l| l.a.tcp_status(c) == Ok(TcpStatus::Closed(CloseReason::Normal)));
        l.a.tcp_release(c, l.now);
        l.b.tcp_release(s, l.now);
        assert_eq!((l.a.tcp_connection_count(), l.b.tcp_connection_count()), (0, 0));
        // On a perfect link exactly one FIN is sent, and it is never repeated.
        assert_eq!(l.sent_by_a.iter().filter(|seg| seg.1 & TCP_FIN != 0).count(), 1);
        assert_eq!(l.a.tcp_status(c), Err(TcpError::BadHandle));
        assert_eq!((l.a.stats.tcp_resets_sent, l.b.stats.tcp_resets_sent), (0, 0));
    }

    #[test]
    fn refused_when_nothing_listens() {
        let mut l = Link::new();
        let c = l.a.tcp_connect(IP_B, 81, 0).unwrap();
        l.run(2_000, "the refusal", |l| l.a.tcp_status(c) == Ok(TcpStatus::Closed(CloseReason::Refused)));
        assert!(l.now < 200, "a refusal is immediate, not a timeout");
        assert_eq!(l.b.stats.tcp_resets_sent, 1);
        let mut buf = [0u8; 4];
        assert_eq!(l.a.tcp_recv(c, &mut buf, l.now), Err(TcpError::Closed(CloseReason::Refused)));
        assert_eq!(l.a.tcp_send(c, b"x", l.now), Err(TcpError::Closed(CloseReason::Refused)));
    }

    #[test]
    fn no_answer_times_out_with_backoff() {
        let mut l = Link::new();
        l.a_to_b_filter = |f| tcp_of(f).is_none(); // b answers ARP but never sees TCP
        let c = l.a.tcp_connect(IP_B, 80, 0).unwrap();
        l.run(120_000, "the timeout", |l| l.a.tcp_status(c) != Ok(TcpStatus::Connecting));
        assert_eq!(l.a.tcp_status(c), Ok(TcpStatus::Closed(CloseReason::TimedOut)));
        // SYN sent six times, the wait doubling: 1, 2, 4, 8, 16 s (capped), then 16 s more.
        let gaps: Vec<u64> = l.sent_by_a.windows(2).map(|w| (w[1].0 - w[0].0 + 500) / 1000).collect();
        assert_eq!(gaps, [1, 2, 4, 8, 16]);
        assert!(l.sent_by_a.iter().all(|s| s.1 == TCP_SYN && s.2 == l.sent_by_a[0].2));
    }

    #[test]
    fn unreachable_host_fails_fast() {
        let mut l = Link::new();
        let c = l.a.tcp_connect(Ipv4([10, 0, 0, 99]), 80, 0).unwrap(); // nobody has that address
        l.run(10_000, "the failure", |l| l.a.tcp_status(c) != Ok(TcpStatus::Connecting));
        assert_eq!(l.a.tcp_status(c), Ok(TcpStatus::Closed(CloseReason::Unreachable)));
        assert!(l.now <= 3_100);
        assert_eq!(Stack::new(MAC_A, 1).tcp_connect(IP_B, 80, 0), Err(Error::NoAddress));
    }

    /// Send `a_data` from a to b and `b_data` from b to a at once, then close both ways.
    fn transfer(l: &mut Link, a_data: &[u8], b_data: &[u8], max_ms: u64) {
        let (c, s) = l.connect();
        let (mut a_sent, mut b_sent, mut a_got, mut b_got) = (0, 0, Vec::new(), Vec::new());
        let (mut a_closed, mut b_closed, mut a_end, mut b_end) = (false, false, false, false);
        l.run(max_ms, "the transfer", |l| {
            if a_sent < a_data.len() {
                a_sent += l.a.tcp_send(c, &a_data[a_sent..], l.now).unwrap();
            } else if !a_closed {
                l.a.tcp_close(c, l.now).unwrap();
                a_closed = true;
            }
            if b_sent < b_data.len() {
                b_sent += l.b.tcp_send(s, &b_data[b_sent..], l.now).unwrap();
            } else if !b_closed {
                l.b.tcp_close(s, l.now).unwrap();
                b_closed = true;
            }
            a_end |= read_all(&mut l.a, c, &mut a_got, l.now);
            b_end |= read_all(&mut l.b, s, &mut b_got, l.now);
            a_end && b_end
        });
        assert_eq!(b_got.len(), a_data.len());
        assert!(b_got == a_data, "data from a to b is damaged");
        assert!(a_got == b_data, "data from b to a is damaged");
        // Both sides finish closing in an orderly way, even on a bad link.
        l.run(120_000, "the close", |l| {
            matches!(l.a.tcp_status(c), Ok(TcpStatus::Closed(CloseReason::Normal)))
                && matches!(l.b.tcp_status(s), Ok(TcpStatus::Closed(CloseReason::Normal)))
        });
    }

    #[test]
    fn large_transfer_on_a_perfect_link() {
        let mut l = Link::new();
        transfer(&mut l, &pattern(1 << 20, 1), &pattern(300_000, 2), 120_000);
        // With no loss nothing is retransmitted: every data byte is sent exactly once.
        let bytes: usize = l.sent_by_a.iter().map(|s| s.4).sum();
        assert_eq!(bytes, 1 << 20);
    }

    #[test]
    fn one_megabyte_over_a_lossy_link() {
        let mut l = Link::new();
        (l.loss, l.duplicate, l.jitter_ms) = (10, 5, 30); // 10% lost, 5% doubled, reordered
        transfer(&mut l, &pattern(1 << 20, 3), &pattern(200_000, 4), 3_600_000);
    }

    #[test]
    fn terrible_link_still_delivers() {
        let mut l = Link::new();
        (l.loss, l.duplicate, l.jitter_ms) = (30, 20, 200);
        transfer(&mut l, &pattern(40_000, 5), &pattern(40_000, 6), 3_600_000);
    }

    #[test]
    fn flow_control_and_window_probes() {
        let mut l = Link::new();
        let (c, s) = l.connect();
        let data = pattern(60_000, 7);
        let mut sent = l.a.tcp_send(c, &data, l.now).unwrap();
        assert_eq!(sent, SEND_BUFFER, "only what fits in the send buffer is taken");
        // The server application does not read: its buffer fills up...
        l.settle(1_000);
        assert_eq!(l.a.tcp_unsent(c), Ok(0));
        // ...and what the client sends next has to wait.
        sent += l.a.tcp_send(c, &data[sent..], l.now).unwrap();
        l.settle(120_000);
        assert_eq!(l.a.tcp_status(c), Ok(TcpStatus::Open), "a full receiver is not an error, however long it takes");
        assert!(l.a.tcp_unsent(c).unwrap() >= SEND_BUFFER - 1, "nothing more may be delivered");
        let before = l.sent_by_a.len();
        l.settle(60_000);
        let probes = l.sent_by_a.len() - before;
        assert!((1..=10).contains(&probes), "a closed window is probed now and then, not flooded ({probes})");
        // Now the application reads: everything flows again and arrives intact.
        let mut got = Vec::new();
        l.run(600_000, "the rest of the data", |l| {
            if sent < data.len() {
                sent += l.a.tcp_send(c, &data[sent..], l.now).unwrap();
            }
            read_all(&mut l.b, s, &mut got, l.now);
            got.len() == data.len()
        });
        assert!(got == data);
    }

    /// A forged segment "from b" to a's connection.
    fn forged(l: &Link, seq: u32, ack: u32, flags: u8, data: &[u8]) -> Vec<u8> {
        let t = Tcp { src_port: 80, dst_port: l.a_port, seq, ack, flags, window: 1000, mss: None, data };
        build_eth(&MAC_A, &MAC_B, ETHERTYPE_IPV4, &build_ipv4(IP_B, IP_A, PROTO_TCP, 64, 1, &build_tcp(IP_B, IP_A, &t)))
    }

    #[test]
    fn resets_must_match_exactly() {
        let mut l = Link::new();
        let (c, s) = l.connect();
        l.a.tcp_send(c, b"ping", l.now).unwrap();
        l.settle(200);
        let (_, _, a_seq, a_ack, a_len) = *l.sent_by_a.last().unwrap();
        // Blind resets with a wrong sequence number (even one inside the window) are ignored.
        for wrong in [a_ack.wrapping_add(1), a_ack.wrapping_add(1000), a_ack.wrapping_sub(1), 0, 0x8000_0000] {
            let f = forged(&l, wrong, 0, TCP_RST, b"");
            l.a.receive(&f, l.now);
        }
        // So are forged data with an impossible acknowledgement, and stray SYNs.
        let f = forged(&l, a_ack, a_seq.wrapping_add(a_len as u32).wrapping_add(50_000), TCP_ACK, b"evil");
        l.a.receive(&f, l.now);
        let f = forged(&l, 12345, 0, TCP_SYN, b"");
        l.a.receive(&f, l.now);
        l.settle(100);
        assert_eq!(l.a.tcp_status(c), Ok(TcpStatus::Open));
        let mut buf = [0u8; 16];
        assert_eq!(l.a.tcp_recv(c, &mut buf, l.now), Ok(0), "forged data must not be delivered");
        // The exact sequence number resets the connection.
        let f = forged(&l, a_ack, 0, TCP_RST, b"");
        l.a.receive(&f, l.now);
        assert_eq!(l.a.tcp_status(c), Ok(TcpStatus::Closed(CloseReason::Reset)));
        assert_eq!(l.b.tcp_status(s), Ok(TcpStatus::Open));
    }

    #[test]
    fn abort_resets_the_other_side() {
        let mut l = Link::new();
        let (c, s) = l.connect();
        l.a.tcp_abort(c, l.now).unwrap();
        assert_eq!(l.a.tcp_status(c), Ok(TcpStatus::Closed(CloseReason::Aborted)));
        l.run(1_000, "the reset", |l| l.b.tcp_status(s) == Ok(TcpStatus::Closed(CloseReason::Reset)));
        // Releasing a live connection aborts it too.
        let (c2, s2) = l.connect();
        l.a.tcp_release(c2, l.now);
        l.run(1_000, "the reset", |l| l.b.tcp_status(s2) == Ok(TcpStatus::Closed(CloseReason::Reset)));
    }

    #[test]
    fn half_close_keeps_the_other_direction_open() {
        let mut l = Link::new();
        let (c, s) = l.connect();
        l.a.tcp_send(c, b"request", l.now).unwrap();
        l.a.tcp_close(c, l.now).unwrap(); // "I have nothing more to say"
        l.run(1_000, "the server noticing", |l| l.b.tcp_status(s) == Ok(TcpStatus::PeerClosed));
        // The server can still answer, at length.
        let reply = pattern(20_000, 8);
        let (mut sent, mut got, mut end) = (0, Vec::new(), false);
        l.run(60_000, "the reply", |l| {
            if sent < reply.len() {
                sent += l.b.tcp_send(s, &reply[sent..], l.now).unwrap();
            } else {
                let _ = l.b.tcp_close(s, l.now);
            }
            end |= read_all(&mut l.a, c, &mut got, l.now);
            end
        });
        assert!(got == reply);
    }

    #[test]
    fn released_after_close_still_delivers_everything() {
        let mut l = Link::new();
        (l.loss, l.jitter_ms) = (10, 20);
        let (c, s) = l.connect();
        let data = pattern(SEND_BUFFER, 9);
        assert_eq!(l.a.tcp_send(c, &data, l.now), Ok(SEND_BUFFER));
        l.a.tcp_close(c, l.now).unwrap();
        l.a.tcp_release(c, l.now); // "send it and forget about it"
        let (mut got, mut end) = (Vec::new(), false);
        l.run(120_000, "all the data and the end", |l| {
            end |= read_all(&mut l.b, s, &mut got, l.now);
            end
        });
        assert!(got == data);
        // The server closes; the released connection disappears by itself.
        l.b.tcp_close(s, l.now).unwrap();
        l.run(60_000, "the cleanup", |l| l.a.tcp_connection_count() == 0);
        l.run(60_000, "the server side", |l| l.b.tcp_status(s) == Ok(TcpStatus::Closed(CloseReason::Normal)));

        // If the other side never finishes, the released connection is reset after a while.
        let mut l = Link::new();
        let (c, s) = l.connect();
        l.a.tcp_close(c, l.now).unwrap();
        l.a.tcp_release(c, l.now);
        l.run(LINGER_MS + 1_000, "the forced cleanup", |l| l.a.tcp_connection_count() == 0);
        l.run(1_000, "the reset", |l| l.b.tcp_status(s) == Ok(TcpStatus::Closed(CloseReason::Reset)));
    }

    #[test]
    fn both_close_at_once() {
        let mut l = Link::new();
        let (c, s) = l.connect();
        l.a.tcp_close(c, l.now).unwrap();
        l.b.tcp_close(s, l.now).unwrap();
        l.run(TIME_WAIT_MS + 2_000, "both closed", |l| {
            l.a.tcp_status(c) == Ok(TcpStatus::Closed(CloseReason::Normal)) && l.b.tcp_status(s) == Ok(TcpStatus::Closed(CloseReason::Normal))
        });
    }

    #[test]
    fn dead_peer_is_detected() {
        let mut l = Link::new();
        let (c, _s) = l.connect();
        l.a_to_b_filter = |_| false; // the cable is cut
        l.a.tcp_send(c, b"anyone there?", l.now).unwrap();
        l.run(300_000, "the timeout", |l| l.a.tcp_status(c) != Ok(TcpStatus::Open));
        assert_eq!(l.a.tcp_status(c), Ok(TcpStatus::Closed(CloseReason::TimedOut)));
    }

    #[test]
    fn limits_and_garbage() {
        let mut l = Link::new();
        l.b.tcp_listen(80).unwrap();
        assert_eq!(l.b.tcp_listen(80), Err(Error::PortInUse));
        let handles: Vec<_> = (0..MAX_CONNECTIONS).map(|_| l.a.tcp_connect(IP_B, 80, 0).unwrap()).collect();
        assert_eq!(l.a.tcp_connect(IP_B, 80, 0), Err(Error::NoSockets));
        l.settle(3_000);
        // The server takes at most BACKLOG connections nobody accepted; the rest keep trying.
        let open = handles.iter().filter(|h| l.a.tcp_status(**h) == Ok(TcpStatus::Open)).count();
        assert_eq!(open, BACKLOG);
        for _ in 0..BACKLOG {
            assert!(l.b.tcp_accept(80).is_some());
        }
        assert!(l.b.tcp_accept(80).is_none());
        l.settle(10_000);
        assert_eq!(handles.iter().filter(|h| l.a.tcp_status(**h) == Ok(TcpStatus::Open)).count(), MAX_CONNECTIONS);

        // Broken segments: cut off at every length, wrong checksum, random bytes. Nothing may panic.
        let (c, s) = (handles[0], l.b.tcp_accept(80).unwrap());
        let good = forged(&l, 1, 1, TCP_ACK, b"some data");
        let dropped = l.a.stats.dropped;
        for len in 34..good.len() {
            let mut f = good[..len].to_vec();
            let total = (len - 14) as u16; // keep the IP header consistent, so the TCP parser sees the cut
            f[16..18].copy_from_slice(&total.to_be_bytes());
            f[24] = 0;
            f[25] = 0;
            let sum = checksum(&f[14..34]);
            f[24..26].copy_from_slice(&sum.to_be_bytes());
            l.a.receive(&f, l.now);
        }
        assert_eq!(l.a.stats.dropped - dropped, (good.len() - 34) as u64);
        let mut x = 7u64;
        for _ in 0..3000 {
            let mut f = good.clone();
            for b in f[34..].iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            l.a.receive(&f, l.now);
        }
        assert_eq!(l.a.tcp_status(c), Ok(TcpStatus::Open));
        let _ = s;
    }

    #[test]
    fn segments_for_nobody_get_a_correct_reset() {
        let mut l = Link::new();
        l.settle(50);
        // Data with an ACK to a port without a connection: RST with seq = their ack.
        let t = Tcp { src_port: 4000, dst_port: 9, seq: 111, ack: 222, flags: TCP_ACK, window: 1, mss: None, data: b"abc" };
        let f = build_eth(&MAC_A, &MAC_B, ETHERTYPE_IPV4, &build_ipv4(IP_B, IP_A, PROTO_TCP, 64, 1, &build_tcp(IP_B, IP_A, &t)));
        l.a.receive(&f, l.now);
        // A SYN to a closed port: RST+ACK acknowledging the SYN.
        let t = Tcp { src_port: 4001, dst_port: 9, seq: 500, ack: 0, flags: TCP_SYN, window: 1, mss: None, data: b"" };
        let f = build_eth(&MAC_A, &MAC_B, ETHERTYPE_IPV4, &build_ipv4(IP_B, IP_A, PROTO_TCP, 64, 1, &build_tcp(IP_B, IP_A, &t)));
        l.a.receive(&f, l.now);
        // A reset is never answered with a reset.
        let t = Tcp { src_port: 4002, dst_port: 9, seq: 1, ack: 0, flags: TCP_RST, window: 0, mss: None, data: b"" };
        let f = build_eth(&MAC_A, &MAC_B, ETHERTYPE_IPV4, &build_ipv4(IP_B, IP_A, PROTO_TCP, 64, 1, &build_tcp(IP_B, IP_A, &t)));
        l.a.receive(&f, l.now);
        l.settle(100); // a looks up b's hardware address, then sends its answers
        let replies: Vec<_> = l.sent_by_a.iter().map(|(_, flags, seq, ack, _)| (*flags, *seq, *ack)).collect();
        assert_eq!(replies, [(TCP_RST, 222, 0), (TCP_RST | TCP_ACK, 0, 501)]);
        assert_eq!(l.a.stats.tcp_resets_sent, 2);
    }
}
