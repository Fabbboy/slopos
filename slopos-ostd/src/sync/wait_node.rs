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

/// Stack- or heap-resident wait-list entry.
pub struct WaitNode {
    link: Link<WaitNode>,
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

// SAFETY: the embedded `Link<WaitNode>` is a stable, addressable field
// of `Self` and is the only intrusive link slot on this type, so it
// cannot be donated to a second list.
unsafe impl Linked for WaitNode {
    #[inline]
    fn link(&self) -> &Link<Self> {
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

    /// `true` if this node currently has a `next` slot pointing at
    /// another node — used by waiters returning from a wait to decide
    /// whether they need to remove themselves from the queue or
    /// whether the wake side already popped them.
    ///
    /// Note: a node that is the *tail* of a non-empty list has
    /// `next == null`, indistinguishable from a node that is not
    /// linked at all. Callers must therefore confirm membership by
    /// taking the queue's `SpinLock` and walking the list, or by
    /// using the queue-provided helpers that already do so. This
    /// predicate is only a fast hint for unlinked-from-the-middle
    /// or unlinked-from-the-head cases.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn link_has_next(&self) -> bool {
        self.link.has_next()
    }
}

impl Default for WaitNode {
    fn default() -> Self {
        Self::new()
    }
}
