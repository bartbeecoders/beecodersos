//! 16550 serial port (UART), shared by COM1 (serial.rs: the console) and
//! COM2 (aibridge.rs: the link to beehive).
//!
//! Written for real hardware as much as for QEMU: a PC may have no serial
//! port at all, or one that never becomes ready. So the port is detected
//! first, and nothing here waits without a limit: a missing or stuck port
//! costs a moment once, then output to it is dropped until it works again.

use crate::port::{inb, outb};

pub const COM1: u16 = 0x3F8;
pub const COM2: u16 = 0x2F8;

const DATA: u16 = 0;
const INTERRUPT_ENABLE: u16 = 1;
const FIFO_CONTROL: u16 = 2;
const LINE_CONTROL: u16 = 3;
const MODEM_CONTROL: u16 = 4;
const LINE_STATUS: u16 = 5;
const SCRATCH: u16 = 7;

const STATUS_DATA_READY: u8 = 0x01;
const STATUS_CAN_SEND: u8 = 0x20;

/// How often the "can send" bit is read before a byte is given up: far more
/// than a working port needs (it sends a byte in under 0.1 ms).
pub const SEND_SPINS: usize = 100_000;
/// Bytes taken per call of `receive`: more than the chip's buffer holds, so
/// only a broken port ("always has data") reaches it.
pub const RECEIVE_LIMIT: usize = 256;

/// Access to I/O ports: the real ones, or a fake in the self-tests.
pub trait Ports {
    fn read(&mut self, port: u16) -> u8;
    fn write(&mut self, port: u16, value: u8);
}

/// The machine's I/O ports.
pub struct Hardware;

impl Ports for Hardware {
    fn read(&mut self, port: u16) -> u8 {
        unsafe { inb(port) }
    }
    fn write(&mut self, port: u16, value: u8) {
        unsafe { outb(port, value) }
    }
}

pub struct Uart {
    base: u16,
    present: bool,
    /// The last byte could not be sent in time: don't wait again until the
    /// port is ready by itself.
    stalled: bool,
}

impl Uart {
    pub const fn new(base: u16) -> Self {
        Uart { base, present: false, stalled: false }
    }

    pub fn present(&self) -> bool {
        self.present
    }

    /// Look for the chip and set it up: 115200 baud, 8N1, buffers on,
    /// interrupts off. Returns whether a port is there.
    pub fn init(&mut self, io: &mut impl Ports) -> bool {
        let base = self.base;
        // A UART keeps what is written to its scratch register; a port that
        // is not there reads as 0xFF whatever was written.
        self.present = [0x5A, 0xA5].iter().all(|&pattern| {
            io.write(base + SCRATCH, pattern);
            io.read(base + SCRATCH) == pattern
        }) && io.read(base + LINE_STATUS) != 0xFF;
        if !self.present {
            return false;
        }
        io.write(base + INTERRUPT_ENABLE, 0x00);
        io.write(base + LINE_CONTROL, 0x80); // divisor access
        io.write(base + DATA, 0x01); // divisor 1: 115200 baud
        io.write(base + INTERRUPT_ENABLE, 0x00);
        io.write(base + LINE_CONTROL, 0x03); // 8 bits, no parity, 1 stop bit
        io.write(base + FIFO_CONTROL, 0xC7); // buffers on and emptied
        io.write(base + MODEM_CONTROL, 0x0B); // DTR, RTS, OUT2 (OUT2 lets interrupts through)
        true
    }

    /// Drop what was received so far and raise an interrupt for every new byte.
    pub fn enable_rx_interrupt(&mut self, io: &mut impl Ports) {
        if self.present {
            self.receive(io, |_| {});
            io.write(self.base + INTERRUPT_ENABLE, 0x01);
        }
    }

    /// Send one byte. False when it was dropped: no port, or the port did
    /// not become ready in time.
    pub fn send(&mut self, io: &mut impl Ports, byte: u8) -> bool {
        if !self.present {
            return false;
        }
        let spins = if self.stalled { 1 } else { SEND_SPINS };
        let ready = (0..spins).any(|_| io.read(self.base + LINE_STATUS) & STATUS_CAN_SEND != 0);
        self.stalled = !ready;
        if ready {
            io.write(self.base + DATA, byte);
        }
        ready
    }

    /// Hand every received byte to `take` (at most `RECEIVE_LIMIT` per call).
    pub fn receive(&self, io: &mut impl Ports, mut take: impl FnMut(u8)) {
        if !self.present {
            return;
        }
        for _ in 0..RECEIVE_LIMIT {
            if io.read(self.base + LINE_STATUS) & STATUS_DATA_READY == 0 {
                return;
            }
            take(io.read(self.base + DATA));
        }
    }
}
