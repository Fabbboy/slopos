//! Global registration hook for the kernel's [`FrameAlloc`].
//!
//! `slopos-ostd` ships only the trait (in [`super::frame`]); the concrete
//! buddy/per-CPU-cache implementation lives outside the trusted core. Boot
//! wires the production allocator through this hook; tests install a scratch
//! impl on the same hook.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::mm::frame::FrameAlloc;
use crate::sync::BspToken;

// `dyn` trait objects are wide pointers, so the slot stores the address of a
// `&'static dyn FrameAlloc` — thin — and reconstructs the wide one on read.

struct AllocSlot {
    /// `*const &'static dyn FrameAlloc` reinterpreted as `*mut ()`.
    inner: AtomicPtr<()>,
}

static FRAME_ALLOCATOR: AllocSlot = AllocSlot {
    inner: AtomicPtr::new(core::ptr::null_mut()),
};

/// One-shot wiring point. Takes a reference to a consumer-side `static`
/// holding a `&'static dyn FrameAlloc`; that allocator must be sound for
/// concurrent `alloc`/`dealloc` from any CPU.
pub fn register_frame_allocator<'brand>(
    _token: &BspToken<'brand>,
    slot: &'static &'static dyn FrameAlloc,
) {
    let raw = slot as *const &'static dyn FrameAlloc as *mut ();
    let prev = FRAME_ALLOCATOR.inner.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::mm::frame_alloc::register_frame_allocator called twice"
    );
}

/// Look up the registered allocator. Returns `None` until
/// [`register_frame_allocator`] has been called.
pub fn current_frame_allocator() -> Option<&'static dyn FrameAlloc> {
    let raw = FRAME_ALLOCATOR.inner.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` was produced by `register_frame_allocator` from a
    // `&'static &'static dyn FrameAlloc`; that storage is `'static`
    // by contract, so the dereference is sound.
    let slot = unsafe { &*(raw as *const &'static dyn FrameAlloc) };
    Some(*slot)
}

/// Test-only: clear the slot so a fresh allocator can be installed.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    FRAME_ALLOCATOR
        .inner
        .store(core::ptr::null_mut(), Ordering::Release);
}
