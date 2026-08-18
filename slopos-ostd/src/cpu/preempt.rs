//! Preemption control surface.
//!
//! [`DisabledPreemptGuard`] is the RAII gate for atomic context: construction
//! increments the active [`PreemptBackend`]'s preempt count, drop decrements
//! it. The backend is a one-shot registration because the per-CPU preempt
//! counters live in the kernel's PCR, which OSTD does not own.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use crate::cpu::x86_64::pcr;
use crate::cpu::x86_64::{restore_flags, save_flags_cli};
use crate::sync::BspToken;

/// Per-CPU preempt-count operations.
///
/// Implementations must operate only on the *current* CPU's preempt-count
/// storage (the guard is `!Send`, so it cannot escape its CPU) and keep
/// `enter` / `leave` symmetric for nesting.
pub trait PreemptBackend: Send + Sync + 'static {
    fn enter(&self);

    /// Decrement, running any pending reschedule callback when the count
    /// returns to zero.
    fn leave(&self);

    /// Decrement *without* running any deferred reschedule callback: the
    /// variant used at the IRET-edge of an IST exception handler, where running
    /// the scheduler would corrupt the IST stack.
    fn leave_quiet(&self) {
        self.leave();
    }

    fn count(&self) -> u32;
}

/// Fallback used until a production backend is registered. Tracks one global
/// `AtomicU32` so host-side unit tests can observe the guard surface without a
/// per-CPU PCR.
pub struct NoOpBackend {
    count: AtomicU32,
}

impl NoOpBackend {
    pub const fn new() -> Self {
        Self {
            count: AtomicU32::new(0),
        }
    }

    #[cfg(any(test, feature = "test-helpers"))]
    pub fn reset(&self) {
        self.count.store(0, Ordering::Release);
    }
}

impl Default for NoOpBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PreemptBackend for NoOpBackend {
    #[inline]
    fn enter(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn leave(&self) {
        let prev = self.count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "NoOpBackend preempt_count underflow");
    }

    #[inline]
    fn leave_quiet(&self) {
        let prev = self.count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "NoOpBackend preempt_count underflow");
    }

    #[inline]
    fn count(&self) -> u32 {
        self.count.load(Ordering::Relaxed)
    }
}

static DEFAULT_BACKEND: NoOpBackend = NoOpBackend::new();

/// Exposed for test setup that asserts against the default backend's count
/// after `reset_for_test`.
#[cfg(any(test, feature = "test-helpers"))]
pub fn default_backend() -> &'static NoOpBackend {
    &DEFAULT_BACKEND
}

/// `PreemptBackend` impl that proxies to the per-CPU preempt count
/// living in `slopos_ostd::cpu::x86_64::pcr::ProcessorControlRegion::preempt_count`.
///
/// Registered at boot via [`register_preempt_backend`], after the BSP PCR has
/// been installed; the [`NoOpBackend`] default is active until then.
pub struct PcrPreemptBackend;

pub static DEFAULT_PCR_PREEMPT: PcrPreemptBackend = PcrPreemptBackend;

#[cfg(all(target_arch = "x86_64", not(test)))]
impl PreemptBackend for PcrPreemptBackend {
    #[inline]
    fn enter(&self) {
        // Single-instruction gs-relative increment — migration-atomic, same
        // rationale as `PreemptGuard::new`.
        crate::cpu::x86_64::pcr::preempt_count_inc();
    }

    #[inline]
    fn leave(&self) {
        let prev = crate::cpu::x86_64::pcr::preempt_count_dec_fetch_prev();
        // Always-on: an unmatched decrement wraps the unsigned count silently
        // and resurfaces as a context-free `PreemptGuard::drop` underflow, so
        // fail here at the unbalanced leave instead.
        assert!(prev > 0, "preempt_count underflow (backend leave)");
    }

    #[inline]
    fn count(&self) -> u32 {
        crate::cpu::x86_64::pcr::preempt_count_get()
    }
}

// Host-test stub: no PCR, no GS-base.
#[cfg(not(all(target_arch = "x86_64", not(test))))]
impl PreemptBackend for PcrPreemptBackend {
    fn enter(&self) {}
    fn leave(&self) {}
    fn count(&self) -> u32 {
        0
    }
}

struct BackendSlot(UnsafeCell<MaybeUninit<&'static dyn PreemptBackend>>);
// SAFETY: writes are gated by `BACKEND_INSTALLED.swap(true, AcqRel)`
// (one-shot); subsequent reads only happen after observing the flag
// with Acquire, so the read sees the published reference. Inv. 2.
unsafe impl Sync for BackendSlot {}

static BACKEND_SLOT: BackendSlot = BackendSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production preempt backend; until it is
/// called, [`DisabledPreemptGuard`] uses the OSTD-internal [`NoOpBackend`].
/// The `&BspToken<'brand>` witnesses BSP-only init.
pub fn register_preempt_backend<'brand>(
    _token: &BspToken<'brand>,
    backend: &'static dyn PreemptBackend,
) {
    let was_installed = BACKEND_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(
        !was_installed,
        "slopos_ostd::cpu::preempt::register_preempt_backend called twice"
    );
    // SAFETY: the swap above transitioned us from "uninstalled" to
    // "installed" exclusively; no other writer can be racing.
    unsafe {
        (*BACKEND_SLOT.0.get()).write(backend);
    }
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    DEFAULT_BACKEND.reset();
    BACKEND_INSTALLED.store(false, Ordering::Release);
}

