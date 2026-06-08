//! Wait queue primitive for blocking/waking kernel tasks.
//!
//! Implements the scheduler's wait/wake/block protocol — the lock-pair
//! full-barrier discipline that closes the two-atomic observation race
//! between a waiter's block and a waker's wake.
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
//! - Calls `mark_current_blocked()` (Running→Blocked CAS, under WQ lock)
//!   + `yield_blocked_task()` (state-aware deschedule, outside WQ lock)
//!   + `unblock_task()` (Blocked→Ready CAS, producer side) on the
//!   registered backend. Wakeup-loss is prevented by serialising the
//!   block and the wake through the queue's own SpinLock — see the
//!   contract block below.
//!
//! # Wait/wake correctness contract
//!
//! Producers do **NOT** need to issue explicit memory fences between the
//! condition update (e.g. a data store, `exit_info.try_set`, a pipe-buffer
//! fill/drain, a socket `rx_ready` flag flip) and the call to
//! [`WaitQueue::wake_one`] / [`WaitQueue::wake_all`]. The internal
//! [`SpinLock`](super::spin::SpinLock) supplies the release-acquire pair
//! that anchors the protocol:
//!
//! - The producer's `wake_*` acquires the queue's SpinLock to dequeue a
//!   waiter. Whatever stores the producer made before `wake_*()` are
//!   sequenced before the producer's lock-acquire (program order),
//!   which is then ordered before any subsequent lock-acquire on the
//!   same queue (lock-pair).
//! - The consumer's [`wait_event`](WaitQueue::wait_event) enqueues +
//!   commits `Running → Blocked` under the queue's SpinLock, releases
//!   the lock, and then re-checks `condition()` *outside* the queue
//!   lock. The condition check is therefore sequenced after our
//!   release of the queue lock, which is sequenced after any earlier
//!   producer's lock-acquire/release pair — so the condition observes
//!   any prior producer store.
//!
//! # Why condition() runs OUTSIDE the queue lock (anti-AB-BA)
//!
//! Evaluating `condition()` under the queue's SpinLock would pull
//! whatever lock the closure takes (typically a per-resource data
//! lock — pipe slot, socket inner, etc.) into the queue's lock
//! hierarchy. If any producer pattern holds the same data lock around
//! its `wake_*` call (a structurally tempting pattern, e.g.
//! "fill the buffer under `slot.lock()`, then `wake_one()`"), that
//! mismatch produces the classical AB-BA:
//!
//! - Producer: `data.lock() → wake_*` takes `WQ_lock` while holding
//!   `data.lock`.
//! - Consumer: `wait_event` takes `WQ_lock` then `condition()` takes
//!   `data.lock`.
//!
//! Both spin under disabled IRQs and freeze the kernel. The new
//! protocol forbids the second half of that ordering by construction:
//! `condition()` is never under `WQ_lock`. Producers can safely call
//! `wake_*` while holding any data lock — though
//! [`fs::pipe_file_ops::PipeWriteOps`] and friends still release the
//! data lock first, as defence in depth.
//!
//! # Race-close
//!
//! A producer's `wake_*` that fires between consumer's WQ-unlock and
//! consumer's post-unlock condition check observes our `Blocked`
//! task state (committed under WQ-lock in step 2 of the protocol)
//! and CAS-flips us to `Ready` + enqueues us on a runqueue. Two
//! consumer-side branches handle this race:
//!
//! - If our post-unlock `condition()` returns true, we call
//!   [`WaitQueueBackend::set_current_runnable`] which force-stores
//!   `Running` and strips the stale runqueue presence — we keep
//!   executing on this CPU.
//! - If `condition()` returns false but a wake CAS'd us to `Ready`
//!   anyway, [`WaitQueueBackend::yield_blocked_task`] is state-aware:
//!   it sees state != `Blocked`, restores `Running`, strips the
//!   runqueue presence, and returns without context-switching. The
//!   `wait_event` loop iterates and the next condition check observes
//!   the data the producer stored before its `wake_*`.
//!
//! Subsystems that inherit this contract (and therefore do **not**
//! need their own fences before `wake_*`):
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
use crate::sync::BspToken;
use crate::sync::intrusive::IntrusiveLinkedList;
use crate::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use crate::sync::spin::SpinLock;
use crate::sync::wait_node::WaitNode;
use crate::sync::wait_node::WaitQueueRole;

/// Opaque task identifier carried by the wait queue. The actual
/// representation is the kernel scheduler's task pointer.
pub type WaitTaskHandle = *mut c_void;

/// A null task handle sentinel.
const NULL_HANDLE: WaitTaskHandle = core::ptr::null_mut();

// ---------------------------------------------------------------------------
// WaitOutcome — the canonical result type for generic-return waits.
// ---------------------------------------------------------------------------

