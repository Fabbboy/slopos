//! Async byte I/O over a raw fd.
//!
//! [`AsyncFd`] is a non-owning async view: it submits `OP_READ`/`OP_WRITE`/
//! `OP_POLL_ADD`/`OP_ACCEPT` for a fd whose lifetime the caller owns. Only the
//! data plane is async — connection setup (`socket`/`connect`/`bind`/`listen`)
//! stays a plain syscall.

use slopos_abi::syscall::POLLIN;

use super::{BufResult, accept, poll_add, read, write};

/// A non-owning async view over a raw fd.
#[derive(Clone, Copy)]
pub struct AsyncFd {
    fd: i32,
}

impl AsyncFd {
    pub fn new(fd: i32) -> Self {
        Self { fd }
    }

    pub fn raw(&self) -> i32 {
        self.fd
    }

    /// Read up to `len` bytes into `buf` (capacity must be `>= len`). The
    /// buffer is owned by the reactor while in flight and returned in the
    /// [`BufResult`].
    pub async fn read(&self, buf: Vec<u8>, len: u32) -> BufResult {
        read(self.fd, buf, len).await
    }

    /// Write all of `buf`. Returns the byte count (or negated errno) plus the
    /// buffer for reuse.
    pub async fn write(&self, buf: Vec<u8>) -> BufResult {
        write(self.fd, buf).await
    }

    /// Resolve when the fd is readable (`OP_POLL_ADD` with `POLLIN`); `res`
    /// carries the ready `revents`.
    pub async fn readable(&self) -> i32 {
        poll_add(self.fd, POLLIN).await
    }

    /// Accept a connection (`OP_ACCEPT`); resolves to the new fd or a
    /// negated errno.
    pub async fn accept(&self) -> i32 {
        accept(self.fd).await
    }
}
