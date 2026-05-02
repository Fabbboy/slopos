//! Local-CPU TLB-flush hook.
//!
//! Exposes a registration hook so [`super::vm_space::CursorMut`] can
//! invalidate a stale translation without OSTD reaching into the
//! cross-CPU IPI machinery directly. The consumer-side driver
//! installs an impl that issues `INVLPG`; tests install a no-op.
//!
//! Same one-shot AcqRel pattern as
//! [`crate::mm::frame_alloc::register_frame_allocator`].

use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_abi::addr::VirtAddr;

/// Trait the consumer-side TLB driver implements. Only the
/// local-CPU `INVLPG` is exposed; cross-CPU broadcast TLB shootdown
/// lives outside OSTD.
pub trait LocalTlbFlush: Send + Sync {
    /// Invalidate the translation for `vaddr` on the local CPU.
    fn invlpg(&self, vaddr: VirtAddr);
}

static FLUSHER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// One-shot wiring point. See [`register_frame_allocator`] for the
/// double-reference layout.
///
/// [`register_frame_allocator`]: crate::mm::frame_alloc::register_frame_allocator
///
/// # Safety
///
/// `slot` must outlive the kernel. The underlying `dyn LocalTlbFlush`
/// must be sound for concurrent calls from any CPU.
pub unsafe fn register_local_tlb_flusher(slot: &'static &'static dyn LocalTlbFlush) {
    let raw = slot as *const &'static dyn LocalTlbFlush as *mut ();
    let prev = FLUSHER.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::mm::tlb::register_local_tlb_flusher called twice"
    );
}

/// Invalidate `vaddr` on the local CPU. No-op until a flusher is
/// registered (so map-only test paths don't need to install one).
pub fn flush_local(vaddr: VirtAddr) {
    let raw = FLUSHER.load(Ordering::Acquire);
    if raw.is_null() {
        return;
    }
    // SAFETY: `raw` was produced by `register_local_tlb_flusher` from
    // a `&'static &'static dyn LocalTlbFlush`; that storage is
    // `'static` by contract.
    let slot = unsafe { &*(raw as *const &'static dyn LocalTlbFlush) };
    slot.invlpg(vaddr);
}

/// Test-only reset hook.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    FLUSHER.store(core::ptr::null_mut(), Ordering::Release);
}
