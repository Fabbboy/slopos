//! Ownership hand-off between a `KArc<Task>` and a scheduler placement slot.
//!
//! The scheduler's placement containers — the per-CPU ready queue, the
//! remote-wake inbox, the deferred previous-task slot, and the wait maps — hold
//! their member task by an intrusive link (or map entry) plus one strong
//! reference *parked* as a raw pointer, and these primitives are the sole
//! sanctioned way to move a strong reference into and out of such a slot. They
//! balance one-to-one: an unmatched park inflates the task's strong count
//! forever, so the allocation never returns to the heap; a double reclaim frees
//! one reference too many.
//!
//! # The existence reference
//!
//! No container covers a blocked kernel thread, a task holding a placement
//! reservation that has not reached its queue, one registered before it is
//! published, or a forked child before it joins its parent's list. So a task
//! also owns one reference to *itself*, handed to it at registration by
//! [`task_existence_park`] and taken back exactly once, when it is reaped, by
//! [`task_existence_release`]. Linux gives `task_struct` the same
//! self-reference. Two properties follow:
//!
//! - While a task is live, every container's release is provably *not* the final
//!   one, so it stays a bare atomic decrement — safe under a lock and with
//!   interrupts disabled.
//! - A task is registered if and only if it holds this reference, so the
//!   registry can be a pure weak index and a lookup is a liveness-checked
//!   upgrade rather than a fabricated strong reference.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::KArc;
use crate::task::kernel_task::TaskInner;

/// Number of tasks currently holding their own existence reference.
///
/// Equal, at every quiescent point, to the number of tasks the registry has
/// published and not yet reaped — a leak tripwire: divergence means a reap
/// released the reference without unhashing, or an unhash dropped an entry
/// without reaping.
static EXISTENCE_REFS_PARKED: AtomicUsize = AtomicUsize::new(0);

/// Hand a task one strong reference to *itself*, to be taken back exactly once
/// when it is reaped. Returns `false` if it already holds one.
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
    task_placement_retain(task);
    if flag
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // The caller's own owning reference means giving back the one we minted
        // cannot be the final release, so this is a bare atomic decrement.
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

/// Borrow a task through a reference some container has parked on it.
///
/// A ready queue peeking at its tail, or a wait map's predicate, holds only the
/// node — reclaiming to read it would take a membership the container still
/// owns. Scoped rather than returning a reference, so the borrow's lifetime is
/// the call and the caller cannot choose it.
///
/// # Correctness
/// Same liveness contract as [`task_placement_clone`]: the caller must hold, or
/// be covered by, a live strong reference to `node` for the duration.
#[inline]
pub fn with_parked_node<K, U, R>(
    node: NonNull<TaskInner<K, U>>,
    f: impl FnOnce(&TaskInner<K, U>) -> R,
) -> R {
    // SAFETY: per the contract the caller is covered by a live strong
    // reference, so the body is initialised and stays so across `f`.
    f(unsafe { node.as_ref() })
}

/// Whether a task a caller has parked a reference on has finished exiting.
///
/// The `waitpid` predicate outlives the registry guard that found the target: a
/// waiter killed mid-wait never unwinds its own stack, so the owning reference
/// is parked in the wait map instead and the stack keeps only the node. A borrow
/// tied to the guard therefore cannot be what the predicate holds.
///
/// Two reads, because either is sufficient and they differ: `exit_info` is the
/// value the exit path publishes, and the exited-status check covers a path that
/// flipped state without publishing one. Both are atomic loads and nothing else,
/// which is what a wait predicate is allowed to be.
///
/// # Correctness
/// Same liveness contract as [`task_placement_clone`]: the caller must hold, or
/// be covered by, a live strong reference to `node` for the duration.
#[inline]
pub fn parked_task_has_exited<K, U>(node: NonNull<TaskInner<K, U>>) -> bool {
    with_parked_node(node, |task| task.exit_info().is_set() || task.is_exited())
}

/// Reverse a detached intrusive chain, returning its new head and node count.
///
/// A Treiber container hands its whole chain over in one swap, so the caller
/// that performed the swap is the sole owner of every node on it, and each node
/// is still backed by the owning reference its producer parked. Reversing it is
/// what turns LIFO push order into FIFO drain order.
///
/// The walk is expressed on pointers rather than borrows because the link field
/// *is* the placement slot, and a `&T` held across the rewrite would name a node
/// the chain no longer links.
///
/// # Correctness
/// `head` must be a chain this caller detached and not yet published anywhere
/// else, threaded through `Role`'s link on every node.
#[inline]
pub fn reverse_detached_chain<T, Role>(head: *mut T) -> (*mut T, u32)
where
    T: crate::sync::intrusive::Linked<Role>,
{
    let mut reversed: *mut T = core::ptr::null_mut();
    let mut cursor = head;
    let mut count = 0u32;
    while !cursor.is_null() {
        // SAFETY: per the contract the caller solely owns the detached chain,
        // and each node is kept alive by its producer's parked reference.
        let link = unsafe { crate::sync::intrusive::Linked::<Role>::link(&*cursor) };
        let next = link.load();
        link.store_relaxed(reversed);
        reversed = cursor;
        cursor = next;
        count = count.saturating_add(1);
    }
    (reversed, count)
}

