//! Per-direction byte FIFO used by connected AF_UNIX pairs.
//!
//! Backed by `KVecDeque<u8>` — the kernel-blessed standard collection
//! for an unbounded ring buffer.  Capacity is enforced by the wrapper:
//! [`UnixFifo::new`] pre-reserves exactly `UNIX_BUF_SIZE` bytes via
//! `try_reserve_exact`, so subsequent `push_back` calls within the cap
//! never reallocate.  The 16 KiB allocation lives entirely on the heap
//! — no rvalue ever materialises on a kernel stack frame.

use slopos_alloc::{AllocError, KVecDeque};

/// Per-direction FIFO size (16 KiB).
pub const UNIX_BUF_SIZE: usize = 16384;

/// Bounded byte FIFO with a fixed capacity of [`UNIX_BUF_SIZE`].
///
/// `write` and `read` never panic; they cap at the available space or
/// available bytes respectively and return the count actually
/// transferred.
pub(super) struct UnixFifo {
    inner: KVecDeque<u8>,
}

impl UnixFifo {
    /// Allocate a 16 KiB-capacity FIFO. Heap-allocates exactly once;
    /// subsequent `push_back` calls within capacity never realloc.
    pub(super) fn new() -> Result<Self, AllocError> {
        Ok(Self {
            inner: KVecDeque::with_capacity(UNIX_BUF_SIZE)?,
        })
    }

    /// Append up to `data.len()` bytes, capped at remaining capacity.
    /// Returns bytes actually written.
    pub(super) fn write(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.remaining());
        for &b in &data[..n] {
            self.inner.push_back(b).expect("pre-reserved");
        }
        n
    }

    /// Drain up to `out.len()` bytes from the front. Returns bytes read.
    pub(super) fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.inner.len());
        for slot in &mut out[..n] {
            *slot = self.inner.pop_front().expect("len-bounded");
        }
        n
    }

    pub(super) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub(super) fn remaining(&self) -> usize {
        UNIX_BUF_SIZE - self.inner.len()
    }

    pub(super) fn has_space(&self) -> bool {
        self.remaining() > 0
    }
}
