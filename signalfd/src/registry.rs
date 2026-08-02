//! Global signalfd registry.
//!
//! Each `FileKind::Signalfd` fd stores a generation-counter [`Handle`]
//! packed into its open-file `handle: usize`; resolution validates the
//! generation, so a closed-then-reused fd or a stale handle resolves to a
//! typed miss, never UB (the same AD-11 discipline the ring registry uses).
//!
//! Unlike a ring, signalfd state is immutable after creation — just the
//! owner task id + the subscribed signal mask — so the table stores the
//! plain `Copy` value and no per-entry lock is needed.

use slopos_ostd::handle::{Handle, HandleTable};
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

/// Maximum concurrent signalfds system-wide (fixed-capacity table).
const MAX_SIGNALFDS: usize = 256;

/// Slot-index bit width in the packed fd handle; the remaining bits hold the
/// generation (see [`Handle::pack`]).
const SLOT_BITS: u32 = 12;

/// Immutable per-signalfd state: which task's signals to watch, and the
/// subscribed mask. `Copy` so lookups hand back a value, not a borrow.
#[derive(Clone, Copy)]
pub struct SignalfdState {
    pub owner_task_id: u32,
    pub mask: u64,
}

static REGISTRY: SpinLock<Option<HandleTable<SignalfdState>>> =
    SpinLock::new(None, lock_class!("signalfd.REGISTRY", LOCK_LEVEL_REGISTRY));

fn with_registry<R>(f: impl FnOnce(&mut HandleTable<SignalfdState>) -> R) -> R {
    let mut guard = REGISTRY.lock();
    let table = guard.get_or_insert_with(|| {
        HandleTable::with_fixed_capacity(MAX_SIGNALFDS).expect("signalfd registry alloc")
    });
    f(table)
}

/// Insert fresh signalfd state; returns the packed fd-handle, or `None` if
/// the registry is full.
pub fn insert(state: SignalfdState) -> Option<usize> {
    with_registry(|t| t.insert(state).ok().map(|h| h.pack(SLOT_BITS)))
}

/// Resolve a packed fd handle to its (copied) state, or `None` if stale.
pub fn get(raw_handle: usize) -> Option<SignalfdState> {
    let h = Handle::unpack(raw_handle, SLOT_BITS);
    with_registry(|t| t.get(h).copied().ok())
}

/// Remove a signalfd from the table (last fd closed). No-op on a stale handle.
pub fn remove(raw_handle: usize) {
    let h = Handle::unpack(raw_handle, SLOT_BITS);
    with_registry(|t| {
        let _ = t.remove(h);
    });
}
