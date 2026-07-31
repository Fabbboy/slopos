//! `FileKind::Ring` file operations (SLOPRING § 3, § 14).
//!
//! A ring is an open file, so it inherits the fd lifecycle: `close`,
//! `dup`, and exec teardown. It is *not* inherited across `fork` — the
//! descriptor is installed process-private, so the child's table has no
//! entry for it. The fd's `handle: usize` is the packed ring
//! [`Handle`](slopos_ostd::handle::Handle) from the registry.
//! Read/write/poll on a ring fd are meaningless (use `ring_enter`), so
//! they return `-EINVAL` / `POLLNVAL`.
//!
//! The fd's owning [`RingBacking`] removes the ring from the registry
//! on last close, dropping the ring object — which releases the
//! kernel's `RingMeta` frame refs. Any still-mapped user PTE holds its
//! own ref, so frames survive until the mapping is also torn down (no
//! mmap-after-close UAF). Intra-process dup'd ring fds share the one
//! open-file description, so the teardown runs exactly once.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileBacking, FileKind, FileOps};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::syscall::POLLNVAL;
use slopos_ostd::KArc;

pub struct RingFileOps;

pub static RING_FILE_OPS: RingFileOps = RingFileOps;

/// Sole owner of one registry slot; dropping it drops the ring object.
struct RingBacking {
    handle: usize,
}

impl FileBacking for RingBacking {}

impl Drop for RingBacking {
    fn drop(&mut self) {
        // The registry remove is idempotent / stale-safe.
        crate::registry::remove(self.handle);
    }
}

/// Wrap ownership of a freshly-registered ring. On allocation failure
/// the registry entry is removed before returning, so it cannot leak.
pub(crate) fn ring_backing(handle: usize) -> Option<KArc<dyn FileBacking>> {
    match KArc::try_new(RingBacking { handle }) {
        Ok(backing) => Some(backing),
        Err(_) => {
            crate::registry::remove(handle);
            None
        }
    }
}

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

    fn poll_events(&self, _handle: usize, _events: u16) -> u16 {
        POLLNVAL
    }
}
