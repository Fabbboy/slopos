//! Singly-linked, head/tail intrusive list.
//!
//! `Role` is a zero-sized type tag: an element type `T` that
//! participates in multiple lists embeds one `Link<T, Role>` per
//! role and implements `Linked<Role>` once per role, returning a
//! distinct field each time. Lists parameterised by different roles
//! are distinct types, so a list of one role cannot splice an
//! element's link slot belonging to another role — that mismatch is
//! a compile error, not a runtime invariant.

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// Per-list link slot embedded in the list element, tagged with the
/// list `Role` it participates in.
///
/// Membership is tracked by an explicit `linked` flag, not inferred
/// from `next != null`. A tail node has `next = null` but is still
/// linked, so a `next`-only check would let a tail be re-pushed and
/// silently form a self-loop.
pub struct Link<T, Role> {
    next: AtomicPtr<T>,
    linked: AtomicBool,
    // `fn() -> Role` lets `Role` be uninhabited (the typical case).
    _role: PhantomData<fn() -> Role>,
}

impl<T, Role> Link<T, Role> {
    pub const fn new() -> Self {
        Self {
            next: AtomicPtr::new(core::ptr::null_mut()),
            linked: AtomicBool::new(false),
            _role: PhantomData,
        }
    }

    /// True iff this slot is currently a member of some list of `Role`.
    #[inline]
    pub fn is_linked(&self) -> bool {
        self.linked.load(Ordering::Acquire)
    }

    #[inline]
    pub fn load(&self) -> *mut T {
        self.next.load(Ordering::Acquire)
    }

    #[inline]
    pub fn store(&self, next: *mut T) {
        self.next.store(next, Ordering::Release);
    }

    /// Store the successor with relaxed ordering. For lock-free intrusive
    /// stacks, the publishing CAS on the stack head supplies the release edge;
    /// callers that are not doing such a publish should use [`Link::store`].
    #[inline]
    pub fn store_relaxed(&self, next: *mut T) {
        self.next.store(next, Ordering::Relaxed);
    }

    /// Atomically claim this link slot for membership in exactly one list/stack
    /// of `Role`. Returns `false` if the slot was already linked.
    #[inline]
    pub fn try_mark_linked(&self) -> bool {
        self.linked
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Mark this slot unlinked and clear its successor pointer.
    #[inline]
    pub fn mark_unlinked(&self) {
        self.next.store(core::ptr::null_mut(), Ordering::Release);
        self.linked.store(false, Ordering::Release);
    }

    /// Mark this slot unlinked without touching its successor pointer. This is
    /// useful while draining a lock-free stack whose caller has already loaded
    /// the successor and wants to preserve it until the next loop iteration.
    #[inline]
    pub fn mark_unlinked_keep_next(&self) {
        self.linked.store(false, Ordering::Release);
    }

    /// Restore the slot to "not linked, no successor." Used after
    /// bytewise copies (e.g. fork's `clone_from_raw`) where the link
    /// state was inherited from the source.
    #[inline]
    pub fn reset(&self) {
        self.next.store(core::ptr::null_mut(), Ordering::Release);
        self.linked.store(false, Ordering::Release);
    }
}

impl<T, Role> Default for Link<T, Role> {
    fn default() -> Self {
        Self::new()
    }
}

/// CI guard: a `Linked<RoleA>`-only type cannot be used with an
/// `IntrusiveLinkedList<_, RoleB>`. If this ever compiles, role
/// separation has regressed.
///
/// ```compile_fail
/// use slopos_ostd::sync::intrusive::{IntrusiveLinkedList, Link, Linked};
///
/// pub enum RoleA {}
/// pub enum RoleB {}
///
/// struct N {
///     link_a: Link<N, RoleA>,
/// }
///
/// unsafe impl Linked<RoleA> for N {
///     fn link(&self) -> &Link<N, RoleA> { &self.link_a }
/// }
///
/// let _list: IntrusiveLinkedList<N, RoleB> = IntrusiveLinkedList::new();
/// ```
///
/// # Safety
///
/// - The returned reference must point at a `Link<Self, Role>` field
///   inside `Self` whose address is stable for the lifetime of any
///   list membership.
/// - Distinct `Role`s must return distinct fields. Two impls aliasing
///   the same slot would let lists of different roles corrupt each
///   other — re-introducing the very class of bug `Role` exists to
///   prevent.
pub unsafe trait Linked<Role>: Sized {
    fn link(&self) -> &Link<Self, Role>;
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

/// Singly-linked, head/tail intrusive list.
///
/// Push transfers no ownership; pop returns a `NonNull<T>` that the
/// caller must keep alive for the duration of any subsequent borrow.
/// Operations are `&self` but not lock-free across each other —
/// callers serialise externally (per-CPU ready queue, zombie list,
/// wait queue all wrap this in a `SpinLock`).
pub struct IntrusiveLinkedList<T, Role>
where
    T: Linked<Role>,
{
    head: AtomicPtr<T>,
    tail: AtomicPtr<T>,
    count: AtomicUsize,
    _marker: PhantomData<fn() -> Role>,
}

// SAFETY: cross-CPU access is mediated by the caller's outer lock;
// the atomic head/tail prevent torn reads on the unlocked
// fast-paths (`len`, `is_empty`, `iter`'s head load).
unsafe impl<T, Role> Send for IntrusiveLinkedList<T, Role> where T: Linked<Role> + Send {}
unsafe impl<T, Role> Sync for IntrusiveLinkedList<T, Role> where T: Linked<Role> + Send {}

impl<T, Role> IntrusiveLinkedList<T, Role>
where
    T: Linked<Role>,
{
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(core::ptr::null_mut()),
            tail: AtomicPtr::new(core::ptr::null_mut()),
            count: AtomicUsize::new(0),
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire).is_null()
    }