/// Outcome of a generic-return wait (see [`WaitQueue::wait_event_until`]
/// and [`WaitQueue::wait_event_timeout_until`]).
///
/// Modelled on illumos's `cv_wait_sig` return semantics: the caller wants
/// to distinguish a satisfied condition (carrying its result) from a
/// timeout or an unavailable-runtime early-exit, without re-checking
/// task state afterwards. This eliminates a class of "I forgot to
/// re-check the timeout / signal-pending state after wait_event returned
/// `false`" bugs that the bool-returning API admits.
///
/// The variant set is deliberately small. Signal-pending is intentionally
/// **not** an outcome — callers check `has_pending_signal()` themselves
/// after the wait returns, matching the existing SlopOS discipline used
/// by `PipeReadOps::read`, the TTY input path, and the poll/select loop.
/// Adding `Signal` here would invite double-checking and is left for a
/// future API revision if a caller-driven need emerges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome<R> {
    /// Condition observed true; the closure's return value is carried.
    Ready(R),
    /// Deadline elapsed without the condition becoming true. Only emitted
    /// by [`WaitQueue::wait_event_timeout_until`].
    Timeout,
    /// Wait queue backend is not registered yet, or the current task
    /// handle is null (idle CPU / pre-init / test harness reset). The
    /// unbounded [`WaitQueue::wait_event_until`] returns `None` rather
    /// than this variant; this is only surfaced by the timeout flavour.
    NoRuntime,
}

impl<R> WaitOutcome<R> {
    /// `true` iff the outcome is `Ready(_)`.
    #[inline]
    pub const fn is_ready(&self) -> bool {
        matches!(self, WaitOutcome::Ready(_))
    }

    /// Unwrap to `Option<R>`, discarding the `Timeout` / `NoRuntime`
    /// distinction.
    #[inline]
    pub fn into_ready(self) -> Option<R> {
        match self {
            WaitOutcome::Ready(r) => Some(r),
            _ => None,
        }
    }
}

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
    ///
    /// # State-aware contract
    ///
    /// The wait-queue protocol now checks `condition()` *outside* the
    /// queue's internal SpinLock (see [`WaitQueue::wait_event`]). This
    /// creates a window where a producer's `wake_*` may CAS
    /// `Blocked → Ready` between our `mark_current_blocked` and our
    /// `yield_blocked_task` call. Implementations MUST observe that
    /// window: if the task is no longer `Blocked` at entry, restore
    /// it to `Running`, remove any spurious runqueue presence, and
    /// return without context-switching. Otherwise the racing wake
    /// would be silently dropped.
    fn yield_blocked_task(&self);

    /// Variant of [`yield_blocked_task`] that also arms a
    /// millisecond-resolution timeout. The sleep-queue entry will
    /// CAS `Blocked → Ready` when the deadline fires; if a
    /// `wake_*` arrives first, the racing `cancel_sleep` removes
    /// the entry. Must be called outside any SpinLock. Carries the
    /// same state-aware contract as [`yield_blocked_task`].
    fn yield_blocked_task_with_timeout(&self, timeout_ms: u32);

    /// Force the current task's state back to `Running`. Used by the
    /// wait-queue protocol to cancel a previously committed
    /// `Running → Blocked` CAS when the wait condition is observed
    /// true *after* the queue's SpinLock has been released. The
    /// analog of Linux's `set_current_state(TASK_RUNNING)` in
    /// `finish_wait`.
    ///
    /// Unconditional — implementations must force-store `Running`
    /// regardless of the prior state. A concurrent wake that fired
    /// between [`mark_current_blocked`](Self::mark_current_blocked)
    /// and this call may have transitioned us to `Ready` and
    /// enqueued our task on a run-queue; implementations should
    /// remove that stale runqueue presence so the task does not get
    /// double-dispatched.
    ///
    /// The force-store-not-CAS soundness rests on Linux's idempotency
    /// argument from `include/linux/sched.h:201-208`. See the
    /// production-side implementation doc for the verbatim quote
    /// (kernel-side `set_current_runnable` in `scheduler.rs`).
    fn set_current_runnable(&self);

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
    fn mark_current_blocked(&self) -> bool {
        false
    }
    fn yield_blocked_task(&self) {}
    fn yield_blocked_task_with_timeout(&self, _timeout_ms: u32) {}
    fn set_current_runnable(&self) {}
    unsafe fn unblock_task(&self, _task: WaitTaskHandle) -> i32 {
        0
    }
    fn get_time_ms(&self) -> u64 {
        0
    }
}

static DEFAULT_BACKEND: UnregisteredBackend = UnregisteredBackend;

// ---------------------------------------------------------------------------
// Function-pointer ops table — production-backend registration shape.
// ---------------------------------------------------------------------------

