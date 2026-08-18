//! Singly-linked, head/tail intrusive list.
//!
//! `Role` is a zero-sized type tag: an element embeds one
//! `Link<T, Role>` per list it can join, so splicing an element into a
//! list of the wrong role is a compile error rather than a runtime
//! invariant.

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// Per-list link slot embedded in the list element, tagged with the
/// list `Role` it participates in.
///
/// Membership is an explicit `linked` flag, not `next != null`: a tail
/// node has a null successor yet is still linked, and a `next`-only
/// check would let it be re-pushed into a self-loop.
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

    /// Store the successor with relaxed ordering: only for a lock-free stack
    /// push whose head CAS supplies the release edge. Otherwise use
    /// [`Link::store`].
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

    /// Mark this slot unlinked without touching its successor pointer, for a
    /// lock-free stack drain that has already loaded the successor.
    #[inline]
    pub fn mark_unlinked_keep_next(&self) {
        self.linked.store(false, Ordering::Release);
    }

    /// Restore the slot to "not linked, no successor", after a bytewise copy
    /// (fork's `clone_from_raw`) inherited the source's link state.
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
    NotPresent,
    AlreadyLinked,
}

/// Singly-linked, head/tail intrusive list.
///
/// Push transfers no ownership; pop returns a `NonNull<T>` that the
/// caller must keep alive for the duration of any subsequent borrow.
/// Operations take `&self` but are not lock-free against each other —
/// callers serialise externally.
pub struct IntrusiveLinkedList<T, Role>
where
    T: Linked<Role>,
{
    head: AtomicPtr<T>,
    tail: AtomicPtr<T>,
    count: AtomicUsize,
    _marker: PhantomData<fn() -> Role>,
}

// SAFETY: cross-CPU access is mediated by the caller's outer lock; the atomic
// head/tail prevent torn reads on the unlocked `len`/`is_empty`/`iter` paths.
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

    /// Push `node` at the tail; `Err(AlreadyLinked)` on a same-role double push.
    pub fn push(&self, node: NonNull<T>) -> Result<(), LinkError> {
        // SAFETY for the unsafe blocks below: the `Linked<Role>` contract makes
        // the link field a stable, addressable member of the element type, and
        // list membership keeps the element alive, so pointers loaded from a
        // link slot stay valid until we mutate that slot.
        let link: &Link<T, Role> = unsafe { <T as Linked<Role>>::link(node.as_ref()) };
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
        // SAFETY: `Linked` contract; the popped node stays alive because the
        // caller still holds the original handle.
        let popped_link = unsafe { <T as Linked<Role>>::link(&*head_ptr) };
        let next = popped_link.next.load(Ordering::Acquire);
        self.head.store(next, Ordering::Release);
        if next.is_null() {
            self.tail.store(core::ptr::null_mut(), Ordering::Release);
        }
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
    /// `node` is only compared, so it may be derived from anything — a `&T`,
    /// say. Act only on the returned pointer: reconstituting an owning handle
    /// reaches backwards out of `T` into the allocation header, which a
    /// `&T`-derived pointer has no provenance over.
    pub fn remove(&self, node: NonNull<T>) -> Result<NonNull<T>, LinkError> {
        let target = node.as_ptr();
        let mut prev: *mut T = core::ptr::null_mut();
        let mut cursor = self.head.load(Ordering::Acquire);

        // SAFETY: every dereference is of a pointer loaded from a list-internal
        // slot under the `Linked` contract, or derived from such a load.
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
                // SAFETY: `cursor` matched a non-null `target`, and is the
                // pointer the chain stored rather than the one searched with.
                return Ok(unsafe { NonNull::new_unchecked(cursor) });
            }
            prev = cursor;
            cursor = cursor_link.next.load(Ordering::Acquire);
        }
        Err(LinkError::NotPresent)
    }

    /// Snapshot iterator over the chain reachable from `head` at call time.
    /// A concurrent push/pop may make the view stale but cannot cause UB.
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

/// Yields `NonNull<T>` so consumers pick their own borrow form.
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
