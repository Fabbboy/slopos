//! Reclaim: giving pages back under pressure.
//!
//! The quota bounds *acquisition*. Without reclaim it does not bound *holding
//! time*, which is a first-come land grab with better bookkeeping — a
//! principal that got there first keeps what it took for as long as it likes,
//! and every later one is refused against memory nobody is using.
//!
//! # Shape
//!
//! Two calls per registrant, following the split Linux settled on after its
//! single-callback shrinker proved unworkable: [`reclaimable_pages`] answers
//! *how much could be given back* without giving any back, and [`reclaim`]
//! actually does it. Splitting them is what lets a caller decide whether the
//! walk is worth making at all, and what keeps the counting path free of the
//! locks the freeing path needs.
//!
//! [`reclaimable_pages`]: Reclaimable::reclaimable_pages
//! [`reclaim`]: Reclaimable::reclaim
//!
//! # What may implement it
//!
//! A registrant must be able to drop what it holds **without asking anyone**:
//! no callback into a subsystem that might be mid-operation, no wait, and no
//! failure mode worse than reclaiming nothing. Both in-tree implementors were
//! chosen for that and nothing else — the per-CPU stack-VA cache is a pool
//! that can already shrink to zero, and a clean page-cache frame is one whose
//! dirty bit says the disk already has it.
//!
//! A registrant that cannot take its lock must return zero rather than block.
//! Reclaim runs on an allocation failure path, which is reachable from under
//! other subsystems' locks; a reclaimer that waited would deadlock against the
//! very allocation it was called to satisfy.
//!
//! # Where it is driven from
//!
//! Never from `try_charge`. The arena takes no locks by construction, and a
//! reclaim hook there would give it an inbound edge from every charge site at
//! once. Reclaim is driven by the *caller* of a refused allocation, at a
//! syscall boundary where blocking is legal and a failure has an errno to
//! travel back on.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::sync::{BspToken, KernelSync};

/// A subsystem that can give pages back on demand.
///
/// Implemented on a zero-sized type, not on the pool itself: the registry is
/// `.bss` and holds no state, so a registrant is a vtable and nothing else.
pub trait Reclaimable: Sync {
    /// Name, for the diagnostic dump. A reclaimer nobody can see reclaiming is
    /// indistinguishable from one that never runs.
    fn name(&self) -> &'static str;

    /// Pages that could be released right now, without releasing any.
    ///
    /// An estimate is fine and an over-estimate is normal — this decides
    /// whether a pass is worth making, not how much will come back.
    fn reclaimable_pages(&self) -> u32;

    /// Release at most `want` pages. Returns how many were actually released.
    ///
    /// Must not block, must not allocate, and must return zero rather than
    /// wait for a lock.
    fn reclaim(&self, want: u32) -> u32;
}

/// Registrants, in the order they are asked.
///
/// Fixed-size and `.bss`: registration happens during boot, and a table that
/// allocated would be unusable from the path that runs when allocation has
/// already failed.
const MAX_RECLAIMERS: usize = 8;

/// One slot, holding a `&'static dyn Reclaimable` behind a lock-free
/// publish/observe pair.
///
/// A `SpinLock<Option<&dyn _>>` would be the obvious shape and is the wrong
/// one: `run` is called from an allocation-failure path that may already hold
/// an allocator lock, so the registry has to be readable without taking
/// anything. The reference itself is a fat pointer and cannot be one atomic,
/// so `armed` publishes it — `Release` on the store, `Acquire` on the load —
/// and the reference is only read once that flag is observed set.
struct Slot {
    armed: AtomicBool,
    reclaimer: KernelSync<UnsafeCell<Option<&'static dyn Reclaimable>>>,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            armed: AtomicBool::new(false),
            reclaimer: KernelSync::new(UnsafeCell::new(None)),
        }
    }
}

static RECLAIMERS: [Slot; MAX_RECLAIMERS] = [const { Slot::empty() }; MAX_RECLAIMERS];

/// Slots claimed so far. Claiming is a `fetch_add`, so two CPUs registering at
/// once take different slots and neither observes a half-written one.
static REGISTERED: AtomicUsize = AtomicUsize::new(0);

/// Passes one [`run`] will make before giving up.
///
/// Bounded because a registrant may free a page that another registrant was
/// holding a reference to, so one pass can unblock another. Two is enough for
/// that and stops a pathological pair from spinning.
const MAX_PASSES: u32 = 2;

/// Register a reclaimer. Boot only.
///
/// Ordered by registration, and that order is policy: the cheapest and most
/// certainly-recoverable pools go first, so a small shortfall is met without
/// touching a cache that costs I/O to refill.
pub fn register<'brand>(_token: &BspToken<'brand>, reclaimer: &'static dyn Reclaimable) {
    let slot_idx = REGISTERED.fetch_add(1, Ordering::AcqRel);
    let Some(slot) = RECLAIMERS.get(slot_idx) else {
        panic!("mm::reclaim: more than {MAX_RECLAIMERS} reclaimers registered");
    };
    // SAFETY: this CPU holds the only claim on `slot_idx` (the `fetch_add`
    // hands each index out once) and `armed` is still false, so no reader can
    // be observing the cell. The `Release` store below is what publishes it.
    unsafe { *slot.reclaimer.get().get() = Some(reclaimer) };
    slot.armed.store(true, Ordering::Release);
}

/// Visit every registered reclaimer, in registration order.
fn for_each(mut f: impl FnMut(&'static dyn Reclaimable)) {
    for slot in RECLAIMERS.iter() {
        if !slot.armed.load(Ordering::Acquire) {
            continue;
        }
        // SAFETY: `armed` was stored with `Release` after the cell was
        // written and is never cleared, so observing it `Acquire` makes that
        // write visible. The cell is written once, by `register`, before its
        // flag is set — so there is no writer to race, and the shared
        // reference below is the only kind of access that exists.
        let Some(reclaimer) = (unsafe { *slot.reclaimer.get().get() }) else {
            continue;
        };
        f(reclaimer);
    }
}

/// Pages every registrant believes it could release.
pub fn reclaimable_pages() -> u32 {
    let mut total = 0u32;
    for_each(|r| total = total.saturating_add(r.reclaimable_pages()));
    total
}

/// Try to release `want` pages, returning how many actually came back.
///
/// Asks each registrant in turn for what is still outstanding, so a pool that
/// can satisfy the whole request spares the rest. Stops as soon as the target
/// is met.
pub fn run(want: u32) -> u32 {
    if want == 0 {
        return 0;
    }
    let mut freed = 0u32;
    for _ in 0..MAX_PASSES {
        let before = freed;
        for_each(|r| {
            if freed >= want {
                return;
            }
            freed = freed.saturating_add(r.reclaim(want - freed));
        });
        if freed >= want || freed == before {
            break;
        }
    }
    freed
}

/// Report each registrant's name and what it believes it holds.
pub fn for_each_reclaimer(mut f: impl FnMut(&'static str, u32)) {
    for_each(|r| f(r.name(), r.reclaimable_pages()));
}
