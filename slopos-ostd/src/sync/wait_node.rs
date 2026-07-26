//! Wait-list entry for [`WaitQueue`](super::wait_queue::WaitQueue).
//!
//! A `WaitNode` is the per-waiter slot embedded in the queue's intrusive
//! list. There are two distinct lifecycle flavours:
//!
//! - **Stack-pinned** (created by `wait_event` / `wait_event_until` /
//!   `wait_event_timeout` / `wait_event_timeout_until`): the node lives
//!   on the waiter's kernel stack, pinned via
//!   [`core::pin::pin!`] for the duration of the wait. Memory is owned by
//!   the waiter's stack frame; the wait queue never frees it.
//! - **Heap-owned** (created by `enqueue_current`): the node is allocated
//!   via `KBox<WaitNode>` and `into_raw`'d into the queue. Whoever
//!   eventually dequeues it (the wake side via `wake_one` / `wake_all`,
//!   or the cleanup side via `remove_current`) reclaims it via
//!   `KBox::from_raw` and drops it.
//!
//! The [`WaitNode::heap_owned`] flag is set once at construction and
//! never changes, so wake / remove paths can read it Relaxed and decide
//! whether to drop the heap allocation.
//!
//! # Defense-in-depth fields
//!
//! Beyond the link, task handle, and heap-owned discriminant, every
//! `WaitNode` carries two more atomics:
//!
//! - [`has_woken`](Self::has_woken_load): set to `true` by the wake side
//!   under the queue's `SpinLock` whenever this node is popped. The
//!   waiter's post-WQ-unlock recheck can `Acquire`-load it to detect a
//!   wake that raced between WQ-unlock and the recheck. Lifted from
//!   Asterinas's `Waker::has_woken` pattern, this is an **auxiliary**
//!   signal — the WQ-lock-pair barrier remains the ground-truth
//!   synchronization edge.
//! - [`queue_ptr`](Self::queue_load): a back-pointer to the owning
//!   `WaitQueue` (stored as `*mut c_void` to avoid a circular type
//!   dependency between this module and `wait_queue.rs`). Set under the
//!   WQ lock in `push_node`; cleared under the WQ lock in every pop
//!   path *before* the lock is released. `Drop for WaitNode` consults
//!   this back-pointer to unlink the node if some upstream code-path
//!   forgot — defense in depth against a future refactor.
//!
//! # Safety contract for stack-pinned nodes
//!
//! The node's address MUST stay stable for as long as it is linked into
//! a `WaitQueue`. Stack-pinning via [`core::pin::pin!`] (or equivalent)
//! plus the kernel's invariant that task stacks do not move while the
//! task is blocked together provide that guarantee. The waiter is
//! responsible for unlinking the node before its stack frame returns —
//! `wait_event` does so under the queue's `SpinLock`, which is the
//! same lock the wake side takes, so the unlink and the wake cannot
//! race.
//!
//! # `!Send + !Sync` typestate
//!
//! `WaitNode` is deliberately neither `Send` nor `Sync` (enforced by a
//! `PhantomData<*const ()>` marker, since `feature(negative_impls)` is
//! not enabled in `slopos-ostd`). This prevents user code from
//! accidentally carrying a held data lock across a wait point onto
//! another thread — the only structurally-safe way to interact with a
//! wait queue is through the `WaitQueue` API, which manages its
//! `WaitNode`s internally.
//!
//! The queue's own `unsafe impl Send for WaitQueueInner` (in
//! `wait_queue.rs`) *overrides* the field-derived non-Send status that
//! flows through `IntrusiveLinkedList<WaitNode>`: the queue serialises
//! cross-CPU access through its `SpinLock`, so the inner list of
//! `WaitNode` pointers is safely sendable in aggregate even though an
//! individual `WaitNode` value is not.

use core::ffi::c_void;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use slopos_abi::task::INVALID_TASK_ID;

use crate::sync::intrusive::{Link, Linked};

/// Role tag for the wait-queue intrusive list.
pub enum WaitQueueRole {}

