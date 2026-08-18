//! Generic kernel service registration cell.

use slopos_ostd::sync::OnceLock;

/// A cell for single-registration kernel service tables.
pub struct ServiceCell<T: 'static> {
    inner: OnceLock<&'static T>,
    name: &'static str,
}

impl<T: 'static> ServiceCell<T> {
    /// `name` appears in panic messages.
    #[inline]
    pub const fn new(name: &'static str) -> Self {
        Self {
            inner: OnceLock::new(),
            name,
        }
    }

    /// Panics if already registered.
    #[inline]
    pub fn register(&self, services: &'static T) {
        let mut placed = false;
        self.inner.call_once(|| {
            placed = true;
            services
        });
        assert!(placed, "{} already registered", self.name);
    }

    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.inner.is_completed()
    }

    /// Panics if not initialized.
    #[inline]
    pub fn get(&self) -> &'static T {
        match self.inner.get() {
            Some(r) => *r,
            None => panic!("{} not initialized", self.name),
        }
    }

    #[inline]
    pub fn try_get(&self) -> Option<&'static T> {
        self.inner.get().copied()
    }

    #[inline]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}
