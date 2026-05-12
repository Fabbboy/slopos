//! `HermeticState` trait — the contract every snapshot/restore
//! implementer must satisfy.
//!
//! Implementing this trait declares that a piece of kernel-mutable
//! singleton state is **hermetic**: the framework can capture its
//! pre-test value with `snapshot()`, run a test (which may mutate the
//! singleton arbitrarily), and reinstate the pre-test value via
//! `restore()` on scope drop.
//!
//! The trait is `unsafe` because incorrect impls silently corrupt the
//! kernel between tests — exactly the bug class this framework exists
//! to close. Make the obligation explicit.
//!
//! Lives in OSTD so the [`crate::hermetic_state`] declarative macro
//! can emit `unsafe impl HermeticState for X` without reaching
//! across crates. The kernel-side `slopos-hermetic` crate re-exports
//! this trait for source compatibility.

use crate::AllocError;

/// A subsystem with mutable global state that must be saved before a
/// hermetic test enters and restored when the scope drops.
///
/// # Safety
/// - `snapshot` must read all kernel-globally-observable state owned by
///   `Self` and return a value sufficient to reconstruct it.
/// - `restore` must, given the snapshot, recreate the exact pre-snapshot
///   observable state.
/// - Both run on BSP under `pause_all_aps + drain_remote_inbox + synchronize_rcu`
///   quiescence — the scope ensures this. Implementers should not pause
///   APs themselves.
/// - The implementer is responsible for whatever locking is needed
///   (`SpinLock::lock()` etc.) — locking discipline varies by lock level
///   so the framework cannot pick one for you.
pub unsafe trait HermeticState: 'static {
    /// Snapshot value. Must be `Send` because the scope owns it across
    /// the test body's possible task-migration window. `'static` because
    /// the scope's vtable is `'static` and stores it type-erased.
    type Snapshot: Send + 'static;

    /// Diagnostic name. Surfaces in klog when restore is attempted and
    /// in `hermetic_audit.py` output.
    ///
    /// Const item (not a method) so the linker-section vtable entry can
    /// be `const`-constructed by [`crate::hermetic_state`].
    const NAME: &'static str;

    /// Names of states whose `snapshot` must precede this one's, and
    /// whose `restore` must follow this one's. Empty for leaf state.
    ///
    /// The scope topo-sorts the registry by this list at enter; cycles
    /// trigger a panic.
    const DEPENDS_ON: &'static [&'static str] = &[];

    /// Capture the singleton's pre-test value into a heap-allocated
    /// snapshot. Returns `Err(AllocError)` if `KBox::try_new` fails.
    fn snapshot() -> Result<Self::Snapshot, AllocError>;

    /// Reinstate `snap` as the singleton's value.
    ///
    /// # Safety
    /// May only be called from `KernelTestScope::Drop`, with APs paused
    /// and the inbox/RCU quiescence barrier complete. The framework
    /// arranges this; impls must not call `restore` directly.
    unsafe fn restore(snap: Self::Snapshot);
}
