//! Doubly-linked, head/tail intrusive list with self-identifying membership.
//!
//! Each [`DLink`] carries an `owner` back-pointer to its list, non-null exactly
//! while linked, so membership is read off the link slot alone and
//! [`dlist_unlink`] needs no head pointer. Ownership lists need both properties
//! the singly-linked
//! [`IntrusiveLinkedList`](super::intrusive::IntrusiveLinkedList) lacks: O(1)
//! removal, since a mass reap walks these edges and a head scan would make it
//! quadratic; and a node that knows its own list, since deriving that from a
//! parallel field (a parent id, say) gives membership two sources of truth
//! whose disagreement is the double-free/leak class this type prevents.
//!
//! `Role` is a zero-sized tag, as in the singly-linked list: an element type
//! participating in several lists embeds one link per role, so a list of one
//! role cannot splice a link slot belonging to another.
//!
//! Operations are `&self` but **not** lock-free against each other. Every list
//! sharing a `Role` for a given element type must be serialised by one outer
//! lock, because [`dlist_unlink`] mutates whichever list owns the node — a
//! caller holding only "its own" list's lock would still race.

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use super::intrusive::LinkError;

/// Per-list link slot embedded in the list element, tagged with the list
/// `Role` it participates in.
///
/// Membership is `owner != null`, not `next != null`: a sole element has null
/// `next` *and* null `prev` while still being linked.
pub struct DLink<T, Role> {
    next: AtomicPtr<T>,
    prev: AtomicPtr<T>,
    /// The list currently holding this node, type-erased: only ever compared,
    /// or cast back to the list type this `Role` pins.
    owner: AtomicPtr<()>,
    // `fn() -> Role` lets `Role` be uninhabited (the typical case).
    _role: PhantomData<fn() -> Role>,
}

impl<T, Role> DLink<T, Role> {
    pub const fn new() -> Self {
        Self {
            next: AtomicPtr::new(core::ptr::null_mut()),
            prev: AtomicPtr::new(core::ptr::null_mut()),
            owner: AtomicPtr::new(core::ptr::null_mut()),
            _role: PhantomData,
        }
    }

    #[inline]
    pub fn is_linked(&self) -> bool {
        !self.owner.load(Ordering::Acquire).is_null()
    }

    /// Clear the slot without touching any list, for one whose bytes were
    /// copied from another task (fork) or whose list was abandoned wholesale.
    #[inline]
    pub fn reset(&self) {
        self.next.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.prev.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.owner.store(core::ptr::null_mut(), Ordering::Release);
    }
}

impl<T, Role> Default for DLink<T, Role> {
    fn default() -> Self {
        Self::new()
    }
}

/// Element types that embed a [`DLink`] for `Role`.
///
/// # Safety
///
/// - The returned reference must point at a `DLink<Self, Role>` field inside
///   `Self` whose address is stable for the lifetime of any list membership.
/// - Distinct `Role`s must return distinct fields.
pub unsafe trait DLinked<Role>: Sized {
    fn dlink(&self) -> &DLink<Self, Role>;
}

/// Doubly-linked, head/tail intrusive list. Push transfers no ownership by
/// itself; consumers pair membership with a parked reference.
pub struct IntrusiveDList<T, Role>
where
    T: DLinked<Role>,
{
    head: AtomicPtr<T>,
    tail: AtomicPtr<T>,
    count: AtomicUsize,
    _marker: PhantomData<fn() -> Role>,
}

// SAFETY: cross-CPU access is mediated by the caller's outer lock; the atomic
// head/tail/count prevent torn reads on the unlocked fast paths.
unsafe impl<T, Role> Send for IntrusiveDList<T, Role> where T: DLinked<Role> + Send {}
// SAFETY: see the `Send` implementation above.
unsafe impl<T, Role> Sync for IntrusiveDList<T, Role> where T: DLinked<Role> + Send {}

