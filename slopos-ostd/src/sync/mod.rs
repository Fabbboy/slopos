//! Synchronisation primitives.
//!
//! `WaitQueue` and `RCU` reach the scheduler and platform clock through
//! one-shot-registered backends ([`wait_queue::WaitQueueBackend`],
//! [`rcu::RcuBackend`]) that the kernel installs at boot, so OSTD does not
//! depend on `slopos-kernel-services`.

pub mod append_log;
pub mod atomic_cell;
pub mod bh;
pub mod cpu_local;
pub mod epoch;
pub mod event_bus;
pub mod init_flag;
pub mod init_in_place;
pub mod intrusive;
pub mod intrusive_dlist;
pub mod kernel_io_task;
pub mod kernel_sync;
pub mod lock_graph;
pub mod lock_tracking;
pub mod mutex;
pub mod once_lock;
pub mod panic_recovery;
pub mod per_cpu_slot;
pub mod poll_waiter;
pub mod raw_link;
pub mod raw_table;
pub mod rcu;
pub mod seqlock;
pub mod spin;
pub mod wait_node;
pub mod wait_queue;

pub use atomic_cell::AtomicCell;
pub use cpu_local::{CacheAligned, CpuLocal, CpuPinned, CpuPinnedMut};
pub use epoch::{Epoch, EpochGuard};
#[cfg(any(test, feature = "test-helpers"))]
pub use event_bus::TEST_BUS;
pub use event_bus::{BUS, EventBus, Subscription, ensure_socket_queues_allocated};
pub use init_flag::{InitFlag, StateFlag};
pub use init_in_place::InitInPlace;
pub use intrusive::{IntrusiveLinkedList, Iter as IntrusiveIter, Link, LinkError, Linked};
pub use intrusive_dlist::{DIter as IntrusiveDIter, DLink, DLinked, IntrusiveDList, dlist_unlink};
pub use kernel_sync::{ApToken, BspToken, CpuInitWitness, KernelSync, run_ap_init, run_bsp_init};
#[cfg(any(test, feature = "test-helpers"))]
pub use kernel_sync::{
    reset_ap_token_for_tests, reset_bsp_token_for_tests, run_ap_init_for_test,
    run_bsp_init_for_test,
};
pub use lock_tracking::{
    DeclareOrderError, LOCK_LEVEL_ALLOCATOR, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE,
    LOCK_LEVEL_SCHEDULER, LOCK_LEVEL_UNORDERED, LockClassKey, LockdepMode, chain_count, chain_hits,
    chain_misses, class_collisions, class_count, class_info, class_slots_leaked, declare_order,
    declared_count, declared_observed, edge_count, enable_lock_tracking, enter_fatal_bypass,
    fatal_bypassed, for_each_held_lock_name, for_each_held_lock_name_for_cpu, graph_overflowed,
    held_depth_max, held_depth_overflows, held_lock_addrs, held_lock_addrs_for_cpu,
    held_lock_count, lockdep_mode, overflow_reported, poison_unlock_all_held,
    registered_class_count, report_only_violations, set_lockdep_mode, tracking_enabled,
    validator_alive, violation_reports, violations_reported,
};
pub use mutex::{Mutex, MutexGuard};
pub use once_lock::OnceLock;
pub use panic_recovery::poison_all_held_locks;
pub use per_cpu_slot::PerCpuSlot;
pub use poll_waiter::{PollWaiter, PollWaiterRef};
pub use raw_link::{ByteChain, RawLink};
pub use raw_table::RawTable;
pub use rcu::{
    RcuArcSlot, RcuBackend, RcuCell, RcuCellGuard, RcuReadGuard, call_rcu, rcu_barrier,
    rcu_gp_poll, rcu_gp_seq, rcu_note_cpu_idle_enter, rcu_note_cpu_idle_exit, rcu_note_qs,
    rcu_note_qs_from_interrupt, rcu_process_callbacks, rcu_qs_counter, rcu_raise_softirq,
    rcu_read_lock, register_rcu_backend, synchronize_rcu,
};
#[cfg(any(test, feature = "test-helpers"))]
pub use rcu::{rcu_gp_poll_count, rcu_sync_entry_count};
pub use seqlock::{SeqLock, SeqLockWriteGuard};
pub use spin::{
    IrqRwLock, IrqRwLockReadGuard, IrqRwLockWriteGuard, PreemptMutex, PreemptMutexGuard,
    register_spin_relax_hook, spin_relax,
};
pub use spin::{SpinLock, SpinLockGuard};
pub use wait_node::{NO_POLL_ERA, WaitNode};
pub use wait_queue::{
    WaitAbort, WaitQueue, WaitQueueBackend, WaitResult, WaitTaskHandle, register_wait_queue_backend,
};

pub use crate::cpu::preempt::{
    IrqPreemptGuard, PreemptGuard, is_preemption_disabled, preempt_count_pcr as preempt_count,
    register_reschedule_callback,
};
