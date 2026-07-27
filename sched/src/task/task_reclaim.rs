//! Universal task-reference release, and the graveyard that makes it safe from
//! any context.
//!
//! # Why a graveyard
//!
//! `Task`'s destructor is allocator-heavy: it frees the kernel stack, the FPU
//! area and the address space back to the buddy allocator, whose reuse path
//! performs synchronous cross-CPU TLB shootdowns. Running it with interrupts
//! disabled, under a lock, or on the dying task's own stack is the known
//! slab/buddy deadlock — not a latency blip.
//!
//! But the *last* reference to a task can legitimately be released from exactly
//! those contexts: a registry lookup guard dropped inside the registry
//! cli-spinlock, a batch of guards dropped on the timer tick, a ready-queue
//! entry released under the queue lock, or the outgoing dispatch reference in
//! the interrupts-off switch tail.
//!
//! So [`task_put`] splits the two halves. The decrement always happens
//! immediately; the destruction happens inline only when the context provably
//! allows it, and otherwise the (now uniquely owned) allocation is parked here
//! for the idle dispatcher to destroy with interrupts on and no lock held.
//!
//! This is `put_task_struct` with PREEMPT_RT's `call_rcu` escape hatch, and the
//! two properties copied from it matter: finality is decided by the decrement
//! rather than by reading the count first, and the deferral decision is made on
//! *context*, never on a count.
//!
//! # Why the stack is safe without a lock
//!
//! A pusher reaches [`graveyard_push`] only by winning the one-to-zero strong
//! transition, and exactly one thread can win it per allocation. So the pusher
//! owns the node outright: no other thread holds a reference, and the
//! `reclaim_link` slot has no contention at all. That is what lets the stack be
//! a bare CAS with no lock — which in turn is what lets a push happen under the
//! registry lock, where taking any further lock would risk the very deadlock
//! this module exists to avoid.
//!
//! # The reclaim token
//!
//! "The pusher won the final release" is carried by a value rather than by this
//! comment. `task_release_strong` hands back a `ParkedTask` only to the winner;
//! every operation here takes it by value, so the allocation has exactly one
//! owner at each point and a double-destroy does not typecheck. The stack
//! itself is threaded through the task bodies and can only hold a pointer, so
//! the token is surrendered on push and reconstituted on drain — one place,
//! marked as such below.

use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_ostd::KArc;
use slopos_ostd::cpu::preempt::PreemptGuard;
use slopos_ostd::sync::held_lock_count;
use slopos_ostd::task::accessors::{
    task_reclaim_next_load, task_reclaim_next_store_relaxed, task_reclaim_try_link,
    task_reclaim_unlink,
};
use slopos_ostd::task::{
    ParkedTask, task_destroy_parked, task_parked_leak, task_parked_reclaim, task_release_strong,
};

use super::Task;
use crate::task_stack::{KernelStack, UnsafeStack};

/// The reclaim token for this crate's concrete `Task`.
type ParkedTaskRef = ParkedTask<KernelStack, UnsafeStack>;

/// Tasks whose final reference was released where the destructor could not run.
///
/// Global rather than per-CPU: a per-CPU stack strands its contents when that
/// CPU stops idling or is paused for teardown, and this is a genuinely cold
/// path — one push per task *death*, not per reference release — so the single
/// CAS point costs nothing.
static TASK_GRAVEYARD: AtomicPtr<Task> = AtomicPtr::new(ptr::null_mut());

/// Release one strong task reference from **any** context.
///
/// This is SlopOS's `put_task_struct`, and the sole sanctioned way to drop a
/// `KArc<Task>`. A non-final release is one atomic decrement. A final release
/// destroys inline when the context allows and otherwise parks the allocation
/// for [`task_graveyard_drain`].
#[inline]
pub fn task_put(arc: KArc<Task>) {
    let Some(parked) = task_release_strong(arc) else {
        // Other references remain: a bare decrement, safe anywhere.
        return;
    };
    // The token is the proof that we won the one-to-zero transition, so the
    // allocation is ours alone. Both arms consume it, so exactly one of them
    // can run and neither can run twice.
    if destroy_context_is_safe(parked.node()) {
        task_destroy_parked(parked);
    } else {
        graveyard_push(parked);
    }
}

/// Whether the current context may run `Task`'s destructor.
///
/// Deliberately a property of the *context*, never of a reference count: a
/// count-based test cannot be made race-free, whereas these are all facts about
/// the calling CPU right now.
fn destroy_context_is_safe(node: NonNull<Task>) -> bool {
    // The first two mirror `Task::drop`'s own assertions, so the predicate and
    // the tripwire can never disagree.
    if !slopos_ostd::cpu::x86_64::interrupts::are_interrupts_enabled() || held_lock_count() != 0 {
        return false;
    }
    // `task_terminate` holds a whole-sequence preempt guard, and the buddy's
    // reuse path can spin on cross-CPU shootdowns; keep the guarded region free
    // of destructors.
    if PreemptGuard::is_active() {
        return false;
    }
    // Never free the stack a CPU is running on, and never free one another CPU
    // is still switching off of. Shares its predicate with the reap gate so the
    // two can never disagree about when a task is still dispatch-pinned.
    !crate::scheduler::task_is_dispatch_pinned(node.as_ptr())
}

/// Park a uniquely-owned dead task for later destruction.
///
/// Consumes the reclaim token: from here the stack owns the allocation, and the
/// only way back to a token is [`task_graveyard_drain`] detaching the node.
fn graveyard_push(parked: ParkedTaskRef) {
    let raw = task_parked_leak(parked).as_ptr();
    // Claim membership unconditionally — `debug_assert!` does not evaluate its
    // expression in release builds, so the claim cannot live inside one.
    let claimed = task_reclaim_try_link(raw);
    debug_assert!(
        claimed,
        "task pushed to the graveyard twice: two threads believed they won the \
         same final release"
    );
    loop {
        let head = TASK_GRAVEYARD.load(Ordering::Acquire);
        task_reclaim_next_store_relaxed(raw, head);
        if TASK_GRAVEYARD
            .compare_exchange_weak(head, raw, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Whether any task is awaiting destruction.
#[inline]
pub fn task_graveyard_pending() -> bool {
    !TASK_GRAVEYARD.load(Ordering::Acquire).is_null()
}

/// Destroy every task parked so far.
///
/// Must be called with interrupts enabled and no lock held — the idle
/// dispatcher's off-lock window, plus explicit calls at shutdown and registry
/// reset so neither leaves a corpse.
///
/// Detaching the whole chain with a swap (rather than popping) makes this
/// ABA-free by construction, so several CPUs may drain concurrently, each
/// taking a disjoint chain.
pub fn task_graveyard_drain() {
    let mut cursor = TASK_GRAVEYARD.swap(ptr::null_mut(), Ordering::AcqRel);
    while let Some(node) = NonNull::new(cursor) {
        // Read the successor and clear membership *before* destroying: the
        // destructor frees the memory the link lives in.
        cursor = task_reclaim_next_load(node.as_ptr());
        task_reclaim_next_store_relaxed(node.as_ptr(), ptr::null_mut());
        task_reclaim_unlink(node.as_ptr());

        // The single reconstitution point for the reclaim token, and the one
        // place in the kernel where unique ownership rests on an argument
        // rather than on the type. It holds because the swap above detached
        // this whole chain atomically: every node on it was parked by the
        // winner of its own final strong release, no other drainer can hold
        // the same chain, and the node is unlinked before it is reclaimed, so
        // a second push cannot alias it.
        task_destroy_parked(task_parked_reclaim(node));
    }
}