#[inline]
fn current_backend() -> &'static dyn PreemptBackend {
    if !BACKEND_INSTALLED.load(Ordering::Acquire) {
        return &DEFAULT_BACKEND;
    }
    // SAFETY: paired Release in `register_preempt_backend`; once the flag is
    // observed set, the slot is initialised and the reference is `'static`.
    unsafe { *(*BACKEND_SLOT.0.get()).as_ptr() }
}

/// RAII guard that disables preemption while held.
///
/// `!Send`: a guard cannot migrate across CPUs, because the count it
/// manipulates is per-CPU.
#[must_use = "if unused, preemption will immediately re-enable"]
pub struct DisabledPreemptGuard {
    _not_send: PhantomData<*const ()>,
}

impl DisabledPreemptGuard {
    #[inline]
    pub fn new() -> Self {
        current_backend().enter();
        Self {
            _not_send: PhantomData,
        }
    }
}

impl Default for DisabledPreemptGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DisabledPreemptGuard {
    #[inline]
    fn drop(&mut self) {
        current_backend().leave();
    }
}

#[inline]
pub fn preempt_count() -> u32 {
    current_backend().count()
}

#[inline]
pub fn is_preempt_disabled() -> bool {
    preempt_count() > 0
}

/// Internal hook called by [`crate::irq::idt::IrqEntryGuard::enter`] for
/// IST-using vectors.
#[inline]
pub(crate) fn irq_entry_bump() {
    current_backend().enter();
}

/// Internal hook called by [`crate::irq::idt::IrqEntryGuard::drop`] for
/// IST-using vectors; decrements via the quiet path.
#[inline]
pub(crate) fn irq_entry_leave_quiet() {
    current_backend().leave_quiet();
}

/// Release the single exception/IRQ-entry preempt hold when a handler
/// **diverges** instead of returning, so the [`crate::irq::idt`] entry-guard's
/// `Drop` never runs to balance its `irq_entry_bump`.
///
/// Use *only* on a path that abandons its exception-handler frame via an
/// unconditional reschedule or halt; on any normal return path the RAII guard
/// performs the leave and calling this as well would double-decrement.
#[inline]
pub fn release_diverging_exception_hold() {
    irq_entry_leave_quiet();
}

// `PreemptGuard` complements `DisabledPreemptGuard` with deferred-reschedule
// semantics: a drop that sees the count return to zero with
// `reschedule_pending` set invokes the registered callback. It runs against the
// PCR directly rather than through `PreemptBackend`, because the kernel call
// sites observe that field directly.

static RESCHEDULE_CALLBACK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// RAII guard that disables preemption while held.
/// Guards are nestable - preemption re-enables only when all guards drop.
/// !Send/!Sync: must stay on same CPU context.
#[must_use = "if unused, preemption will be immediately re-enabled"]
pub struct PreemptGuard {
    _marker: PhantomData<*mut ()>,
}