/// Function-pointer table that consumers use to wire up the production
/// wait-queue backend without taking a dependency on the OSTD-internal
/// [`WaitQueueBackend`] trait shape. The trait's safety contract is
/// transferred onto the populated table: every fn pointer must honour
/// the equivalent method's contract on [`WaitQueueBackend`].
pub struct WaitQueueOps {
    /// See [`WaitQueueBackend::is_runtime_initialised`].
    pub is_runtime_initialised: fn() -> bool,
    /// See [`WaitQueueBackend::current_task_handle`].
    pub current_task_handle: fn() -> WaitTaskHandle,
    /// See [`WaitQueueBackend::mark_current_blocked`].
    pub mark_current_blocked: fn() -> bool,
    /// See [`WaitQueueBackend::yield_blocked_task`].
    pub yield_blocked_task: fn(),
    /// See [`WaitQueueBackend::yield_blocked_task_with_timeout`].
    pub yield_blocked_task_with_timeout: fn(u32),
    /// See [`WaitQueueBackend::set_current_runnable`].
    pub set_current_runnable: fn(),
    /// See [`WaitQueueBackend::unblock_task`].
    ///
    /// The fn pointer is `safe` because consumers always pass it
    /// through the [`WaitQueueBackend::unblock_task`] trait method
    /// (which is `unsafe fn`) and the surrounding ops-backend wraps
    /// the call in an unsafe block that re-asserts the contract.
    pub unblock_task: fn(WaitTaskHandle) -> i32,
    /// See [`WaitQueueBackend::get_time_ms`].
    pub get_time_ms: fn() -> u64,
}

struct OpsBackend(&'static WaitQueueOps);

// SAFETY: every method delegates to the registered ops table; the
// caller of `register_wait_queue_backend` certifies the table honours
// the `WaitQueueBackend` contract documented on each fn pointer.
unsafe impl WaitQueueBackend for OpsBackend {
    fn is_runtime_initialised(&self) -> bool {
        (self.0.is_runtime_initialised)()
    }
    fn current_task_handle(&self) -> WaitTaskHandle {
        (self.0.current_task_handle)()
    }
    fn mark_current_blocked(&self) -> bool {
        (self.0.mark_current_blocked)()
    }
    fn yield_blocked_task(&self) {
        (self.0.yield_blocked_task)()
    }
    fn yield_blocked_task_with_timeout(&self, timeout_ms: u32) {
        (self.0.yield_blocked_task_with_timeout)(timeout_ms)
    }
    fn set_current_runnable(&self) {
        (self.0.set_current_runnable)()
    }
    unsafe fn unblock_task(&self, task: WaitTaskHandle) -> i32 {
        // The ops-table fn pointer is `fn` (not `unsafe fn`); the
        // trait-method safety contract is owned by the caller of
        // `WaitQueueBackend::unblock_task`.
        (self.0.unblock_task)(task)
    }
    fn get_time_ms(&self) -> u64 {
        (self.0.get_time_ms)()
    }
}

struct BackendSlot(UnsafeCell<MaybeUninit<OpsBackend>>);
// SAFETY: writes are gated by `BACKEND_INSTALLED.swap(true, AcqRel)`
// (one-shot); subsequent reads only happen after observing the flag
// with Acquire, so the read sees the published reference.
unsafe impl Sync for BackendSlot {}

static BACKEND_SLOT: BackendSlot = BackendSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production wait-queue backend. The
/// `&BspToken<'brand>` witnesses BSP-only init; the table's fn-pointer
/// entries must honour the [`WaitQueueBackend`] contract: task handles
/// refer to live tasks, `unblock_task` tolerates stale handles, and
/// every method is safe to call from the contexts documented on the
/// trait — that's a caller invariant covering the kernel-services
/// bridge wiring rather than the registration itself.
pub fn register_wait_queue_backend<'brand>(_token: &BspToken<'brand>, ops: &'static WaitQueueOps) {
    let was_installed = BACKEND_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(!was_installed, "register_wait_queue_backend called twice");
    // SAFETY: the swap above transitioned us from "uninstalled" to
    // "installed" exclusively; no other writer can be racing.
    unsafe {
        (*BACKEND_SLOT.0.get()).write(OpsBackend(ops));
    }
}

#[inline]
fn backend() -> &'static dyn WaitQueueBackend {
    if !BACKEND_INSTALLED.load(Ordering::Acquire) {
        return &DEFAULT_BACKEND;
    }
    // SAFETY: paired Release in `register_wait_queue_backend`; the
    // Acquire load above synchronises with the publishing write.
    unsafe { (*BACKEND_SLOT.0.get()).assume_init_ref() }
}

