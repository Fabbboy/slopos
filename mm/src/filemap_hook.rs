//! The filesystem's per-inode page set, as `mm` sees it.
//!
//! A shared file mapping's pages are owned by `slopos_fs`, and `fs` depends on
//! `mm`, so the dependency is inverted through a `&'static dyn` registered
//! from boot — the shape `fileio_register_tty_ops` already uses.
//!
//! Both operations are counted in *pages*, not mappings: a partial `munmap`
//! splits a VMA and hands back only the carved range, so a per-VMA count could
//! not be decremented correctly.

use slopos_ostd::lock_class;
use slopos_ostd::process::AccountId;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use crate::vma_region::FileMapRef;

/// Implemented by the filesystem's page-set registry.
///
/// `retain` and `release` may not block, allocate, or reach a filesystem: both
/// are called under the per-process spinning lock, and the release side also
/// runs from a `Drop` on the task-exit path. What they queue is completed by
/// [`drain`](Self::drain), which *may* block.
pub trait FileMapOps: Sync {
    /// Add `pages` mapping references. `writable` arms the registry's
    /// writeback: a read-only mapping must not cause an unmodified file to be
    /// rewritten. `false` if the handle is stale, so the pages are not pinned.
    ///
    /// `holder` is the principal taking the references, which the registry
    /// charges the set to rather than leaving it charged to an exited owner.
    fn retain(&self, map: FileMapRef, pages: u32, writable: bool, holder: AccountId) -> bool;

    /// Drop `pages` mapping references.
    fn release(&self, map: FileMapRef, pages: u32);

    /// Complete whatever the releases queued. Blocks; the caller must hold no
    /// lock and must be in a context that may sleep.
    fn drain(&self);
}

static FILEMAP_OPS: SpinLock<Option<&'static dyn FileMapOps>> =
    SpinLock::new(None, lock_class!("FILEMAP_OPS", LOCK_LEVEL_RESOURCE));

/// Publish the filesystem's page-set registry. Called once, from boot.
pub fn filemap_register_ops(ops: &'static dyn FileMapOps) {
    *FILEMAP_OPS.lock() = Some(ops);
}

/// Swap the registry, answering the previous one, so a test can observe the
/// retain/release calls and then put the real one back.
#[cfg(feature = "test-hooks")]
pub fn filemap_swap_ops(ops: Option<&'static dyn FileMapOps>) -> Option<&'static dyn FileMapOps> {
    let mut guard = FILEMAP_OPS.lock();
    core::mem::replace(&mut *guard, ops)
}

fn ops() -> Option<&'static dyn FileMapOps> {
    *FILEMAP_OPS.lock()
}

/// `false` if no registry is published or the handle is stale.
pub fn filemap_retain(map: FileMapRef, pages: u32, writable: bool, holder: AccountId) -> bool {
    ops().is_some_and(|o| o.retain(map, pages, writable, holder))
}

pub fn filemap_release(map: FileMapRef, pages: u32) {
    if let Some(o) = ops() {
        o.release(map, pages);
    }
}

/// Complete what the releases queued. Blocks, so the caller must be in syscall
/// context with the per-process lock dropped.
pub fn filemap_drain() {
    if let Some(o) = ops() {
        o.drain();
    }
}
