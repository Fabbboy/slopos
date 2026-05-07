//! Singly-linked, head/tail intrusive list.
//!
//! Wraps the `next: *mut T` pattern used by the kernel scheduler's
//! `ReadyQueue` (`core/src/scheduler/per_cpu.rs`) and `ZombieList`
//! (`core/src/scheduler/task/task_table.rs`). Each list element
//! embeds a [`Link<T>`] field; the list itself owns nothing — it
//! merely splices link slots — so reference-counting and lifecycle
//! remain the caller's responsibility, matching the behaviour of
//! the legacy raw-pointer queues this primitive replaces.
//!
//! Public API is fully safe; the small amount of `unsafe` is
//! confined to this file and gated by the [`Linked`] invariant.

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Per-list link slot embedded in the list element.
///
/// `Link<T>` is `#[repr(transparent)]` over an [`AtomicPtr<T>`];
/// elements that participate in a list embed exactly one as a
/// stable, addressable field.
#[repr(transparent)]
pub struct Link<T> {
    next: AtomicPtr<T>,
}

impl<T> Link<T> {
    pub const fn new() -> Self {
        Self {
            next: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// `true` when this slot's `next` pointer is non-null. A linked
    /// element's tail-node has `next = null`, so this method
    /// distinguishes "linked at the tail" from "not linked"
    /// imperfectly — callers that need a true linked-state bit
    /// should track it externally (the legacy `ReadyQueue` does
    /// not, and neither does this primitive).
    #[inline]
    pub fn has_next(&self) -> bool {
        !self.next.load(Ordering::Acquire).is_null()
    }
}

impl<T> Default for Link<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait that lets [`IntrusiveLinkedList`] find the per-list link
/// slot embedded in `Self`.
///
/// # Safety
///
/// Implementations must:
/// - return a stable reference to a [`Link<Self>`] field that lives
///   inside `Self`;
/// - keep that field at a fixed address while the element is linked
///   into a list (i.e. the element must not be moved while linked);
/// - not expose the link slot to any other intrusive list at the
///   same time (one slot per list).
pub unsafe trait Linked: Sized {
    fn link(&self) -> &Link<Self>;
}

/// Errors returned by [`IntrusiveLinkedList::push`] and
/// [`IntrusiveLinkedList::remove`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// The element passed to a list operation was not present.
    NotPresent,
    /// The element was already linked (push) or could not be relinked.
    AlreadyLinked,
}

/// Singly-linked, head/tail intrusive list with an atomic-counted
/// length.
///
/// The list does not own its elements — push transfers no ownership
/// and pop returns a `NonNull<T>` that the caller must keep alive
/// for the duration of any subsequent borrow.
///
/// Concurrency: all public operations are `&self` and use atomics
/// internally, but operations are not lock-free across each other —
/// real callers (per-CPU ready queues, the global zombie list)
/// already serialise via `PreemptGuard` / `IrqMutex`. The atomics
/// guarantee that field reads from raw pointers do not tear, which
/// the legacy raw-pointer queues relied on implicitly.
pub struct IntrusiveLinkedList<T: Linked> {
    head: AtomicPtr<T>,
    tail: AtomicPtr<T>,
    count: AtomicUsize,
    _marker: PhantomData<T>,
}

// SAFETY: the list shuttles `*mut T` between threads via its head /
// tail atomics; `T: Send` is what the consumer asserts when it
// allows its instances to live in a list shared across CPUs (the
// legacy `ReadyQueue` makes the same assertion via its containing
// `IrqMutex`).
unsafe impl<T: Linked + Send> Send for IntrusiveLinkedList<T> {}
// SAFETY: as `Send`; cross-thread access is mediated by the
// caller's outer lock.
unsafe impl<T: Linked + Send> Sync for IntrusiveLinkedList<T> {}

impl<T: Linked> IntrusiveLinkedList<T> {
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(core::ptr::null_mut()),
            tail: AtomicPtr::new(core::ptr::null_mut()),
            count: AtomicUsize::new(0),
            _marker: PhantomData,
        }
    }

    /// Number of elements currently linked into the list.
    #[inline]
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// `true` when no elements are linked.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire).is_null()
    }

    /// Push `node` at the tail.
    ///
    /// Returns `Err(LinkError::AlreadyLinked)` if `node`'s link
    /// slot already points somewhere — the legacy `ReadyQueue::enqueue`
    /// would tolerate a re-push by overwriting; this primitive
    /// instead refuses, which surfaces double-enqueue bugs.
    pub fn push(&self, node: NonNull<T>) -> Result<(), LinkError> {
        // SAFETY: `node` is a NonNull<T>; per the `Linked` contract,
        // the element it points at hosts a stable `Link<T>` field
        // that lives at least as long as the list membership.
        let link = unsafe { node.as_ref().link() };
        if link.has_next() {
            return Err(LinkError::AlreadyLinked);
        }
        // The tail node currently has `next = null`; this is what
        // the new node will inherit, so explicit reset is a no-op
        // but preserves the invariant if a caller pushes a node
        // whose previous list set `next` and then forgot to clear.
        link.next.store(core::ptr::null_mut(), Ordering::Release);

        let new = node.as_ptr();
        let prev_tail = self.tail.swap(new, Ordering::AcqRel);
        if prev_tail.is_null() {
            // Empty list: new is both head and tail.
            self.head.store(new, Ordering::Release);
        } else {
            // SAFETY: `prev_tail` was the list's tail; per the
            // `Linked` contract its `Link<T>` slot lives as long
            // as it's linked into the list. We hold the &self
            // borrow exclusively (caller-side), so `prev_tail`
            // remains valid for this store.
            let prev_link = unsafe { &*(*prev_tail).link() as *const Link<T> };
            // SAFETY: `prev_link` was just produced from a live
            // reference; the AtomicPtr inside is `Sync`.
            unsafe { (*prev_link).next.store(new, Ordering::Release) };
        }

        self.count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Pop from the head. Returns `None` if the list is empty.
    pub fn pop(&self) -> Option<NonNull<T>> {
        let head_ptr = self.head.load(Ordering::Acquire);
        if head_ptr.is_null() {
            return None;
        }
        // SAFETY: head was just observed non-null and per the
        // `Linked` contract the element at `head_ptr` is alive for
        // the duration of its list membership.
        let next = unsafe { (*head_ptr).link().next.load(Ordering::Acquire) };
        self.head.store(next, Ordering::Release);
        if next.is_null() {
            // Popped the last element: tail must follow head down.
            self.tail.store(core::ptr::null_mut(), Ordering::Release);
        }
        // Reset the popped node's next slot so it can be re-pushed.
        // SAFETY: as above; the popped node is still alive (caller
        // still holds the original pointer).
        unsafe {
            (*head_ptr)
                .link()
                .next
                .store(core::ptr::null_mut(), Ordering::Release)
        };
        self.count.fetch_sub(1, Ordering::AcqRel);
        // SAFETY: `head_ptr` was observed non-null above.
        Some(unsafe { NonNull::new_unchecked(head_ptr) })
    }

    /// Remove `node` from the list (linear scan from the head).
    ///
    /// Returns `Ok(())` if the node was found and removed,
    /// `Err(LinkError::NotPresent)` otherwise.
    pub fn remove(&self, node: NonNull<T>) -> Result<(), LinkError> {
        let target = node.as_ptr();
        let mut prev: *mut T = core::ptr::null_mut();
        let mut cursor = self.head.load(Ordering::Acquire);

        while !cursor.is_null() {
            if cursor == target {
                // Splice out.
                // SAFETY: cursor is non-null and points at a live
                // list element per the `Linked` contract.
                let next = unsafe { (*cursor).link().next.load(Ordering::Acquire) };
                if prev.is_null() {
                    self.head.store(next, Ordering::Release);
                } else {
                    // SAFETY: prev is non-null and points at a live
                    // list element per the `Linked` contract.
                    unsafe { (*prev).link().next.store(next, Ordering::Release) };
                }
                if next.is_null() {
                    // Removed the tail: pull tail back to prev.
                    self.tail.store(prev, Ordering::Release);
                }
                // SAFETY: cursor still alive; clear its link.
                unsafe {
                    (*cursor)
                        .link()
                        .next
                        .store(core::ptr::null_mut(), Ordering::Release)
                };
                self.count.fetch_sub(1, Ordering::AcqRel);
                return Ok(());
            }
            prev = cursor;
            // SAFETY: cursor non-null per loop guard.
            cursor = unsafe { (*cursor).link().next.load(Ordering::Acquire) };
        }
        Err(LinkError::NotPresent)
    }

    /// Snapshot iterator. Walks the chain starting from `head` at
    /// call time. Concurrent push/pop during iteration may yield a
    /// stale view but cannot trigger UB (each step null-checks).
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            cursor: self.head.load(Ordering::Acquire),
            _marker: PhantomData,
        }
    }
}

impl<T: Linked> Default for IntrusiveLinkedList<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Iterator over [`IntrusiveLinkedList`] elements.
///
/// Yields `NonNull<T>` rather than `&T` so callers — which are
/// usually scheduler hot-paths that need to mutate the elements —
/// can pick the borrow form they need without fighting the iterator
/// over lifetimes.
pub struct Iter<'a, T: Linked> {
    cursor: *mut T,
    _marker: PhantomData<&'a IntrusiveLinkedList<T>>,
}

impl<'a, T: Linked> Iterator for Iter<'a, T> {
    type Item = NonNull<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor.is_null() {
            return None;
        }
        // SAFETY: cursor non-null and points at a live list element
        // per the `Linked` contract held by the parent list.
        let next = unsafe { (*self.cursor).link().next.load(Ordering::Acquire) };
        // SAFETY: cursor non-null per the guard above.
        let cur = unsafe { NonNull::new_unchecked(self.cursor) };
        self.cursor = next;
        Some(cur)
    }
}
