//! Futex (fast userspace mutex) wait queues: FUTEX_WAIT and FUTEX_WAKE over a
//! fixed-size hash table of buckets keyed by the futex word's address, each
//! holding an unbounded intrusive list of waiting tasks.
//!
//! The list link lives in the task itself, so enqueue allocates nothing and a
//! bucket has no capacity to exhaust. A fixed-capacity bucket would have to
//! refuse the surplus waiter, and every userland futex wrapper discards that
//! error and retries — turning a blocked waiter into a full-core busy-spin.

use core::ptr::NonNull;
use core::sync::atomic::Ordering;
use slopos_ostd::lock_class;

use slopos_abi::task::BlockReason;
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::sync::{IntrusiveDList, KernelSync, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::task::{FutexRole, placement};

use super::scheduler::{
    mark_current_blocked, set_current_runnable, unblock_task, yield_blocked_task,
    yield_blocked_task_with_timeout,
};
use super::task::{TaskRef, task_put};

/// Must be a power of two.
const FUTEX_HASH_BUCKETS: usize = 64;

/// Membership parks one strong reference per waiter, so a waiter cannot be
/// freed out from under the bucket ("linked implies owned").
///
/// The `KernelSync` asserts that cross-CPU access to the raw pointers inside
/// `Task` is serialised by the bucket lock.
type FutexBucket = KernelSync<IntrusiveDList<crate::task_struct::Task, FutexRole>>;

static FUTEX_TABLE: [SpinLock<FutexBucket>; FUTEX_HASH_BUCKETS] = {
    const BUCKET: SpinLock<FutexBucket> = SpinLock::new(
        KernelSync::new(IntrusiveDList::new()),
        lock_class!("FUTEX_TABLE", LOCK_LEVEL_RESOURCE),
    );
    [BUCKET; FUTEX_HASH_BUCKETS]
};

/// Unlink `task` from `bucket`, releasing the reference membership parked.
/// The caller holds the bucket's lock. `false` when it was not a member.
fn unlink_waiter(bucket: &FutexBucket, task: NonNull<crate::task_struct::Task>) -> bool {
    if bucket.remove(task).is_err() {
        return false;
    }
    // Membership parked exactly one reference in `futex_wait`; the unlink
    // above is what consumes it.
    task_put(TaskRef::from_placement(task));
    true
}

/// The futex word a parked waiter is waiting on.
fn parked_futex_addr(node: NonNull<crate::task_struct::Task>) -> u64 {
    placement::with_parked_node(node, |task| task.futex_addr.load(Ordering::Relaxed))
}

#[inline]
fn futex_hash(addr: u64) -> usize {
    // Shifted by 2 because futex words are 4-byte aligned; the prime multiply
    // spreads sequential addresses across buckets.
    let h = (addr >> 2).wrapping_mul(0x9E3779B97F4A7C15);
    (h as usize) & (FUTEX_HASH_BUCKETS - 1)
}

/// FUTEX_WAIT: atomically check that `*uaddr == expected` and block the
/// calling task on the futex queue keyed by `uaddr`.
///
/// `timeout_ms` of `None` waits indefinitely. The deadline is absolute and
/// derived once, so a wait that loops over a spurious wake cannot re-arm its
/// own budget.
///
/// Returns:
///  *  0 on success (woken by FUTEX_WAKE)
///  * -EAGAIN if `*uaddr != expected` at the time of the check
///  * -ENOMEM if the bucket is full
///  * -EINTR if the caller was marked for death or has a signal to act on
///  * -ETIMEDOUT if the deadline elapsed
///
/// `uaddr` must be a user-space virtual address of a u32 aligned to 4 bytes.
/// The caller (syscall handler) is responsible for validating the pointer.
pub fn futex_wait(uaddr: u64, expected: u32, timeout_ms: Option<u64>) -> i64 {
    let bucket_idx = futex_hash(uaddr);
    let deadline_ms =
        timeout_ms.map(|ms| slopos_kernel_services::platform::get_time_ms().saturating_add(ms));

    let Some(current_guard) = crate::task_struct::Current::get() else {
        return slopos_abi::syscall::ERRNO_EAGAIN as i64;
    };
    let current = current_guard.as_ptr();
    let my_id = current_guard.id();

    loop {
        // The bucket lock covers the read+compare of `*uaddr`, the enqueue and
        // the Running→Blocked CAS; FUTEX_WAKE takes the same lock to dequeue.
        // Doing the CAS under the lock is what closes the lost-wakeup window: a
        // waker that observes our waiter necessarily observes Blocked too.
        let blocked = {
            let bucket = FUTEX_TABLE[bucket_idx].lock();

            // `copy_from_user`, not a raw load: it opens the kernel's sole AC
            // window, without which the access faults under CR4.SMAP. Under the
            // bucket lock with IRQs off one aligned 4-byte copy is equivalent
            // to an atomic load for the compare.
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

            // `current` is the running task, kept alive by its dispatch
            // reference, so parking a reference here is sound.
            let Some(node) = NonNull::new(current) else {
                return slopos_abi::syscall::ERRNO_EAGAIN as i64;
            };
            current_guard
                .task()
                .futex_addr
                .store(uaddr, Ordering::Relaxed);
            // Retain before link: membership must never name a task the
            // bucket does not hold a reference to.
            placement::task_placement_retain(node);
            if bucket.push_back(node).is_err() {
                task_put(TaskRef::from_placement(node));
                return slopos_abi::syscall::ERRNO_EAGAIN as i64;
            }

            // Stamped before the status flip, so a reader never observes
            // Blocked without the reason that goes with it.
            current_guard
                .task()
                .store_block_reason(BlockReason::FutexWait);

            mark_current_blocked()
        };

        // A failed Running→Blocked CAS means a wake got there first, and that
        // wakeup is already preserved in the current Running/Ready status.
        if blocked {
            match deadline_ms {
                None => yield_blocked_task(),
                Some(deadline) => {
                    let now = slopos_kernel_services::platform::get_time_ms();
                    if now >= deadline {
                        set_current_runnable();
                    } else {
                        let remaining = deadline.saturating_sub(now).min(u32::MAX as u64) as u32;
                        yield_blocked_task_with_timeout(remaining);
                    }
                }
            }
        }

        // FUTEX_WAKE unlinks us when it is the one that woke us, so still
        // being linked means a signal, a kill or the deadline did — and
        // leaving the link would strand the bucket's strong reference.
        if !futex_remove_self(bucket_idx, uaddr, my_id) {
            return 0;
        }

        if current_guard.task().is_killed()
            || crate::task::task_has_deliverable_signal(current_guard.task())
        {
            return slopos_abi::syscall::ERRNO_EINTR as i64;
        }
        if deadline_ms.is_some_and(|d| slopos_kernel_services::platform::get_time_ms() >= d) {
            return slopos_abi::syscall::ERRNO_ETIMEDOUT as i64;
        }
    }
}

/// Remove this task's entry for `uaddr` from `bucket_idx`, reporting whether
/// one was found. `false` means a `futex_wake` had already claimed it.
fn futex_remove_self(bucket_idx: usize, uaddr: u64, _task_id: u32) -> bool {
    let Some(current_guard) = crate::task_struct::Current::get() else {
        return false;
    };
    let Some(node) = NonNull::new(current_guard.as_ptr()) else {
        return false;
    };
    let bucket = FUTEX_TABLE[bucket_idx].lock();
    // The link slot admits one membership, so the task is in at most one
    // bucket; the address check keeps a caller naming a different futex from
    // unlinking a wait it does not own.
    if parked_futex_addr(node) != uaddr {
        return false;
    }
    unlink_waiter(&bucket, node)
}

/// FUTEX_WAKE: wake up to `max_wake` tasks waiting on the futex at `uaddr`.
///
/// Returns the number of tasks actually woken.
pub fn futex_wake(uaddr: u64, max_wake: u32) -> i64 {
    let bucket_idx = futex_hash(uaddr);
    let mut woken = 0u32;

    let bucket = FUTEX_TABLE[bucket_idx].lock();

    // One bucket serves every address that hashes to it, so the walk skips
    // waiters parked on a different word.
    while woken < max_wake {
        let Some(node) = bucket.iter().find(|&n| parked_futex_addr(n) == uaddr) else {
            break;
        };

        // The bucket lock is held across the unblock. Lock order is bucket
        // (RESOURCE) → run queue, no cycle. The reference released below is
        // never the last: the task holds its own until reap.
        if bucket.remove(node).is_err() {
            break;
        }
        let owned = TaskRef::from_placement(node);
        let _ = unblock_task(&owned);
        task_put(owned);
        woken += 1;
    }

    drop(bucket);
    woken as i64
}

/// Wake one waiter on the given futex address.
///
/// Used by the CLONE_CHILD_CLEARTID thread-exit path after the kernel writes 0
/// to the TID address, so `pthread_join` can complete.
pub fn futex_wake_one(uaddr: u64) -> i64 {
    futex_wake(uaddr, 1)
}

#[cfg(feature = "test-hooks")]
pub fn futex_waiters_for_test(uaddr: u64) -> usize {
    let bucket = FUTEX_TABLE[futex_hash(uaddr)].lock();
    bucket
        .iter()
        .filter(|&node| parked_futex_addr(node) == uaddr)
        .count()
}

/// Queue the current task for `uaddr` without blocking it, so a test can
/// observe the dequeue side without also having to be descheduled.
#[cfg(feature = "test-hooks")]
pub fn futex_park_for_test(uaddr: u64) -> bool {
    let Some(current_guard) = crate::task_struct::Current::get() else {
        return false;
    };
    let Some(node) = NonNull::new(current_guard.as_ptr()) else {
        return false;
    };
    let bucket = FUTEX_TABLE[futex_hash(uaddr)].lock();
    current_guard
        .task()
        .futex_addr
        .store(uaddr, Ordering::Relaxed);
    placement::task_placement_retain(node);
    if bucket.push_back(node).is_err() {
        task_put(TaskRef::from_placement(node));
        return false;
    }
    true
}

/// See [`futex_remove_self`].
#[cfg(feature = "test-hooks")]
pub fn futex_remove_self_for_test(uaddr: u64, task_id: u32) -> bool {
    futex_remove_self(futex_hash(uaddr), uaddr, task_id)
}
