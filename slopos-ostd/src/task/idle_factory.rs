//! Global registration hook for the per-CPU idle-task factory.
//!
//! `slopos-ostd` does not own the idle task body or its construction.
//! The out-of-OSTD scheduler crate (`sched/`) provides an
//! [`IdleTaskFactory`] function pointer; boot wires it through this
//! hook. SMP bring-up then calls [`current_idle_task_factory`] for
//! each CPU as it comes online to mint that CPU's idle task.
//!
//! One-shot init pattern matches
//! [`crate::task::scheduler_registry::register_scheduler`] and
//! [`crate::mm::frame_alloc::register_frame_allocator`].

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::sync::BspToken;

/// Signature for the per-CPU idle task factory. Called once per CPU
/// during SMP bring-up. The factory creates the idle task and installs
/// it (typically via `pcr::set_idle_task`); returns `0` on success or
/// a negative errno on failure. OSTD does not own the resulting idle
/// task — lifetime is managed by the scheduler crate.
pub type IdleTaskFactory = fn(cpu_id: usize) -> i32;

struct FactorySlot {
    /// Erased `IdleTaskFactory` cast through `*mut ()`.
    inner: AtomicPtr<()>,
}

static IDLE_FACTORY: FactorySlot = FactorySlot {
    inner: AtomicPtr::new(core::ptr::null_mut()),
};

/// One-shot wiring point. The `&BspToken<'brand>` witnesses BSP-only
/// init. `f` is a plain `fn` pointer (`Send + Sync` automatically) so
/// the cast through `*mut ()` is purely a layout punning, not a
/// lifetime hazard.
pub fn register_idle_task_factory<'brand>(_token: &BspToken<'brand>, f: IdleTaskFactory) {
    let raw = f as *mut ();
    let prev = IDLE_FACTORY.inner.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::task::idle_factory::register_idle_task_factory called twice"
    );
}

/// Look up the registered factory. Returns `None` until
/// [`register_idle_task_factory`] has been called.
pub fn current_idle_task_factory() -> Option<IdleTaskFactory> {
    let raw = IDLE_FACTORY.inner.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` was produced by `register_idle_task_factory` from
    // an `IdleTaskFactory` (plain `fn` pointer); the cast back is
    // sound because the two pointer types are layout-compatible.
    let f: IdleTaskFactory = unsafe { core::mem::transmute(raw) };
    Some(f)
}

/// Test-only reset hook.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    IDLE_FACTORY
        .inner
        .store(core::ptr::null_mut(), Ordering::Release);
}
