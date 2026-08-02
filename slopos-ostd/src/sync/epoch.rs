//! Scoped RCU epochs.
//!
//! Layered on the kernel's existing RCU machinery (`crate::sync::rcu`),
//! [`Epoch`] gives a subsystem a typed entry point distinct from raw
//! [`rcu_read_lock`]. An [`EpochGuard`] holds an [`RcuReadGuard`] for its
//! lifetime — preemption stays disabled, so no quiescent state can be
//! reported on this CPU while a guard is live, which keeps published
//! pointers valid for the entire scope.
//!
//! When lock tracking is enabled, `Epoch::enter` also records a synthetic
//! lock class on the per-CPU held-lock stack. Acquiring any tracked
//! `SpinLock` while the synthetic class is held panics with a lockdep
//! violation: "SpinLock acquired inside Epoch scope". The motivation is
//! to keep epoch read-side regions structurally short and free of
//! multi-step publish hazards (the same shape that broke SCM_RIGHTS in
//! `unix_sendmsg`).
//!
//! Each subsystem instantiates its own `Epoch` — e.g. `net/`'s
//! `NET_EPOCH` lives next to the TCP demux table. OSTD only ships the
//! type definition; no kernel-wide singleton is implied.
//!
//! # Atomic-publish discipline
//!
//! Writers update RCU-published state via
//! [`crate::sync::RcuCell::replace`]; the displaced box is deferred via
//! [`rcu_call_typed`]. Any auxiliary state a reader depends on must be
//! published *before* the `RcuCell::replace` returns. Treat each
//! `EpochGuard` scope as one consistent reader snapshot.
//!
//! [`rcu_read_lock`]: crate::sync::rcu::rcu_read_lock
//! [`RcuReadGuard`]: crate::sync::rcu::RcuReadGuard
//! [`rcu_call_typed`]: crate::sync::rcu::rcu_call_typed

use core::marker::PhantomData;

use crate::mm::KBox;
use crate::sync::lock_graph;
use crate::sync::lock_graph::LockClassKey;
use crate::sync::rcu;

/// Scoped RCU epoch.
///
/// Each declaration carries its own [`LockClassKey`], so distinct epochs
/// are distinct synthetic classes in [`lock_graph`]. Mint one with
/// [`epoch_class!`](crate::epoch_class).
pub struct Epoch {
    class: &'static LockClassKey,
}

impl Epoch {
    /// Construct an empty epoch. Suitable for `pub static` declarations.
    #[inline]
    pub const fn new(class: &'static LockClassKey) -> Self {
        Self { class }
    }

    /// Open an epoch read-side critical section.
    ///
    /// The returned [`EpochGuard`] disables preemption on the current
    /// CPU and registers a synthetic lock-class entry so that any
    /// subsequent `SpinLock` acquisition (while the guard is live) is
    /// reported by lockdep.
    #[inline]
    #[must_use = "dropping the guard immediately ends the epoch critical section"]
    pub fn enter(&self) -> EpochGuard<'_> {
        let rcu_guard = rcu::rcu_read_lock();
        let addr = self as *const _ as *const ();
        // SAFETY: preemption is disabled by `rcu_read_lock`'s embedded
        // `PreemptGuard`, which pins the CPU whose held stack is updated;
        // `push_epoch` masks interrupts itself, because this path acquires
        // with them enabled. `addr` is the address of a `pub static Epoch`
        // (caller responsibility: do not invoke on a stack-allocated
        // `Epoch`). The matching `pop_epoch` runs in `EpochGuard::drop`.
        unsafe {
            lock_graph::push_epoch(addr, self.class);
        }
        EpochGuard {
            _rcu: rcu_guard,
            epoch_addr: addr,
            _lt: PhantomData,
        }
    }

    /// Block until every online CPU has observed at least one
    /// quiescent state since this call. Wraps
    /// [`crate::sync::rcu::synchronize_rcu`].
    #[inline]
    pub fn wait(&self) {
        rcu::synchronize_rcu();
    }

    /// Schedule `value` for drop after the next grace period. Safe
    /// wrapper over [`crate::sync::rcu::rcu_call_typed`].
    #[inline]
    pub fn defer_kbox<T: Send + 'static>(&self, value: KBox<T>) {
        rcu::rcu_call_typed::<T>(value, drop_typed::<T>);
    }
}

fn drop_typed<T: Send + 'static>(_b: KBox<T>) {
    // `KBox::drop` releases the heap allocation; the typed `_b` parameter
    // exists to give `rcu_call_typed` a monomorphisation handle.
}

/// RAII guard returned by [`Epoch::enter`]. Carries an embedded
/// [`RcuReadGuard`] so preemption stays disabled for the scope; lockdep
/// observes a synthetic class entry tied to the source `Epoch`'s
/// address.
#[must_use = "dropping the guard immediately ends the epoch critical section"]
pub struct EpochGuard<'e> {
    _rcu: rcu::RcuReadGuard,
    epoch_addr: *const (),
    _lt: PhantomData<&'e Epoch>,
}

impl<'e> Drop for EpochGuard<'e> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: paired with the `push_epoch` in `Epoch::enter`. The
        // `epoch_addr` is the same `*const Epoch` we pushed, and
        // preemption is still disabled (the embedded `_rcu` drops
        // after this body returns).
        unsafe {
            lock_graph::pop_epoch(self.epoch_addr);
        }
    }
}