    /// Push `node` at the tail. `Err(AlreadyLinked)` if the node's
    /// link slot is non-null (same-role double-push tripwire).
    pub fn push(&self, node: NonNull<T>) -> Result<(), LinkError> {
        // SAFETY for the unsafe blocks below: the `Linked<Role>`
        // contract makes the link field a stable, addressable member
        // of the element type. Membership in a list keeps the element
        // alive (the consumer's refcount discipline); pointers we
        // load from one node's link slot therefore stay valid until
        // we mutate that slot.
        let link: &Link<T, Role> = unsafe { <T as Linked<Role>>::link(node.as_ref()) };
        // Atomic claim of the linked-state. If `linked` was already
        // true, the node is in some list (head, mid, or tail — all
        // three are detected uniformly) and we refuse.
        if link.linked.swap(true, Ordering::AcqRel) {
            return Err(LinkError::AlreadyLinked);
        }
        link.next.store(core::ptr::null_mut(), Ordering::Release);

        let new = node.as_ptr();
        let prev_tail = self.tail.swap(new, Ordering::AcqRel);
        if prev_tail.is_null() {
            self.head.store(new, Ordering::Release);
        } else {
            let prev_link =
                unsafe { <T as Linked<Role>>::link(&*prev_tail) as *const Link<T, Role> };
            unsafe { (*prev_link).next.store(new, Ordering::Release) };
        }

        self.count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    pub fn pop(&self) -> Option<NonNull<T>> {
        let head_ptr = self.head.load(Ordering::Acquire);
        if head_ptr.is_null() {
            return None;
        }
        // SAFETY (this fn): `Linked` contract; popped node stays
        // alive because the caller still holds the original handle.
        let popped_link = unsafe { <T as Linked<Role>>::link(&*head_ptr) };
        let next = popped_link.next.load(Ordering::Acquire);
        self.head.store(next, Ordering::Release);
        if next.is_null() {
            self.tail.store(core::ptr::null_mut(), Ordering::Release);
        }
        // Mark unlinked and clear the chain pointer so the node can
        // be re-pushed cleanly.
        popped_link
            .next
            .store(core::ptr::null_mut(), Ordering::Release);
        popped_link.linked.store(false, Ordering::Release);
        self.count.fetch_sub(1, Ordering::AcqRel);
        Some(unsafe { NonNull::new_unchecked(head_ptr) })
    }

    /// Remove `node` from the list via linear scan from the head, and hand back
    /// the pointer the list itself held.
    ///
    /// `node` is only ever compared, so a caller may search with an address
    /// derived from anything — a `&T`, say. The returned pointer is the one the
    /// link chain stored, which is the one a caller may *act* on: reconstituting
    /// an owning handle from it reaches backwards out of `T` into the
    /// allocation header, and a pointer derived from a `&T` carries provenance
    /// over `T` alone. Callers that only need "was it there" discard it.
    pub fn remove(&self, node: NonNull<T>) -> Result<NonNull<T>, LinkError> {
        let target = node.as_ptr();
        let mut prev: *mut T = core::ptr::null_mut();
        let mut cursor = self.head.load(Ordering::Acquire);

        // SAFETY (this fn): all dereferences are of pointers either
        // loaded from a list-internal slot under the `Linked` contract
        // or of `cursor`/`prev` derived from such a load.
        while !cursor.is_null() {
            let cursor_link = unsafe { <T as Linked<Role>>::link(&*cursor) };
            if cursor == target {
                let next = cursor_link.next.load(Ordering::Acquire);
                if prev.is_null() {
                    self.head.store(next, Ordering::Release);
                } else {
                    unsafe {
                        <T as Linked<Role>>::link(&*prev)
                            .next
                            .store(next, Ordering::Release)
                    };
                }
                if next.is_null() {
                    self.tail.store(prev, Ordering::Release);
                }
                cursor_link
                    .next
                    .store(core::ptr::null_mut(), Ordering::Release);
                cursor_link.linked.store(false, Ordering::Release);
                self.count.fetch_sub(1, Ordering::AcqRel);
                // SAFETY: `cursor` matched a non-null `target`, and it is the
                // pointer the link chain stored rather than the one searched
                // with.
                return Ok(unsafe { NonNull::new_unchecked(cursor) });
            }
            prev = cursor;
            cursor = cursor_link.next.load(Ordering::Acquire);
        }
        Err(LinkError::NotPresent)
    }

    /// Snapshot iterator over the chain reachable from `head` at call
    /// time. Concurrent push/pop may yield a stale view but cannot
    /// trigger UB (each step null-checks).
    pub fn iter(&self) -> Iter<'_, T, Role> {
        Iter {
            cursor: self.head.load(Ordering::Acquire),
            _marker: PhantomData,
        }
    }
}

impl<T, Role> Default for IntrusiveLinkedList<T, Role>
where
    T: Linked<Role>,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Yields `NonNull<T>` so consumers (scheduler hot-paths) can pick
/// their own borrow form without lifetime gymnastics.
pub struct Iter<'a, T, Role>
where
    T: Linked<Role>,
{
    cursor: *mut T,
    _marker: PhantomData<&'a IntrusiveLinkedList<T, Role>>,
}

impl<'a, T, Role> Iterator for Iter<'a, T, Role>
where
    T: Linked<Role>,
{
    type Item = NonNull<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor.is_null() {
            return None;
        }
        // SAFETY: cursor non-null per the guard; live per `Linked` contract.
        let next = unsafe {
            <T as Linked<Role>>::link(&*self.cursor)
                .next
                .load(Ordering::Acquire)
        };
        let cur = unsafe { NonNull::new_unchecked(self.cursor) };
        self.cursor = next;
        Some(cur)
    }
}
