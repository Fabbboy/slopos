//! Wait-list entry for [`WaitQueue`](super::wait_queue::WaitQueue).
//!
//! A `WaitNode` is the per-waiter slot in the queue's intrusive list, in one of
//! two lifecycle flavours: **stack-pinned** (`wait_event*`), owned by the
//! waiter's stack frame and never freed by the queue, and **heap-owned**
//! (`enqueue_current`), `into_raw`'d into the queue and reclaimed with
//! `KBox::from_raw` by whichever path dequeues it.
//!
//! A stack-pinned node's address must stay stable for as long as it is linked,
//! and the waiter must unlink before its stack frame returns; `wait_event` does
//! so under the queue's `SpinLock`, the same lock the wake side takes, so the
//! unlink and the wake cannot race.
//!
//! `WaitNode` is deliberately neither `Send` nor `Sync` — enforced by a
//! `PhantomData<*const ()>` marker, since `feature(negative_impls)` is not
//! enabled in `slopos-ostd` — so a held data lock cannot be carried across a
//! wait point onto another thread. `wait_queue.rs`'s `unsafe impl Send for
//! WaitQueueInner` overrides that for the list in aggregate, which the queue's
//! `SpinLock` serialises.

use core::ffi::c_void;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use slopos_abi::task::INVALID_TASK_ID;

use crate::sync::intrusive::{Link, Linked};

/// Sentinel era for a node enqueued while no poll token was armed. No wake can
/// be recorded against it, so such a node takes the ordinary unblock path.
///
/// Also the `None` encoding for the era in [`WaitQueueOps`], whose plain `fn`
/// signatures cannot carry an `Option<u8>`.
///
/// [`WaitQueueOps`]: crate::sync::wait_queue::WaitQueueOps
pub const NO_POLL_ERA: u32 = u32::MAX;

/// Role tag for the wait-queue intrusive list.
pub enum WaitQueueRole {}

/// Stack- or heap-resident wait-list entry.
pub struct WaitNode {
    link: Link<WaitNode, WaitQueueRole>,
    /// Id of the task this node represents, or `INVALID_TASK_ID` before it is
    /// claimed.
    ///
    /// An **id**, not a task pointer: a waiter can be killed and reaped while
    /// its node is still linked, so a pointer stored here would outlive the
    /// allocation it names. Resolving an id is a registry weak upgrade that
    /// returns `None` for a dead task.
    task: AtomicU32,
    /// `true` for nodes constructed via `KBox<WaitNode>` and consumed via
    /// `KBox::from_raw`. Set once at construction, so reads may be `Relaxed`.
    heap_owned: AtomicBool,
    /// Generation of the poll token live when this node was enqueued, or
    /// [`NO_POLL_ERA`] for a registration made without one.
    ///
    /// The wake side delivers *after* releasing the queue lock, so by then the
    /// waiter may have finished its poll and a fresh one may have armed. The
    /// era recorded here is what lets the delivery address the token that this
    /// registration was actually made under, rather than whichever token
    /// happens to be armed on arrival. Set once before the push, so reads may
    /// be `Relaxed`.
    poll_era: AtomicU32,
    /// Set by the wake side under the WQ lock when this node is popped; the
    /// waiter's post-WQ-unlock recheck `Acquire`-loads it to detect a wake that
    /// raced its decision to yield. An auxiliary signal — the WQ-lock pair
    /// remains the ground-truth synchronization edge. Reset by the waiter under
    /// the WQ lock at the top of every `wait_event*` iteration so a previous
    /// iteration's wake does not false-positive the next one.
    has_woken: AtomicBool,
    /// Back-pointer to the owning [`WaitQueue`](super::wait_queue::WaitQueue),
    /// stored as `*mut c_void` to avoid a circular module dependency. Non-null
    /// exactly when this node is currently linked into a queue's list.
    ///
    /// Stored `Release` under the WQ lock in `push_node` and cleared `Release`
    /// under the WQ lock in every pop path *before* the lock is released;
    /// `Drop` loads `Acquire` and unlinks if some path forgot.
    queue_ptr: AtomicPtr<c_void>,
    /// Opts out of the auto-derived `Send` and `Sync`; see the module doc.
    _not_send: PhantomData<*const ()>,
}

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
    /// Construct an empty stack-pinned node.
    pub const fn new() -> Self {
        Self {
            link: Link::new(),
            task: AtomicU32::new(INVALID_TASK_ID),
            heap_owned: AtomicBool::new(false),
            poll_era: AtomicU32::new(NO_POLL_ERA),
            has_woken: AtomicBool::new(false),
            queue_ptr: AtomicPtr::new(core::ptr::null_mut()),
            _not_send: PhantomData,
        }
    }

    /// Construct a heap-owned node, for `enqueue_current`'s case of a queue
    /// entry with no stack frame to anchor it.
    pub(crate) const fn new_heap() -> Self {
        Self {
            link: Link::new(),
            task: AtomicU32::new(INVALID_TASK_ID),
            heap_owned: AtomicBool::new(true),
            poll_era: AtomicU32::new(NO_POLL_ERA),
            has_woken: AtomicBool::new(false),
            queue_ptr: AtomicPtr::new(core::ptr::null_mut()),
            _not_send: PhantomData,
        }
    }

    /// Publish the task id. Called once, before the node is pushed.
    #[inline]
    pub(crate) fn set_task(&self, task: u32) {
        self.task.store(task, Ordering::Release);
    }

    /// Read the task id; wake and scan paths read under the queue's `SpinLock`.
    #[inline]
    pub(crate) fn task(&self) -> u32 {
        self.task.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn is_heap_owned(&self) -> bool {
        self.heap_owned.load(Ordering::Relaxed)
    }

    /// Stamp the poll-token era. Called once, before the node is pushed.
    #[inline]
    pub(crate) fn set_poll_era(&self, era: u32) {
        self.poll_era.store(era, Ordering::Release);
    }

    /// The era this registration was made under, or [`NO_POLL_ERA`].
    #[inline]
    pub(crate) fn poll_era(&self) -> u32 {
        self.poll_era.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn has_woken_load(&self) -> bool {
        self.has_woken.load(Ordering::Acquire)
    }

    /// Called by the wake side under the WQ lock during pop; the previous
    /// value lets `wake_one` elide a double-wake.
    #[inline]
    pub(crate) fn has_woken_swap_true(&self) -> bool {
        self.has_woken.swap(true, Ordering::AcqRel)
    }

    #[inline]
    pub(crate) fn has_woken_reset(&self) {
        self.has_woken.store(false, Ordering::Release);
    }

    /// Called by `push_node` under the WQ lock, immediately before `list.push`.
    #[inline]
    pub(crate) fn queue_store(&self, queue: *mut c_void) {
        self.queue_ptr.store(queue, Ordering::Release);
    }

    /// Called by every pop path under the WQ lock, before the lock is released.
    /// Clearing outside the critical section would let a heap-owned node's
    /// `Drop` — which runs after `KBox::from_raw`, outside the lock — observe a
    /// still-set pointer and re-enter the lock for nothing.
    #[inline]
    pub(crate) fn queue_clear(&self) {
        self.queue_ptr
            .store(core::ptr::null_mut(), Ordering::Release);
    }

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

// `Drop for WaitNode` lives in `wait_queue.rs` (`WaitQueue::drop_unlink`) so it
// can use the queue's `try_lock` + `list.remove` without re-exposing them here.
// In steady state `wait_event` has already unlinked, so it is a no-op.
