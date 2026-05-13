//! Futex (fast userspace mutex) wait queue implementation.
//!
//! Provides FUTEX_WAIT and FUTEX_WAKE operations for userspace synchronization
//! primitives (mutexes, condition variables, thread join via CLONE_CHILD_CLEARTID).
//!
//! The implementation uses a fixed-size hash table of wait queue buckets,
//! keyed by the physical address of the futex word. Each bucket holds a
//! small fixed-capacity list of waiting tasks.

use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::task::BlockReason;
use slopos_ostd::sync::{KernelSync, LOCK_LEVEL_RESOURCE, SpinLock};

use super::scheduler::{
    mark_current_blocked, scheduler_get_current_task, unblock_task, yield_blocked_task,
};
use super::task_struct::Task;

/// Number of hash buckets. Must be a power of two.
const FUTEX_HASH_BUCKETS: usize = 64;

/// Maximum number of waiters per bucket.
const FUTEX_MAX_WAITERS_PER_BUCKET: usize = 16;

/// A single waiter entry in a futex bucket.
#[derive(Clone, Copy)]
struct FutexWaiter {
    /// Physical address of the futex word (used as the key).
    futex_addr: u64,
    /// Pointer to the blocked task.
    task: KernelSync<*mut Task>,
}

impl FutexWaiter {
    const fn empty() -> Self {
        Self {
            futex_addr: 0,
            task: KernelSync::new(ptr::null_mut()),
        }
    }

    fn is_empty(&self) -> bool {
        self.task.is_null()
    }
}

struct FutexBucket {
    waiters: [FutexWaiter; FUTEX_MAX_WAITERS_PER_BUCKET],
    count: usize,
}

impl FutexBucket {
    const fn new() -> Self {
        Self {
            waiters: [FutexWaiter::empty(); FUTEX_MAX_WAITERS_PER_BUCKET],
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

    let current = scheduler_get_current_task();
    if current.is_null() {
        return slopos_abi::syscall::ERRNO_EAGAIN as i64;
    }

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

        // Read the current value at the futex address.
        // SAFETY: The syscall handler has validated that uaddr is a valid,
        // mapped, 4-byte-aligned user address in the current process.
        let current_val =
            unsafe { ptr::read_volatile(uaddr as *const AtomicU32) }.load(Ordering::SeqCst);

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

        bucket.waiters[idx] = FutexWaiter {
            futex_addr: uaddr,
            task: KernelSync::new(current),
        };
        bucket.count += 1;

        // Stamp the block reason before flipping status so any tracer
        // (or future signal-aware unblock path) sees the committed
        // reason at the same moment the status flips.
        // SAFETY: `current` came from `scheduler_get_current_task`; the
        // store is a relaxed atomic on the fused state word.
        unsafe {
            (*current).store_block_reason(BlockReason::FutexWait);
        }

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
        let waiter = &mut bucket.waiters[i];
        if !waiter.is_empty() && waiter.futex_addr == uaddr {
            let task = *waiter.task;
            *waiter = FutexWaiter::empty();
            bucket.count = bucket.count.saturating_sub(1);

            // Release the bucket lock before unblocking to avoid
            // potential lock ordering issues with the scheduler.
            // Actually, we need to keep the lock to avoid races with
            // concurrent FUTEX_WAIT adding to the same bucket.
            // unblock_task handles its own locking internally.
            let _ = unblock_task(task);
            woken += 1;
        }
    }

    drop(bucket);
    woken as i64
}

/// Remove a specific task from all futex wait queues.
///
/// Called when a task is terminated or exits abnormally while
/// blocked on a futex. This prevents dangling pointers in the
/// wait queue.
pub fn futex_remove_task(task: *mut Task) {
    if task.is_null() {
        return;
    }

    for bucket_mutex in FUTEX_TABLE.iter() {
        let mut bucket = bucket_mutex.lock();
        let mut removed = 0usize;
        for waiter in bucket.waiters.iter_mut() {
            if !waiter.is_empty() && *waiter.task == task {
                *waiter = FutexWaiter::empty();
                removed += 1;
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
