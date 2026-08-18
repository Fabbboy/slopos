//! `HermeticState` trait — the contract every snapshot/restore
//! implementer must satisfy.
//!
//! The trait is `unsafe` because an incorrect impl silently corrupts the
//! kernel between tests. It lives in OSTD so the [`crate::hermetic_state`]
//! macro can emit `unsafe impl HermeticState for X` without reaching across
//! crates; the kernel-side `slopos-hermetic` crate re-exports it.

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
    /// `Send` because the scope owns it across the test body's possible
    /// task-migration window; `'static` because the scope stores it
    /// type-erased behind a `'static` vtable.
    type Snapshot: Send + 'static;

    /// Diagnostic name, surfacing in klog and `hermetic_audit.py` output.
    /// A const item rather than a method so the linker-section vtable entry
    /// can be `const`-constructed by [`crate::hermetic_state`].
    const NAME: &'static str;

    /// Names of states whose `snapshot` must precede this one's, and whose
    /// `restore` must follow it. The scope topo-sorts the registry by this
    /// list at enter; cycles panic.
    const DEPENDS_ON: &'static [&'static str] = &[];

    /// Capture the singleton's pre-test value into a heap-allocated snapshot.
    fn snapshot() -> Result<Self::Snapshot, AllocError>;

    /// Reinstate `snap` as the singleton's value.
    ///
    /// # Safety
    /// May only be called from `KernelTestScope::Drop`, with APs paused
    /// and the inbox/RCU quiescence barrier complete. The framework
    /// arranges this; impls must not call `restore` directly.
    unsafe fn restore(snap: Self::Snapshot);
}
