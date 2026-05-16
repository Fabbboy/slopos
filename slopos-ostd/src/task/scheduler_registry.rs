//! Global registration hook for the kernel's [`Scheduler`].
//!
//! `slopos-ostd` ships only the trait (in [`super::scheduler`]); the
//! concrete preemptive priority implementation lives outside the
//! trusted core (`sched/` crate). Boot wires the production scheduler
//! through this hook; tests install a scratch impl on the same hook.
//!
//! One-shot init pattern matches [`crate::mm::frame_alloc::register_frame_allocator`]:
//! an `AtomicPtr` AcqRel-swapped against null, with a panic on
//! double-init.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::sync::BspToken;
use crate::task::scheduler::Scheduler;

// We store a pointer to a `&'static dyn Scheduler`. `dyn` trait
// objects are wide pointers (data + vtable), so we double-box: the
// caller hands us a `&'static dyn Scheduler`, we store its address
// (a thin `*const ()`) and reconstruct on read.

struct SchedSlot {
    /// `*const &'static dyn Scheduler` reinterpreted as `*mut ()`.
    inner: AtomicPtr<()>,
}

static SCHEDULER: SchedSlot = SchedSlot {
    inner: AtomicPtr::new(core::ptr::null_mut()),
};

/// One-shot wiring point. Pass a reference to a `&'static dyn Scheduler`
/// — typically a `static` consumer-side wrapping the production
/// scheduler singleton, then a reference to *that*. The
/// `&BspToken<'brand>` witnesses BSP-only init via the HRTB closure
/// minted by [`crate::sync::run_bsp_init`]; the underlying `dyn
/// Scheduler` is required to be `Send + Sync` for concurrent
/// `enqueue`/`local_rq_with` from any CPU (the static double-reference
/// guarantees `'static` storage).
pub fn register_scheduler<'brand>(
    _token: &BspToken<'brand>,
    slot: &'static &'static dyn Scheduler,
) {
    let raw = slot as *const &'static dyn Scheduler as *mut ();
    let prev = SCHEDULER.inner.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::task::scheduler_registry::register_scheduler called twice"
    );
}

/// Look up the registered scheduler. Returns `None` until
/// [`register_scheduler`] has been called.
pub fn current_scheduler() -> Option<&'static dyn Scheduler> {
    let raw = SCHEDULER.inner.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` was produced by `register_scheduler` from a
    // `&'static &'static dyn Scheduler`; that storage is `'static`
    // by contract, so the dereference is sound.
    let slot = unsafe { &*(raw as *const &'static dyn Scheduler) };
    Some(*slot)
}

/// Test-only reset hook. Allows host integration-test binaries to
/// re-install a fresh scheduler between test binary invocations.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    SCHEDULER
        .inner
        .store(core::ptr::null_mut(), Ordering::Release);
}