// ---------------------------------------------------------------------------
// WaitQueue.
// ---------------------------------------------------------------------------

/// Internal storage: an intrusive list of [`WaitNode`]s and nothing
/// else. The list's `len()` doubles as the public `waiter_count()`.
struct WaitQueueInner {
    list: IntrusiveLinkedList<WaitNode, WaitQueueRole>,
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
    /// # Race-close protocol (post-AB-BA fix)
    ///
    /// The closure is **deliberately never** evaluated while the
    /// queue's internal `SpinLock` is held. Linux's `wait_event`
    /// works this way too (see `___wait_event` +
    /// `prepare_to_wait_event` in `kernel/sched/wait.c`); SlopOS's
    /// earlier "re-check condition under the lock" shortcut leaked
    /// whatever lock the closure took (typically a per-resource
    /// data lock such as the pipe slot lock) into the wait-queue's
    /// lock hierarchy. Combined with any producer that briefly held
    /// the same data lock around its `wake_*` call (e.g.
    /// `PipeWriteOps::write` doing `slot.lock(); write_from;
    /// wake_one()`), the two paths formed a classical AB-BA
    /// (`PS → WQ` on the wake side, `WQ → PS` on the wait side)
    /// that froze two CPUs on busy-spinning ticket locks with IRQs
    /// disabled.
    ///
    /// The new protocol:
    ///
    /// 1. Pre-check condition outside any lock (fast path).
    /// 2. Under the queue's SpinLock: unlink any stale node, push a
    ///    fresh one, CAS `Running → Blocked`. Then drop the lock.
    /// 3. Re-check condition **outside** any lock.
    /// 4. If true: force state back to `Running` (via
    ///    [`WaitQueueBackend::set_current_runnable`]), unlink, return.
    /// 5. Else: yield. [`WaitQueueBackend::yield_blocked_task`] is
    ///    state-aware and silently no-ops if a wake raced in between
    ///    step 2 and step 5, so a producer that fires `wake_*`
    ///    between WQ-unlock and our yield does not lose its wake.
    ///
    /// # Memory-ordering proof
    ///
    /// The producer's contract is: store the wake-signal data, then
    /// call `wake_*()`. `wake_*()` acquires this queue's SpinLock.
    /// Two cases for the producer's WQ-acquire timing relative to
    /// our step 2 WQ-acquire:
    ///
    /// - Producer **before** our step 2: lock-pair release/acquire
    ///   establishes producer-store → our step 2 WQ.acquire →
    ///   our step 2 WQ.release → our step 3 condition check. The
    ///   condition closure observes the producer's data (any per-
    ///   resource lock the closure also takes only sharpens the
    ///   happens-before).
    /// - Producer **after** our step 2: producer sees our queued
    ///   node + `Blocked` state. `unblock_task` CAS `Blocked → Ready`
    ///   succeeds. Either:
    ///   - Our step 3 sees the data → we cancel-block, return true.
    ///     The producer's prior `schedule_task` enqueue is benign
    ///     (the runqueue is idempotent on duplicate push).
    ///   - Our step 3 doesn't see the data → step 5 yield is a no-op
    ///     (state is `Ready` not `Blocked`), we loop, fast-path's
    ///     condition check (next iteration step 1) sees the data
    ///     because the producer's WQ-acquire/release pair preceded
    ///     it.
    ///
    /// Producers do NOT need to issue an explicit memory fence
    /// between their data store and `wake_*()`. The SpinLock acquire
    /// supplies the necessary release-barrier paired with our
    /// matching acquire when we next take the queue's lock or run
    /// any other piece of code that takes the data lock the producer
    /// also used.
    pub fn wait_event<F: FnMut() -> bool>(&self, mut condition: F) -> bool {
        self.wait_event_until(|| if condition() { Some(()) } else { None })
            .is_some()
    }

