//! Const-generic fixed-size ring buffer for TTY subsystem buffers.
//!
//! Replaces repeated manual head/tail/count arithmetic in `LineDisc` and
//! `RawDisc` with a single tested type.  The buffer lives inline (no alloc)
//! and is const-constructible for static initialisation.

/// A fixed-size ring buffer backed by a `[u8; N]` array.
///
/// `N` **must** be a power of two — a compile-time assertion enforces this.
/// All index arithmetic uses bitmask `& (N - 1)` instead of modulo for
/// branch-free wrapping on the hot path.
pub(crate) struct RingBuf<const N: usize> {
    buf: [u8; N],
    head: usize,
    tail: usize,
    count: usize,
}

const fn is_power_of_two(n: usize) -> bool {
    n != 0 && (n & (n - 1)) == 0
}

impl<const N: usize> RingBuf<N> {
    const MASK: usize = N - 1;

    pub(crate) const fn new() -> Self {
        const { assert!(is_power_of_two(N), "RingBuf size must be a power of two") }
        Self {
            buf: [0; N],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    pub(crate) fn push(&mut self, c: u8) -> bool {
        if self.is_full() {
            return false;
        }
        self.buf[self.head] = c;
        self.head = (self.head + 1) & Self::MASK;
        self.count += 1;
        true
    }

    pub(crate) fn pop(&mut self) -> Option<u8> {
        if self.is_empty() {
            return None;
        }
        let byte = self.buf[self.tail];
        self.tail = (self.tail + 1) & Self::MASK;
        self.count -= 1;
        Some(byte)
    }

    pub(crate) fn peek(&self) -> Option<u8> {
        if self.is_empty() {
            None
        } else {
            Some(self.buf[self.tail])
        }
    }

    /// Bulk read into `out`, returning the number of bytes copied.
    ///
    /// Uses contiguous `copy_from_slice` segments instead of per-byte pop
    /// for throughput on large reads.
    pub(crate) fn read(&mut self, out: &mut [u8]) -> usize {
        let to_copy = out.len().min(self.count);
        if to_copy == 0 {
            return 0;
        }

        let first_len = (N - self.tail).min(to_copy);
        out[..first_len].copy_from_slice(&self.buf[self.tail..self.tail + first_len]);

        let second_len = to_copy - first_len;
        if second_len > 0 {
            out[first_len..to_copy].copy_from_slice(&self.buf[..second_len]);
        }

        self.tail = (self.tail + to_copy) & Self::MASK;
        self.count -= to_copy;
        to_copy
    }

    pub(crate) fn count(&self) -> usize {
        self.count
    }

    pub(crate) fn free(&self) -> usize {
        N - self.count
    }

    pub(crate) fn is_full(&self) -> bool {
        self.count >= N
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub(crate) fn capacity(&self) -> usize {
        N
    }

    pub(crate) fn flush(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
    }

    pub(crate) fn peek_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.count {
            return None;
        }
        let idx = (self.tail + offset) & Self::MASK;
        Some(self.buf[idx])
    }
}
