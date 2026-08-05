//! The bottom-half point: where deferred, allocator-heavy, non-blocking kernel
//! work is legal on a CPU that is under load.
//!
//! OSTD owns the *point* and nothing else — no queue, no priority, no execution
//! thread. [`BhContext`] witnesses that interrupts are on, that this is not
//! interrupt context, that no unwind is in flight, that no tracked lock is held,
//! and that a context switch cannot intervene. What runs there is policy, and
//! lives outside the trusted core.
//!
//! # Why the scope holds a preemption guard
//!
//! It discharges three obligations with one mechanism. It pins the CPU, so the
//! per-CPU claim cannot be stranded by a migration. It makes re-entry from a
//! nested unlock impossible, because the release hook fires only at the
//! outermost one and inside the scope the count is higher. And every context
//! switch funnels through the scheduler's preempt-count assertion, so a bottom
//! half that blocks fails there rather than at a rule restated in front of every
//! blocking primitive.
//!
//! # Why the claim outlives the guard
//!
//! The guard's own drop *is* a release hook at the outermost unlock. A claim
//! released inside the guard's scope would re-enter this module from that
//! destructor, and a callback that re-raised would make the recursion unbounded
//! against a 4 KiB stack. [`BhClaim`] is declared before the guard so it drops
//! after it.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::cpu::preempt::PreemptGuard;
use crate::cpu::x86_64::{interrupts, pcr};
use crate::sync::BspToken;

/// Witness that the holder stands in bottom-half context on this CPU.
///
/// `!Send + !Sync`: the facts are about one CPU at one instant, and the token
/// must not outlive the scope that observed them.
#[derive(Debug)]
pub struct BhContext<'a> {
    _scope: PhantomData<&'a ()>,
    _not_send: PhantomData<*const ()>,
}

/// Work that needs the preemption guard *released* — the task graveyard, whose
/// own push-side predicate refuses to destroy while preemption is disabled, and
/// the idle input drain. One slot rather than a table: the kernel has one such
/// consumer and it composes whatever it needs behind that.
///
/// Returns whether it left more to do.
pub type RelaxedDrain = fn() -> bool;

static RELAXED_DRAIN: AtomicUsize = AtomicUsize::new(0);

/// Passes inside one invocation.
///
/// Bounds the case where a callback re-raises: without it a self-arming
/// consumer could hold a CPU in the drain indefinitely. What is left over stays
/// pending for the next point.
const MAX_BH_PASSES: u32 = 8;

/// Declines whose cause is the calling context rather than re-entrancy. Should
/// stay near zero on a healthy boot: a drain site whose preconditions quietly
/// stopped holding shows up here rather than as a growing backlog.
static DECLINED_CONTEXT: AtomicU64 = AtomicU64::new(0);

/// Declines because this CPU was already draining. Expected and large — every
/// unlock inside a drain lands here — so it is counted apart.
static DECLINED_REENTRANT: AtomicU64 = AtomicU64::new(0);

/// Completed drains, so a caller can tell "the point was reached" from "there
/// was nothing to do" — which is otherwise the same observation.
static DRAINS: AtomicU64 = AtomicU64::new(0);

/// Wire the bottom-half point.
///
/// Registration is what arms it, on every CPU at once. A per-CPU armed flag
/// would have to be set by each CPU as it came up, and an AP that reached its
/// scheduler before the BSP got here would never set it.
pub fn arm<'brand>(_token: &BspToken<'brand>, relaxed: RelaxedDrain) {
    RELAXED_DRAIN.store(relaxed as usize, Ordering::Release);
}

/// The registered relaxed drain, if the point is wired.
#[inline]
fn relaxed_drain() -> Option<RelaxedDrain> {
    let raw = RELAXED_DRAIN.load(Ordering::Acquire);
    crate::util::fn_ptr::fn_ptr_decode_opt::<RelaxedDrain>(raw as *mut ())
}

/// Mark that this CPU has bottom-half work.
///
/// One `gs`-relative byte store: no lock, no allocation, no compare-exchange.
/// Legal from a hard IRQ handler and from under a cli-spinlock. Nothing on this
/// path may ever take a lock, allocate, or log — every caller is somewhere that
/// none of those are available.
#[inline]
pub fn raise() {
    pcr::bh_pending_set();
}

/// Run this CPU's bottom half if the current context permits it.
///
/// Total by construction: every failed predicate declines and nothing asserts.
/// Two of its three call sites are inside a `Drop`, where refusing is the only
/// acceptable answer and a panic is forbidden outright.
#[inline]
pub fn run_pending_if_due() {
    if !pcr::bh_pending_get() {
        return; // one gs-relative byte load: the whole cost when there is none
    }
    run_pending_slow();
}

/// Clears the per-CPU claim on the way out. Declared before the guard in
/// [`run_pending_slow`], so it drops after it.
struct BhClaim;

impl Drop for BhClaim {
    #[inline]
    fn drop(&mut self) {
        pcr::bh_active_clear();
    }
}

#[inline(never)]
#[cold]
fn run_pending_slow() {
    // Not wired yet. Leaves the flag set: early boot raises before the point
    // exists, and that work still has to run once it does.
    let Some(relaxed) = relaxed_drain() else {
        return;
    };
    // Cheapest decline first. `in_interrupt_context` is subsumed by the
    // interrupt-enable test at the preempt-drop site — an ISR body's outermost
    // unlock restores IF=0 — but is asked anyway, so the witness is sound by
    // check rather than by an argument about one of its call sites.
    if !interrupts::are_interrupts_enabled()
        || pcr::in_interrupt_context()
        || PreemptGuard::is_active()
        || pcr::panic_in_flight_depth() != 0
        || crate::sync::held_lock_count() != 0
    {
        DECLINED_CONTEXT.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // Before any guard exists: a decline that had already created one would fire
    // the release hook again from that guard's own drop.
    if pcr::bh_active_swap(true) {
        DECLINED_REENTRANT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let _claim = BhClaim;

    {
        let _pinned = PreemptGuard::new();
        let ctx = BhContext {
            _scope: PhantomData,
            _not_send: PhantomData,
        };
        for _ in 0..MAX_BH_PASSES {
            // Cleared before the work runs, so a raise from inside a callback
            // re-arms rather than being swallowed.
            if !pcr::bh_pending_take() {
                break;
            }
            crate::sync::rcu::invoke_callbacks(&ctx);
        }
    } // `_pinned` drops here; its release hook sees the claim still held.

    // Outside the guard, still inside the claim: the graveyard's push-side
    // predicate refuses a destroy while preemption is disabled, so running it
    // inside would destroy corpses in a context their pusher declined.
    if relaxed() {
        pcr::bh_pending_set();
    }

    DRAINS.fetch_add(1, Ordering::Relaxed);
}

/// Declines whose cause was the calling context rather than re-entrancy.
#[inline]
pub fn declined_context() -> u64 {
    DECLINED_CONTEXT.load(Ordering::Relaxed)
}

/// Declines because this CPU was already inside a drain. Expected to be large.
#[inline]
pub fn declined_reentrant() -> u64 {
    DECLINED_REENTRANT.load(Ordering::Relaxed)
}

/// Drains that ran to completion.
#[inline]
pub fn drains() -> u64 {
    DRAINS.load(Ordering::Relaxed)
}
