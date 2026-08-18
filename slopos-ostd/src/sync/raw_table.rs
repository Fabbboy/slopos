//! Boot-installed, caller-mutex-protected flat array.
//!
//! `RawTable<T>` wraps a `(*mut T, len)` pair behind safe accessors, for
//! one-shot tables sized at boot and backed by storage outside the heap — the
//! kernel's physical-page descriptor table and its like.
//!
//! **Caller's contract** (identical to [`RawLink::with_mut`]): every
//! `get_mut(idx)` reborrow must occur under exclusive access to slot `idx`
//! — typically by holding the page-allocator's `SpinLock` **or** by being
//! the per-CPU cache that exclusively owns slot `idx` while pinned by a
//! `PreemptGuard`.
//!
//! [`RawLink::with_mut`]: crate::sync::raw_link::RawLink::with_mut

use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

pub struct RawTable<T: 'static> {
    base: AtomicPtr<T>,
    len: AtomicUsize,
}

// SAFETY: Cross-thread transfer is sound because `get` / `get_mut` carry
// the caller's exclusivity contract (see module docs); the slot itself
// holds no aliasing-sensitive state until `install` runs at boot.
unsafe impl<T: Send + 'static> Send for RawTable<T> {}
unsafe impl<T: Send + 'static> Sync for RawTable<T> {}

impl<T: 'static> RawTable<T> {
    pub const fn empty() -> Self {
        Self {
            base: AtomicPtr::new(core::ptr::null_mut()),
            len: AtomicUsize::new(0),
        }
    }

    /// May be called at most once; subsequent calls panic.
    pub fn install(&self, slice: &'static mut [T]) {
        let base = slice.as_mut_ptr();
        let len = slice.len();
        let prev = self.base.swap(base, Ordering::AcqRel);
        if !prev.is_null() {
            panic!("RawTable::install: already installed");
        }
        self.len.store(len, Ordering::Release);
    }

    #[inline]
    pub fn is_installed(&self) -> bool {
        !self.base.load(Ordering::Acquire).is_null()
    }

    /// Returns 0 before `install`.
    #[inline]
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `None` if uninstalled or out of range.
    ///
    /// **Caller's contract:** no concurrent `get_mut` for slot `idx` may
    /// be active.
    #[inline]
    pub fn get(&self, idx: usize) -> Option<&T> {
        let base = self.base.load(Ordering::Acquire);
        if base.is_null() {
            return None;
        }
        if idx >= self.len.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `base` is non-null and `idx < len` per the bounds check.
        // Caller's contract excludes concurrent `&mut` to slot `idx`.
        Some(unsafe { &*base.add(idx) })
    }

    /// Returns `None` if uninstalled or out of range.
    ///
    /// **Caller's contract:** holds exclusive access to slot `idx` for the
    /// returned reference's lifetime.
    #[inline]
    pub fn get_mut(&self, idx: usize) -> Option<&mut T> {
        let base = self.base.load(Ordering::Acquire);
        if base.is_null() {
            return None;
        }
        if idx >= self.len.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `base` is non-null and `idx < len`. Caller's contract
        // establishes exclusivity.
        Some(unsafe { &mut *base.add(idx) })
    }

    #[inline]
    pub fn with_mut<R>(&self, idx: usize, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        self.get_mut(idx).map(f)
    }
}

impl<T: 'static> Default for RawTable<T> {
    fn default() -> Self {
        Self::empty()
    }
}
