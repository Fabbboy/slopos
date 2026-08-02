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
    ACQ_NONE, ACQ_RECURSIVE, ClassInfo, LO_BLESSED, LO_DUPOK, LOCK_LEVEL_ALLOCATOR,
    LOCK_LEVEL_EPOCH, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, LOCK_LEVEL_SCHEDULER,
    LOCK_LEVEL_UNORDERED, LockClassKey, LockdepMode, MAX_CHAINS, MAX_CLASSES, MAX_EDGES,
    MAX_HELD_LOCKS, PoisonUnlockFn, REGISTRABLE_CLASSES, chain_count, chain_hits, chain_misses,
    class_collisions, class_count, class_info, class_slots_leaked, edge_count,
    enable_lock_tracking, enter_fatal_bypass, fatal_bypassed, graph_overflowed, held_depth_mark,
    held_depth_max, held_depth_overflows, held_lock_addrs, held_lock_addrs_for_cpu,
    held_lock_count, held_lock_snapshot, innermost_held_lock, lockdep_mode, overflow_reported,
    poison_drained, poison_unlock_all_held, poison_unlock_held_above, pop_lock, pop_misses,
    push_lock, push_lock_ex, report_only_violations, set_lockdep_mode, tracking_enabled,
    validator_alive, violation_reports, violations_reported,
};

#[cfg(any(test, feature = "test-helpers"))]
pub use super::lock_graph::{
    PushIrqState, SelfTestGuard, push_irq_state, register_class_for_test, reserve_self_test_class,
    reset_for_test,
};
