//! `FileKind::Ring` file operations (SLOPRING § 3, § 14).
//!
//! A ring is an open file, so it inherits the fd lifecycle: `close`,
//! `dup`, fork-inheritance, exec teardown. The fd's `handle: usize` is
//! the packed ring [`Handle`](slopos_ostd::handle::Handle) from the
//! registry. Read/write/poll on a ring fd are meaningless (use
//! `ring_enter`), so they return `-EINVAL` / `POLLNVAL`.
//!
//! `release` (last fd close) removes the ring from the registry,
//! dropping the ring object — which releases the kernel's `RingMeta`
//! frame refs. Any still-mapped user PTE holds its own ref, so frames
//! survive until the mapping is also torn down (no mmap-after-close
//! UAF).

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::syscall::POLLNVAL;

pub struct RingFileOps;

pub static RING_FILE_OPS: RingFileOps = RingFileOps;

impl FileOps for RingFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Ring
    }

    fn read(&self, _handle: usize, _buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        // A ring fd is driven by ring_enter, not read(2).
        Errno::EINVAL.as_isize()
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn release(&self, handle: usize) {
        // Last fd closed: drop the ring object (SLOPRING § 14). The
        // registry remove is idempotent / stale-safe.
        crate::registry::remove(handle);
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        // Intra-process dup is allowed (it is why the per-ring lock
        // exists, SLOPRING § 6.3). The ring object is shared; both fds
        // resolve to the same registry slot. The ring object itself is
        // not refcounted per-fd here — a dup'd ring fd that closes must
        // NOT drop the ring while another fd is live. To keep this
        // simple and correct, dup returns the same handle but the
        // ring is only removed when the *open-file* refcount (managed by
        // the fd layer) hits zero, which calls `release` exactly once.
        Some(handle)
    }

    fn poll_events(&self, _handle: usize, _events: u16) -> u16 {
        POLLNVAL
    }
}
