//! Per-CPU interior-mutable storage with a checked borrow state.
//!
//! For per-CPU state a caller indexes by CPU number rather than reaching
//! through `CpuLocal`'s current-CPU accessor: the LUF drain rings and the
//! ASID slot table are both walked by explicit index, including from a CPU
//! other than the slot's owner.
//!
//! # Why the borrow word
//!
//! The idiom this replaces was `KernelSync<UnsafeCell<T>>` plus an accessor
//! that handed out `&mut T` under two written obligations: interrupts are
//! off, and nothing else on this CPU aliases the slot. The first is now the
//! [`IrqDisabled`] argument. The second cannot be typed — two sequential
//! calls in one scope produce two `&mut` and the compiler has no reason to
//! object — so it is checked instead, by an atomic borrow word, in every
//! build.
//!
//! Two hazards make that worth an uncontended atomic on the context-switch
//! path. A per-CPU ring whose owner is mid-update can be re-entered by an
//! IPI handler that reaches the same slot, which is why the accessor takes
//! the IRQ witness; and an index is a parameter, so a *foreign* CPU can name
//! a slot its owner is writing, which no per-CPU argument ever ruled out.
//! Under the borrow word both become a declined borrow rather than aliasing
//! UB.
//!
//! Shared and exclusive borrows follow `RefCell`'s rule — any number of
//! readers, or one writer — with the counts in one `AtomicUsize` and the
//! release in a drop guard, so an unwind out of the closure cannot strand
//! the slot.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::cpu::x86_64::interrupts::IrqDisabled;

/// Borrow-word value meaning "one writer holds the slot". Distinct from any
/// reader count because the reader count is bounded by the number of CPUs.
const WRITING: usize = usize::MAX;

/// A per-CPU `T` whose borrows are checked at runtime.
///
/// `Sync` regardless of `T: Sync`: every path to the contents goes through
/// the borrow word, which serialises writers against readers and against
/// each other, on this CPU and on any other.
pub struct PerCpuSlot<T> {
    value: UnsafeCell<T>,
    borrow: AtomicUsize,
}

// SAFETY: the contents are reachable only through `with_mut` / `try_with_ref`,
// which never hand out overlapping `&mut`, or a `&mut` overlapping a `&`,
// because the borrow word denies the second acquisition. `Send` on `T` is
// what makes moving the value between CPUs sound; `T: Sync` is not required
// because a shared borrow is only ever handed out while no writer holds it.
unsafe impl<T: Send> Sync for PerCpuSlot<T> {}
// SAFETY: see the `Sync` impl.
unsafe impl<T: Send> Send for PerCpuSlot<T> {}

/// Releases the slot's borrow when the accessor's closure returns or unwinds.
struct BorrowGuard<'a> {
    borrow: &'a AtomicUsize,
    exclusive: bool,
}

impl Drop for BorrowGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        if self.exclusive {
            self.borrow.store(0, Ordering::Release);
        } else {
            self.borrow.fetch_sub(1, Ordering::Release);
        }
    }
}

impl<T> PerCpuSlot<T> {
    #[inline]
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
            borrow: AtomicUsize::new(0),
        }
    }

    /// Exclusive access for the duration of `f`.
    ///
    /// `None` when the slot is already borrowed — by an interrupted frame on
    /// this CPU, or by another CPU that named this slot by index. A caller
    /// that cannot legally collide should `expect` the result rather than
    /// silently skipping its work.
    ///
    /// The [`IrqDisabled`] witness is the other half: it keeps an interrupt
    /// from arriving mid-update and turning a would-be re-entry into a
    /// declined borrow the interrupted code never asked for.
    #[inline]
    pub fn with_mut<'a, R>(
        &'a self,
        _irq: &'a IrqDisabled<'a>,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        self.borrow
            .compare_exchange(0, WRITING, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        let _guard = BorrowGuard {
            borrow: &self.borrow,
            exclusive: true,
        };
        // SAFETY: the compare-exchange above took the slot from unborrowed to
        // exclusively borrowed, and the guard holds it there until `f`
        // returns or unwinds. No other `&T` or `&mut T` into this slot can
        // exist for that window, on this CPU or any other.
        Some(f(unsafe { &mut *self.value.get() }))
    }

    /// Shared access for the duration of `f`.
    ///
    /// `None` while a [`with_mut`](Self::with_mut) holds the slot. Readers do
    /// not exclude each other, so a diagnostic sweep across CPUs composes.
    #[inline]
    pub fn try_with_ref<R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        let mut cur = self.borrow.load(Ordering::Relaxed);
        loop {
            if cur == WRITING {
                return None;
            }
            match self.borrow.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
        let _guard = BorrowGuard {
            borrow: &self.borrow,
            exclusive: false,
        };
        // SAFETY: the loop above joined the reader count, which `with_mut`
        // cannot do while non-zero, so no `&mut T` into this slot exists for
        // the duration of `f`.
        Some(f(unsafe { &*self.value.get() }))
    }

    /// Exclusive access proven by `&mut self` rather than by the borrow word.
    /// Reachable only before the slot is shared.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

impl<T: Default> Default for PerCpuSlot<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_borrow_round_trips() {
        let slot = PerCpuSlot::new(7u32);
        let doubled = IrqDisabled::with(|irq| {
            slot.with_mut(irq, |v| {
                *v *= 2;
                *v
            })
        });
        assert_eq!(doubled, Some(14));
        assert_eq!(slot.try_with_ref(|v| *v), Some(14));
    }

    /// The property the borrow word exists for: a second exclusive borrow
    /// while the first is live is declined, not granted.
    #[test]
    fn nested_exclusive_borrow_is_declined() {
        let slot = PerCpuSlot::new(0u32);
        let inner = IrqDisabled::with(|irq| {
            slot.with_mut(irq, |_| slot.with_mut(irq, |_| ()))
                .expect("outer borrow succeeds")
        });
        assert!(inner.is_none(), "nested exclusive borrow must be declined");
    }

    #[test]
    fn shared_borrow_is_declined_while_writing() {
        let slot = PerCpuSlot::new(0u32);
        let inner = IrqDisabled::with(|irq| {
            slot.with_mut(irq, |_| slot.try_with_ref(|v| *v))
                .expect("outer borrow succeeds")
        });
        assert!(inner.is_none(), "read under a live writer must be declined");
    }

    #[test]
    fn shared_borrows_compose() {
        let slot = PerCpuSlot::new(3u32);
        let nested = slot
            .try_with_ref(|a| slot.try_with_ref(|b| *a + *b))
            .expect("outer read succeeds");
        assert_eq!(nested, Some(6));
    }

    /// The slot must be usable again after a declined borrow, and after the
    /// granted one that caused the decline has been released.
    #[test]
    fn borrow_is_released_after_the_scope() {
        let slot = PerCpuSlot::new(1u32);
        IrqDisabled::with(|irq| {
            assert!(slot.with_mut(irq, |_| slot.try_with_ref(|_| ())).is_some());
        });
        assert_eq!(slot.try_with_ref(|v| *v), Some(1));
        assert_eq!(IrqDisabled::with(|irq| slot.with_mut(irq, |v| *v)), Some(1));
    }
}
