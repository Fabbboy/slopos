use core::sync::atomic::{AtomicBool, Ordering};

pub use slopos_arch::InitFlag;

/// Atomic flag for tracking in-progress operations.
///
/// Similar to `InitFlag` but with semantics suited for tracking whether
/// an operation is currently in progress (shutdown, panic handling, etc).
///
/// The key difference from `InitFlag`:
/// - `InitFlag`: "Has X been done?" (monotonic: false -> true, stays true)
/// - `StateFlag`: "Is X currently happening?" (can toggle)
#[repr(transparent)]
pub struct StateFlag {
    flag: AtomicBool,
}

impl StateFlag {
    /// Create a new inactive flag.
    #[inline]
    pub const fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
        }
    }

    /// Atomically try to enter this state.
    ///
    /// Returns `true` if this call entered the state (was previously inactive).
    /// Returns `false` if already in this state.
    ///
    /// Typical usage for shutdown/panic handling:
    ///
    /// ```ignore
    /// if !SHUTDOWN_IN_PROGRESS.enter() {
    ///     // Already shutting down, just halt
    ///     halt();
    /// }
    /// // ... perform shutdown ...
    /// ```
    #[inline]
    pub fn enter(&self) -> bool {
        !self.flag.swap(true, Ordering::SeqCst)
    }

    /// Check if currently in this state.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Check if currently in this state (relaxed ordering).
    #[inline]
    pub fn is_active_relaxed(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Mark as active.
    #[inline]
    pub fn set_active(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Mark as inactive.
    #[inline]
    pub fn set_inactive(&self) {
        self.flag.store(false, Ordering::Release);
    }

    /// Leave this state (mark inactive).
    #[inline]
    pub fn leave(&self) {
        self.set_inactive();
    }

    /// Atomically check if active and clear if so (consume pattern).
    ///
    /// Returns `true` if the flag was active (and is now inactive).
    /// Returns `false` if the flag was already inactive.
    ///
    /// This is useful for one-shot state consumption:
    ///
    /// ```ignore
    /// if HAS_CPU_STATE.take() {
    ///     // State was set, now cleared - use the associated data
    /// }
    /// ```
    #[inline]
    pub fn take(&self) -> bool {
        self.flag.swap(false, Ordering::SeqCst)
    }
}

impl Default for StateFlag {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: StateFlag is just an AtomicBool wrapper, which is Send + Sync
unsafe impl Send for StateFlag {}
unsafe impl Sync for StateFlag {}
