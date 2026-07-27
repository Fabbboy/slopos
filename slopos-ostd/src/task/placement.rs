//! Ownership hand-off between a `KArc<Task>` and a scheduler placement slot.
//!
//! The scheduler's placement containers — the per-CPU ready queue, the
//! remote-wake inbox, the deferred previous-task slot, and the wait maps — hold
//! their member task by an intrusive link (or map entry) plus one strong
//! reference *parked* as a raw pointer. These primitives are the sole
//! sanctioned way to move a strong reference into and out of such a slot:
//!
//! - [`task_placement_clone`] mints a fresh owning [`KArc`] from a still-live
//!   task pointer (one atomic increment — the enqueue/wake fast path).
//! - [`task_placement_retain`] parks one owning reference into a container
//!   without materialising a handle ([`task_placement_clone`] then forget).
//! - [`task_placement_leak`] parks an owning handle as a raw pointer.
//! - [`task_placement_reclaim`] takes a parked reference back out as a handle.
//!
//! They balance one-to-one: every retain/leak must pair with exactly one
//! reclaim (or a matching drop of the cloned handle). An unmatched park
//! inflates the task's strong count forever, so the allocation never returns to
//! the heap; a double reclaim frees one reference too many.
//!
//! # The existence reference
//!
//! Containers do not cover every state a live task can be in. A blocked kernel
//! thread sits in no queue, has no parent, and is named by its waiter node only
//! through an opaque handle; a task holding a placement *reservation* has not
//! reached its queue yet; a freshly created task is registered before it is
//! published; a forked child is registered before it joins its parent's list. In
//! each of those, no container holds a reference.
//!
//! So a task also owns one reference to *itself*, handed to it at registration
//! by [`task_existence_park`] and taken back exactly once, when it is reaped, by
//! [`task_existence_release`]. Linux gives `task_struct` the same self-reference
//! and takes it back in `release_task`. Two properties follow, and the rest of
//! the ownership model leans on both:
//!
//! - While a task is live, every container's release is provably *not* the final
//!   one, so it stays a bare atomic decrement — safe under a lock and with
//!   interrupts disabled.
//! - A task is registered if and only if it holds this reference, so the
//!   registry can be a pure weak index: it observes tasks without keeping any
//!   alive, and a lookup is a liveness-checked upgrade rather than a fabricated
//!   strong reference.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::KArc;
use crate::task::kernel_task::TaskInner;

/// Number of tasks currently holding their own existence reference.
///
/// Equal, at every quiescent point, to the number of tasks the registry has
/// published and not yet reaped — which is what makes it a leak tripwire: the
/// two diverging means a reap released the reference without unhashing, or an
/// unhash dropped an entry without reaping.
static EXISTENCE_REFS_PARKED: AtomicUsize = AtomicUsize::new(0);

/// Hand a task one strong reference to *itself*, to be taken back exactly once
/// when it is reaped. Returns `false` if it already holds one.
///
/// This is the reference that keeps a task alive in the states where no
/// container holds it — blocked with its waiter node holding only an opaque
/// handle, holding a placement reservation that has not reached a queue,
/// registered but not yet published, or mid-fork before joining its parent's
/// list. Linux gives `task_struct` the same self-reference and takes it back in
/// `release_task`.
///
/// # Correctness
/// Same liveness contract as [`task_placement_clone`]: `task`'s strong count
/// must currently be non-zero, which the caller's own owning reference
/// establishes. The retain happens *before* the flag is published, so a
/// releaser that observes the flag necessarily observes the incremented count.
#[inline]
pub fn task_existence_park<K, U>(task: NonNull<TaskInner<K, U>>) -> bool {
    // SAFETY: per the contract the caller holds an owning reference, so the
    // referent is live for the duration of this call.
    let flag = unsafe { &task.as_ref().existence_ref_parked };
    // Retain before claiming the flag, never after: a releaser that observes
    // the flag must be guaranteed the count it is about to take back already
    // exists. The cost is that a loser has to undo its retain.
    task_placement_retain(task);
    if flag
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Another caller parked first. Give back the reference we minted; the
        // caller's own owning reference means this cannot be the final one, so
        // it is a bare atomic decrement.
        drop(task_placement_reclaim(task));
        return false;
    }
    EXISTENCE_REFS_PARKED.fetch_add(1, Ordering::Relaxed);
    true
}

