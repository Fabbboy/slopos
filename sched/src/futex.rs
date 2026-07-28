//! Futex (fast userspace mutex) wait queue implementation.
//!
//! Provides FUTEX_WAIT and FUTEX_WAKE operations for userspace synchronization
//! primitives (mutexes, condition variables, thread join via CLONE_CHILD_CLEARTID).
//!
//! The implementation uses a fixed-size hash table of wait queue buckets,
//! keyed by the physical address of the futex word. Each bucket holds a
//! small fixed-capacity list of waiting tasks.

use core::ptr::NonNull;

use slopos_abi::task::BlockReason;
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::sync::{KernelSync, LOCK_LEVEL_RESOURCE, SpinLock};

use super::scheduler::{mark_current_blocked, unblock_task, yield_blocked_task};
use super::task::{INVALID_TASK_ID, TaskRef, task_put};

/// Number of hash buckets. Must be a power of two.
const FUTEX_HASH_BUCKETS: usize = 64;

/// Maximum number of waiters per bucket.
const FUTEX_MAX_WAITERS_PER_BUCKET: usize = 16;

/// A single waiter entry in a futex bucket.
///
/// The bucket owns one strong reference to each blocked waiter (`task`), so a
/// waiter cannot be freed out from under the queue. `task_id` identifies the
/// waiter for teardown removal without dereferencing the handle.
struct FutexWaiter {
    /// Physical address of the futex word (used as the key).
    futex_addr: u64,
    /// Owning reference to the blocked task, or `None` for a free slot. The
    /// `KernelSync` wrapper asserts the cross-CPU access to the raw pointers
    /// inside `Task` is serialised by the bucket lock.
    task: KernelSync<Option<TaskRef>>,
    /// The waiter's task id, or `INVALID_TASK_ID` for a free slot.
    task_id: u32,
}

impl FutexWaiter {
    const fn empty() -> Self {
        Self {
            futex_addr: 0,
            task: KernelSync::new(None),
            task_id: INVALID_TASK_ID,
        }
    }

    fn is_empty(&self) -> bool {
        self.task.is_none()
    }
}

struct FutexBucket {
    waiters: [FutexWaiter; FUTEX_MAX_WAITERS_PER_BUCKET],
    count: usize,
}

impl FutexBucket {
    const fn new() -> Self {
        Self {
            waiters: [const { FutexWaiter::empty() }; FUTEX_MAX_WAITERS_PER_BUCKET],
            count: 0,
        }
    }
}

// Wrap each bucket in an SpinLock for interrupt-safe locking.
static FUTEX_TABLE: [SpinLock<FutexBucket>; FUTEX_HASH_BUCKETS] = {
    // const-init all buckets
    const BUCKET: SpinLock<FutexBucket> = SpinLock::new(FutexBucket::new(), LOCK_LEVEL_RESOURCE);
    [BUCKET; FUTEX_HASH_BUCKETS]
};

/// Hash a futex address to a bucket index.
#[inline]
fn futex_hash(addr: u64) -> usize {
    // Mix with a prime to spread sequential addresses across buckets.
    // Shift right by 2 since futex words are 4-byte aligned.
    let h = (addr >> 2).wrapping_mul(0x9E3779B97F4A7C15);
    (h as usize) & (FUTEX_HASH_BUCKETS - 1)
}

