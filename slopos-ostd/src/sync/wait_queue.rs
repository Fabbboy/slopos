//! Wait queue primitive for blocking/waking kernel tasks.
//!
//! See `docs/scheduler/wait_protocol.md` for the full wait/wake/block
//! protocol — the lock-pair full-barrier proof, the cookbook for adding
//! a new wait subsystem, and the migration outlook to async.
//!
//! Provides an unbounded intrusive linked list of [`WaitNode`]s; each
//! waiter contributes its own node and the list links them together
//! without per-queue capacity limits. The queue talks to the kernel's
//! task runtime through a one-shot-registered [`WaitQueueBackend`]
//! trait object — OSTD does not depend on the scheduler crate, so the
//! kernel installs a backend at boot that delegates to its task
//! runtime.
//!
//! # Design
//!
//! - Intrusive linked list of [`WaitNode`]s ([`super::intrusive::IntrusiveLinkedList`]).
//! - Protected by [`SpinLock`](super::spin::SpinLock) for interrupt-safe access.
//! - Two flavours of node co-exist in the same list:
//!   - **Stack-pinned**: created by [`WaitQueue::wait_event`] and friends
//!     via [`core::pin::pin!`]. The waiter's stack frame owns the
//!     allocation; the queue never frees it.
//!   - **Heap-owned**: created by [`WaitQueue::enqueue_current`] and
//!     friends via `KBox<WaitNode>`. Whichever side dequeues the node
//!     (the wake side or [`WaitQueue::remove_current`]) reclaims the
//!     `KBox` and drops it.
//! - Calls `block_current_task()` (Running→Blocked CAS + yield) and
//!   `unblock_task()` (Blocked→Ready CAS) on the registered backend.
//!   Wakeup-loss is prevented by serialising the block and the wake
//!   through the queue's own SpinLock — see the contract block below.
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
//! `prepare_to_wait_event` / `wake_up` — the equivalent of Linux's
//! `prepare_to_wait_event` is, here, the act of pushing the
//! [`WaitNode`] under [`SpinLock`](super::spin::SpinLock) inside
//! [`wait_event`](WaitQueue::wait_event)'s loop body before the
//! backend's `block_current_task` call. Subsystems that depend on
//! this property (and therefore do **not** need their own fences
//! before `wake_*`):
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
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::mm::KBox;
use crate::sync::intrusive::IntrusiveLinkedList;
use crate::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use crate::sync::spin::SpinLock;
use crate::sync::wait_node::WaitNode;

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

    /// Block the current task until something calls
    /// [`unblock_task`](Self::unblock_task) on its handle.
    ///
    /// Performs a `Running → Blocked` CAS internally and yields. If
    /// a concurrent `unblock_task` already CAS'd the task off
    /// `Running` (e.g. `Blocked → Ready` from a wake that arrived
    /// between the caller's pre-block check and this call), the CAS
    /// is a no-op and this returns immediately — the wait-queue
    /// caller's loop re-checks the condition and observes the
    /// pending wake.
    fn block_current_task(&self);

    /// Mark the *current* task `Blocked` immediately, without yielding.
    /// Returns `true` if the CAS `Running → Blocked` succeeded; `false`
    /// if the task wasn't `Running` (e.g. a wake already won the race).
    ///
    /// Used by the wait-queue protocol to commit the block under the
    /// queue's SpinLock so a producer's `wake_one` observed after this
    /// call necessarily sees `Blocked` and CAS-flips us to `Ready`.
    /// The actual yield happens after the lock is released, via
    /// [`yield_blocked_task`](Self::yield_blocked_task).
    fn mark_current_blocked(&self) -> bool;

    /// Yield a task that has already been CAS-flipped to `Blocked`
    /// by [`mark_current_blocked`](Self::mark_current_blocked).
    /// Removes the task from runqueues and calls `schedule()`. Must
    /// be called *outside* any SpinLock — `schedule()` is not
    /// reentrant-safe under our locks.
    fn yield_blocked_task(&self);

    /// Variant of [`yield_blocked_task`] that also arms a
    /// millisecond-resolution timeout. The sleep-queue entry will
    /// CAS `Blocked → Ready` when the deadline fires; if a
    /// `wake_*` arrives first, the racing `cancel_sleep` removes
    /// the entry. Must be called outside any SpinLock.
    fn yield_blocked_task_with_timeout(&self, timeout_ms: u32);

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
    fn block_current_task(&self) {}
    fn mark_current_blocked(&self) -> bool {
        false
    }
    fn yield_blocked_task(&self) {}
    fn yield_blocked_task_with_timeout(&self, _timeout_ms: u32) {}
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

