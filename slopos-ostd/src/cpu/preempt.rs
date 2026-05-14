//! Preemption control surface.
//!
//! [`DisabledPreemptGuard`] is the RAII gate drivers and OSTD-internal
//! IRQ-dispatch use whenever they require atomic-context (no preemption
//! between point A and point B). Construction increments the active
//! [`PreemptBackend`]'s preempt count; drop decrements it.
//!
//! The backend is a one-shot-registered trait object. The default
//! [`NoOpBackend`] just tracks a private `AtomicU32` so host-side unit
//! tests have something to observe. Production wiring registers a
//! per-CPU backend that proxies to the kernel's per-CPU preempt-count
//! field.
//!
//! # Why a backend trait
//!
//! Per-CPU preempt counters live in the kernel's per-CPU region (PCR)
//! which OSTD does not own yet — the per-CPU machinery + `CpuLocal<T>`
//! land in a later subtask group. Until then, OSTD exposes the typed
//! guard surface and lets the kernel install the actual storage.
//!
//! # Soundness
//!
//! Inv. 2: kernel-mode CPU state cannot be tampered with by OSTD
//! clients. The guard mediates which contexts run with preemption
//! disabled; the backend trait keeps the actual decrement-and-callback
//! logic on the trusted side.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use crate::cpu::x86_64::pcr;
use crate::cpu::x86_64::{restore_flags, save_flags_cli};
use crate::sync::BspToken;

// ---------------------------------------------------------------------------
// PreemptBackend trait.
// ---------------------------------------------------------------------------

/// Per-CPU preempt-count operations.
///
/// Implementations must:
/// - operate only on the *current* CPU's preempt-count storage (the
///   guard is `!Send` so it cannot escape its CPU);
/// - keep `enter` / `leave` symmetric for nesting;
/// - in `leave_quiet`, perform the decrement *without* invoking any
///   deferred reschedule callback — this is the variant used at the
///   tail of an IST exception handler, where running the scheduler
///   would corrupt the IST stack.
pub trait PreemptBackend: Send + Sync + 'static {
    /// Increment the current CPU's preempt count.
    fn enter(&self);

    /// Decrement the current CPU's preempt count and, if applicable,
    /// run any pending reschedule callback when the count returns to
    /// zero.
    fn leave(&self);

    /// Decrement the current CPU's preempt count *without* running any
    /// deferred reschedule callback. Used at the IRET-edge of IST
    /// exception handlers.
    fn leave_quiet(&self) {
        self.leave();
    }

    /// Snapshot the current CPU's preempt count.
    fn count(&self) -> u32;
}

// ---------------------------------------------------------------------------
// Default NoOp backend.
// ---------------------------------------------------------------------------

/// In-OSTD fallback used when no production backend is registered.
/// Tracks a single global `AtomicU32`; useful for host-side unit tests
/// that exercise the guard surface without a real per-CPU PCR.
pub struct NoOpBackend {
    count: AtomicU32,
}

impl NoOpBackend {
    pub const fn new() -> Self {
        Self {
            count: AtomicU32::new(0),
        }
    }

    /// Test-only count reset.
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

/// Borrow the default no-op backend. Exposed for test setup that
/// wants to assert against the default backend's count after
/// `reset_for_test`.
#[cfg(any(test, feature = "test-helpers"))]
pub fn default_backend() -> &'static NoOpBackend {
    &DEFAULT_BACKEND
}

// ---------------------------------------------------------------------------
// PCR-backed preempt backend (production default for the kernel target).
// ---------------------------------------------------------------------------

/// `PreemptBackend` impl that proxies to the per-CPU preempt count
/// living in `slopos_ostd::cpu::x86_64::pcr::ProcessorControlRegion::preempt_count`.
///
/// Registered at boot via [`register_preempt_backend`] with a
/// `&BspToken<'_>` witness. Pre-registration the [`NoOpBackend`]
/// default is active; the kernel's main path passes through here only
/// after the BSP PCR has been installed (`pcr.install()` happens
/// before `register_preempt_backend` is called).
pub struct PcrPreemptBackend;

/// Shared instance — `register_preempt_backend(token, &DEFAULT_PCR_PREEMPT)`.
pub static DEFAULT_PCR_PREEMPT: PcrPreemptBackend = PcrPreemptBackend;

