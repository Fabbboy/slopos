//! Synchronisation primitives.
//!
//! All synchronisation primitives the kernel needs live here: spinning
//! ticket locks, sleeping mutexes, RCU, wait queues, sequence locks,
//! per-CPU storage, init/state flags, lazy `OnceLock`, and the per-CPU
//! lock-tracking + ordering enforcement.
//!
//! `WaitQueue` and `RCU` reach the kernel scheduler / platform clock
//! through one-shot-registered backends ([`wait_queue::WaitQueueBackend`],
//! [`rcu::RcuBackend`]) — OSTD does not depend on `slopos-kernel-services`
//! or `slopos-utils`. The kernel installs production backends at boot.

pub mod cpu_local;
pub mod init_flag;
pub mod intrusive;
pub mod lock_tracking;
pub mod mutex;
pub mod once_lock;
pub mod raw_link;
pub mod raw_table;
pub mod rcu;
pub mod seqlock;
pub mod spin;
pub mod wait_queue;

pub use cpu_local::{CacheAligned, CpuLocal, CpuPinned, CpuPinnedMut};
pub use init_flag::{InitFlag, StateFlag};
pub use intrusive::{IntrusiveLinkedList, Iter as IntrusiveIter, Link, LinkError, Linked};
pub use lock_tracking::{
    LOCK_LEVEL_ALLOCATOR, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, LOCK_LEVEL_SCHEDULER,
    LOCK_LEVEL_UNORDERED, enable_lock_tracking, held_lock_count, poison_unlock_all_held,
};
pub use mutex::{Mutex, MutexGuard};
pub use once_lock::OnceLock;
pub use raw_link::{ByteChain, RawLink};
pub use raw_table::RawTable;
pub use rcu::{
    RcuBackend, RcuReadGuard, call_rcu, rcu_note_qs, rcu_process_callbacks, rcu_raise_softirq,
    rcu_read_lock, register_rcu_backend, synchronize_rcu,
};
pub use seqlock::{SeqLock, SeqLockWriteGuard};
pub use spin::{
    IrqRwLock, IrqRwLockReadGuard, IrqRwLockWriteGuard, PreemptMutex, PreemptMutexGuard,
};
pub use spin::{SpinLock, SpinLockGuard};
pub use wait_queue::{WaitQueue, WaitQueueBackend, WaitTaskHandle, register_wait_queue_backend};

// PCR-backed preempt guards re-exported here so the sync surface
// stays self-contained.
pub use crate::cpu::preempt::{
    IrqPreemptGuard, PreemptGuard, is_preemption_disabled, preempt_count_pcr as preempt_count,
    register_reschedule_callback,
};