/// Stack- or heap-resident wait-list entry.
pub struct WaitNode {
    link: Link<WaitNode, WaitQueueRole>,
    /// Id of the task this node represents, or `INVALID_TASK_ID` for a node
    /// that has not been claimed yet. Read-only after construction; the wait
    /// queue's `SpinLock` synchronises wake-side reads against waiter-side
    /// reads.
    ///
    /// An **id**, not a task pointer. A waiter can be killed and reaped while
    /// its node is still linked — a blocked task never unwinds its own stack —
    /// so a pointer stored here outlives the allocation it names, and the wake
    /// side dereferences it on whichever CPU called `wake_one`/`wake_all`. An
    /// id cannot dangle: resolving it is a registry weak upgrade that returns
    /// `None` for a dead task.
    task: AtomicU32,
    /// `true` for nodes constructed via `KBox<WaitNode>` and consumed
    /// via `KBox::from_raw`. `false` for stack-pinned nodes whose
    /// lifetime is bound by their owning stack frame.
    heap_owned: AtomicBool,
    /// Set by the wake side (under the WQ lock) when this node is
    /// popped. The waiter's post-WQ-unlock recheck loads with `Acquire`
    /// to detect a wake that raced its decision to yield. Reset to
    /// `false` by the waiter (under the WQ lock) at the top of every
    /// `wait_event*` iteration so a previous-iteration wake does not
    /// false-positive the next iteration's check.
    has_woken: AtomicBool,
    /// Back-pointer to the owning [`WaitQueue`](super::wait_queue::WaitQueue),
    /// stored as `*mut c_void` to avoid a circular module dependency.
    /// Non-null exactly when this node is currently linked into a
    /// queue's list. Used by `Drop` to unlink the node if upstream code
    /// forgot — see module-level doc.
    ///
    /// **Ordering invariant:** set under the WQ lock in `push_node`
    /// with `Release`; cleared under the WQ lock in every pop path
    /// (`wake_one`, `wake_all`, `remove_task`, `unlink_locked`) with
    /// `Release`, *before* the lock is released. `Drop` loads with
    /// `Acquire` to synchronise with the producer's clear.
    queue_ptr: AtomicPtr<c_void>,
    /// `PhantomData<*const ()>` opts out of the auto-derived `Send`
    /// and `Sync` impls. See module-level doc for the rationale —
    /// briefly, this prevents user code from carrying held data locks
    /// across wait points onto other threads.
    _not_send: PhantomData<*const ()>,
}

// `WaitNode` is deliberately NOT `Send` and NOT `Sync`. See module doc.
// The opt-out is enforced via the `PhantomData<*const ()>` field above,
// since `feature(negative_impls)` is not enabled in this crate.

// SAFETY: `link` is a stable in-`Self` field; `WaitNode` participates
// in only one list role, so a single impl satisfies the distinct-field
// rule trivially.
unsafe impl Linked<WaitQueueRole> for WaitNode {
    #[inline]
    fn link(&self) -> &Link<Self, WaitQueueRole> {
        &self.link
    }
}

impl WaitNode {
    /// Construct an empty stack-pinned node. The task field is null
    /// until [`set_task`](Self::set_task) is called.
    pub const fn new() -> Self {
        Self {
            link: Link::new(),
            task: AtomicU32::new(INVALID_TASK_ID),
            heap_owned: AtomicBool::new(false),
            has_woken: AtomicBool::new(false),
            queue_ptr: AtomicPtr::new(core::ptr::null_mut()),
            _not_send: PhantomData,
        }
    }

    /// Construct a heap-owned node. Used by `enqueue_current` to
    /// place a node into the queue without a stack frame to anchor
    /// it. Heap-owned nodes carry a discriminant that tells the
    /// dequeue side to reclaim them via `KBox::from_raw`.
    pub(crate) const fn new_heap() -> Self {
        Self {
            link: Link::new(),
            task: AtomicU32::new(INVALID_TASK_ID),
            heap_owned: AtomicBool::new(true),
            has_woken: AtomicBool::new(false),
            queue_ptr: AtomicPtr::new(core::ptr::null_mut()),
            _not_send: PhantomData,
        }
    }

