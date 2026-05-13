//! Backwards-compatibility shim over [`super::lock_graph`].
//!
//! Pre-existing call sites (`SpinLock::lock`, `PreemptMutex::lock`,
//! `IrqRwLock::*`, `utils/src/panic_recovery.rs`, `boot/src/idt.rs`) call
//! into the `lock_tracking` namespace with `push_lock` / `pop_lock` /
//! `poison_unlock_all_held` / `enable_lock_tracking` / `LOCK_LEVEL_*` /
//! `held_lock_count`. The actual validator now lives in
//! [`super::lock_graph`] (dependency-graph + cycle detection); this shim
//! preserves the public surface so no call site needs to change.
//!
//! Everything here is a `pub use`; no logic lives in this file.

pub use super::lock_graph::{
    LO_BLESSED, LO_DUPOK, LO_TRYLOCK, LOCK_LEVEL_ALLOCATOR, LOCK_LEVEL_REGISTRY,
    LOCK_LEVEL_RESOURCE, LOCK_LEVEL_SCHEDULER, LOCK_LEVEL_UNORDERED, PoisonUnlockFn,
    enable_lock_tracking, enter_panic_bypass, held_lock_count, poison_unlock_all_held, pop_lock,
    push_lock,
};

#[cfg(any(test, feature = "test-helpers"))]
pub use super::lock_graph::reset_for_test;
