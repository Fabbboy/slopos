//! `FileKind::Ring` file operations (SLOPRING § 3, § 14).
//!
//! A ring inherits the fd lifecycle (`close`, `dup`, exec teardown) but is
//! *not* inherited across `fork` — the descriptor is installed
//! process-private. The fd's `handle: usize` is the packed ring
//! [`Handle`](slopos_ostd::handle::Handle) from the registry.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::quota::ObjectRow;
use slopos_abi::syscall::POLLNVAL;
use slopos_ostd::KArc;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Charge, FileBacking, try_charge};

pub struct RingFileOps;

pub static RING_FILE_OPS: RingFileOps = RingFileOps;

/// Sole owner of one registry slot; dropping it drops the ring object.
#[derive(slopos_ostd::Charged)]
struct RingBacking {
    handle: usize,
    object_charge: Charge<ObjectRow>,
}

slopos_ostd::charge_audit!(RingBacking);

impl FileBacking for RingBacking {}

impl Drop for RingBacking {
    fn drop(&mut self) {
        crate::registry::remove(self.handle);
    }
}

/// Wrap ownership of a freshly-registered ring. On allocation failure
/// the registry entry is removed before returning, so it cannot leak.
pub(crate) fn ring_backing(handle: usize, account: AccountId) -> Option<KArc<dyn FileBacking>> {
    let Ok(reservation) = try_charge::<ObjectRow>(account, 1) else {
        crate::registry::remove(handle);
        return None;
    };
    match KArc::try_new(RingBacking {
        handle,
        object_charge: Charge::commit(reservation),
    }) {
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
        Errno::EINVAL.as_isize()
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn poll_events(&self, _handle: usize, _events: u16) -> u16 {
        POLLNVAL
    }
}