// AUDIT 2B: futex wait/wake protocol — bucket SpinLock IS the barrier.
//
// The per-bucket `SpinLock<FutexBucket>` (FUTEX_TABLE: 64 buckets) covers
// the entire publish-and-condition-check window: under the same lock we
// (a) read `*uaddr`, (b) compare against `expected`, and (c) enqueue the
// waiter. FUTEX_WAKE takes the same bucket lock to dequeue. The lock-pair
// gives a bidirectional full barrier identical to Linux's `wq_head->lock`
// pattern in `wake_q_add` / `prepare_to_wait_event`.
/// FUTEX_WAIT: atomically check that `*uaddr == expected` and block the
/// calling task on the futex queue keyed by `uaddr`.
///
/// Returns:
///  *  0 on success (was woken by FUTEX_WAKE)
///  * -EAGAIN if `*uaddr != expected` at time of check
///  * -ENOMEM if the wait queue bucket is full
///
/// `uaddr` must be a user-space virtual address of a u32 aligned to 4 bytes.
/// The caller (syscall handler) is responsible for validating the pointer.
///
/// The timeout parameter is currently accepted but not enforced (always waits
/// indefinitely). This matches the rollback plan in the task description.
pub fn futex_wait(uaddr: u64, expected: u32, _timeout_ms: u64) -> i64 {
    let bucket_idx = futex_hash(uaddr);

    // The running task, as a borrow: the bucket needs its identity and its
    // block reason, and both come off the guard the syscall path already has.
    let Some(current_guard) = crate::task_struct::Current::get() else {
        return slopos_abi::syscall::ERRNO_EAGAIN as i64;
    };
    let current = current_guard.as_ptr();

    // The bucket SpinLock plays the same role as a WaitQueue's internal
    // SpinLock in the harmonic-cascade wait protocol: under the lock we
    // (a) read+compare *uaddr, (b) enqueue the waiter, AND
    // (c) CAS Running→Blocked. FUTEX_WAKE takes the same lock to dequeue
    // and CAS Blocked→Ready. Doing the consumer's state CAS *under* the
    // lock is what closes the lost-wakeup window: a producer that
    // observes our waiter on the bucket also necessarily observes
    // status=Blocked, and its `unblock_task` Blocked→Ready CAS succeeds.
    let blocked = {
        let mut bucket = FUTEX_TABLE[bucket_idx].lock();

        // Read the futex word through the SMAP-safe, fault-recoverable
        // user-copy path. A raw kernel load of the user page faults under
        // CR4.SMAP (no STAC window); `copy_from_user` opens the kernel's sole
        // AC window around the read. Under the bucket lock + IRQs-off a single
        // aligned 4-byte copy is equivalent to an atomic load for the compare.
        let current_val = match UserPtr::<u32>::try_new(uaddr)
            .ok()
            .and_then(|p| slopos_mm::user_copy::copy_from_user::<u32>(p).ok())
        {
            Some(v) => v,
            None => return slopos_abi::syscall::ERRNO_EFAULT as i64,
        };

        if current_val != expected {
            return slopos_abi::syscall::ERRNO_EAGAIN as i64;
        }

        // Find a free slot in the bucket.
        let mut slot_idx = None;
        for i in 0..FUTEX_MAX_WAITERS_PER_BUCKET {
            if bucket.waiters[i].is_empty() {
                slot_idx = Some(i);
                break;
            }
        }

        let Some(idx) = slot_idx else {
            return slopos_abi::syscall::ERRNO_ENOMEM as i64;
        };

        // The bucket owns a strong reference to the blocked waiter. `current`
        // is the running task, kept alive by its dispatch reference, so cloning
        // its handle here is sound.
        let Some(node) = NonNull::new(current) else {
            return slopos_abi::syscall::ERRNO_EAGAIN as i64;
        };
        bucket.waiters[idx] = FutexWaiter {
            futex_addr: uaddr,
            task: KernelSync::new(Some(TaskRef::clone_of(node))),
            task_id: current_guard.id(),
        };
        bucket.count += 1;

        // Stamp the block reason before flipping status so any tracer
        // (or future signal-aware unblock path) sees the committed
        // reason at the same moment the status flips.
        current_guard
            .task()
            .store_block_reason(BlockReason::FutexWait);

        mark_current_blocked()
    };
    // Bucket lock is dropped here.

    // Yield only if we successfully transitioned Running→Blocked. If the
    // CAS failed (e.g. a wake-side path got there first via some other
    // route), don't yield — the wakeup is already preserved as the
    // current Running/Ready status.
    if blocked {
        yield_blocked_task();
    }

    0
}

/// FUTEX_WAKE: wake up to `max_wake` tasks waiting on the futex at `uaddr`.
///
/// Returns the number of tasks actually woken.
pub fn futex_wake(uaddr: u64, max_wake: u32) -> i64 {
    let bucket_idx = futex_hash(uaddr);
    let mut woken = 0u32;

    let mut bucket = FUTEX_TABLE[bucket_idx].lock();

    for i in 0..FUTEX_MAX_WAITERS_PER_BUCKET {
        if woken >= max_wake {
            break;
        }
        if bucket.waiters[i].is_empty() || bucket.waiters[i].futex_addr != uaddr {
            continue;
        }
        // Take the waiter's owning reference out of the slot. The bucket lock
        // is held across the unblock so a concurrent FUTEX_WAIT reusing this
        // freed slot races only against a different task; the woken task stays
        // alive because we still hold `arc`. `unblock_task`'s enqueue path
        // clones its own membership reference (bucket lock RESOURCE → run-queue
        // lock, no cycle), then we drop the waiter's guard — never the last
        // reference, since the task holds its own existence reference until it
        // is reaped.
        let taken = core::mem::replace(&mut bucket.waiters[i], FutexWaiter::empty());
        bucket.count = bucket.count.saturating_sub(1);
        if let Some(waiter) = taken.task.into_inner() {
            let _ = unblock_task(&waiter);
            task_put(waiter);
        }
        woken += 1;
    }

    drop(bucket);
    woken as i64
}

/// Remove a specific task from all futex wait queues.
///
/// Called when a task is terminated or exits abnormally while
/// blocked on a futex. This prevents dangling pointers in the
/// wait queue.
///
/// Keyed by id, not by address: the buckets store ids, so an id is the only
/// thing this needs to match against them.
pub fn futex_remove_task(target_id: u32) {
    for bucket_mutex in FUTEX_TABLE.iter() {
        let mut bucket = bucket_mutex.lock();
        let mut removed = 0usize;
        for waiter in bucket.waiters.iter_mut() {
            if waiter.is_empty() || waiter.task_id != target_id {
                continue;
            }
            let taken = core::mem::replace(waiter, FutexWaiter::empty());
            removed += 1;
            // Drop the bucket's owning reference. Never the last one — the
            // task's own existence reference outlives it — so this is a bare decrement under
            // the bucket lock, and any retirement it triggers self-defers.
            if let Some(waiter) = taken.task.into_inner() {
                task_put(waiter);
            }
        }
        bucket.count = bucket.count.saturating_sub(removed);
    }
}

/// Wake one waiter on the given futex address.
///
/// Convenience function used by the thread-exit path for
/// CLONE_CHILD_CLEARTID: the kernel writes 0 to the TID address
/// and then wakes one waiter so pthread_join can complete.
pub fn futex_wake_one(uaddr: u64) -> i64 {
    futex_wake(uaddr, 1)
}
