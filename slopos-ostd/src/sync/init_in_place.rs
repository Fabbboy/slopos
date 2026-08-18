//! One-shot in-place initialiser for self-referential statics.
//!
//! Storage sits at the final `'static` address, so `*const` fields the
//! initialiser writes into the value stay valid for the kernel's lifetime.
//! `OnceCell` / `OnceLock` do not fit: the value already exists at static
//! construction with default fields, and only the pointer-mutation step is
//! gated.
//!
//! Before [`InitInPlace::init_once`] runs, [`InitInPlace::as_ptr`] returns the
//! unmodified default value, which the consumer detects via a sentinel field.

use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

/// Self-referential one-shot in-place initialiser. `T` must be `Sync`: the
/// cell ships a `*const T` to consumers and its storage is shared `'static`.
/// After `init_once` returns, no further `&mut T` reborrow is obtainable.
pub struct InitInPlace<T> {
    cell: SyncUnsafeCell<T>,
    gate: AtomicBool,
}

// SAFETY: writes to the single `T` are serialised by the gate (one CAS-once
// writer, every later reader observes a non-mutated value), and the consumer
// surface hands out `*const T` rather than `&T`.
unsafe impl<T: Sync> Sync for InitInPlace<T> {}

impl<T> InitInPlace<T> {
    /// Construct a cell pre-populated with `value`, which must satisfy the
    /// consumer's "uninitialised but C-ABI-valid" sentinel contract (e.g.
    /// `entry_count = 0`).
    pub const fn new(value: T) -> Self {
        Self {
            cell: SyncUnsafeCell::new(value),
            gate: AtomicBool::new(false),
        }
    }

    /// Run `init` exactly once with a `&mut T` reborrow. Returns `true` if
    /// this call performed the initialisation; `false` if a prior call did.
    ///
    /// The cell is at its final `'static` address, so self-referential pointers
    /// the closure writes (e.g. `T::entries = &T::entries_storage`) stay valid.
    pub fn init_once(&self, init: impl FnOnce(&mut T)) -> bool {
        if self.gate.swap(true, Ordering::SeqCst) {
            return false;
        }
        // SAFETY: the swap returned `false` (was uninit), so this thread holds
        // exclusive write access; readers are ordered by the `SeqCst` swap.
        let slot = unsafe { &mut *self.cell.get() };
        init(slot);
        true
    }

    /// The C-ABI consumer's view of the value; stable for the cell's `'static`
    /// lifetime. Pair with [`Self::is_initialised`] to gate on init.
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.cell.get() as *const T
    }

    #[inline]
    pub fn is_initialised(&self) -> bool {
        self.gate.load(Ordering::Acquire)
    }
}