/// Take back the existence reference [`task_existence_park`] handed out.
///
/// `Some` for the single caller that wins the flag, `None` for every later one
/// and for a task that never held one — so a reap is idempotent and two racing
/// reapers cannot both release. The returned handle is an ordinary owning
/// reference; dispose of it through the task-release path so a final drop lands
/// in a context that may run the destructor.
///
/// # Correctness
/// The caller must hold an owning reference of its own for the duration of the
/// call. That is what makes reading the flag sound, and it is why the returned
/// handle can never be the referent's last reference *at the moment of return*.
#[inline]
pub fn task_existence_release<K, U>(
    task: NonNull<TaskInner<K, U>>,
) -> Option<KArc<TaskInner<K, U>>> {
    // SAFETY: per the contract the caller holds an owning reference, so the
    // referent is live for the duration of this call.
    let flag = unsafe { &task.as_ref().existence_ref_parked };
    if flag
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    EXISTENCE_REFS_PARKED.fetch_sub(1, Ordering::Relaxed);
    Some(task_placement_reclaim(task))
}

/// Whether `task` currently holds its own existence reference.
///
/// # Correctness
/// Same liveness contract as [`task_placement_clone`].
#[inline]
pub fn task_existence_is_parked<K, U>(task: NonNull<TaskInner<K, U>>) -> bool {
    // SAFETY: per the contract the caller holds an owning reference.
    unsafe { task.as_ref().existence_ref_parked.load(Ordering::Acquire) }
}

/// How many tasks currently hold their own existence reference. Diagnostics and
/// leak assertions only.
#[inline]
pub fn task_existence_parked_count() -> usize {
    EXISTENCE_REFS_PARKED.load(Ordering::Relaxed)
}

/// Move one strong reference out of a `KArc` and into a raw placement slot,
/// returning the task's stable base pointer.
///
/// The returned pointer equals [`KArc::as_ptr`] — the `TaskInner` base address,
/// which is also the node pointer the intrusive ready-queue / inbox links are
/// keyed on. It stays valid until the matching [`task_placement_reclaim`].
#[inline]
pub fn task_placement_leak<K, U>(arc: KArc<TaskInner<K, U>>) -> NonNull<TaskInner<K, U>> {
    let raw = KArc::into_raw(arc).cast_mut();
    // `KArc::into_raw` yields `addr_of!(inner.data)`, which is never null.
    NonNull::new(raw).expect("KArc placement pointer is non-null")
}

/// Reconstitute the strong reference a prior [`task_placement_leak`] parked in
/// a placement slot.
///
/// # Correctness
/// `ptr` must be the still-live result of exactly one [`task_placement_leak`]
/// for the same task, not already reclaimed. Reclaiming a pointer that was
/// never leaked, or reclaiming one twice, frees a strong reference that does
/// not exist.
#[inline]
pub fn task_placement_reclaim<K, U>(ptr: NonNull<TaskInner<K, U>>) -> KArc<TaskInner<K, U>> {
    // SAFETY: the caller contract above is exactly `KArc::from_raw`'s — `ptr`
    // is a live, not-yet-reconstituted parked reference for this `T`.
    unsafe { KArc::from_raw(ptr.as_ptr().cast_const()) }
}

/// Mint a fresh owning [`KArc`] from a still-live task pointer — one atomic
/// strong-count increment, no allocation.
///
/// # Correctness
/// `ptr` must address a task whose strong count is currently non-zero (it has a
/// live owning reference — its own existence reference, an on-CPU dispatch one, or
/// an existing container membership — that keeps the allocation alive for the
/// duration of the call). The returned handle is an independent owning
/// reference the caller disposes normally.
#[inline]
pub fn task_placement_clone<K, U>(ptr: NonNull<TaskInner<K, U>>) -> KArc<TaskInner<K, U>> {
    // Reconstruct a handle onto the shared allocation without taking ownership,
    // clone it to mint one fresh strong reference, then hand the borrowed
    // reference back untouched via `into_raw`. Net effect: exactly one new
    // strong reference; the caller's borrowed pointer is unchanged.
    // SAFETY: per the contract `ptr`'s strong count is non-zero, so
    // reconstructing and re-parking the borrowed reference is a balanced no-op
    // and the clone observes strong > 0.
    let borrowed = unsafe { KArc::from_raw(ptr.as_ptr().cast_const()) };
    let cloned = borrowed.clone();
    let _ = KArc::into_raw(borrowed);
    cloned
}