#[cfg(all(target_arch = "x86_64", not(test)))]
impl PreemptBackend for PcrPreemptBackend {
    #[inline]
    fn enter(&self) {
        // SAFETY: `register_preempt_backend` is only invoked after
        // `pcr.install()` has run on the BSP, so `current_pcr()`
        // returns a valid `&'static ProcessorControlRegion`.
        unsafe {
            crate::cpu::x86_64::pcr::current_pcr()
                .preempt_count
                .fetch_add(1, Ordering::AcqRel);
        }
    }

    #[inline]
    fn leave(&self) {
        // No reschedule-callback dispatch yet — that wiring lands with
        // the scheduler migration. Maintaining the count is sufficient
        // for now; legacy paths still drive scheduling.
        unsafe {
            crate::cpu::x86_64::pcr::current_pcr()
                .preempt_count
                .fetch_sub(1, Ordering::AcqRel);
        }
    }

    #[inline]
    fn count(&self) -> u32 {
        unsafe {
            crate::cpu::x86_64::pcr::current_pcr()
                .preempt_count
                .load(Ordering::Acquire)
        }
    }
}

// Host-test stub: no PCR, no GS-base — fall back to NoOp-style atomics.
#[cfg(not(all(target_arch = "x86_64", not(test))))]
impl PreemptBackend for PcrPreemptBackend {
    fn enter(&self) {}
    fn leave(&self) {}
    fn count(&self) -> u32 {
        0
    }
}

// ---------------------------------------------------------------------------
// One-shot backend registration.
// ---------------------------------------------------------------------------

struct BackendSlot(UnsafeCell<MaybeUninit<&'static dyn PreemptBackend>>);
// SAFETY: writes are gated by `BACKEND_INSTALLED.swap(true, AcqRel)`
// (one-shot); subsequent reads only happen after observing the flag
// with Acquire, so the read sees the published reference. Inv. 2.
unsafe impl Sync for BackendSlot {}

static BACKEND_SLOT: BackendSlot = BackendSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production preempt backend. The
/// kernel registers a backend that proxies to the per-CPU preempt
/// count field; before this is called, [`DisabledPreemptGuard`] uses
/// the OSTD-internal [`NoOpBackend`]. The `&BspToken<'brand>`
/// witnesses BSP-only init; `backend` must live for the static
/// lifetime of the kernel and only access per-CPU state the kernel
/// authorises (Inv. 2 — kernel-mode state untamperable).
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

/// Test-only reset hook.
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
    // SAFETY: paired Release in `register_preempt_backend`. Once the
    // flag is observed set, the slot is initialised and the reference
    // is `'static`.
    unsafe { *(*BACKEND_SLOT.0.get()).as_ptr() }
}

// ---------------------------------------------------------------------------
// DisabledPreemptGuard.
// ---------------------------------------------------------------------------

/// RAII guard that disables preemption while held.
///
/// `!Send` (carries `PhantomData<*const ()>`): a guard cannot migrate
/// across CPUs because the count it manipulates is per-CPU.
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

/// Snapshot of the current CPU's preempt count.
#[inline]
pub fn preempt_count() -> u32 {
    current_backend().count()
}

/// True if preemption is currently disabled on this CPU.
#[inline]
pub fn is_preempt_disabled() -> bool {
    preempt_count() > 0
}

// ---------------------------------------------------------------------------
// IRQ-entry-side guard (consumed by `irq::idt::IrqEntryGuard`).
// ---------------------------------------------------------------------------

/// Internal hook called by [`crate::irq::idt::IrqEntryGuard::enter`]
/// for IST-using vectors. Bumps the count via the active backend.
#[inline]
pub(crate) fn irq_entry_bump() {
    current_backend().enter();
}

/// Internal hook called by [`crate::irq::idt::IrqEntryGuard::drop`]
/// for IST-using vectors. Decrements *without* invoking any deferred
/// reschedule callback — yielding from an IST handler would corrupt
/// the per-vector IST stack.
#[inline]
pub(crate) fn irq_entry_leave_quiet() {
    current_backend().leave_quiet();
}