impl PreemptGuard {
    #[inline]
    pub fn new() -> Self {
        // A single gs-relative RMW is migration-atomic: interrupts are only
        // recognised at instruction boundaries. This guard is constructed at
        // the preemptible baseline, so resolving the PCR pointer and then
        // incrementing through it would open a window where a reschedule
        // migrates the task and the increment lands on the previous CPU's
        // count — that CPU never preempts again, and the matching drop here
        // underflows.
        pcr::preempt_count_inc();
        Self {
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn is_active() -> bool {
        pcr::preempt_count_get() > 0
    }

    #[inline]
    pub fn count() -> u32 {
        pcr::preempt_count_get()
    }

    #[inline]
    pub fn set_reschedule_pending() {
        pcr::reschedule_pending_set();
    }

    #[inline]
    pub fn is_reschedule_pending() -> bool {
        pcr::reschedule_pending_get() != 0
    }

    #[inline]
    pub fn clear_reschedule_pending() {
        pcr::reschedule_pending_clear();
    }
}

impl Default for PreemptGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PreemptGuard {
    #[inline]
    fn drop(&mut self) {
        // Migration-atomic, see `new`. A non-zero count pins the task to its
        // CPU, so this decrement always executes on the CPU that ran the
        // matching increment.
        let prev = pcr::preempt_count_dec_fetch_prev();
        assert!(prev > 0, "preempt_count underflow");

        // The deferred reschedule fires only at the running, IRQs-enabled
        // baseline. With IRQs disabled this 1→0 is inside a handler or an
        // IRQ-disabled critical section, where a nested `schedule()` would run
        // `switch_context`'s count swap from a non-baseline context, saving and
        // restoring the per-task preempt_count under the wrong logical task.
        // Nothing is lost: the handler's tail consumes the pending flag at the
        // correct boundary (`scheduler_handoff_on_trap_exit`), so it is
        // deliberately left set on this path.
        //
        // Once the count reaches zero the task is preemptible again and a
        // migration may slip in before the check below. Benign: the migrating
        // IRQ's own trap-exit handoff consumes the old CPU's pending flag, and
        // the gs-relative check targets whichever CPU runs this task now. The
        // cheap load gates the locked `xchg` take.
        if prev == 1 && crate::cpu::x86_64::interrupts::are_interrupts_enabled() {
            // Bottom half first: the drain is bounded and does not switch, and
            // a reschedule request survives it, where the other order would
            // switch away and leave the work for an arbitrary later moment.
            // This is the only such point a lock-taking kernel task that never
            // returns to userland reaches.
            crate::sync::bh::run_pending_if_due();

            if pcr::reschedule_pending_get() != 0 && pcr::reschedule_pending_take() != 0 {
                let fn_ptr = RESCHEDULE_CALLBACK.load(Ordering::Acquire);
                if !fn_ptr.is_null() {
                    // SAFETY: fn_ptr was set via register_reschedule_callback with a valid fn().
                    let callback: fn() = unsafe { core::mem::transmute(fn_ptr) };
                    callback();
                }
            }
        }
    }
}

/// Combined IRQ-disable + Preemption-disable guard.
/// On drop: restore flags, then preempt guard drops (may trigger deferred reschedule).
#[must_use = "if unused, protection will be immediately released"]
pub struct IrqPreemptGuard {
    saved_flags: u64,
    _preempt: PreemptGuard,
}

impl IrqPreemptGuard {
    #[inline]
    pub fn new() -> Self {
        let saved_flags = save_flags_cli();
        Self {
            saved_flags,
            _preempt: PreemptGuard::new(),
        }
    }

    #[inline]
    pub fn saved_flags(&self) -> u64 {
        self.saved_flags
    }
}

impl Default for IrqPreemptGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IrqPreemptGuard {
    #[inline]
    fn drop(&mut self) {
        // Flags first: `_preempt` drops after this body, so its reschedule
        // callback runs with interrupts enabled.
        restore_flags(self.saved_flags);
    }
}

/// Register a function to be invoked when a preempt guard drop returns the
/// count to zero with `reschedule_pending` set. The `&BspToken<'brand>`
/// witnesses BSP-only init.
pub fn register_reschedule_callback<'brand>(_token: &BspToken<'brand>, callback: fn()) {
    RESCHEDULE_CALLBACK.store(callback as *mut (), Ordering::Release);
}

/// True if preemption is currently disabled on this CPU, read from the per-CPU
/// PCR directly rather than through [`PreemptBackend`].
#[inline]
pub fn is_preemption_disabled() -> bool {
    PreemptGuard::is_active()
}

/// PCR-backed preempt count, distinct from [`preempt_count`], which reads the
/// [`PreemptBackend`] surface.
#[inline]
pub fn preempt_count_pcr() -> u32 {
    PreemptGuard::count()
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    fn isolate<R>(f: impl FnOnce() -> R) -> R {
        // These tests share `BACKEND_INSTALLED` / `DEFAULT_BACKEND` with every
        // other test in the process, so serialise and reset to a baseline.
        let _g = crate::test_support::global_lock::lock_global_test_state();
        reset_for_test();
        let r = f();
        reset_for_test();
        r
    }

    #[test]
    fn default_backend_starts_at_zero() {
        isolate(|| {
            assert_eq!(preempt_count(), 0);
            assert!(!is_preempt_disabled());
        });
    }

    #[test]
    fn guard_increments_then_drop_decrements() {
        isolate(|| {
            let g = DisabledPreemptGuard::new();
            assert_eq!(preempt_count(), 1);
            assert!(is_preempt_disabled());
            drop(g);
            assert_eq!(preempt_count(), 0);
        });
    }

    #[test]
    fn guards_nest() {
        isolate(|| {
            let _a = DisabledPreemptGuard::new();
            let _b = DisabledPreemptGuard::new();
            let _c = DisabledPreemptGuard::new();
            assert_eq!(preempt_count(), 3);
        });
        assert_eq!(preempt_count(), 0);
    }

    #[test]
    fn irq_entry_pair_balances() {
        isolate(|| {
            irq_entry_bump();
            irq_entry_bump();
            assert_eq!(preempt_count(), 2);
            irq_entry_leave_quiet();
            irq_entry_leave_quiet();
            assert_eq!(preempt_count(), 0);
        });
    }
}