/// Park one fresh owning reference into a container, keyed on the task's stable
/// base pointer, without materialising a handle. The parked reference is later
/// recovered with [`task_placement_reclaim`]. Same liveness contract as
/// [`task_placement_clone`].
#[inline]
pub fn task_placement_retain<K, U>(ptr: NonNull<TaskInner<K, U>>) {
    core::mem::forget(task_placement_clone(ptr));
}

/// Release one strong reference without running `TaskInner`'s destructor.
///
/// `Some(node)` exactly when this call was the final release, in which case the
/// caller uniquely owns the allocation — the task body is still initialised and
/// nothing else can reach it — and must pass `node` to
/// [`task_destroy_parked`] exactly once. `None` means other references remain
/// and this was a bare atomic decrement.
///
/// This is the split that lets a task's final release happen in a context where
/// the allocator-heavy destructor must not run (interrupts off, a lock held, or
/// on the dying task's own stack): release here, park `node`, destroy later.
/// Whether this call is the final one is decided by the decrement itself, never
/// by reading the count first — a `strong_count == 1` pre-check is racy.
#[inline]
pub fn task_release_strong<K, U>(arc: KArc<TaskInner<K, U>>) -> Option<NonNull<TaskInner<K, U>>> {
    KArc::release_deferrable(arc)
}

/// Run the destructor that [`task_release_strong`] deferred, returning the
/// allocation to the heap.
///
/// # Correctness
/// `node` must be the result of exactly one [`task_release_strong`] call that
/// returned `Some`, not already destroyed. Because that call proved unique
/// ownership, no other reference to the task can exist.
#[inline]
pub fn task_destroy_parked<K, U>(node: NonNull<TaskInner<K, U>>) {
    // SAFETY: the caller contract above is exactly `KArc::destroy_deferred`'s.
    unsafe { KArc::destroy_deferred(node) };
}

/// Borrow a task whose last strong reference is already gone.
///
/// The reclaim path has to ask questions of a task it has just won the
/// one-to-zero release on: `task_put` consults the dispatch-pin predicate
/// before deciding whether the destructor may run in this context. At that
/// moment the strong count is zero and no [`KArc`] exists, so there is no
/// owning handle to borrow from and [`task_placement_clone`] would be
/// resurrection rather than a clone.
///
/// This is the one sanctioned way to form a `&TaskInner` without an owning
/// reference behind it, and it is sound for the opposite reason to every other
/// borrow in this module: not because someone else is keeping the task alive,
/// but because *nobody else can reach it at all*.
///
/// # Correctness
/// `node` must be the `Some` result of exactly one [`task_release_strong`]
/// that has not yet been passed to [`task_destroy_parked`]. That call proved
/// unique ownership — the body is still initialised and no other reference to
/// it can exist — so the borrow is exclusive by construction. `f` must not
/// store the borrow anywhere that outlives the call.
#[inline]
pub fn with_parked<K, U, R>(
    node: NonNull<TaskInner<K, U>>,
    f: impl FnOnce(&TaskInner<K, U>) -> R,
) -> R {
    // SAFETY: per the contract the caller uniquely owns `node` and its body is
    // still initialised, so this reference is valid and unaliased for the
    // duration of `f`.
    f(unsafe { node.as_ref() })
}

/// Read a live task's current strong reference count without taking a
/// reference. For diagnostics and invariant assertions only. Same liveness
/// contract as [`task_placement_clone`].
#[inline]
pub fn task_placement_strong_count<K, U>(ptr: NonNull<TaskInner<K, U>>) -> usize {
    // Reconstruct a borrowed handle, read the count, hand it back untouched.
    // SAFETY: per the contract `ptr`'s strong count is non-zero, so
    // reconstructing and re-parking the borrowed reference is a balanced no-op.
    let borrowed = unsafe { KArc::from_raw(ptr.as_ptr().cast_const()) };
    let count = KArc::strong_count(&borrowed);
    let _ = KArc::into_raw(borrowed);
    count
}
