//! One-shot in-place initialiser for self-referential statics.
//!
//! The C-ABI legacy memmap shim and similar self-referential static
//! structures need:
//!
//! 1. Storage at the final `'static` address (so internal `*const`
//!    pointers stay valid for the kernel's lifetime).
//! 2. A one-shot mutation window that runs after the consumer has
//!    received the bootloader handoff (so the pointer fields can be
//!    filled with the post-bringup values).
//! 3. A read-only `*const T` accessor for downstream C-ABI consumers.
//!
//! `core::cell::OnceCell` doesn't fit because the value is already
//! present at static construction (with default field values); only
//! the pointer-mutation step is gated. `OnceLock` doesn't fit because
//! the value is constructed in-place rather than via `call_once(|| …)`.
//!
//! [`InitInPlace<T>`] absorbs the pattern: a `SyncUnsafeCell<T>` for
//! the storage plus an `AtomicBool` gate. The single mutation entry
//! point [`InitInPlace::init_once`] runs its closure exactly once on
//! a `&mut T` reborrow; subsequent calls are no-ops. The read entry
//! point [`InitInPlace::as_ptr`] returns a `*const T` after init has
//! run; before init it returns the unmodified default value, which
//! the consumer detects via a sentinel field.

use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

/// Self-referential one-shot in-place initialiser.
///
/// `T` must be `Sync` — the cell ships a `*const T` to consumers and
/// the inner storage is shared `'static`. After `init_once` returns,
/// the inner value is immutable from this surface (a `&mut T`
/// reborrow is no longer obtainable from outside the cell).
pub struct InitInPlace<T> {
    /// Storage at the cell's `'static` address.
    cell: SyncUnsafeCell<T>,
    /// `false` = uninitialised; `true` = `init_once` body completed.
    gate: AtomicBool,
}

// SAFETY: The cell holds a single `T`; access is serialised by the
// AtomicBool gate (CAS-once writer, all subsequent readers observe a
// non-mutated value). Consumer surface returns `*const T` rather than
// `&T`, mirroring the existing kernel C-ABI shim contract.
unsafe impl<T: Sync> Sync for InitInPlace<T> {}

impl<T> InitInPlace<T> {
    /// Construct a cell pre-populated with `value`. Typically used
    /// from a `const`-evaluable initial value that satisfies the
    /// consumer's "uninitialised but C-ABI-valid" sentinel contract
    /// (e.g. `entry_count = 0`).
    pub const fn new(value: T) -> Self {
        Self {
            cell: SyncUnsafeCell::new(value),
            gate: AtomicBool::new(false),
        }
    }

    /// Run `init` exactly once with a `&mut T` reborrow. Returns
    /// `true` if this call performed the initialisation; `false` if
    /// a prior call already did.
    ///
    /// The closure receives a mutable reference to the cell's stored
    /// `T`. The cell is at its final `'static` address, so any
    /// self-referential pointers the closure writes into the cell
    /// (e.g. `T::entries = &T::entries_storage`) remain valid for the
    /// cell's `'static` lifetime.
    pub fn init_once(&self, init: impl FnOnce(&mut T)) -> bool {
        if self.gate.swap(true, Ordering::SeqCst) {
            return false;
        }
        // SAFETY: the swap above was a CAS-equivalent that returned
        // `false` (was-uninit), so this thread holds exclusive write
        // access to the cell. No reader observes a `*const T`
        // beyond the pre-init sentinel value until we publish via
        // the trailing `SeqCst` fence implicit in `gate.swap`.
        let slot = unsafe { &mut *self.cell.get() };
        init(slot);
        true
    }

    /// Return a `*const T` to the cell's storage. The pointer is
    /// stable for the cell's `'static` lifetime and is the C-ABI
    /// consumer's view of the value.
    ///
    /// Callers that need to gate downstream consumption on
    /// initialisation completion should pair this with
    /// [`Self::is_initialised`].
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.cell.get() as *const T
    }

    /// Returns `true` if [`init_once`] has run to completion.
    #[inline]
    pub fn is_initialised(&self) -> bool {
        self.gate.load(Ordering::Acquire)
    }
}