    /// Publish the id of the task this node represents. Called once,
    /// before the node is pushed onto the queue.
    #[inline]
    pub(crate) fn set_task(&self, task: u32) {
        self.task.store(task, Ordering::Release);
    }

    /// Read the task id. Reads happen under the queue's `SpinLock`
    /// in wake / scan paths; the Acquire pairs with the Release store
    /// in [`set_task`](Self::set_task).
    #[inline]
    pub(crate) fn task(&self) -> u32 {
        self.task.load(Ordering::Acquire)
    }

    /// `true` if this node was constructed via `KBox<WaitNode>` and
    /// must be reclaimed by whichever path dequeues it.
    #[inline]
    pub(crate) fn is_heap_owned(&self) -> bool {
        self.heap_owned.load(Ordering::Relaxed)
    }

    /// Acquire-load the has_woken flag. Used by the waiter's
    /// post-WQ-unlock recheck to detect a wake that raced.
    #[inline]
    pub(crate) fn has_woken_load(&self) -> bool {
        self.has_woken.load(Ordering::Acquire)
    }

    /// AcqRel-swap has_woken to `true`, returning the previous value.
    /// Called by the wake side *under the WQ lock* during pop. The
    /// return value lets `wake_one` elide a double-wake if the node
    /// was already marked (defensive — should not happen for a
    /// just-popped node, but defends against future re-entry paths).
    #[inline]
    pub(crate) fn has_woken_swap_true(&self) -> bool {
        self.has_woken.swap(true, Ordering::AcqRel)
    }

    /// Release-store has_woken to `false`. Called by the waiter
    /// *under the WQ lock* at the top of every `wait_event*` iteration
    /// so a previous-iteration wake does not false-positive the next
    /// iteration's recheck.
    #[inline]
    pub(crate) fn has_woken_reset(&self) {
        self.has_woken.store(false, Ordering::Release);
    }

    /// Release-store the queue back-pointer. Called by `push_node`
    /// *under the WQ lock*, immediately before `list.push`. The
    /// argument is `*mut c_void` because `wait_queue.rs`'s `WaitQueue`
    /// type cannot be named from this module (the dependency would be
    /// circular). Callers in `wait_queue.rs` cast their `&WaitQueue`
    /// to `*const WaitQueue as *mut c_void` at the call site.
    #[inline]
    pub(crate) fn queue_store(&self, queue: *mut c_void) {
        self.queue_ptr.store(queue, Ordering::Release);
    }

    /// Release-store the queue back-pointer to null. Called by every
    /// pop path *under the WQ lock*, after the pop / remove succeeds
    /// but before the lock is released. This ordering is load-bearing:
    /// heap-owned nodes are reclaimed via `KBox::from_raw` *outside*
    /// the WQ lock, and their `Drop for WaitNode` reads the back-pointer
    /// (Acquire) to decide whether to re-acquire the WQ lock. If we
    /// cleared the back-pointer outside the WQ critical section, the
    /// Drop could observe a still-set pointer and re-enter the lock
    /// for nothing.
    #[inline]
    pub(crate) fn queue_clear(&self) {
        self.queue_ptr
            .store(core::ptr::null_mut(), Ordering::Release);
    }

    /// Acquire-load the queue back-pointer. Called by `Drop for WaitNode`.
    /// Returns null if the node is currently unlinked (i.e. some pop
    /// path has already cleared the back-pointer under the WQ lock).
    #[inline]
    pub(crate) fn queue_load(&self) -> *mut c_void {
        self.queue_ptr.load(Ordering::Acquire)
    }
}

impl Default for WaitNode {
    fn default() -> Self {
        Self::new()
    }
}

// `Drop for WaitNode` is implemented in `wait_queue.rs` so it can call
// into the queue's `try_lock` + `list.remove` machinery without
// re-exposing those internals at the crate boundary. See
// `WaitQueue::drop_unlink` for the body. The Drop is "defense in
// depth": in steady-state code `wait_event` always unlinks before
// returning, so the back-pointer is null by the time `Drop` runs and
// the impl is a no-op.
