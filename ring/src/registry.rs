//! Global ring registry (SLOPRING § 9, § 13.5).
//!
//! Rings live in a single generation-counter [`HandleTable`] (AD-11), the
//! [`Handle`] packed losslessly into the open-file `handle: usize`; resolution
//! validates the generation, so a closed-then-reused fd, a foreign `FileKind`,
//! or a stale handle all resolve to a typed error — never UB. Every route to a
//! ring is additionally gated on its `owner` (see [`owner_is`]).

use slopos_fs::fileio::FdTable;
use slopos_ostd::KArc;
use slopos_ostd::handle::{Handle, HandleError, HandleTable};
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, Mutex, SpinLock};

use crate::ring_obj::Ring;

/// Maximum concurrent rings system-wide; bounds the registry's fixed-capacity
/// table so it never reallocates.
const MAX_RINGS: usize = 256;

/// Slot-index bit width in the packed fd handle: 12 bits of slot index, the
/// remaining 52 the generation, which increments once per slot reuse and so
/// never wraps in any realistic uptime.
const SLOT_BITS: u32 = 12;

/// One registry slot: a reference-counted, individually-locked ring
/// (SLOPRING § 6.3, § 9). The [`KArc`] keeps the ring alive for a `with_ring`
/// caller even if a concurrent `close` removes it from the table.
///
/// A **sleeping [`Mutex`]**, not a `SpinLock`: the submit and harvest closures
/// run opcode probes inline under it, and a filesystem opcode descends into the
/// VFS, whose I/O completion waits are scheduler-backed — the holder
/// legitimately deschedules mid-probe, which under a preemption-disabling lock
/// would be scheduling-while-atomic. Only ever taken in task context, never
/// from an interrupt handler, so sleeping on it is always legal.
type RingSlot = KArc<Mutex<Ring>>;

/// The global registry-table lock. Never held while a per-ring lock is held —
/// that decoupling is what keeps the close path (`FILEIO_SLOT → REGISTRY`) from
/// forming a cycle with the enter path (per-ring lock → `FILEIO_SLOT`).
static REGISTRY: SpinLock<Option<HandleTable<RingSlot>>> =
    SpinLock::new(None, lock_class!("ring.REGISTRY", LOCK_LEVEL_REGISTRY));

fn with_registry<R>(f: impl FnOnce(&mut HandleTable<RingSlot>) -> R) -> R {
    let mut guard = REGISTRY.lock();
    let table = guard.get_or_insert_with(|| {
        HandleTable::with_fixed_capacity(MAX_RINGS).expect("ring registry alloc")
    });
    f(table)
}

/// Look up and **clone** the [`RingSlot`] for a packed fd handle; the table
/// lock is dropped on return, so callers take the per-ring lock outside it.
fn slot_for(raw_handle: usize) -> Result<RingSlot, HandleError> {
    let h = Handle::unpack(raw_handle, SLOT_BITS);
    with_registry(|t| t.get(h).map(|a| a.clone()))
}

/// Insert a fresh ring; returns the packed fd-handle value, or `None`
/// if the registry is full or the per-ring allocation fails.
pub fn insert(ring: Ring) -> Option<usize> {
    let slot = KArc::try_init(Mutex::init_owned(
        ring,
        lock_class!("Ring.inner", LOCK_LEVEL_RESOURCE),
    ))
    .ok()?;
    with_registry(|t| t.insert(slot).ok().map(|h| h.pack(SLOT_BITS)))
}

/// Run `f` with a mutable borrow of the ring named by the packed fd
/// handle, holding **only the per-ring lock** for the closure. Returns
/// `Err(HandleError)` for a stale / out-of-range / foreign handle.
///
/// Two threads racing `ring_enter` on one fd serialize here (SLOPRING § 6.3);
/// distinct rings proceed concurrently. The harvest *block* must run outside
/// the per-ring lock; the caller drops it before sleeping (see `enter.rs`).
pub fn with_ring<R>(raw_handle: usize, f: impl FnOnce(&mut Ring) -> R) -> Result<R, HandleError> {
    let slot = slot_for(raw_handle)?;
    // A lock abort never runs `f`; reported as `Stale` rather than widening
    // `HandleError` with a scheduling concern no other table consumer observes.
    let Ok(mut ring) = slot.lock() else {
        return Err(HandleError::Stale);
    };
    Ok(f(&mut ring))
}

/// Remove a ring from the table (last fd closed — SLOPRING § 14). Drops only
/// the **table's** [`KArc`], so a concurrent `with_ring` that already cloned
/// the slot keeps the ring alive until it finishes. No-op on a stale handle.
///
/// The removed slot is dropped **after** the table lock is released: a last
/// drop tears down every in-flight op's `FileRef`, which can take arbitrary
/// subsystem locks and even re-enter this registry for a passed ring fd.
pub fn remove(raw_handle: usize) {
    let h = Handle::unpack(raw_handle, SLOT_BITS);
    let removed = with_registry(|t| t.remove(h).ok());
    drop(removed);
}

/// `true` iff the packed handle resolves to a live ring owned by `table`.
///
/// The check gating `ring_enter` and the four register ops against a foreign or
/// stale ring. Reads the owner under the per-ring lock, having released the
/// table lock — never holding both at once.
pub fn owner_is(raw_handle: usize, table: FdTable) -> bool {
    match slot_for(raw_handle) {
        // An abort denies the ring, which is the safe direction.
        Ok(slot) => slot.lock().is_ok_and(|ring| ring.owner == table),
        Err(_) => false,
    }
}

#[cfg(test)]
pub fn reset_for_test() {
    let mut guard = REGISTRY.lock();
    *guard = None;
}
