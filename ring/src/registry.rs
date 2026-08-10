//! Global ring registry (SLOPRING § 9, § 13.5).
//!
//! Rings live in a single generation-counter [`HandleTable`] (AD-11). A
//! ring fd stores the ring's [`Handle`] packed
//! losslessly into the open-file `handle: usize`; resolution validates
//! the generation, so a closed-then-reused fd, a foreign `FileKind`, or
//! a stale handle all resolve to a typed error — never UB.
//!
//! Each ring also carries its `owner`. [`owner_is`] is the primary
//! containment for a ring reached from the wrong process: `ring_enter`
//! and all four register ops reject a handle whose owner is not the
//! caller. It holds for every route to the handle — including an
//! intra-process `dup`, which is a legitimate alias of a ring the caller
//! does own, and a handle a process guessed rather than opened.

use slopos_fs::fileio::FdTable;
use slopos_ostd::KArc;
use slopos_ostd::handle::{Handle, HandleError, HandleTable};
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, Mutex, SpinLock};

use crate::ring_obj::Ring;

/// Maximum concurrent rings system-wide. Bounded so the registry's
/// fixed-capacity table never reallocates (lock-free-scan-safe shape,
/// though we always hold the lock here).
const MAX_RINGS: usize = 256;

/// Slot-index bit width in the packed fd handle. 12 bits → up to 4096
/// rings (we cap at `MAX_RINGS`); the remaining 52 bits hold the
/// generation, which increments by one per slot reuse and so never
/// wraps in any realistic uptime.
const SLOT_BITS: u32 = 12;

/// One registry slot: a reference-counted, individually-locked ring
/// (SLOPRING § 6.3, § 9). The per-ring lock is the per-ring serialization
/// lock; the [`KArc`] keeps the ring alive for a `with_ring` caller even
/// if a concurrent `close` removes it from the table (no UAF).
///
/// It is a **sleeping [`Mutex`]**, not a `SpinLock`, because the submit
/// and harvest closures run opcode probes inline under it (`ring_enter`'s
/// `submit` / `harvest_step`), and a probe for a filesystem opcode
/// (`OP_OPENAT`, `OP_READ`/`OP_WRITE` on a regular fd, …) descends into
/// the VFS, whose ext2 + block-device I/O completion waits are now
/// scheduler-backed — the holder legitimately deschedules mid-probe. A
/// spinning, preemption-disabling lock here would make that a
/// scheduling-while-atomic violation (the held lock travels with the
/// blocked task while every contender spins unpreemptibly with IRQs
/// masked on the BSP — exactly the freeze the block-I/O rework set out to
/// kill). The lock is only ever taken in task context (`with_ring` /
/// `owner_is` from the `ring_enter` / `ring_register` syscall paths),
/// never from an interrupt handler, so sleeping on it is always legal.
type RingSlot = KArc<Mutex<Ring>>;

/// The global registry-table lock. Held only briefly: to insert, to
/// remove, or to look up and **clone** a [`RingSlot`] handle. It is
/// never held while a per-ring lock is held — that decoupling is what
/// keeps the close path (`FILEIO_SLOT → REGISTRY`) from forming a cycle
/// with the enter path (per-ring lock → `FILEIO_SLOT`).
static REGISTRY: SpinLock<Option<HandleTable<RingSlot>>> =
    SpinLock::new(None, lock_class!("ring.REGISTRY", LOCK_LEVEL_REGISTRY));

fn with_registry<R>(f: impl FnOnce(&mut HandleTable<RingSlot>) -> R) -> R {
    let mut guard = REGISTRY.lock();
    let table = guard.get_or_insert_with(|| {
        HandleTable::with_fixed_capacity(MAX_RINGS).expect("ring registry alloc")
    });
    f(table)
}

/// Look up and **clone** the [`RingSlot`] for a packed fd handle. The
/// table lock is dropped on return (the clone — a refcount bump — keeps
/// the ring alive). Callers then take the per-ring lock outside the
/// table lock, which is the load-bearing decoupling.
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
/// The closure runs under the per-ring serialization lock (SLOPRING
/// § 6.3) for the submit and CQE-post bookkeeping — two threads racing
/// `ring_enter` on one fd are serialized here, while distinct rings
/// proceed concurrently. The global registry-table lock is acquired
/// only to clone the ring's [`RingSlot`] handle and is **released
/// before** the per-ring lock is taken, so no `REGISTRY → per-ring`
/// edge exists. The harvest *block* must still run outside the per-ring
/// lock; the caller drops it before sleeping (see `enter.rs`).
pub fn with_ring<R>(raw_handle: usize, f: impl FnOnce(&mut Ring) -> R) -> Result<R, HandleError> {
    let slot = slot_for(raw_handle)?;
    // A task aborted while contending for the per-ring lock never runs `f`.
    // Reported as `Stale` because that is what the caller can act on — this
    // handle will not resolve for you — rather than widening a handle-table
    // error with a scheduling concern no other table consumer can observe.
    let Ok(mut ring) = slot.lock() else {
        return Err(HandleError::Stale);
    };
    Ok(f(&mut ring))
}

/// Remove a ring from the table (last fd closed — SLOPRING § 14). This
/// drops only the **table's** [`KArc`] reference; the [`Ring`] is freed
/// when the last clone drops — so a concurrent `with_ring` that already
/// cloned the slot keeps the ring alive until it finishes (no UAF). The
/// ring's `Drop` then releases the kernel's `RingMeta` frame refs; any
/// user PTE still mapped holds its own ref, so frames survive until the
/// mapping is torn down too. Safe to call on a stale handle (no-op).
///
/// The removed slot is taken out **under** the table lock but dropped
/// **after** it is released: when this was the last clone the `Ring` drops
/// here, tearing down every in-flight op's held `FileRef`, and that
/// teardown can take arbitrary subsystem locks (even re-enter the registry
/// for a passed ring fd) — which must never run under the table spinlock.
pub fn remove(raw_handle: usize) {
    let h = Handle::unpack(raw_handle, SLOT_BITS);
    let removed = with_registry(|t| t.remove(h).ok());
    drop(removed);
}

/// `true` iff the packed handle resolves to a live ring owned by `table`.
///
/// The primary check gating `ring_enter` and the four register ops against a
/// foreign or stale ring. Clones the slot under the table lock, releases it,
/// then reads the owner under the per-ring lock — never holding both at once.
///
/// Comparing [`FdTable`]s rather than pids is what makes "foreign" mean the
/// process and not the number: a caller that inherited the creator's recycled
/// id compares unequal, because its generation differs.
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
