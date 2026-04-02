//! Wait queue primitive for blocking/waking kernel tasks.
//!
//! Provides a fixed-capacity queue of blocked tasks that can be woken
//! individually (`wake_one`) or all at once (`wake_all`).  Integrates with
//! the scheduler through the `driver_runtime` kernel service — no direct
//! dependency on the `core` crate.
//!
//! # Design
//!
//! Modeled after the futex wait queue in `core/src/scheduler/futex.rs`:
//! - Fixed-capacity array of opaque task handles (`DriverTaskHandle`)
//! - Protected by `IrqMutex` for interrupt-safe access
//! - Uses scheduler wait-gating (`prepare_to_wait`/`finish_wait`) with
//!   `block_current_task()` / `unblock_task()` from the driver runtime
//!
//! # Usage
//!
//! ```rust,ignore
//! static MY_WQ: WaitQueue = WaitQueue::new();
//!
//! // Waiting side (consumer):
//! MY_WQ.wait_event(|| has_data());
//!
//! // Waking side (producer):
//! MY_WQ.wake_one();
//! ```

use core::sync::atomic::{AtomicU32, Ordering};

use crate::IrqMutex;
use slopos_kernel_services::driver_runtime::{
    self, block_current_task, current_task, finish_wait, prepare_to_wait, unblock_task,
    DriverTaskHandle,
};

/// Maximum number of tasks that can wait on a single `WaitQueue`.
const WAITQUEUE_CAPACITY: usize = 32;

/// A null task handle sentinel.
const NULL_HANDLE: DriverTaskHandle = core::ptr::null_mut();

/// Inner state of a wait queue, protected by `IrqMutex`.
struct WaitQueueInner {
    /// Waiting task handles.  Null entries are empty slots.
    waiters: [DriverTaskHandle; WAITQUEUE_CAPACITY],
    /// Number of active waiters.
    count: usize,
}

impl WaitQueueInner {
    const fn new() -> Self {
        Self {
            waiters: [NULL_HANDLE; WAITQUEUE_CAPACITY],
            count: 0,
        }
    }

    /// Add `task` to the queue.  Returns `true` on success, `false` if full.
    fn enqueue(&mut self, task: DriverTaskHandle) -> bool {
        if task.is_null() {
            return false;
        }
        for slot in self.waiters.iter_mut() {
            if slot.is_null() {
                *slot = task;
                self.count += 1;
                return true;
            }
        }
        false
    }

    /// Remove and return the first waiting task, or `None`.
    fn dequeue_one(&mut self) -> Option<DriverTaskHandle> {
        for slot in self.waiters.iter_mut() {
            if !slot.is_null() {
                let task = *slot;
                *slot = NULL_HANDLE;
                self.count = self.count.saturating_sub(1);
                return Some(task);
            }
        }
        None
    }

    /// Remove all waiting tasks, returning the count.  Calls `f` for each.
    fn dequeue_all(&mut self, mut f: impl FnMut(DriverTaskHandle)) -> usize {
        let mut woken = 0;
        for slot in self.waiters.iter_mut() {
            if !slot.is_null() {
                let task = *slot;
                *slot = NULL_HANDLE;
                f(task);
                woken += 1;
            }
        }
        self.count = 0;
        woken
    }

    /// Remove a specific task from the queue (e.g. on timeout or cancel).
    fn remove_task(&mut self, task: DriverTaskHandle) -> bool {
        for slot in self.waiters.iter_mut() {
            if *slot == task {
                *slot = NULL_HANDLE;
                self.count = self.count.saturating_sub(1);
                return true;
            }
        }
        false
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }
}

// SAFETY: `DriverTaskHandle` (`*mut c_void`) is managed by the scheduler.
// Access is synchronized through the `IrqMutex`.
unsafe impl Send for WaitQueueInner {}

/// A wait queue for blocking and waking kernel tasks.
///
/// Tasks call [`wait_event`] to sleep until a condition is met.
/// Producers call [`wake_one`] or [`wake_all`] when the condition changes.
///
/// This is the fundamental building block for blocking socket syscalls,
/// pipe reads, and any other blocking I/O operation.
pub struct WaitQueue {
    inner: IrqMutex<WaitQueueInner>,
    /// Monotonic counter incremented on each wake, used for spurious-wakeup
    /// detection and debugging.
    generation: AtomicU32,
}

// SAFETY: The WaitQueue is protected by IrqMutex and only stores opaque
// scheduler-managed task handles.
unsafe impl Sync for WaitQueue {}
unsafe impl Send for WaitQueue {}

impl WaitQueue {
    /// Create a new empty wait queue.
    pub const fn new() -> Self {
        Self {
            inner: IrqMutex::new(WaitQueueInner::new()),
            generation: AtomicU32::new(0),
        }
    }

    /// Block the current task until `condition()` returns `true`.
    ///
    /// The condition is checked under the wait queue lock before sleeping.
    /// If the condition is already true, returns immediately without blocking.
    ///
    /// Returns `true` if the condition was met, `false` if the wait queue
    /// was full (could not enqueue — caller should retry or return EAGAIN).
    ///
    pub fn wait_event<F: Fn() -> bool>(&self, condition: F) -> bool {
        loop {
            // Check condition first — fast path.
            if condition() {
                return true;
            }

            // Ensure the runtime is initialized before blocking.
            if !driver_runtime::is_driver_runtime_initialized() {
                return false;
            }

            let task = current_task();
            if task.is_null() {
                return false;
            }

            prepare_to_wait();

            {
                let mut inner = self.inner.lock();
                // Re-check condition under lock to close the race window.
                if condition() {
                    finish_wait();
                    return true;
                }
                if !inner.enqueue(task) {
                    // Queue full — cannot wait.
                    finish_wait();
                    return false;
                }
            }

            block_current_task();
            finish_wait();

            // We were woken up (or spurious wakeup).  Re-check condition
            // at the top of the loop.
        }
    }