/// Proof that the holder uniquely owns a task allocation whose strong count has
/// reached zero — the body still initialised, and nothing else able to reach it.
///
/// The field is private and there is no public constructor, so the token cannot
/// be fabricated from an arbitrary address. It is obtained by winning the
/// one-to-zero transition ([`task_release_strong`]) or by reclaiming one this
/// module itself parked ([`task_parked_reclaim`]). [`task_destroy_parked`]
/// consumes it by value, which makes a double-destroy *unrepresentable* rather
/// than merely forbidden.
///
/// The token's existence is the safety argument, so [`with_parked`] forms a
/// `&TaskInner` with **no caller obligation at all** — which matters because
/// every kernel crate outside OSTD compiles under `#![forbid(unsafe_code)]`, and
/// a safe function that dereferences a caller-supplied address lets such a crate
/// reach undefined behaviour without ever writing the keyword.
///
/// No `Drop`: destroying a task is allocator-heavy work that must not run under
/// a lock, with interrupts off, or on the dying task's own stack, and a `Drop`
/// impl would run it at whatever moment the token went out of scope. A dropped
/// token leaks the allocation instead, which is memory-safe, and `#[must_use]`
/// catches the accident at compile time.
///
/// Neither `Clone` nor `Copy`: two tokens for one allocation would be two
/// claims of unique ownership.
#[must_use = "dropping a ParkedTask leaks the task allocation — pass it to task_destroy_parked"]
pub struct ParkedTask<K, U> {
    node: NonNull<TaskInner<K, U>>,
}

impl<K, U> ParkedTask<K, U> {
    /// The allocation's node pointer, for identity comparisons and for the
    /// predicates the reclaim path consults before choosing a destroy context.
    ///
    /// Borrows rather than consumes: the token still owns the allocation.
    /// Handing a pointer *out* is not the direction that was unsound — what the
    /// private field prevents is an arbitrary address coming *in*.
    #[inline]
    pub fn node(&self) -> NonNull<TaskInner<K, U>> {
        self.node
    }
}

/// Release one strong reference without running `TaskInner`'s destructor.
///
/// `Some(token)` exactly when this call was the final release, in which case
/// the caller uniquely owns the allocation and must pass the token to
/// [`task_destroy_parked`] exactly once. `None` means other references remain
/// and this was a bare atomic decrement.
///
/// This is the split that lets a task's final release happen in a context where
/// the allocator-heavy destructor must not run (interrupts off, a lock held, or
/// on the dying task's own stack). Whether this call is the final one is decided
/// by the decrement itself, never by reading the count first — a
/// `strong_count == 1` pre-check is racy.
#[inline]
pub fn task_release_strong<K, U>(arc: KArc<TaskInner<K, U>>) -> Option<ParkedTask<K, U>> {
    KArc::release_deferrable(arc).map(|node| ParkedTask { node })
}

/// Run the destructor that [`task_release_strong`] deferred, returning the
/// allocation to the heap.
#[inline]
pub fn task_destroy_parked<K, U>(parked: ParkedTask<K, U>) {
    // SAFETY: `parked` witnesses that its holder won the one-to-zero release
    // and has not destroyed the allocation yet — exactly
    // `KArc::destroy_deferred`'s contract, now carried by the type.
    unsafe { KArc::destroy_deferred(parked.node) };
}

/// Surrender a reclaim token to a raw slot, yielding the node pointer to store.
///
/// The graveyard is an intrusive stack threaded through the task bodies
/// themselves, so what it can hold is a pointer, not a Rust value.
#[inline]
pub fn task_parked_leak<K, U>(parked: ParkedTask<K, U>) -> NonNull<TaskInner<K, U>> {
    parked.node
}

/// Reconstitute the token a prior [`task_parked_leak`] parked in a raw slot.
///
/// This is the **only** way to obtain a [`ParkedTask`] other than winning a
/// final release, and therefore the one point at which the token's guarantee
/// rests on a caller obligation rather than on the type.
///
/// # Correctness
/// `node` must be the result of exactly one [`task_parked_leak`], recovered
/// from the slot it was parked in, and not already reclaimed. Reclaiming a
/// pointer that was never parked, or reclaiming one twice, manufactures a claim
/// of unique ownership that is not true.
#[inline]
pub fn task_parked_reclaim<K, U>(node: NonNull<TaskInner<K, U>>) -> ParkedTask<K, U> {
    ParkedTask { node }
}

/// Borrow a task whose last strong reference is already gone.
///
/// The reclaim path has to ask questions of a task it has just won the
/// one-to-zero release on: `task_put` consults the dispatch-pin predicate
/// before deciding whether the destructor may run in this context. At that
/// moment the strong count is zero and no [`KArc`] exists, so
/// [`task_placement_clone`] would be resurrection rather than a clone.
///
/// Sound for the opposite reason to every other borrow in this module: not
/// because someone else is keeping the task alive, but because *nobody else can
/// reach it at all*. Borrowing the token rather than consuming it means the
/// allocation cannot be destroyed while `f` holds the reference, so there is no
/// caller obligation left to state.
#[inline]
pub fn with_parked<K, U, R>(parked: &ParkedTask<K, U>, f: impl FnOnce(&TaskInner<K, U>) -> R) -> R {
    // SAFETY: `parked` witnesses unique ownership of a still-initialised task
    // body, and holding it by shared reference keeps it alive across `f`, so
    // this reference is valid and unaliased for the duration of the call.
    f(unsafe { parked.node.as_ref() })
}

/// Read a live task's current strong reference count without taking a
/// reference. For diagnostics and invariant assertions only. Same liveness
/// contract as [`task_placement_clone`].
#[inline]
pub fn task_placement_strong_count<K, U>(ptr: NonNull<TaskInner<K, U>>) -> usize {
    // SAFETY: per the contract `ptr`'s strong count is non-zero, so
    // reconstructing and re-parking the borrowed reference is a balanced no-op.
    let borrowed = unsafe { KArc::from_raw(ptr.as_ptr().cast_const()) };
    let count = KArc::strong_count(&borrowed);
    let _ = KArc::into_raw(borrowed);
    count
}
