//! Per-direction byte FIFO used by connected AF_UNIX pairs.
//!
//! [`UnixFifo::new`] pre-reserves exactly `UNIX_BUF_SIZE` bytes, so subsequent
//! `push_back` calls within the cap never reallocate.

use slopos_ostd::{AllocError, KVecDeque};

/// Per-direction FIFO size (16 KiB).
pub const UNIX_BUF_SIZE: usize = 16384;

/// Bounded byte FIFO with a fixed capacity of [`UNIX_BUF_SIZE`].
///
/// `write` and `read` never panic; they cap at the available space or bytes and
/// return the count actually transferred.
pub(super) struct UnixFifo {
    inner: KVecDeque<u8>,
}

impl UnixFifo {
    pub(super) fn new() -> Result<Self, AllocError> {
        Ok(Self {
            inner: KVecDeque::with_capacity(UNIX_BUF_SIZE)?,
        })
    }

    pub(super) fn write(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.remaining());
        for &b in &data[..n] {
            self.inner.push_back(b).expect("pre-reserved");
        }
        n
    }

    pub(super) fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.inner.len());
        for slot in &mut out[..n] {
            *slot = self.inner.pop_front().expect("len-bounded");
        }
        n
    }

    /// Appends bytes pulled straight from the pinned user pages: each byte is
    /// volatile-read into the deque, so there is no bulk kernel scratch.
    pub(super) fn write_from(&mut self, reader: &mut slopos_ostd::mm::VmReader<'_>) -> usize {
        let mut written = 0;
        while reader.has_remain() && self.has_space() {
            let mut byte = [0u8; 1];
            if reader.read(&mut byte) != 1 {
                break;
            }
            self.inner.push_back(byte[0]).expect("pre-reserved");
            written += 1;
        }
        written
    }

    /// Drains bytes from the front straight into the pinned user pages, one
    /// volatile byte at a time, so there is no bulk kernel scratch.
    pub(super) fn read_into(&mut self, writer: &mut slopos_ostd::mm::VmWriter<'_>) -> usize {
        let mut read = 0;
        while writer.has_remain() {
            let Some(b) = self.inner.pop_front() else {
                break;
            };
            if writer.write(&[b]) != 1 {
                break;
            }
            read += 1;
        }
        read
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
