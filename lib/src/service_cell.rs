//! Generic kernel service registration cell.
//!
//! Eliminates duplicated `AtomicPtr` boilerplate for kernel service tables.

use core::sync::atomic::{AtomicPtr, Ordering};

/// A cell for single-registration kernel service tables.
pub struct ServiceCell<T> {
    ptr: AtomicPtr<T>,
    name: &'static str,
}

// SAFETY: Only stores pointer to 'static T; AtomicPtr provides synchronization.
unsafe impl<T> Sync for ServiceCell<T> {}

impl<T> ServiceCell<T> {
    /// Create an uninitialized cell. `name` appears in panic messages.
    #[inline]
    pub const fn new(name: &'static str) -> Self {
        Self {
            ptr: AtomicPtr::new(core::ptr::null_mut()),
            name,
        }
    }

    /// Register the service table. Panics if already registered.
    #[inline]
    pub fn register(&self, services: &'static T) {
        let prev = self
            .ptr
            .swap(services as *const T as *mut T, Ordering::Release);
        assert!(prev.is_null(), "{} already registered", self.name);
    }

    #[inline]
    pub fn is_initialized(&self) -> bool {
        !self.ptr.load(Ordering::Acquire).is_null()
    }

    /// Get the service table. Panics if not initialized.
    #[inline]
    pub fn get(&self) -> &'static T {
        let ptr = self.ptr.load(Ordering::Acquire);
        assert!(!ptr.is_null(), "{} not initialized", self.name);
        // SAFETY: Only store valid &'static T pointers; assert ensures non-null.
        unsafe { &*ptr }
    }

    /// Try to get the service table, returns `None` if not registered.
    #[inline]
    pub fn try_get(&self) -> Option<&'static T> {
        let ptr = self.ptr.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            // SAFETY: Only store valid &'static T pointers.
            Some(unsafe { &*ptr })
        }
    }

    #[inline]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}
