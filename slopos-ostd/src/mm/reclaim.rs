//! Reclaim: giving pages back under pressure.
//!
//! The quota bounds acquisition, not holding time. Driven by the *caller* of a
//! refused allocation at a syscall boundary, never from `try_charge` — a hook
//! there would give the lock-free arena an inbound edge from every charge site.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::sync::{BspToken, KernelSync};

/// A subsystem that can give pages back on demand.
///
/// Implemented on a zero-sized type: the registry is `.bss` and holds no
/// state, so a registrant is a vtable and nothing else.
pub trait Reclaimable: Sync {
    /// Name, for the diagnostic dump.
    fn name(&self) -> &'static str;

    /// Pages that could be released right now, without releasing any.
    ///
    /// An over-estimate is fine: this decides whether a pass is worth making.
    fn reclaimable_pages(&self) -> u32;

    /// Release `want` pages. Returns how many were actually released.
    ///
    /// A budget, not a ceiling: a reclaimer whose unit is indivisible — the
    /// quarantine releases whole buddy blocks — stops as soon as the budget is
    /// met, so the total may exceed `want` by less than one of its units. The
    /// alternative, declining a unit that would overshoot, is a reclaimer that
    /// reports zero forever while `reclaimable_pages` says there is work.
    ///
    /// Must not block, must not allocate, and must return zero rather than
    /// wait for a lock.
    fn reclaim(&self, want: u32) -> u32;
}

/// Fixed-size and `.bss`: a table that allocated would be unusable from the
/// path that runs when allocation has already failed.
const MAX_RECLAIMERS: usize = 8;

/// One slot, holding a `&'static dyn Reclaimable` behind a lock-free
/// publish/observe pair.
///
/// `run` may already hold an allocator lock, so the registry has to be
/// readable without taking anything. The fat pointer cannot be one atomic, so
/// `armed` publishes it and is the only thing read before it.
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

/// Slots claimed so far.
static REGISTERED: AtomicUsize = AtomicUsize::new(0);

/// Passes one [`run`] will make before giving up. More than one because a
/// registrant may free a page another was holding a reference to; bounded so a
/// pathological pair cannot spin.
const MAX_PASSES: u32 = 2;

/// Register a reclaimer. Boot only.
///
/// Registration order is policy: the cheapest and most certainly-recoverable
/// pools go first.
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
/// Ask the registrants for `want` pages, stopping at the first pass that meets
/// the budget or frees nothing.
///
/// The total may exceed `want` by less than one reclaimer unit, for the reason
/// [`Reclaimable::reclaim`] gives. `run` stops asking the moment the budget is
/// met, so the overshoot is one unit for the whole call, not one per pass.
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
