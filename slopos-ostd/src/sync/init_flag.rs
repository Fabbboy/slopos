//! Atomic boolean flags: [`InitFlag`] gates one-shot init, [`StateFlag`]
//! tracks toggleable runtime state such as in-progress shutdown.

use core::sync::atomic::{AtomicBool, Ordering};

#[repr(transparent)]
pub struct InitFlag {
    flag: AtomicBool,
}

impl InitFlag {
    #[inline]
    pub const fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
        }
    }

    #[inline]
    pub fn init_once(&self) -> bool {
        !self.flag.swap(true, Ordering::SeqCst)
    }

    #[inline]
    pub fn claim(&self) -> bool {
        self.init_once()
    }

    #[inline]
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_set_relaxed(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn mark_set(&self) {
        self.flag.store(true, Ordering::Release);
    }

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

/// Atomic flag for an in-progress operation. Unlike [`InitFlag`], which is
/// monotonic false -> true, this one toggles.
#[repr(transparent)]
pub struct StateFlag {
    flag: AtomicBool,
}

impl StateFlag {
    #[inline]
    pub const fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
        }
    }

    /// Returns `true` if this call entered the state, `false` if already in it.
    #[inline]
    pub fn enter(&self) -> bool {
        !self.flag.swap(true, Ordering::SeqCst)
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_active_relaxed(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_active(&self) {
        self.flag.store(true, Ordering::Release);
    }

    #[inline]
    pub fn set_inactive(&self) {
        self.flag.store(false, Ordering::Release);
    }

    #[inline]
    pub fn leave(&self) {
        self.set_inactive();
    }

    /// Returns `true` if the flag was active, clearing it.
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

// SAFETY: StateFlag is just an AtomicBool wrapper, which is Send + Sync.
unsafe impl Send for StateFlag {}
unsafe impl Sync for StateFlag {}
