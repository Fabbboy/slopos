//! Wait queue primitive for blocking/waking kernel tasks.
//!
//! Provides a fixed-capacity queue of blocked tasks that can be woken
//! individually (`wake_one`) or all at once (`wake_all`). Integrates with
//! the scheduler through a one-shot-registered [`WaitQueueBackend`] trait
//! object — OSTD does not depend on the scheduler crate, so the kernel
//! installs a backend at boot that delegates to its task runtime.
//!
//! # Design
//!
//! - Fixed-capacity array of opaque task handles ([`WaitTaskHandle`])
//! - Protected by [`SpinLock`](super::spin::SpinLock) for interrupt-safe access
//! - Uses scheduler wait-gating (`prepare_to_wait`/`finish_wait`) with
//!   `block_current_task()` / `unblock_task()` from the registered backend
//!
//! # Wait/wake correctness contract (AUDIT 2C)
//!
//! Producers do **NOT** need to issue explicit memory fences between the
//! condition update (e.g. a data store, `exit_info.try_set`, a pipe-buffer
//! fill/drain, a socket `rx_ready` flag flip) and the call to
//! [`WaitQueue::wake_one`] / [`WaitQueue::wake_all`]. The internal
//! [`SpinLock`](super::spin::SpinLock) supplies the bidirectional full
//! barrier required by the wait/wake protocol:
//!
//! - The producer's release-on-unlock when `wake_*` drops the lock pairs
//!   with the consumer's acquire-on-lock when [`wait_event`](WaitQueue::wait_event)
//!   re-runs the condition closure under the same lock. Any stores made
//!   before `wake_*` (and before any prior unlock of an unrelated lock the
//!   producer also held) are visible to the closure.
//! - Symmetrically, the consumer's enqueue under the lock pairs with the
//!   producer's lock acquire in `wake_*`, so a producer that observes
//!   "no waiters" did so AFTER any condition update the consumer's
//!   pre-enqueue check could have observed.
//!
//! This is the same contract Linux relies on for `wq_head->lock` in
//! `prepare_to_wait_event` / `wake_up`. Subsystems that depend on this
//! property (and therefore do **not** need their own fences before
//! `wake_*`):
//!
//! - Pipes (`fs/src/pipe.rs`, `fs/src/pipe_file_ops.rs`) — `READER_WQS`
//!   / `WRITER_WQS`, woken on buffer-fill / drain / EOF.
//! - TTY (`drivers/src/tty.rs` and friends) — line-discipline read/write
//!   wait queues woken on input echo / drain.
//! - Sockets (`net/src/socket.rs`, `net/src/socket_file_ops.rs`,
//!   `net/src/unix_socket_file_ops.rs`) — accept / recv / send queues.
//! - Futex (`core/src/scheduler/futex.rs`) — bucket-locked, same pattern.
//! - Per-task `waiters` (`core/src/scheduler/task_struct.rs::Task::waiters`)
//!   — woken from `mark_task_terminated` after `exit_info.try_set`.
//!
//! Adding a fresh subsystem? You inherit this contract for free as long
//! as your producer goes condition-update -> `wake_*` and your consumer
//! uses `wait_event(|| condition_holds())`. Don't add a `compiler_fence`
//! or `atomic::fence` "just in case"; it is dead code.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use crate::sync::spin::SpinLock;

/// Opaque task identifier carried by the wait queue. The actual
/// representation is the kernel scheduler's task pointer.
pub type WaitTaskHandle = *mut c_void;

/// A null task handle sentinel.
const NULL_HANDLE: WaitTaskHandle = core::ptr::null_mut();

// ---------------------------------------------------------------------------
// WaitQueueBackend trait + one-shot registration.
// ---------------------------------------------------------------------------

/// Hooks the wait queue uses to talk to the kernel's task runtime.
///
/// Registered exactly once at boot via [`register_wait_queue_backend`].
/// Until registered, all blocking methods on [`WaitQueue`] return early
/// (treated as "runtime not initialised").
///
/// # Safety
///
/// Every method must be safe to call from any context the wait queue may
/// run in (task context for `wait_*`, IRQ context for `wake_*`,
/// timer-tick context for `current_task_handle`). Implementations must
/// not panic on null returns; the queue treats nulls as "no current task".
pub unsafe trait WaitQueueBackend: Send + Sync + 'static {
    /// True once the kernel's task runtime has been initialised.
    fn is_runtime_initialised(&self) -> bool;

    /// Opaque handle for the task currently running on this CPU, or
    /// null if there is no current task.
    fn current_task_handle(&self) -> WaitTaskHandle;

    /// Mark the current task as preparing to wait. Counterpart to
    /// [`finish_wait`](Self::finish_wait); together they close the
    /// "miss the wakeup" race window.
    fn prepare_to_wait(&self);

    /// Cancel an outstanding `prepare_to_wait` without sleeping.
    fn finish_wait(&self);

    /// Block the current task until something calls
    /// [`unblock_task`](Self::unblock_task) on its handle.
    fn block_current_task(&self);

    /// Block the current task with a millisecond-resolution timeout.
    fn block_current_task_with_timeout(&self, timeout_ms: u32);

    /// Wake a previously-blocked task. Returns 0 on success or a
    /// negative errno-shaped value on failure.
    ///
    /// # Safety
    ///
    /// `task` must have been obtained from [`current_task_handle`](Self::current_task_handle)
    /// and must still refer to a live task.
    unsafe fn unblock_task(&self, task: WaitTaskHandle) -> i32;

    /// Current monotonic time in milliseconds.
    fn get_time_ms(&self) -> u64;
}

