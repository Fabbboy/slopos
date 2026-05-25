//! Single-publisher / multi-observer atomic cell.
//!
//! Part of the scheduler's wait/wake/block protocol: this cell is the
//! "durable exit value" primitive paired with the per-task `WaitQueue` to
//! close the two-atomic observation race.
//!
//! [`AtomicCell<T>`] is the durable-publish primitive used by the
//! scheduler's wait/wake protocol: a producer publishes a value
//! exactly once, observers see either *not yet* or *the value*, and
//! the value remains addressable for the cell's lifetime once
//! published. Pairing a durable per-task `exit_info` cell with a
//! per-task `WaitQueue` collapses the two-atomic observation race
//! that the previous `(status, waiting_on)` pair had.
//!
//! Storage is heap-backed: a single [`AtomicPtr<T>`] holding either
//! null or a [`KBox<T>`] leaked via [`KBox::into_raw`]. The cell
//! owns whatever it points to; `Drop` reclaims it. One alloc per
//! publish — for `ExitInfo` that is one alloc per task termination,
//! which is fine.
//!
//! # Memory ordering
//!
//! - [`try_set`](AtomicCell::try_set): `AcqRel` CAS publishes the
//!   pointer. Failure path returns the value to the caller (the
//!   transient `KBox` is reclaimed via [`KBox::from_raw`]).
//! - [`try_get`](AtomicCell::try_get): `Acquire` load — the
//!   referent's writes performed before publish are visible.
//! - [`take`](AtomicCell::take): `AcqRel` swap-to-null transfers
//!   ownership of the inner `KBox` to the caller.
//! - [`is_set`](AtomicCell::is_set): `Acquire` load.
//! - [`reset`](AtomicCell::reset): `Release` swap-to-null +
//!   in-place drop of the prior pointee. Marked `unsafe` because
//!   the caller must serialise externally with all readers; given
//!   exclusivity, the load half of the RMW is irrelevant and any
//!   ordering would be sound, but `Release` documents intent.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::mm::heap::KBox;

pub struct AtomicCell<T> {
    raw: AtomicPtr<T>,
    _marker: PhantomData<KBox<T>>,
}

// SAFETY: AtomicCell is the sole owner of the heap-allocated `T`.
// Sharing across threads is gated by AcqRel CAS / Acquire loads on
// `raw`; readers only ever observe a fully-constructed `T` because
// publish stores happen with Release ordering. The `Send + Sync`
// bounds on `T` mirror what `KBox<T>` itself would expose.
unsafe impl<T: Send> Send for AtomicCell<T> {}
unsafe impl<T: Send + Sync> Sync for AtomicCell<T> {}

impl<T> AtomicCell<T> {
    #[inline]
    pub const fn empty() -> Self {
        Self {
            raw: AtomicPtr::new(core::ptr::null_mut()),
            _marker: PhantomData,
        }
    }

    /// Publish `value` if the cell is still empty.
    ///
    /// On success the cell takes ownership of `value` (heap-backed).
    /// On loss (someone published first) the original `value` is
    /// returned in `Err`, so the caller can recover or drop it.
    ///
    /// `AcqRel` on success synchronises subsequent observers; the
    /// `Acquire` failure ordering is harmless — the lost-CAS path
    /// reclaims the still-uninstalled allocation locally.
    pub fn try_set(&self, value: T) -> Result<(), T> {
        // Heap-allocate first; we publish a stable pointer, not an
        // inline value. OOM panics at the heap layer (matching the
        // rest of slopos-ostd's allocation discipline) — surfacing
        // `AllocError` here would force every caller to handle a
        // path that, in practice, is unrepresentable in this kernel.
        let boxed = KBox::try_new(value)
            .unwrap_or_else(|_| panic!("AtomicCell::try_set: heap allocation failed"));
        let new_ptr = KBox::into_raw(boxed);
        match self.raw.compare_exchange(
            core::ptr::null_mut(),
            new_ptr,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(_) => {
                // SAFETY: `new_ptr` came from `KBox::into_raw` just
                // above and was never published (the CAS failed),
                // so we are still the sole owner. Reconstructing the
                // KBox lets us move the inner `T` back out for the
                // caller while freeing the heap slot.
                let boxed = unsafe { KBox::from_raw(new_ptr) };
                Err(KBox::into_inner(boxed))
            }
        }
    }

