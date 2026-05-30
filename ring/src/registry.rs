//! Global ring registry (SLOPRING § 9, § 13.5).
//!
//! Rings live in a single generation-counter [`HandleTable`] (AD-11). A
//! ring fd stores the ring's [`Handle`] packed
//! losslessly into the open-file `handle: usize`; resolution validates
//! the generation, so a closed-then-reused fd, a foreign `FileKind`, or
//! a stale handle all resolve to a typed error — never UB.
//!
//! Each ring also carries its `owner_pid`; `ring_enter` rejects a ring
//! entered from a process other than its creator (defence in depth on
//! top of the close-on-fork fd policy, SLOPRING § 14).

use slopos_ostd::handle::{Handle, HandleError, HandleTable};
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

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
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;

static REGISTRY: SpinLock<Option<HandleTable<Ring>>> = SpinLock::new(None, LOCK_LEVEL_REGISTRY);

/// Pack a [`Handle<Ring>`] into the `usize` stored in the fd's open-file
/// handle field. Lossless for slot < 4096 and generation < 2^52.
fn pack(h: Handle<Ring>) -> usize {
    (((h.generation() & ((1 << 52) - 1)) << SLOT_BITS) | (h.slot() as u64 & SLOT_MASK)) as usize
}

/// Inverse of [`pack`].
fn unpack(raw: usize) -> Handle<Ring> {
    let raw = raw as u64;
    let slot = (raw & SLOT_MASK) as u32;
    let generation = raw >> SLOT_BITS;
    Handle::from_parts(slot, generation)
}

fn with_registry<R>(f: impl FnOnce(&mut HandleTable<Ring>) -> R) -> R {
    let mut guard = REGISTRY.lock();
    let table = guard.get_or_insert_with(|| {
        HandleTable::with_fixed_capacity(MAX_RINGS).expect("ring registry alloc")
    });
    f(table)
}

/// Insert a fresh ring; returns the packed fd-handle value, or `None`
/// if the registry is full.
pub fn insert(ring: Ring) -> Option<usize> {
    with_registry(|t| t.insert(ring).ok().map(pack))
}

/// Run `f` with a mutable borrow of the ring named by the packed fd
/// handle, holding the registry lock for the duration. Returns
/// `Err(HandleError)` for a stale / out-of-range / foreign handle.
///
/// The closure runs **under the registry lock**, which doubles as the
/// per-ring serialization lock (SLOPRING § 6.3) for the submit and
/// CQE-post bookkeeping — two threads racing `ring_enter` on one fd are
/// serialized here. The harvest *block* must run outside this lock; the
/// caller drops it before sleeping (see `enter.rs`).
pub fn with_ring<R>(raw_handle: usize, f: impl FnOnce(&mut Ring) -> R) -> Result<R, HandleError> {
    let h = unpack(raw_handle);
    with_registry(|t| t.get_mut(h).map(f))
}

/// Remove and drop a ring (last fd closed — SLOPRING § 14). The ring's
/// `Drop` releases the kernel's `RingMeta` frame refs; any user PTE
/// still mapped holds its own ref, so frames survive until the mapping
/// is torn down too. Safe to call on a stale handle (no-op).
pub fn remove(raw_handle: usize) {
    let h = unpack(raw_handle);
    with_registry(|t| {
        let _ = t.remove(h);
    });
}

/// `true` iff the packed handle resolves to a live ring owned by
/// `pid`. Used by `ring_enter` to reject foreign / stale rings.
pub fn owner_is(raw_handle: usize, pid: u32) -> bool {
    let h = unpack(raw_handle);
    with_registry(|t| matches!(t.get(h), Ok(r) if r.owner_pid == pid))
}

#[cfg(test)]
pub fn reset_for_test() {
    let mut guard = REGISTRY.lock();
    *guard = None;
}
