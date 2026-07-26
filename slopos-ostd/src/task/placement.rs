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

use core::ptr::NonNull;

use crate::KArc;
use crate::task::kernel_task::TaskInner;

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
/// live owning reference — its registry owner, an on-CPU dispatch reference, or
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