    /// Block the current task until `condition()` returns `Some(R)`,
    /// returning the carried value. The Asterinas-style generic-return
    /// variant; `wait_event` is now a thin wrapper around this.
    ///
    /// Returns `None` only when the wait-queue backend is unregistered
    /// (pre-init, test-harness reset) or the current task handle is
    /// null (idle CPU calling wait, which should not happen but is
    /// defended against).
    ///
    /// # Race-close protocol
    ///
    /// The closure is **deliberately never** evaluated while the
    /// queue's internal `SpinLock` is held. Linux's `wait_event`
    /// works this way too (see `___wait_event` +
    /// `prepare_to_wait_event` in `kernel/sched/wait.c`); SlopOS's
    /// earlier "re-check condition under the lock" shortcut leaked
    /// whatever lock the closure took (typically a per-resource
    /// data lock such as the pipe slot lock) into the wait-queue's
    /// lock hierarchy. Combined with any producer that briefly held
    /// the same data lock around its `wake_*` call (e.g.
    /// `PipeWriteOps::write` doing `slot.lock(); write_from;
    /// wake_one()`), the two paths formed a classical AB-BA
    /// (`PS → WQ` on the wake side, `WQ → PS` on the wait side)
    /// that froze two CPUs on busy-spinning ticket locks with IRQs
    /// disabled.
    ///
    /// The protocol per iteration:
    ///
    /// 1. Pre-check condition outside any lock (fast path). If
    ///    `Some(r)`, return `Some(r)` immediately.
    /// 2. Under the queue's SpinLock: unlink any stale node, push a
    ///    fresh one (which also resets `has_woken` and sets the queue
    ///    back-pointer — see `push_node`), CAS `Running → Blocked`.
    ///    Drop the lock.
    /// 3. Re-check condition **outside** any lock, and Acquire-load
    ///    `has_woken` to detect a producer wake that raced our
    ///    decision to yield.
    /// 4. Three-way decision:
    ///    - `Some(r)`: return `Some(r)` (cancelling our Blocked CAS
    ///      via `set_current_runnable`). The producer's wake — if any
    ///      — already dequeued our node and `unblock_task` raced
    ///      `Blocked → Ready`; force-Running through this path is
    ///      sound because `set_current_runnable` is unconditional
    ///      (Linux's `__set_current_state(TASK_RUNNING)` model — see
    ///      `WaitQueueBackend::set_current_runnable`).
    ///    - `None` but `woke == true`: a spurious wake fired before
    ///      our recheck observed the data (e.g. the producer's data
    ///      lock is not the data lock our closure takes, so the
    ///      lock-pair barrier across the producer's data store and
    ///      our condition load is not yet established). Cancel the
    ///      block and loop — do NOT yield, because our state is
    ///      already `Ready` and a yield would deschedule us into
    ///      a permanently-blocked state.
    ///    - `None` and `woke == false`: yield. The state-aware
    ///      [`WaitQueueBackend::yield_blocked_task`] handles the
    ///      rare race where a wake fires between our load of
    ///      `has_woken` and our yield call (state is `Ready` at
    ///      that point; yield observes it and short-circuits).
    pub fn wait_event_until<F, R>(&self, mut condition: F) -> Option<R>
    where
        F: FnMut() -> Option<R>,
    {
        let bk = backend();
        let node = core::pin::pin!(WaitNode::new());

        loop {
            // Step 1: pre-check condition (no locks).
            if let Some(r) = condition() {
                self.unlink_if_linked(node.as_ref());
                return Some(r);
            }

            if !bk.is_runtime_initialised() {
                self.unlink_if_linked(node.as_ref());
                return None;
            }

            let task = bk.current_task_handle();
            if task.is_null() {
                self.unlink_if_linked(node.as_ref());
                return None;
            }

            // Step 2: enqueue and commit Blocked under the queue lock.
            // `push_node` resets `has_woken=false` and sets the queue
            // back-pointer under this same critical section, so any
            // wake from this moment on will swap `has_woken=true`
            // (visible to step 3 via Acquire) and clear the back-pointer
            // (visible to a racing Drop via Acquire).
            //
            // We do NOT call `condition()` here — that would pull the
            // closure's data-lock into the queue's lock hierarchy and
            // re-introduce the AB-BA documented above.
            let marked_blocked = {
                let inner = self.inner.lock();
                self.unlink_locked(node.as_ref(), &inner);
                node.as_ref().set_task(task);
                self.push_node(&inner, node.as_ref());
                bk.mark_current_blocked()
            };

            // Step 3: re-check condition outside the queue lock, and
            // also observe `has_woken` to catch a wake that raced our
            // decision to yield. The (Ready) and (spurious-wake) arms
            // share their cleanup — cancel block + unlink — and only
            // differ in whether we return immediately or loop.
            let condition_ready = condition();
            let woke = node.as_ref().has_woken_load();
            if condition_ready.is_some() || woke {
                bk.set_current_runnable();
                self.unlink_if_linked(node.as_ref());
                if let Some(r) = condition_ready {
                    return Some(r);
                }
                // Spurious wake: loop. The next iteration's pre-check
                // observes the producer's data via the data lock's own
                // happens-before chain (not the WQ-lock-pair).
                continue;
            }

            if !marked_blocked {
                // The CAS failed, but the condition and this iteration's wake
                // bit do not explain it. In practice this is the Linux
                // "wake before schedule" case: a prior wake made the current
                // task Ready while it is still executing. Consume that wake by
                // restoring Running and retrying; otherwise the task can run
                // for a while as Ready and later appear stranded to the idle
                // rescue backstop.
                bk.set_current_runnable();
                self.unlink_if_linked(node.as_ref());
                continue;
            }

            // Step 4: neither condition nor wake — yield (state-aware).
            bk.yield_blocked_task();
        }
    }

