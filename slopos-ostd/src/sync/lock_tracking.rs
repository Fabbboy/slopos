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
    ClassInfo, LO_BLESSED, LO_DUPOK, LO_TRYLOCK, LOCK_LEVEL_ALLOCATOR, LOCK_LEVEL_REGISTRY,
    LOCK_LEVEL_RESOURCE, LOCK_LEVEL_SCHEDULER, LOCK_LEVEL_UNORDERED, MAX_CHAINS, MAX_CLASSES,
    MAX_EDGES, PoisonUnlockFn, REGISTRABLE_CLASSES, chain_count, class_count, class_info,
    edge_count, enable_lock_tracking, enter_panic_bypass, graph_overflowed, held_lock_addrs,
    held_lock_addrs_for_cpu, held_lock_count, overflow_reported, panic_bypassed,
    poison_unlock_all_held, pop_lock, push_lock, tracking_enabled, validator_alive,
    violations_reported,
};

#[cfg(any(test, feature = "test-helpers"))]
pub use super::lock_graph::{SelfTestGuard, reserve_self_test_class, reset_for_test};