impl<T, Role> IntrusiveDList<T, Role>
where
    T: DLinked<Role>,
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

    #[inline]
    fn as_owner(&self) -> *mut () {
        core::ptr::from_ref(self).cast::<()>().cast_mut()
    }

    /// Append `node` at the tail. `Err(AlreadyLinked)` when the node is already
    /// a member of this or any other list of the same role.
    pub fn push_back(&self, node: NonNull<T>) -> Result<(), LinkError> {
        // SAFETY: the `DLinked` contract makes the link a stable, addressable
        // member of the element, and membership keeps the element alive, so
        // pointers loaded from link slots stay valid until we mutate them.
        let link = unsafe { node.as_ref().dlink() };
        if link.is_linked() {
            return Err(LinkError::AlreadyLinked);
        }

        let new = node.as_ptr();
        let prev_tail = self.tail.load(Ordering::Acquire);
        link.next.store(core::ptr::null_mut(), Ordering::Relaxed);
        link.prev.store(prev_tail, Ordering::Relaxed);

        if prev_tail.is_null() {
            self.head.store(new, Ordering::Release);
        } else {
            // SAFETY: `prev_tail` came from this list's tail slot, so it names a
            // live member.
            unsafe { (*prev_tail).dlink().next.store(new, Ordering::Release) };
        }
        self.tail.store(new, Ordering::Release);
        self.count.fetch_add(1, Ordering::AcqRel);
        // Publish membership last: `owner` non-null is what makes the node
        // visible as linked, and the chain must be complete before then.
        link.owner.store(self.as_owner(), Ordering::Release);
        Ok(())
    }

    pub fn pop_front(&self) -> Option<NonNull<T>> {
        let head = NonNull::new(self.head.load(Ordering::Acquire))?;
        self.unlink_member(head);
        Some(head)
    }

    #[inline]
    pub fn peek_front(&self) -> Option<NonNull<T>> {
        NonNull::new(self.head.load(Ordering::Acquire))
    }

    /// Remove `node`, which must be a member of *this* list.
    /// `Err(NotPresent)` when it is unlinked or owned by another list.
    pub fn remove(&self, node: NonNull<T>) -> Result<(), LinkError> {
        // SAFETY: `DLinked` contract, as in `push_back`.
        let link = unsafe { node.as_ref().dlink() };
        if link.owner.load(Ordering::Acquire) != self.as_owner() {
            return Err(LinkError::NotPresent);
        }
        self.unlink_member(node);
        Ok(())
    }

    /// Splice out a node already known to belong to this list.
    fn unlink_member(&self, node: NonNull<T>) {
        // SAFETY: `DLinked` contract; the caller established membership.
        let link = unsafe { node.as_ref().dlink() };
        let prev = link.prev.load(Ordering::Acquire);
        let next = link.next.load(Ordering::Acquire);

        if prev.is_null() {
            self.head.store(next, Ordering::Release);
        } else {
            // SAFETY: `prev` is a live member of this list.
            unsafe { (*prev).dlink().next.store(next, Ordering::Release) };
        }
        if next.is_null() {
            self.tail.store(prev, Ordering::Release);
        } else {
            // SAFETY: `next` is a live member of this list.
            unsafe { (*next).dlink().prev.store(prev, Ordering::Release) };
        }

        link.reset();
        self.count.fetch_sub(1, Ordering::AcqRel);
    }

    /// Snapshot iterator from the head. A concurrent mutation may make the view
    /// stale but cannot cause UB.
    pub fn iter(&self) -> DIter<'_, T, Role> {
        DIter {
            cursor: self.head.load(Ordering::Acquire),
            _marker: PhantomData,
        }
    }
}

impl<T, Role> Default for IntrusiveDList<T, Role>
where
    T: DLinked<Role>,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Remove `node` from whichever list currently owns it. Returns whether it was
/// linked.
///
/// The caller must hold the lock serialising every list of this role, not just
/// the one it believes owns the node.
pub fn dlist_unlink<T, Role>(node: NonNull<T>) -> bool
where
    T: DLinked<Role>,
{
    // SAFETY: `DLinked` contract, as in `push_back`.
    let link = unsafe { node.as_ref().dlink() };
    let owner = link.owner.load(Ordering::Acquire);
    if owner.is_null() {
        return false;
    }
    // SAFETY: `owner` was stored by `push_back` on a list of exactly this type,
    // and the role tag makes that list type unique. The caller serialises
    // against that list's other operations.
    let list = unsafe { &*owner.cast::<IntrusiveDList<T, Role>>() };
    list.unlink_member(node);
    true
}

/// Yields `NonNull<T>` so consumers pick their own borrow form.
pub struct DIter<'a, T, Role>
where
    T: DLinked<Role>,
{
    cursor: *mut T,
    _marker: PhantomData<&'a IntrusiveDList<T, Role>>,
}

impl<T, Role> Iterator for DIter<'_, T, Role>
where
    T: DLinked<Role>,
{
    type Item = NonNull<T>;

    fn next(&mut self) -> Option<Self::Item> {
        let current = NonNull::new(self.cursor)?;
        // SAFETY: cursor non-null per the guard; live per the `DLinked`
        // contract and the caller's serialisation.
        self.cursor = unsafe { current.as_ref().dlink().next.load(Ordering::Acquire) };
        Some(current)
    }
}
