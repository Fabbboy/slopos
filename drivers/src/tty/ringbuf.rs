//! Const-generic fixed-size ring buffer for TTY subsystem buffers.
//!
//! Replaces repeated manual head/tail/count arithmetic in `LineDisc` and
//! `RawDisc` with a single tested type.  The buffer lives inline (no alloc)
//! and is const-constructible for static initialisation.

/// A fixed-size ring buffer backed by a `[u8; N]` array.
pub(crate) struct RingBuf<const N: usize> {
    buf: [u8; N],
    head: usize,
    tail: usize,
    count: usize,
}

impl<const N: usize> RingBuf<N> {
    pub(crate) const fn new() -> Self {
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
        self.head = (self.head + 1) % N;
        self.count += 1;
        true
    }

    pub(crate) fn pop(&mut self) -> Option<u8> {
        if self.is_empty() {
            return None;
        }
        let byte = self.buf[self.tail];
        self.tail = (self.tail + 1) % N;
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

    pub(crate) fn read(&mut self, out: &mut [u8]) -> usize {
        let mut copied = 0usize;
        while copied < out.len() {
            let Some(byte) = self.pop() else {
                break;
            };
            out[copied] = byte;
            copied += 1;
        }
        copied
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
        let idx = (self.tail + offset) % N;
        Some(self.buf[idx])
    }
}