    /// Block the current task until `condition()` returns `true` or
    /// `timeout_ms` milliseconds elapse. Returns `true` iff the
    /// condition was observed true; `false` on timeout or runtime
    /// not initialised.
    ///
    /// Carries the same race-close protocol as
    /// [`wait_event`](Self::wait_event): `condition()` is never
    /// evaluated under the queue's internal SpinLock, eliminating
    /// the AB-BA risk between this queue and whatever lock the
    /// closure takes. The timeout is implemented by
    /// `yield_blocked_task_with_timeout` arming a sleep-queue entry
    /// that fires `unblock_task` after the deadline; if a peer
    /// `wake_*` arrives first, that path's `cancel_sleep` removes
    /// the entry to keep the timer from firing spuriously against
    /// the (now-`Ready`) task.
    pub fn wait_event_timeout<F: FnMut() -> bool>(
        &self,
        mut condition: F,
        timeout_ms: u64,
    ) -> bool {
        matches!(
            self.wait_event_timeout_until(
                || if condition() { Some(()) } else { None },
                timeout_ms,
            ),
            WaitOutcome::Ready(_)
        )
    }

    /// Block the current task until `condition()` returns `Some(R)` or
    /// `timeout_ms` milliseconds elapse. Generic-return analogue of
    /// [`wait_event_timeout`]; carries the same race-close protocol as
    /// [`wait_event_until`] including the three-way recheck logic
    /// (`Some` → Ready, spurious wake → cancel + loop, neither →
    /// yield-with-timeout).
    ///
    /// Returns:
    /// - `WaitOutcome::Ready(R)` if the condition was observed before
    ///   the deadline.
    /// - `WaitOutcome::Timeout` if the deadline elapsed first.
    /// - `WaitOutcome::NoRuntime` if the backend is unregistered or
    ///   the current task is null.
    pub fn wait_event_timeout_until<F, R>(
        &self,
        mut condition: F,
        timeout_ms: u64,
    ) -> WaitOutcome<R>
    where
        F: FnMut() -> Option<R>,
    {
        let bk = backend();
        if let Some(r) = condition() {
            return WaitOutcome::Ready(r);
        }

        if !bk.is_runtime_initialised() {
            return WaitOutcome::NoRuntime;
        }

        let task = bk.current_task_handle();
        if task.is_null() {
            return WaitOutcome::NoRuntime;
        }

        let node = core::pin::pin!(WaitNode::new());
        node.as_ref().set_task(task);

        let deadline_ms = bk.get_time_ms().saturating_add(timeout_ms);

        let result = loop {
            let now = bk.get_time_ms();
            if now >= deadline_ms {
                break WaitOutcome::Timeout;
            }

            let remaining = deadline_ms.saturating_sub(now);
            let sleep_ms = remaining.min(500) as u32;

            // Enqueue + commit Blocked under the queue lock. Condition
            // is NOT evaluated here (see wait_event_until for rationale).
            // `push_node` also resets `has_woken` and sets the queue
            // back-pointer under the same critical section.
            let marked_blocked = {
                let inner = self.inner.lock();
                self.unlink_locked(node.as_ref(), &inner);
                self.push_node(&inner, node.as_ref());
                bk.mark_current_blocked()
            };

            // Three-way recheck: condition / has_woken / neither. See
            // `wait_event_until` for the rationale.
            let condition_ready = condition();
            let woke = node.as_ref().has_woken_load();
            if condition_ready.is_some() || woke {
                bk.set_current_runnable();
                self.unlink_if_linked(node.as_ref());
                if let Some(r) = condition_ready {
                    break WaitOutcome::Ready(r);
                }
                continue; // spurious wake — loop, deadline still applies.
            }
            if !marked_blocked {
                bk.set_current_runnable();
                self.unlink_if_linked(node.as_ref());
                continue;
            }
            bk.yield_blocked_task_with_timeout(sleep_ms);
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
    ///
    /// All node mutations (read `task` + `heap_owned`, swap
    /// `has_woken` to `true`, clear the queue back-pointer) happen
    /// *under the WQ critical section* so the consumer's Drop and
    /// the consumer's post-unlock recheck observe a consistent state.
    /// `unblock_task` happens outside the lock — it may take
    /// scheduler runqueue locks and we must never invoke them under
    /// ours.
    pub fn wake_one(&self) -> bool {
        let popped = {
            let inner = self.inner.lock();
            inner.list.pop().map(|nn| {
                // SAFETY: `nn` was just popped from the list and is
                // alive: stack waiters are blocked / will block before
                // freeing their frame, and heap nodes are owned by the
                // queue until we explicitly drop them.
                let n = unsafe { nn.as_ref() };
                let task = n.task();
                let heap_owned = n.is_heap_owned();
                // Mark the node woken AND clear the back-pointer
                // *before* releasing the lock. The has_woken store gives
                // the consumer's post-unlock recheck (or `Drop`) a
                // free signal that a wake happened; the queue_clear is
                // load-bearing for heap-owned nodes whose Drop reads
                // the back-pointer outside the lock to decide whether
                // to re-acquire it.
                let _ = n.has_woken_swap_true();
                n.queue_clear();
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
                    // we now own it and reclaim it. The back-pointer
                    // was cleared under the WQ lock above, so the
                    // KBox's `Drop for WaitNode` will see null and
                    // short-circuit.
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
    ///
    /// Same per-node bookkeeping as [`wake_one`] applies under the lock.
    pub fn wake_all(&self) -> usize {
        let mut woken = 0usize;
        loop {
            let popped = {
                let inner = self.inner.lock();
                inner.list.pop().map(|nn| {
                    // SAFETY: as in `wake_one` — `nn` is freshly popped.
                    let n = unsafe { nn.as_ref() };
                    let task = n.task();
                    let heap_owned = n.is_heap_owned();
                    let _ = n.has_woken_swap_true();
                    n.queue_clear();
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

    /// Check if there are any waiters. **Lock-free** — does not take
    /// the queue's `SpinLock`. Callers that want to skip a `wake_*`
    /// call when no waiters are queued can use this as a cheap
    /// pre-filter without paying the spinlock acquire cost.
    ///
    /// # Soundness vs. a `num_wakers` fast path inside `wake_*`
    ///
    /// The cross-kernel audit considered baking a lock-free fast
    /// path into `wake_one`/`wake_all` itself (so producers would
    /// skip the spinlock entirely when no one was waiting — the
    /// Asterinas `num_wakers` pattern). That was rejected because
    /// the soundness argument burdens every producer with a
    /// non-obvious contract ("you must hold and release the same
    /// data lock the consumer's condition closure uses, around your
    /// data store"). Linux always takes `wq_head->lock` in
    /// `__wake_up_common` for exactly this reason. Exposing the
    /// lock-free probe here, instead of hiding it inside `wake_*`,
    /// makes the caller's decision explicit at the call site —
    /// pipe producers etc. can do `if wq.has_waiters() { wq.wake_one(); }`
    /// with the soundness reasoning visible.
    ///
    /// # Memory ordering
    ///
    /// `IntrusiveLinkedList::is_empty()` reads the `head` pointer
    /// with `Acquire`. That pairs with the `Release` store inside
    /// the WQ critical section of `push_node`. A reader that
    /// observes `is_empty() == false` is guaranteed that some
    /// producer's WQ-locked push happened-before, and thus any
    /// store the producer made before its push is visible.
    /// Observing `is_empty() == true` may race with a
    /// just-committed push and is therefore advisory — the caller
    /// is expected to follow up with a real wake call when in
    /// doubt; the cost of a spurious "no waiters" miss is one
    /// extra spin-loop iteration on the consumer side.
    pub fn has_waiters(&self) -> bool {
        // SAFETY: `as_ptr` exposes a `*const WaitQueueInner` without
        // taking the lock; we only read the `list.head` atomic
        // through it, which is safe because (a) the head pointer
        // is `AtomicPtr` with its own synchronisation and (b) the
        // `WaitQueueInner`'s memory is `'static` (the `WaitQueue`
        // outlives all its waiters by API contract).
        let inner = unsafe { &*self.inner.as_ptr() };
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
        // free the same node out from under us. Same bookkeeping as
        // `wake_*`: mark has_woken + clear back-pointer under the
        // lock before reclaiming the heap allocation.
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
                // SAFETY: same as above — the node is still alive (we
                // are holding the WQ lock so no concurrent reclaimer
                // can run yet).
                let n = unsafe { nn.as_ref() };
                let _ = n.has_woken_swap_true();
                n.queue_clear();
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
    ///
    /// Also performs the defense-in-depth bookkeeping under the
    /// same WQ critical section:
    /// - Reset `has_woken` to `false` so a previous-iteration wake on
    ///   this node does not false-positive the next iteration's check.
    /// - Store the queue back-pointer (`self as *mut c_void`) so a
    ///   future `Drop for WaitNode` can unlink the node if upstream
    ///   code forgot.
    ///
    /// `&self` is now required (was just `inner` before) because we
    /// store the queue back-pointer; the cast to `*mut c_void` happens
    /// here so `wait_node.rs` stays free of `WaitQueue` references.
    fn push_node(&self, inner: &WaitQueueInner, node: Pin<&WaitNode>) {
        // SAFETY: `node` is pinned, so its address is stable until
        // dropped. Converting to `NonNull<WaitNode>` is sound; the
        // intrusive list will only access the embedded
        // `Link<WaitNode, WaitQueueRole>` through that pointer while
        // it is linked, and the waiter is contractually required to
        // unlink before returning from the function that created the
        // pin.
        let nn = NonNull::from(node.get_ref());
        // Reset has_woken + store back-pointer BEFORE list.push. The
        // ordering matters: by the time the node is observable to a
        // wake on this queue, its has_woken flag must be reset and its
        // back-pointer set, so the wake's pop-path stores are
        // consistent with the consumer's view.
        node.get_ref().has_woken_reset();
        node.get_ref()
            .queue_store(self as *const WaitQueue as *mut c_void);
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

    /// Unlink `node` from the list under an already-held lock. Also
    /// clears the queue back-pointer under the lock so `Drop for
    /// WaitNode` sees null and short-circuits.
    fn unlink_locked(&self, node: Pin<&WaitNode>, inner: &WaitQueueInner) {
        // SAFETY: we hold the queue lock; either `node` is a member of
        // the list (in which case removing it is sound) or it isn't
        // (in which case `remove` returns `Err(NotPresent)` and we
        // ignore it). Either way the node's address is the pinned
        // stack address, valid for the duration of this call.
        let nn = NonNull::from(node.get_ref());
        let _ = inner.list.remove(nn);
        node.get_ref().queue_clear();
    }

    /// Called from `Drop for WaitNode` when the node finds itself with
    /// a non-null queue back-pointer at drop time. This means some
    /// upstream code-path failed to unlink the node before its owning
    /// stack frame / `KBox` was destroyed — a defense-in-depth
    /// recovery path.
    ///
    /// Uses `try_lock` rather than `lock()` so a future buggy refactor
    /// that drops a `WaitNode` while still holding the WQ lock surfaces
    /// as a `debug_assert!` in debug builds rather than a permanent
    /// deadlock. In release builds the `try_lock` failure path leaks
    /// the node on the queue (the upstream invariant violation is
    /// already a bug; the goal here is to not make it worse).
    ///
    /// # Safety
    ///
    /// `nn` must point to the `WaitNode` whose `queue_load()` returned
    /// `self as *mut c_void`. The node's address must still be valid
    /// for the duration of this call (true during `Drop` since the
    /// node's memory hasn't been freed yet; Drop runs *before* the
    /// stack frame unwinds / heap block is released).
    pub(crate) unsafe fn drop_unlink(&self, nn: NonNull<WaitNode>) {
        match self.inner.try_lock() {
            Some(inner) => {
                // `remove` is idempotent on unlinked nodes (returns
                // Err(NotPresent)) and only touches the embedded `Link`
                // field — never user data — so it is safe to call on
                // a node whose user data is mid-Drop.
                let _ = inner.list.remove(nn);
            }
            None => {
                debug_assert!(
                    false,
                    "WaitNode dropped while the owning WaitQueue's lock is held by the current CPU — \
                     check that no return path inside `wait_event_*` exits without unlinking the node \
                     first, and that no wake_*() path holds the WQ lock across a panic-recovery \
                     unwind that destroys the node."
                );
                // Release-build: silently leak the node entry. The
                // upstream invariant is already violated; a deadlock
                // here would compound the bug.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Drop for WaitNode — defined here so it can reach `WaitQueue::drop_unlink`
// without exposing the queue internals through `wait_node.rs`.
// ---------------------------------------------------------------------------

impl Drop for WaitNode {
    fn drop(&mut self) {
        let q = self.queue_load();
        if !q.is_null() {
            // SAFETY: `queue_store` always casts a `*const WaitQueue`
            // (a static array element, an inline field of a `TaskInner`
            // kept alive by `TaskRefGuard`, etc.) into the back-pointer.
            // The WaitQueue is required by its API contract to outlive
            // every WaitNode that has been linked into it (see the
            // `wait_node.rs` module doc for the lifetime invariant).
            let queue = unsafe { &*(q as *const WaitQueue) };
            let nn = NonNull::from(&*self);
            // SAFETY: the back-pointer was set under the queue's WQ
            // lock by `push_node`; `nn` is a valid pointer to `self`
            // for the duration of this call (Drop runs before the
            // memory is freed).
            unsafe { queue.drop_unlink(nn) };
        }
        // Set has_woken=true as a final flourish: any wake_one popping
        // a node that this Drop somehow missed (shouldn't be possible
        // — the back-pointer check above covers the case) would see
        // an already-woken sentinel and elide the spurious unblock.
        // Pure defense-in-depth; not load-bearing.
        let _ = self.has_woken_swap_true();
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}
