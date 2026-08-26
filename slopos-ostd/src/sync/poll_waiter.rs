//! The registration token for poll/select-style waits.
//!
//! `wait_event*` owns a stack-pinned node whose `has_woken` flag catches a wake
//! racing its decision to yield. `poll(2)` cannot: it registers on N queues and
//! only afterwards parks, so a wake arriving during the remaining fd scan finds
//! a task still `Running`, which every wake path treats as nothing to do.
//!
//! A `PollWaiter` arms a token in the task's state word for that gap. A wake
//! aimed at an armed task sets `pending` there, and [`block`](PollWaiter::block)
//! consumes it in the same compare-exchange that would otherwise park — so
//! there is no window between testing the token and parking.
//!
//! Registering demands a [`PollWaiterRef`], so a registration no block will
//! consume cannot be written. `new` claims a single per-task slot and answers
//! `None` to a second caller, so a nested poll fails rather than sharing its
//! parent's token. Dropping disarms, so a late wake cannot leave `pending` set
//! for an unrelated later poll.
//!
//! The owning/borrowed split exists because the two ends live in different
//! crates: the syscall owns the lifecycle, but the `poll_fused` impls that
//! register sit behind `slopos_abi`'s `FileOps`, which cannot name an OSTD
//! type. `PollWaiterRef::current` therefore discovers the token instead — and
//! since minting one re-checks the armed bit, discovery is as strong as
//! passing.

use core::marker::PhantomData;

use super::wait_queue::backend;

/// A live registration token for one poll/select-style wait. See the
/// [module docs](self).
///
/// `!Send`/`!Sync` via [`PhantomData`] (`negative_impls` is not enabled here):
/// the token names *the current task*, so carrying one across threads would arm
/// one task's slot and consume another's.
#[must_use = "a PollWaiter does nothing until you register on it and block"]
pub struct PollWaiter {
    _not_send: PhantomData<*const ()>,
}

impl PollWaiter {
    /// Claim the current task's poll-waiter slot.
    ///
    /// `None` when there is no current task, no backend is registered, or this
    /// task already holds one. The caller then has no durable token and must
    /// not park as though it had: poll's fallback is a timed re-scan.
    #[inline]
    pub fn new() -> Option<Self> {
        backend().poll_arm_current().then_some(Self {
            _not_send: PhantomData,
        })
    }

    /// Discard an unconsumed wake, keeping the token armed.
    ///
    /// Call once per iteration *after* [`block`](Self::block), never before:
    /// a token set during the readiness scan must survive into the block, while
    /// one left by the wake that just released it must not, or the next
    /// iteration consumes it and spins. Linux's `smp_store_mb(pwq->triggered,
    /// 0)`, and safe for the same reason — readiness is level-triggered, so a
    /// wake worth acting on is seen again by the next scan.
    #[inline]
    pub fn clear_pending(&self) {
        backend().poll_clear_pending_current();
    }

    /// Consume a pending wake, or park for at most `timeout_ms`. One
    /// compare-exchange decides, so no wake can land between the two.
    #[inline]
    pub fn block(&self, timeout_ms: u32) {
        backend().poll_block_current_timeout(timeout_ms);
    }
}

impl Drop for PollWaiter {
    #[inline]
    fn drop(&mut self) {
        backend().poll_disarm_current();
    }
}

/// Proof that a [`PollWaiter`] is armed for the current task, as
/// [`EventBus::subscribe_current`](super::EventBus::subscribe_current) demands.
///
/// Borrowed, not owning: dropping one disarms nothing, because the owning
/// `PollWaiter` up the stack must outlast every registration made under it.
#[derive(Clone, Copy)]
pub struct PollWaiterRef<'a> {
    _borrow: PhantomData<&'a PollWaiter>,
    _not_send: PhantomData<*const ()>,
}

impl<'a> PollWaiterRef<'a> {
    #[inline]
    pub fn of(waiter: &'a PollWaiter) -> Self {
        let _ = waiter;
        Self {
            _borrow: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// Discover the current task's armed token; `None` when none is armed.
    ///
    /// For `FileOps::poll_fused`, which cannot receive a borrow through a trait
    /// its crate cannot name OSTD from. `None` is the honest answer for a
    /// readiness probe with no poll in progress — the level-triggered
    /// `poll_events` path is one — and such a caller must not register, having
    /// nothing that will unregister it.
    #[inline]
    pub fn current() -> Option<Self> {
        backend().poll_armed_current().then_some(Self {
            _borrow: PhantomData,
            _not_send: PhantomData,
        })
    }
}