// ---------------------------------------------------------------------------
// Default no-op backend used until the kernel registers a real one.
// ---------------------------------------------------------------------------

struct UnregisteredBackend;

// SAFETY: every method short-circuits without touching task state.
unsafe impl WaitQueueBackend for UnregisteredBackend {
    fn is_runtime_initialised(&self) -> bool {
        false
    }
    fn current_task_handle(&self) -> WaitTaskHandle {
        NULL_HANDLE
    }
    fn prepare_to_wait(&self) {}
    fn finish_wait(&self) {}
    fn block_current_task(&self) {}
    fn block_current_task_with_timeout(&self, _timeout_ms: u32) {}
    unsafe fn unblock_task(&self, _task: WaitTaskHandle) -> i32 {
        0
    }
    fn get_time_ms(&self) -> u64 {
        0
    }
}

static DEFAULT_BACKEND: UnregisteredBackend = UnregisteredBackend;

struct BackendSlot(UnsafeCell<MaybeUninit<&'static dyn WaitQueueBackend>>);
// SAFETY: writes are gated by `BACKEND_INSTALLED.swap(true, AcqRel)`
// (one-shot); subsequent reads only happen after observing the flag
// with Acquire, so the read sees the published reference.
unsafe impl Sync for BackendSlot {}

static BACKEND_SLOT: BackendSlot = BackendSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production wait-queue backend.
///
/// # Safety
///
/// `backend` must live for the static lifetime of the kernel. The caller
/// certifies that the backend's task-handle invariants hold (only valid
/// task pointers are produced; wakeups on stale handles are tolerated).
pub unsafe fn register_wait_queue_backend(backend: &'static dyn WaitQueueBackend) {
    let was_installed = BACKEND_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(!was_installed, "register_wait_queue_backend called twice");
    // SAFETY: the swap above transitioned us from "uninstalled" to
    // "installed" exclusively; no other writer can be racing.
    unsafe {
        (*BACKEND_SLOT.0.get()).write(backend);
    }
}

#[inline]
fn backend() -> &'static dyn WaitQueueBackend {
    if !BACKEND_INSTALLED.load(Ordering::Acquire) {
        return &DEFAULT_BACKEND;
    }
    // SAFETY: paired Release in `register_wait_queue_backend`.
    unsafe { *(*BACKEND_SLOT.0.get()).as_ptr() }
}

// ---------------------------------------------------------------------------
// WaitQueue.
// ---------------------------------------------------------------------------

/// Maximum number of tasks that can wait on a single `WaitQueue`.
const WAITQUEUE_CAPACITY: usize = 32;

struct WaitQueueInner {
    waiters: [WaitTaskHandle; WAITQUEUE_CAPACITY],
    count: usize,
}

impl WaitQueueInner {
    const fn new() -> Self {
        Self {
            waiters: [NULL_HANDLE; WAITQUEUE_CAPACITY],
            count: 0,
        }
    }