    /// Block the current task exactly once, without checking any condition.
    ///
    /// Unlike `wait_event`, this does **not** loop or re-check a predicate.
    /// The caller is enqueued, blocked, and then removed from the queue on
    /// wakeup.  This is intended for poll/select-style sleep where the caller
    /// manages its own retry loop and timeout.
    ///
    /// Returns `true` if the task was successfully enqueued and blocked,
    /// `false` if the runtime is not initialized, the task handle is null,
    /// or the queue is full.
    pub fn wait_once(&self) -> bool {
        if !driver_runtime::is_driver_runtime_initialized() {
            return false;
        }

        let task = current_task();
        if task.is_null() {
            return false;
        }

        prepare_to_wait();

        {
            let mut inner = self.inner.lock();
            if !inner.enqueue(task) {
                finish_wait();
                return false;
            }
        }

        block_current_task();
        finish_wait();

        // Remove ourselves in case of spurious wakeup or if wake_all
        // already dequeued us (remove_task is a no-op in that case).
        {
            let mut inner = self.inner.lock();
            inner.remove_task(task);
        }

        true
    }

    /// Enqueue the current task on this wait queue without blocking.
    ///
    /// Used for multi-queue poll registration: the caller enqueues on several
    /// wait queues, then calls `block_current_task()` once.  On wakeup, the
    /// caller must call [`remove_current`] on every queue it registered on.
    ///
    /// Returns `true` if the task was successfully enqueued, `false` if the
    /// runtime is not initialized, the task handle is null, or the queue is full.
    pub fn enqueue_current(&self) -> bool {
        if !driver_runtime::is_driver_runtime_initialized() {
            return false;
        }

        let task = current_task();
        if task.is_null() {
            return false;
        }

        let mut inner = self.inner.lock();
        inner.enqueue(task)
    }

    /// Remove the current task from this wait queue.
    ///
    /// Counterpart to [`enqueue_current`].  Safe to call even if the task
    /// was already removed by a `wake_one` / `wake_all` call (no-op in that
    /// case).
    pub fn remove_current(&self) {
        let task = current_task();
        if task.is_null() {
            return;
        }
        let mut inner = self.inner.lock();
        inner.remove_task(task);
    }

    /// Block the current task until `condition()` returns `true` or
    /// `timeout_ms` milliseconds elapse.
    ///
    /// Returns `true` if the condition was met, `false` on timeout or error.
    pub fn wait_event_timeout<F: Fn() -> bool>(&self, condition: F, timeout_ms: u64) -> bool {
        use slopos_kernel_services::platform;

        if condition() {
            return true;
        }

        if !driver_runtime::is_driver_runtime_initialized() {
            return false;
        }

        let task = current_task();
        if task.is_null() {
            return false;
        }

        let deadline_ms = platform::get_time_ms().saturating_add(timeout_ms);

        loop {
            let now = platform::get_time_ms();
            if now >= deadline_ms {
                let mut inner = self.inner.lock();
                inner.remove_task(task);
                return false;
            }

            prepare_to_wait();

            {
                let mut inner = self.inner.lock();
                if condition() {
                    inner.remove_task(task);
                    finish_wait();
                    return true;
                }
                // Remove any stale entry from a prior iteration before
                // re-enqueuing.  A timeout wakeup does not dequeue us,
                // so without this we'd leak duplicate entries.
                inner.remove_task(task);
                if !inner.enqueue(task) {
                    finish_wait();
                    return false;
                }
            }

            let remaining = deadline_ms.saturating_sub(platform::get_time_ms());
            if remaining == 0 {
                finish_wait();
                let mut inner = self.inner.lock();
                inner.remove_task(task);
                return false;
            }
            let sleep_ms = remaining.min(500) as u32;
            driver_runtime::block_current_task_with_timeout(sleep_ms);
            finish_wait();
        }
    }

    /// Wake one waiting task.
    ///
    /// Returns `true` if a task was woken, `false` if the queue was empty.
    pub fn wake_one(&self) -> bool {
        let task = {
            let mut inner = self.inner.lock();
            inner.dequeue_one()
        };

        if let Some(task) = task {
            self.generation.fetch_add(1, Ordering::Relaxed);
            let _ = unblock_task(task);
            true
        } else {
            false
        }
    }

    /// Wake all waiting tasks.
    ///
    /// Returns the number of tasks woken.
    pub fn wake_all(&self) -> usize {
        // Collect tasks under the lock, then unblock outside the lock
        // to avoid holding the wait queue lock while the scheduler does
        // its work.
        let mut tasks = [NULL_HANDLE; WAITQUEUE_CAPACITY];
        let count = {
            let mut inner = self.inner.lock();
            let mut i = 0;
            inner.dequeue_all(|t| {
                if i < tasks.len() {
                    tasks[i] = t;
                    i += 1;
                }
            })
        };

        if count > 0 {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }

        for task in &tasks[..count] {
            let _ = unblock_task(*task);
        }
        count
    }

    /// Check if there are any waiters.
    pub fn has_waiters(&self) -> bool {
        !self.inner.lock().is_empty()
    }

    /// Get the number of waiting tasks.
    pub fn waiter_count(&self) -> usize {
        self.inner.lock().count
    }

    /// Remove a specific task from the wait queue.
    ///
    /// Used when a task is terminated while waiting, to prevent dangling
    /// handles.
    pub fn remove_task(&self, task: DriverTaskHandle) {
        let mut inner = self.inner.lock();
        inner.remove_task(task);
    }

    /// Get the wake generation counter (for debugging / testing).
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }
}
