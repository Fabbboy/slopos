//! Single-publisher / multi-observer atomic cell.
//!
//! A producer publishes a value exactly once, observers see either *not yet* or
//! *the value*, and the value stays addressable for the cell's lifetime once
//! published. Pairing a durable per-task `exit_info` cell with a per-task
//! `WaitQueue` collapses the two-atomic observation race a
//! `(status, waiting_on)` pair has.
//!
//! Storage is a single [`AtomicPtr<T>`] holding either null or a [`KBox<T>`]
//! leaked via [`KBox::into_raw`]; the cell owns the pointee and `Drop`
//! reclaims it, at one allocation per publish.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::mm::heap::KBox;

pub struct AtomicCell<T> {
    raw: AtomicPtr<T>,
    _marker: PhantomData<KBox<T>>,
}

// SAFETY: the cell is the sole owner of the heap-allocated `T`, and readers
// only observe a fully-constructed `T` because publish is a release store. The
// `T` bounds mirror what `KBox<T>` itself would expose.
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

    /// Publish `value` if the cell is still empty. If another publisher won,
    /// the original `value` comes back in `Err` for the caller to recover or
    /// drop.
    pub fn try_set(&self, value: T) -> Result<(), T> {
        // Allocation failure panics rather than surfacing `AllocError`, which
        // would force every caller to handle a path this kernel cannot reach.
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
                // SAFETY: `new_ptr` came from `KBox::into_raw` above and the
                // failed CAS never published it, so we are still sole owner.
                let boxed = unsafe { KBox::from_raw(new_ptr) };
                Err(KBox::into_inner(boxed))
            }
        }
    }

    /// A published value stays addressable until `Drop` or `reset`, so a late
    /// observer can still borrow it.
    #[inline]
    pub fn try_get(&self) -> Option<&T> {
        let ptr = self.raw.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            // SAFETY: a non-null `raw` was published by `try_set` and is only
            // cleared by `take` or by `reset`, whose caller has promised to
            // serialise with readers; the pointee is live while we hold
            // `&self`.
            Some(unsafe { &*ptr })
        }
    }

    /// Take ownership of the published value; `None` if the cell is empty or a
    /// concurrent `take` won.
    pub fn take(&self) -> Option<KBox<T>> {
        let ptr = self.raw.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if ptr.is_null() {
            None
        } else {
            // SAFETY: the swap atomically transferred ownership of the heap
            // allocation from the cell to us, leaving null behind, so no other
            // observer can reach this pointer.
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
    /// The owner must be provably unreachable from every other CPU.
    /// Calling concurrently with
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
        // `&mut self` proves no aliasing exists; the borrow checker has
        // already serialised us.
        let ptr = *self.raw.get_mut();
        if !ptr.is_null() {
            // SAFETY: a non-null pointer here means no `take` ran, so the cell
            // still owns the allocation.
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

        assert!(!cell.is_set());
        assert!(cell.take().is_none());
        assert!(cell.try_set(100).is_ok());
        assert_eq!(cell.try_get().copied(), Some(100));
    }

    #[test]
    fn test_atomic_cell_concurrent_set_one_winner() {
        // Repeated to exercise different interleavings.
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