/// Internal storage: an intrusive list of [`WaitNode`]s and nothing
/// else. The list's `len()` doubles as the public `waiter_count()`.
struct WaitQueueInner {
    list: IntrusiveLinkedList<WaitNode>,
}

impl WaitQueueInner {
    const fn new() -> Self {
        Self {
            list: IntrusiveLinkedList::new(),
        }
    }
}

// SAFETY: the inner list shuttles `WaitNode` pointers between threads;
// every public method takes the surrounding `SpinLock` before touching
// `list`, which makes the cross-CPU access well-formed. The pointers
// themselves are either stack-pinned by the waiter (alive until the
// waiter unlinks under the same lock) or heap-owned by the queue
// (reclaimed via `KBox::from_raw` on dequeue).
unsafe impl Send for WaitQueueInner {}

/// A wait queue for blocking and waking kernel tasks.
pub struct WaitQueue {
    inner: SpinLock<WaitQueueInner>,
    /// Monotonic counter incremented on each wake.
    generation: AtomicU32,
}

// SAFETY: The WaitQueue is protected by `SpinLock`. Cross-CPU access
// to the wait nodes is mediated by the lock; the nodes themselves
// carry their own memory-ownership discriminant (`heap_owned`).
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

    // ---------------------------------------------------------------
    // Stack-pinned `wait_event` family.
    // ---------------------------------------------------------------

    /// Block the current task until `condition()` returns `true`.
    ///
    /// Race-close protocol (post-Phase-5, no `WillBlock` intermediate):
    ///
    /// 1. Pre-check condition outside any lock (fast path).
    /// 2. Take the queue's SpinLock.
    /// 3. Re-check condition under the lock — drop & return if true.
    /// 4. Push our node onto the list (lock-held).
    /// 5. CAS `Running → Blocked` under the same lock (lock-held).
    /// 6. Drop the lock.
    /// 7. Yield (no-op CAS schedule()) — the lock-released, status-
    ///    Blocked task sleeps until a producer's `wake_*` flips it
    ///    `Blocked → Ready`.
    ///
    /// Why the CAS belongs under the lock: a producer that takes the
    /// queue lock between (4) and (5) and pops our node would see
    /// us still `Running` and call `unblock_task` whose `Blocked →
    /// Ready` CAS would fail — lost wakeup. By doing the
    /// `Running → Blocked` CAS under the same lock, we guarantee
    /// that any `wake_*` that observes our node also observes us as
    /// `Blocked` and successfully CAS-flips us to `Ready`.
    pub fn wait_event<F: Fn() -> bool>(&self, condition: F) -> bool {
        let bk = backend();
        let node = core::pin::pin!(WaitNode::new());

        loop {
            if condition() {
                self.unlink_if_linked(node.as_ref());
                return true;
            }

            if !bk.is_runtime_initialised() {
                self.unlink_if_linked(node.as_ref());
                return false;
            }

            let task = bk.current_task_handle();
            if task.is_null() {
                self.unlink_if_linked(node.as_ref());
                return false;
            }

            let blocked = {
                let inner = self.inner.lock();
                if condition() {
                    drop(inner);
                    self.unlink_if_linked(node.as_ref());
                    return true;
                }
                self.unlink_locked(node.as_ref(), &inner);
                node.as_ref().set_task(task);
                Self::push_node(&inner, node.as_ref());
                bk.mark_current_blocked()
            };

            if blocked {
                bk.yield_blocked_task();
            }

            // Loop back: re-test condition. If `wake_one` popped us,
            // the node is no longer linked; otherwise we'll unlink
            // ourselves on the next iteration's pre-enqueue cleanup.
        }
    }

    /// Block the current task exactly once, without checking any
    /// condition. See [`wait_event`](Self::wait_event) for the
    /// lock-held push + CAS protocol that prevents lost wakeups.
    pub fn wait_once(&self) -> bool {
        let bk = backend();
        if !bk.is_runtime_initialised() {
            return false;
        }

        let task = bk.current_task_handle();
        if task.is_null() {
            return false;
        }

        let node = core::pin::pin!(WaitNode::new());
        node.as_ref().set_task(task);

        let blocked = {
            let inner = self.inner.lock();
            Self::push_node(&inner, node.as_ref());
            bk.mark_current_blocked()
        };

        if blocked {
            bk.yield_blocked_task();
        }

        self.unlink_if_linked(node.as_ref());
        true
    }

    /// Block the current task until `condition()` returns `true` or
    /// `timeout_ms` milliseconds elapse. Returns `true` iff the
    /// condition was observed true; `false` on timeout or runtime
    /// not initialised.
    ///
    /// Uses the same lock-held push + `Running → Blocked` CAS
    /// protocol as [`wait_event`](Self::wait_event). The timeout is
    /// implemented by `yield_blocked_task_with_timeout` arming a
    /// sleep-queue entry that fires `unblock_task` after the
    /// deadline; if a peer `wake_*` arrives first, that path's
    /// `cancel_sleep` removes the entry to keep the timer from
    /// firing spuriously against the (now-`Ready`) task.
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

        let node = core::pin::pin!(WaitNode::new());
        node.as_ref().set_task(task);

        let deadline_ms = bk.get_time_ms().saturating_add(timeout_ms);

        let result = loop {
            let now = bk.get_time_ms();
            if now >= deadline_ms {
                break false;
            }

            let remaining = deadline_ms.saturating_sub(now);
            let sleep_ms = remaining.min(500) as u32;

            let blocked = {
                let inner = self.inner.lock();
                if condition() {
                    drop(inner);
                    break true;
                }
                self.unlink_locked(node.as_ref(), &inner);
                Self::push_node(&inner, node.as_ref());
                bk.mark_current_blocked()
            };

            if blocked {
                bk.yield_blocked_task_with_timeout(sleep_ms);
            }
        };

        // Always unlink before returning so the stack-pinned node
        // isn't left dangling on the queue's list.
        self.unlink_if_linked(node.as_ref());
        result
    }

    // ---------------------------------------------------------------
    // Heap-owned `enqueue_current` family.
    // ---------------------------------------------------------------

    /// Enqueue the current task on this wait queue without blocking.
    /// Used by poll / select-style callers that register on multiple
    /// queues and then call `block_current_task_with_timeout` once.
    /// Each call allocates a heap-owned [`WaitNode`] that is freed
    /// either by [`WaitQueue::remove_current`] (matching cleanup path)
    /// or by [`WaitQueue::wake_one`] / [`WaitQueue::wake_all`] if a
    /// wake reaches us first.
    pub fn enqueue_current(&self) -> bool {
        let bk = backend();
        if !bk.is_runtime_initialised() {
            return false;
        }

        let task = bk.current_task_handle();
        if task.is_null() {
            return false;
        }

        let node = match KBox::try_new(WaitNode::new_heap()) {
            Ok(b) => b,
            Err(_) => return false,
        };
        node.set_task(task);
        let raw = KBox::into_raw(node);
        // SAFETY: `into_raw` returns a non-null pointer to a live
        // allocation we just produced; `NonNull::new_unchecked` is
        // therefore sound.
        let nn = unsafe { NonNull::new_unchecked(raw) };

        let pushed = {
            let inner = self.inner.lock();
            inner.list.push(nn).is_ok()
        };

        if !pushed {
            // The intrusive list refused (e.g. already linked, which
            // a fresh allocation should not exhibit, but be defensive).
            // Reclaim the box so we don't leak.
            // SAFETY: `raw` was just produced via `KBox::into_raw` and
            // the matching `from_raw` reconstructs it without aliasing
            // (no other reference holds it — we never published it).
            unsafe {
                drop(KBox::from_raw(raw));
            }
            return false;
        }

        true
    }

    /// Remove the current task from this wait queue. Intended for the
    /// cleanup path that pairs with [`enqueue_current`]; only removes
    /// heap-owned nodes (stack-pinned `wait_event` nodes manage
    /// themselves).
    pub fn remove_current(&self) {
        let bk = backend();
        let task = bk.current_task_handle();
        if task.is_null() {
            return;
        }
        self.remove_task(task);
    }

    // ---------------------------------------------------------------
    // Wake side.
    // ---------------------------------------------------------------

    /// Wake one waiting task. Returns `true` if a task was woken.
    pub fn wake_one(&self) -> bool {
        let popped = {
            let inner = self.inner.lock();
            inner.list.pop().map(|nn| {
                // Read fields under the lock so a concurrently-timing-out
                // waiter cannot free the stack node before we capture
                // the values we need.
                // SAFETY: `nn` was just popped from the list and is
                // alive: stack waiters are blocked / will block before
                // freeing their frame, and heap nodes are owned by the
                // queue until we explicitly drop them.
                let task = unsafe { nn.as_ref().task() };
                let heap_owned = unsafe { nn.as_ref().is_heap_owned() };
                (nn, task, heap_owned)
            })
        };

        match popped {
            None => false,
            Some((nn, task, heap_owned)) => {
                self.generation.fetch_add(1, Ordering::Relaxed);
                if heap_owned {
                    // SAFETY: `heap_owned` implies the node was
                    // produced by `enqueue_current` via `KBox::into_raw`;
                    // we now own it and reclaim it.
                    unsafe {
                        drop(KBox::from_raw(nn.as_ptr()));
                    }
                }
                // SAFETY: `task` came from `current_task_handle` on the
                // waiter's CPU; the backend tolerates already-woken or
                // stale handles.
                let _ = unsafe { backend().unblock_task(task) };
                true
            }
        }
    }

    /// Wake all waiting tasks. Returns the number woken.
    ///
    /// Drains the queue one node at a time, releasing the inner
    /// `SpinLock` between iterations so `unblock_task` (which may take
    /// scheduler runqueue locks) is never invoked under our lock.
    pub fn wake_all(&self) -> usize {
        let mut woken = 0usize;
        loop {
            let popped = {
                let inner = self.inner.lock();
                inner.list.pop().map(|nn| {
                    // SAFETY: as in `wake_one` — `nn` is freshly popped.
                    let task = unsafe { nn.as_ref().task() };
                    let heap_owned = unsafe { nn.as_ref().is_heap_owned() };
                    (nn, task, heap_owned)
                })
            };

            match popped {
                None => break,
                Some((nn, task, heap_owned)) => {
                    if heap_owned {
                        // SAFETY: see `wake_one`.
                        unsafe {
                            drop(KBox::from_raw(nn.as_ptr()));
                        }
                    }
                    // SAFETY: see `wake_one`.
                    let _ = unsafe { backend().unblock_task(task) };
                    woken += 1;
                }
            }
        }

        if woken > 0 {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        woken
    }

    // ---------------------------------------------------------------
    // Inspection.
    // ---------------------------------------------------------------

    /// Check if there are any waiters.
    pub fn has_waiters(&self) -> bool {
        let inner = self.inner.lock();
        !inner.list.is_empty()
    }

    /// Get the number of waiting tasks.
    pub fn waiter_count(&self) -> usize {
        let inner = self.inner.lock();
        inner.list.len()
    }

    /// Get the wake generation counter (for debugging / testing).
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }

    // ---------------------------------------------------------------
    // Targeted removal (heap-owned nodes only).
    // ---------------------------------------------------------------

    /// Remove a specific task from the wait queue. Only heap-owned
    /// nodes (those created via [`enqueue_current`]) are affected;
    /// stack-pinned `wait_event` nodes manage their own lifecycle and
    /// are left alone here.
    pub fn remove_task(&self, task: WaitTaskHandle) {
        if task.is_null() {
            return;
        }

        // Scan + remove under the lock so a concurrent `wake_*` cannot
        // free the same node out from under us.
        let removed = {
            let inner = self.inner.lock();
            let mut found: Option<NonNull<WaitNode>> = None;
            for nn in inner.list.iter() {
                // SAFETY: `nn` is a live element of `inner.list`; the
                // `Linked` contract guarantees address stability for
                // the duration of its membership, which we hold via
                // the SpinLock.
                let n = unsafe { nn.as_ref() };
                if n.task() == task && n.is_heap_owned() {
                    found = Some(nn);
                    break;
                }
            }
            if let Some(nn) = found {
                // SAFETY: we just produced `nn` from `iter()`; remove
                // it from the same list.
                let _ = inner.list.remove(nn);
            }
            found
        };

        if let Some(nn) = removed {
            // SAFETY: `is_heap_owned` was true under the lock; this
            // node was produced by `enqueue_current` via
            // `KBox::into_raw`. Reclaim and drop.
            unsafe {
                drop(KBox::from_raw(nn.as_ptr()));
            }
        }
    }

    /// Test-only reset of the registration state.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn reset_backend_for_test() {
        BACKEND_INSTALLED.store(false, Ordering::Release);
    }

    // ---------------------------------------------------------------
    // Internal helpers for stack-pinned node lifecycle.
    // ---------------------------------------------------------------

    /// Push a stack-pinned node onto the list under the caller's lock.
    /// The caller must hold `self.inner` and the node must outlive the
    /// list membership (stack-pinning enforces this).
    fn push_node(inner: &WaitQueueInner, node: Pin<&WaitNode>) {
        // SAFETY: `node` is pinned, so its address is stable until
        // dropped. Converting to `NonNull<WaitNode>` is sound; the
        // intrusive list will only access the embedded `Link<WaitNode>`
        // through that pointer while it is linked, and the waiter is
        // contractually required to unlink before returning from the
        // function that created the pin.
        let nn = NonNull::from(node.get_ref());
        // The push refusal (already-linked) is treated as a no-op:
        // `wait_event`'s loop guarantees we unlink before pushing,
        // so this branch is defensive only.
        let _ = inner.list.push(nn);
    }

    /// Take the queue's lock and unlink `node` if it is a member of
    /// the list. Used by `wait_event` callers on every exit path.
    fn unlink_if_linked(&self, node: Pin<&WaitNode>) {
        let inner = self.inner.lock();
        self.unlink_locked(node, &inner);
    }

    /// Unlink `node` from the list under an already-held lock.
    fn unlink_locked(&self, node: Pin<&WaitNode>, inner: &WaitQueueInner) {
        // SAFETY: we hold the queue lock; either `node` is a member of
        // the list (in which case removing it is sound) or it isn't
        // (in which case `remove` returns `Err(NotPresent)` and we
        // ignore it). Either way the node's address is the pinned
        // stack address, valid for the duration of this call.
        let nn = NonNull::from(node.get_ref());
        let _ = inner.list.remove(nn);
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}
