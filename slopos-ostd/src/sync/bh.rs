//! The bottom-half point: where deferred, allocator-heavy, non-blocking kernel
//! work is legal on a CPU that is under load.
//!
//! OSTD owns the *point* and nothing else — no queue, no priority, no execution
//! thread. [`BhContext`] witnesses that interrupts are on, that this is not
//! interrupt context, that no unwind is in flight, that no tracked lock is held,
//! and that a context switch cannot intervene. What runs there is policy, and
//! lives outside the trusted core.
//!
//! The scope's preemption guard discharges three obligations at once: it pins
//! the CPU so the per-CPU claim cannot be stranded by a migration, it makes
//! re-entry from a nested unlock impossible because the release hook fires only
//! at the outermost one, and it routes a bottom half that blocks into the
//! scheduler's preempt-count assertion.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::cpu::preempt::PreemptGuard;
use crate::cpu::x86_64::{interrupts, pcr};
use crate::sync::{BspToken, CacheAligned};

/// Witness that the holder stands in bottom-half context on this CPU.
///
/// `!Send + !Sync`: the facts are about one CPU at one instant, and the token
/// must not outlive the scope that observed them.
#[derive(Debug)]
pub struct BhContext<'a> {
    _scope: PhantomData<&'a ()>,
    _not_send: PhantomData<*const ()>,
}

/// Work that needs the preemption guard *released* — the task graveyard and
/// the idle input drain. One slot rather than a table: the kernel has one such
/// consumer and it composes whatever it needs behind that.
///
/// Returns whether it left more to do.
pub type RelaxedDrain = fn() -> bool;

static RELAXED_DRAIN: AtomicUsize = AtomicUsize::new(0);

/// Passes inside one invocation.
///
/// Bounds a self-arming callback from holding the CPU in the drain
/// indefinitely; what is left over stays pending for the next point.
const MAX_BH_PASSES: u32 = 8;

struct BhCounters {
    declined_context: AtomicU64,
    declined_reentrant: AtomicU64,
    drains: AtomicU64,
}

impl BhCounters {
    const fn new() -> Self {
        Self {
            declined_context: AtomicU64::new(0),
            declined_reentrant: AtomicU64::new(0),
            drains: AtomicU64::new(0),
        }
    }
}

/// Not [`crate::sync::CpuLocal`]: its pin guard's drop is the very hook this module hangs off.
static BH_COUNTERS: [CacheAligned<BhCounters>; pcr::MAX_CPUS] =
    [const { CacheAligned(BhCounters::new()) }; pcr::MAX_CPUS];

#[inline]
fn counters() -> Option<&'static BhCounters> {
    BH_COUNTERS.get(pcr::get_current_cpu()).map(|slot| &slot.0)
}

/// Wire the bottom-half point.
///
/// Registration arms it on every CPU at once: a per-CPU armed flag would never
/// be set by an AP that reached its scheduler before the BSP got here.
pub fn arm<'brand>(_token: &BspToken<'brand>, relaxed: RelaxedDrain) {
    RELAXED_DRAIN.store(relaxed as usize, Ordering::Release);
}

#[inline]
fn relaxed_drain() -> Option<RelaxedDrain> {
    let raw = RELAXED_DRAIN.load(Ordering::Acquire);
    crate::util::fn_ptr::fn_ptr_decode_opt::<RelaxedDrain>(raw as *mut ())
}

/// Mark that this CPU has bottom-half work.
///
/// One `gs`-relative byte store, legal from a hard IRQ handler and from under a
/// cli-spinlock. Nothing on this path may ever take a lock, allocate, or log —
/// no caller is somewhere those are available.
#[inline]
pub fn raise() {
    pcr::bh_pending_set();
}

/// Run this CPU's bottom half if the current context permits it.
///
/// Total by construction: every failed predicate declines and nothing asserts.
/// Most call sites are inside a `Drop`, where a panic is forbidden outright.
#[inline]
pub fn run_pending_if_due() {
    if !pcr::bh_pending_get() {
        return;
    }
    run_pending_slow();
}

/// Clears the per-CPU claim on the way out. Declared before the guard in
/// [`run_pending_slow`] so it drops after it: the guard's own drop is a release
/// hook, and a claim released inside that scope would re-enter this module from
/// the destructor.
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
    // `in_interrupt_context` is subsumed by the interrupt-enable test at the
    // preempt-drop site, but is asked anyway so the witness is sound by check
    // rather than by an argument about one of its call sites.
    if !interrupts::are_interrupts_enabled()
        || pcr::in_interrupt_context()
        || PreemptGuard::is_active()
        || pcr::panic_in_flight_depth() != 0
        || crate::sync::held_lock_count() != 0
    {
        if let Some(counters) = counters() {
            counters.declined_context.fetch_add(1, Ordering::Relaxed);
        }
        return;
    }

    // Before any guard exists: a decline that had already created one would fire
    // the release hook again from that guard's own drop.
    if pcr::bh_active_swap(true) {
        if let Some(counters) = counters() {
            counters.declined_reentrant.fetch_add(1, Ordering::Relaxed);
        }
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
    }

    // Outside the guard, still inside the claim: the graveyard's push-side
    // predicate refuses a destroy while preemption is disabled, so running it
    // inside would destroy corpses in a context their pusher declined.
    if relaxed() {
        pcr::bh_pending_set();
    }

    if let Some(counters) = counters() {
        counters.drains.fetch_add(1, Ordering::Relaxed);
    }
}

/// Calling-CPU declines whose cause was the context rather than re-entrancy.
#[inline]
pub fn declined_context() -> u64 {
    counters().map_or(0, |c| c.declined_context.load(Ordering::Relaxed))
}

/// Calling-CPU declines because it was already inside a drain. Expected to be large.
#[inline]
pub fn declined_reentrant() -> u64 {
    counters().map_or(0, |c| c.declined_reentrant.load(Ordering::Relaxed))
}

/// Drains that ran to completion on the calling CPU.
#[inline]
pub fn drains() -> u64 {
    counters().map_or(0, |c| c.drains.load(Ordering::Relaxed))
}
