//! Wait-list entry for [`WaitQueue`](super::wait_queue::WaitQueue).
//!
//! A `WaitNode` is the per-waiter slot embedded in the queue's intrusive
//! list. There are two distinct lifecycle flavours:
//!
//! - **Stack-pinned** (created by `wait_event` / `wait_event_timeout` /
//!   `wait_once`): the node lives on the waiter's kernel stack, pinned via
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

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::sync::intrusive::{Link, Linked};

/// Role tag for the wait-queue intrusive list.
pub enum WaitQueueRole {}

/// Stack- or heap-resident wait-list entry.
pub struct WaitNode {
    link: Link<WaitNode, WaitQueueRole>,
    /// Opaque task handle — the kernel scheduler's `*mut Task` cast to
    /// `*mut c_void`. Read-only after construction; the wait queue's
    /// `SpinLock` synchronises wake-side reads against waiter-side
    /// reads.
    task: AtomicPtr<c_void>,
    /// `true` for nodes constructed via `KBox<WaitNode>` and consumed
    /// via `KBox::from_raw`. `false` for stack-pinned nodes whose
    /// lifetime is bound by their owning stack frame.
    heap_owned: AtomicBool,
}

// SAFETY: `WaitNode` is shared between the waiter and the wake side
// across CPU boundaries. All cross-CPU access goes through the queue's
// `SpinLock`, which provides the necessary synchronisation; the
// internal atomics defend against torn reads on architectures that
// would otherwise allow them.
unsafe impl Send for WaitNode {}
unsafe impl Sync for WaitNode {}

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
            task: AtomicPtr::new(core::ptr::null_mut()),
            heap_owned: AtomicBool::new(false),
        }
    }

    /// Construct a heap-owned node. Used by `enqueue_current` to
    /// place a node into the queue without a stack frame to anchor
    /// it. Heap-owned nodes carry a discriminant that tells the
    /// dequeue side to reclaim them via `KBox::from_raw`.
    pub(crate) const fn new_heap() -> Self {
        Self {
            link: Link::new(),
            task: AtomicPtr::new(core::ptr::null_mut()),
            heap_owned: AtomicBool::new(true),
        }
    }

    /// Publish the task handle this node represents. Called once,
    /// before the node is pushed onto the queue.
    #[inline]
    pub(crate) fn set_task(&self, task: *mut c_void) {
        self.task.store(task, Ordering::Release);
    }

    /// Read the task handle. Reads happen under the queue's `SpinLock`
    /// in wake / scan paths; the Acquire pairs with the Release store
    /// in [`set_task`](Self::set_task).
    #[inline]
    pub(crate) fn task(&self) -> *mut c_void {
        self.task.load(Ordering::Acquire)
    }

    /// `true` if this node was constructed via `KBox<WaitNode>` and
    /// must be reclaimed by whichever path dequeues it.
    #[inline]
    pub(crate) fn is_heap_owned(&self) -> bool {
        self.heap_owned.load(Ordering::Relaxed)
    }
}

impl Default for WaitNode {
    fn default() -> Self {
        Self::new()
    }
}
