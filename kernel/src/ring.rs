//! Lock-free single-producer / single-consumer byte queue.
//! The interrupt handler pushes, the main loop pops.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

pub struct Ring<const N: usize> {
    buf: UnsafeCell<[u8; N]>,
    head: AtomicUsize, // next slot to read
    tail: AtomicUsize, // next slot to write
}

unsafe impl<const N: usize> Sync for Ring<N> {}

impl<const N: usize> Ring<N> {
    pub const fn new() -> Self {
        Ring { buf: UnsafeCell::new([0; N]), head: AtomicUsize::new(0), tail: AtomicUsize::new(0) }
    }

    /// Producer side (interrupt handler). Drops the byte when full.
    pub fn push(&self, b: u8) {
        let tail = self.tail.load(Ordering::Relaxed);
        let next = (tail + 1) % N;
        if next == self.head.load(Ordering::Acquire) {
            return;
        }
        unsafe { (*self.buf.get())[tail] = b };
        self.tail.store(next, Ordering::Release);
    }

    /// Consumer side (main loop).
    pub fn pop(&self) -> Option<u8> {
        let head = self.head.load(Ordering::Relaxed);
        if head == self.tail.load(Ordering::Acquire) {
            return None;
        }
        let b = unsafe { (*self.buf.get())[head] };
        self.head.store((head + 1) % N, Ordering::Release);
        Some(b)
    }

    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Acquire)
    }
}

/// Mutable global for state only touched from the main (non-interrupt) flow.
pub struct Global<T>(UnsafeCell<T>);

unsafe impl<T> Sync for Global<T> {}

impl<T> Global<T> {
    pub const fn new(v: T) -> Self {
        Global(UnsafeCell::new(v))
    }

    /// SAFETY contract: single core, never called from interrupt handlers,
    /// and the returned reference is not held across another `get()`.
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}