    /// Returns a reference to the published value if any.
    ///
    /// The reference's lifetime is bound to `&self`, so the cell —
    /// and thus the heap allocation — outlives the borrow. Late
    /// observers using this method are why publishes are durable:
    /// the value remains addressable until `Drop` or `reset`.
    #[inline]
    pub fn try_get(&self) -> Option<&T> {
        let ptr = self.raw.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            // SAFETY: a non-null `raw` was published by `try_set`
            // with Release ordering and only mutated again by
            // `take` (AcqRel swap to null) or `reset` (which the
            // caller pinky-promised to serialise with readers).
            // Either way the pointee is a live `T` for as long as
            // we hold `&self`.
            Some(unsafe { &*ptr })
        }
    }

    /// Take ownership of the published value, leaving the cell empty.
    ///
    /// Returns `None` if the cell is empty or if a concurrent
    /// `take` raced first.
    pub fn take(&self) -> Option<KBox<T>> {
        let ptr = self.raw.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if ptr.is_null() {
            None
        } else {
            // SAFETY: the swap atomically transferred ownership of
            // the heap allocation from the cell to us; no other
            // thread can observe `raw` as non-null + this exact
            // pointer after the swap, because the swap published a
            // null in its place.
            Some(unsafe { KBox::from_raw(ptr) })
        }
    }

    #[inline]
    pub fn is_set(&self) -> bool {
        !self.raw.load(Ordering::Acquire).is_null()
    }

    /// Drop any extant value and return the cell to the empty state.
    ///
    /// # Safety
    ///
    /// Caller must serialise externally with all readers and writers.
    /// This is the slot-recycling primitive used by
    /// `Task::reset_in_place`, where the task is provably
    /// unreachable from any other CPU. Calling concurrently with
    /// [`try_get`], [`try_set`], or [`take`] is a use-after-free.
    pub unsafe fn reset(&self) {
        let ptr = self.raw.swap(core::ptr::null_mut(), Ordering::Release);
        if !ptr.is_null() {
            // SAFETY: caller-asserted exclusivity above; reconstructing
            // the KBox here drops the allocation in place.
            let _ = unsafe { KBox::from_raw(ptr) };
        }
    }
}

impl<T> Drop for AtomicCell<T> {
    fn drop(&mut self) {
        // `&mut self` proves no aliasing exists, so a Relaxed load
        // suffices — the borrow checker has already serialised us.
        let ptr = *self.raw.get_mut();
        if !ptr.is_null() {
            // SAFETY: ownership of the heap allocation stayed with
            // the cell from publish until now (no `take`); reclaim it.
            let _ = unsafe { KBox::from_raw(ptr) };
        }
    }
}

impl<T> Default for AtomicCell<T> {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};
    use std::thread;

    #[test]
    fn test_atomic_cell_double_set_rejected() {
        let cell: AtomicCell<u64> = AtomicCell::empty();
        assert!(!cell.is_set());
        assert!(cell.try_set(7).is_ok());
        assert!(cell.is_set());

        let again = cell.try_set(99);
        match again {
            Err(v) => assert_eq!(v, 99, "second set must return the rejected value"),
            Ok(()) => panic!("second set must fail"),
        }
        assert_eq!(cell.try_get().copied(), Some(7));
    }

    #[test]
    fn test_atomic_cell_set_then_get() {
        let cell: AtomicCell<u32> = AtomicCell::empty();
        assert!(cell.try_get().is_none());
        cell.try_set(0xdead_beef).unwrap();
        let r = cell.try_get().expect("set then get must observe value");
        assert_eq!(*r, 0xdead_beef);
        // Lifetime is tied to &cell — multiple gets are fine.
        let r2 = cell.try_get().unwrap();
        assert_eq!(*r2, 0xdead_beef);
    }

    #[test]
    fn test_atomic_cell_take_consumes() {
        let cell: AtomicCell<u64> = AtomicCell::empty();
        assert!(cell.take().is_none());

        cell.try_set(42).unwrap();
        let owned = cell.take().expect("take must return the published box");
        assert_eq!(*owned, 42);

        // Cell is now empty; take returns None and try_set works again.
        assert!(!cell.is_set());
        assert!(cell.take().is_none());
        assert!(cell.try_set(100).is_ok());
        assert_eq!(cell.try_get().copied(), Some(100));
    }

    #[test]
    fn test_atomic_cell_concurrent_set_one_winner() {
        // 8 threads race to publish; exactly one Ok, the other 7 get
        // their value back via Err. Repeat the experiment to exercise
        // different interleavings.
        for trial in 0..32 {
            let cell: Arc<AtomicCell<u64>> = Arc::new(AtomicCell::empty());
            let winners = Arc::new(AtomicUsize::new(0));
            let losers = Arc::new(AtomicUsize::new(0));

            let mut handles = std::vec::Vec::new();
            for tid in 0..8u64 {
                let cell = Arc::clone(&cell);
                let winners = Arc::clone(&winners);
                let losers = Arc::clone(&losers);
                let value = trial * 100 + tid;
                handles.push(thread::spawn(move || match cell.try_set(value) {
                    Ok(()) => {
                        winners.fetch_add(1, StdOrdering::SeqCst);
                    }
                    Err(returned) => {
                        assert_eq!(
                            returned, value,
                            "loser must get its own value back, not someone else's"
                        );
                        losers.fetch_add(1, StdOrdering::SeqCst);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }

            assert_eq!(winners.load(StdOrdering::SeqCst), 1, "trial {trial}");
            assert_eq!(losers.load(StdOrdering::SeqCst), 7, "trial {trial}");
            assert!(cell.is_set());
        }
    }
}
