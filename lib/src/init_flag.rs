//! Atomic initialization and state flags for kernel subsystems.
//!
//! This module provides `InitFlag` - a type-safe abstraction for the common
//! pattern of tracking whether a subsystem has been initialized. It eliminates
//! the boilerplate of manually managing `AtomicBool` statics and their
//! corresponding accessor functions.
//!
//! # Usage
//!
//! ```ignore
//! use slopos_lib::InitFlag;
//!
//! static SUBSYSTEM_INIT: InitFlag = InitFlag::new();
//!
//! pub fn init() {
//!     // Returns true if this is the first call (we should initialize)
//!     // Returns false if already initialized (skip)
//!     if !SUBSYSTEM_INIT.init_once() {
//!         return; // Already initialized
//!     }
//!     // ... perform initialization ...
//! }
//!
//! pub fn is_initialized() -> bool {
//!     SUBSYSTEM_INIT.is_set()
//! }
//! ```
//!
//! # Memory Ordering
//!
//! - `init_once()` uses `SeqCst` swap to ensure visibility across all CPUs
//! - `mark_set()` uses `Release` to publish initialization side-effects
//! - `is_set()` uses `Acquire` to observe initialization side-effects
//! - `is_set_relaxed()` uses `Relaxed` for performance-critical paths where
//!   ordering guarantees aren't needed (e.g., logging guards)

use core::sync::atomic::{AtomicBool, Ordering};

/// Atomic flag for tracking initialization state.
///
/// This is the canonical way to implement init-once semantics in SlopOS.
/// It replaces the common pattern of:
///
/// ```ignore
/// static FOO_INITIALIZED: AtomicBool = AtomicBool::new(false);
///
/// pub fn foo_is_initialized() -> bool {
///     FOO_INITIALIZED.load(Ordering::Acquire)
/// }
/// ```
///
/// With the cleaner:
///
/// ```ignore
/// static FOO_INIT: InitFlag = InitFlag::new();
///
/// pub fn foo_is_initialized() -> bool {
///     FOO_INIT.is_set()
/// }
/// ```
#[repr(transparent)]
pub struct InitFlag {
    flag: AtomicBool,
}

impl InitFlag {
    /// Create a new unset flag.
    #[inline]
    pub const fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
        }
    }

    /// Atomically attempt to initialize.
    ///
    /// Returns `true` if this call performed the initialization (flag was previously unset).
    /// Returns `false` if already initialized (flag was already set).
    ///
    /// This is the idiomatic way to implement init-once:
    ///
    /// ```ignore
    /// if !MY_INIT.init_once() {
    ///     return; // Already initialized, skip
    /// }
    /// // ... do initialization work ...
    /// ```
    ///
    /// Uses `SeqCst` ordering to ensure the initialization is visible to all CPUs.
    #[inline]
    pub fn init_once(&self) -> bool {
        // swap returns the OLD value
        // If old was false (unset), we just set it -> return true (we should init)
        // If old was true (already set), we set it again (no-op) -> return false (skip)
        !self.flag.swap(true, Ordering::SeqCst)
    }

    /// Atomically attempt to claim this flag.
    ///
    /// Same as `init_once()` but with semantics better suited for one-shot
    /// registration patterns (drivers, callbacks, etc).
    ///
    /// Returns `true` if this call claimed the flag (it was previously unclaimed).
    /// Returns `false` if already claimed.
    #[inline]
    pub fn claim(&self) -> bool {
        self.init_once()
    }

    /// Check if the flag is set.
    ///
    /// Uses `Acquire` ordering to ensure any initialization side-effects
    /// performed before `mark_set()` or `init_once()` are visible.
    #[inline]
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Check if the flag is set (relaxed ordering).
    ///
    /// Use this only when you don't need to observe side-effects of initialization,
    /// such as in logging guards or early-exit fast paths.
    #[inline]
    pub fn is_set_relaxed(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Explicitly set the flag.
    ///
    /// Use this when initialization happens in stages and you want to mark
    /// completion at a specific point (rather than using `init_once()` at entry).
    ///
    /// Uses `Release` ordering to publish any initialization side-effects.
    #[inline]
    pub fn mark_set(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Reset the flag to unset state.
    ///
    /// **Warning**: This should rarely be used. It's provided for testing
    /// or for subsystems that support re-initialization.
    ///
    /// Uses `Release` ordering.
    #[inline]
    pub fn reset(&self) {
        self.flag.store(false, Ordering::Release);
    }
}

impl Default for InitFlag {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: InitFlag is just an AtomicBool wrapper, which is Send + Sync
unsafe impl Send for InitFlag {}
unsafe impl Sync for InitFlag {}

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