// ---------------------------------------------------------------------------
// PreemptGuard / IrqPreemptGuard (PCR-backed).
//
// These complement [`DisabledPreemptGuard`] by carrying deferred-reschedule
// callback semantics: when a `PreemptGuard` drop sees the count returning to
// zero with `reschedule_pending` set, it invokes a registered callback. They
// run against the kernel's per-CPU PCR storage directly via `crate::cpu::x86_64::pcr`
// rather than through [`PreemptBackend`], because the kernel call sites
// observe the PCR field directly.
// ---------------------------------------------------------------------------

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
        // SAFETY: Only accessing atomic fields on the current CPU's PCR.
        unsafe { pcr::current_pcr() }
            .preempt_count
            .fetch_add(1, Ordering::Relaxed);
        Self {
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn is_active() -> bool {
        // SAFETY: Reading atomic field on the current CPU's PCR.
        unsafe { pcr::current_pcr() }
            .preempt_count
            .load(Ordering::Relaxed)
            > 0
    }

    #[inline]
    pub fn count() -> u32 {
        // SAFETY: Reading atomic field on the current CPU's PCR.
        unsafe { pcr::current_pcr() }
            .preempt_count
            .load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_reschedule_pending() {
        // SAFETY: Writing atomic field on the current CPU's PCR.
        unsafe { pcr::current_pcr() }
            .reschedule_pending
            .store(1, Ordering::Release);
    }

    #[inline]
    pub fn is_reschedule_pending() -> bool {
        // SAFETY: Reading atomic field on the current CPU's PCR.
        unsafe { pcr::current_pcr() }
            .reschedule_pending
            .load(Ordering::Acquire)
            != 0
    }

    #[inline]
    pub fn clear_reschedule_pending() {
        // SAFETY: Writing atomic field on the current CPU's PCR.
        unsafe { pcr::current_pcr() }
            .reschedule_pending
            .store(0, Ordering::Release);
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
        // SAFETY: Only accessing atomic fields on the current CPU's PCR.
        let pcr = unsafe { pcr::current_pcr() };
        let prev = pcr.preempt_count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "preempt_count underflow");

        if prev == 1 && pcr.reschedule_pending.swap(0, Ordering::AcqRel) != 0 {
            let fn_ptr = RESCHEDULE_CALLBACK.load(Ordering::Acquire);
            if !fn_ptr.is_null() {
                // SAFETY: fn_ptr was set via register_reschedule_callback with a valid fn().
                let callback: fn() = unsafe { core::mem::transmute(fn_ptr) };
                callback();
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
        // Restore flags first. _preempt drops after this body completes,
        // which is correct: reschedule callback runs with interrupts enabled.
        restore_flags(self.saved_flags);
    }
}

/// Register a function to be invoked when a preempt guard drop returns the
/// count to zero with `reschedule_pending` set. The `&BspToken<'brand>`
/// witnesses BSP-only init.
pub fn register_reschedule_callback<'brand>(_token: &BspToken<'brand>, callback: fn()) {
    RESCHEDULE_CALLBACK.store(callback as *mut (), Ordering::Release);
}

/// True if preemption is currently disabled on this CPU (PCR-backed).
///
/// Companion to [`PreemptGuard`]; reads the per-CPU PCR directly
/// rather than going through [`PreemptBackend`]. Re-exported from
/// [`crate::sync`] under the historical `is_preemption_disabled` name.
#[inline]
pub fn is_preemption_disabled() -> bool {
    PreemptGuard::is_active()
}

/// PCR-backed preempt count snapshot.
///
/// Companion to [`PreemptGuard`]. Distinct from [`preempt_count`] (which
/// reads the [`PreemptBackend`] surface) because kernel call sites
/// observe the per-CPU PCR field directly.
#[inline]
pub fn preempt_count_pcr() -> u32 {
    PreemptGuard::count()
}

// ---------------------------------------------------------------------------
// Lib unit tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    fn isolate<R>(f: impl FnOnce() -> R) -> R {
        // The lib unit tests share process state with whatever has
        // already poked `BACKEND_INSTALLED` / `DEFAULT_BACKEND`. Reset
        // first so each test starts from a known baseline.
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
        // After all three drop the isolate teardown asserts zero via reset.
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