    fn enqueue(&mut self, task: WaitTaskHandle) -> bool {
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

    fn dequeue_one(&mut self) -> Option<WaitTaskHandle> {
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

    fn dequeue_all(&mut self, mut f: impl FnMut(WaitTaskHandle)) -> usize {
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

    fn remove_task(&mut self, task: WaitTaskHandle) -> bool {
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

// SAFETY: `WaitTaskHandle` is managed by the scheduler. Access is
// synchronized through the `SpinLock`.
unsafe impl Send for WaitQueueInner {}

/// A wait queue for blocking and waking kernel tasks.
pub struct WaitQueue {
    inner: SpinLock<WaitQueueInner>,
    /// Monotonic counter incremented on each wake.
    generation: AtomicU32,
}

// SAFETY: The WaitQueue is protected by SpinLock and only stores opaque
// scheduler-managed task handles.
unsafe impl Sync for WaitQueue {}
unsafe impl Send for WaitQueue {}

impl WaitQueue {
    /// Create a new empty wait queue.
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(WaitQueueInner::new(), LOCK_LEVEL_RESOURCE),
            generation: AtomicU32::new(0),
        }
    }

    /// Block the current task until `condition()` returns `true`.
    pub fn wait_event<F: Fn() -> bool>(&self, condition: F) -> bool {
        let bk = backend();
        loop {
            if condition() {
                return true;
            }

            if !bk.is_runtime_initialised() {
                return false;
            }

            let task = bk.current_task_handle();
            if task.is_null() {
                return false;
            }

            bk.prepare_to_wait();

            {
                let mut inner = self.inner.lock();
                if condition() {
                    inner.remove_task(task);
                    bk.finish_wait();
                    return true;
                }
                inner.remove_task(task);
                if !inner.enqueue(task) {
                    bk.finish_wait();
                    return false;
                }
            }

            bk.block_current_task();
            bk.finish_wait();
        }
    }

    /// Block the current task exactly once, without checking any condition.
    pub fn wait_once(&self) -> bool {
        let bk = backend();
        if !bk.is_runtime_initialised() {
            return false;
        }

        let task = bk.current_task_handle();
        if task.is_null() {
            return false;
        }

        bk.prepare_to_wait();

        {
            let mut inner = self.inner.lock();
            if !inner.enqueue(task) {
                bk.finish_wait();
                return false;
            }
        }

        bk.block_current_task();
        bk.finish_wait();

        {
            let mut inner = self.inner.lock();
            inner.remove_task(task);
        }

        true
    }

    /// Enqueue the current task on this wait queue without blocking.
    pub fn enqueue_current(&self) -> bool {
        let bk = backend();
        if !bk.is_runtime_initialised() {
            return false;
        }

        let task = bk.current_task_handle();
        if task.is_null() {
            return false;
        }

        let mut inner = self.inner.lock();
        inner.enqueue(task)
    }

    /// Remove the current task from this wait queue.
    pub fn remove_current(&self) {
        let bk = backend();
        let task = bk.current_task_handle();
        if task.is_null() {
            return;
        }
        let mut inner = self.inner.lock();
        inner.remove_task(task);
    }

    /// Block the current task until `condition()` returns `true` or
    /// `timeout_ms` milliseconds elapse.
    pub fn wait_event_timeout<F: Fn() -> bool>(&self, condition: F, timeout_ms: u64) -> bool {
        let bk = backend();
        if condition() {
            return true;
        }

        if !bk.is_runtime_initialised() {
            return false;
        }

        let task = bk.current_task_handle();
        if task.is_null() {
            return false;
        }

        let deadline_ms = bk.get_time_ms().saturating_add(timeout_ms);

        loop {
            let now = bk.get_time_ms();
            if now >= deadline_ms {
                let mut inner = self.inner.lock();
                inner.remove_task(task);
                return false;
            }

            bk.prepare_to_wait();

            {
                let mut inner = self.inner.lock();
                if condition() {
                    inner.remove_task(task);
                    bk.finish_wait();
                    return true;
                }
                inner.remove_task(task);
                if !inner.enqueue(task) {
                    bk.finish_wait();
                    return false;
                }
            }

            let remaining = deadline_ms.saturating_sub(bk.get_time_ms());
            if remaining == 0 {
                bk.finish_wait();
                let mut inner = self.inner.lock();
                inner.remove_task(task);
                return false;
            }
            let sleep_ms = remaining.min(500) as u32;
            bk.block_current_task_with_timeout(sleep_ms);
            bk.finish_wait();
        }
    }

    /// Wake one waiting task. Returns `true` if a task was woken.
    pub fn wake_one(&self) -> bool {
        let task = {
            let mut inner = self.inner.lock();
            inner.dequeue_one()
        };

        if let Some(task) = task {
            self.generation.fetch_add(1, Ordering::Relaxed);
            // SAFETY: handle came from `current_task_handle()`; backend
            // tolerates already-woken / stale handles.
            let _ = unsafe { backend().unblock_task(task) };
            true
        } else {
            false
        }
    }

    /// Wake all waiting tasks. Returns the number woken.
    pub fn wake_all(&self) -> usize {
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

        let bk = backend();
        for task in &tasks[..count] {
            // SAFETY: see `wake_one`.
            let _ = unsafe { bk.unblock_task(*task) };
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
    pub fn remove_task(&self, task: WaitTaskHandle) {
        let mut inner = self.inner.lock();
        inner.remove_task(task);
    }

    /// Get the wake generation counter (for debugging / testing).
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Test-only reset of the registration state.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn reset_backend_for_test() {
        BACKEND_INSTALLED.store(false, Ordering::Release);
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}
