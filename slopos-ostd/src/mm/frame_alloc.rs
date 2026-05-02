//! Global registration hook for the kernel's [`FrameAlloc`].
//!
//! `slopos-ostd` ships only the trait (in [`super::frame`]); the
//! concrete buddy/per-CPU-cache implementation lives outside the
//! trusted core. Boot wires the production allocator through this
//! hook; tests install a scratch impl on the same hook.
//!
//! One-shot init pattern matches [`crate::mm::frame::init_meta_slots`]
//! and [`crate::mm::phys::init_phys_virt_offset`]: an `AtomicPtr`
//! AcqRel-swapped against null, with a panic on double-init.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::mm::frame::FrameAlloc;

// We store a pointer to a `&'static dyn FrameAlloc`. `dyn` trait
// objects are wide pointers (data + vtable), so we double-box: the
// caller hands us a `&'static dyn FrameAlloc`, we store its address
// (a thin `*const ()`) and reconstruct on read.

struct AllocSlot {
    /// `*const &'static dyn FrameAlloc` reinterpreted as `*mut ()`.
    inner: AtomicPtr<()>,
}

static FRAME_ALLOCATOR: AllocSlot = AllocSlot {
    inner: AtomicPtr::new(core::ptr::null_mut()),
};

/// One-shot wiring point. Pass a reference to a `&'static dyn FrameAlloc`
/// — typically `&LEGACY_FRAME_ALLOC_SHIM as &'static dyn FrameAlloc`
/// stored in a `static` consumer-side, then a reference to *that*.
///
/// # Safety
///
/// The caller certifies that `slot` outlives the kernel (`'static`)
/// and that the underlying `dyn FrameAlloc` is sound for concurrent
/// `alloc` / `dealloc` from any CPU.
pub unsafe fn register_frame_allocator(slot: &'static &'static dyn FrameAlloc) {
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

/// Test-only reset hook. Allows host integration-test binaries to
/// re-install a fresh allocator between test binary invocations.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    FRAME_ALLOCATOR
        .inner
        .store(core::ptr::null_mut(), Ordering::Release);
}
