//! Local-CPU TLB-flush hook, letting [`super::vm_space::CursorMut`]
//! invalidate a stale translation without OSTD reaching into the
//! cross-CPU IPI machinery directly.

use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_abi::addr::VirtAddr;

use crate::sync::BspToken;

/// Trait the consumer-side TLB driver implements. Only the local-CPU
/// `INVLPG` is exposed; cross-CPU shootdown lives outside OSTD.
pub trait LocalTlbFlush: Send + Sync {
    fn invlpg(&self, vaddr: VirtAddr);
}

static FLUSHER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// One-shot wiring point; the `&BspToken<'brand>` witnesses BSP-only
/// init. The `dyn LocalTlbFlush` must be sound for concurrent calls
/// from any CPU.
pub fn register_local_tlb_flusher<'brand>(
    _token: &BspToken<'brand>,
    slot: &'static &'static dyn LocalTlbFlush,
) {
    let raw = slot as *const &'static dyn LocalTlbFlush as *mut ();
    let prev = FLUSHER.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::mm::tlb::register_local_tlb_flusher called twice"
    );
}

/// Invalidate `vaddr` on the local CPU; no-op until a flusher is registered.
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

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    FLUSHER.store(core::ptr::null_mut(), Ordering::Release);
}
