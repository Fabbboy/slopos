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

use crate::sync::lock_tracking::LockClassKey;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use slopos_abi::task::INVALID_TASK_ID;

use crate::mm::KBox;
use crate::sync::BspToken;
use crate::sync::intrusive::IntrusiveLinkedList;
use crate::sync::spin::SpinLock;
use crate::sync::wait_node::WaitNode;
use crate::sync::wait_node::WaitQueueRole;

/// Identifier of a task parked on a wait queue: its registry id.
///
/// An id rather than a task pointer, because the wake side runs on an
/// arbitrary CPU at an arbitrary later time. A blocked task that is killed
/// never unwinds its own stack, so its node can still be linked here after the
/// task has been reaped and its allocation returned to the heap; a stored
/// pointer would dangle, and the backend would dereference it. Resolving an id
/// is a registry weak upgrade, which answers "already gone" instead.
pub type WaitTaskHandle = u32;

/// Sentinel for a node with no task claim yet, and the value
/// [`WaitQueueBackend::current_task_handle`] returns when there is no current
/// task (idle CPU, pre-init, or a harness reset).
const NULL_HANDLE: WaitTaskHandle = INVALID_TASK_ID;

// ---------------------------------------------------------------------------
// WaitAbort / WaitResult — how a wait ends.
// ---------------------------------------------------------------------------

/// Why a wait ended without its predicate being satisfied.
///
/// A `Result` rather than a wider enum for one reason: `core::result::Result`
/// is `#[must_use]` and `bool`/`Option` are not, so every blocking call site
/// has to say what it does about each way the wait can end. Adding a variant
/// later is a compile error at each of them rather than a silent new path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitAbort {
    /// The task has been marked for death. Every caller's answer is the same:
    /// release what you hold and return.
    Killed,
    /// A signal the task can act on is pending. Raised only by the
    /// `wait_event_interruptible*` tier.
    Interrupted,
    /// The deadline elapsed. Raised only by the `*_timeout*` entry points.
    Timeout,
    /// No wait-queue backend is registered, or there is no current task —
    /// a pre-scheduler probe path, an idle CPU, or a harness reset. There is
    /// no blocking surface, so the caller must poll or fail rather than park.
    NoRuntime,
}

/// The result of every blocking entry point on [`WaitQueue`].
pub type WaitResult<R> = Result<R, WaitAbort>;

/// Which non-predicate conditions abort a wait. A compile-time constant at
/// each public entry point, so the probe the tier does not want folds away.
#[derive(Clone, Copy)]
struct AbortMask {
    on_kill: bool,
    on_signal: bool,
}

impl AbortMask {
    const KILLABLE: Self = Self {
        on_kill: true,
        on_signal: false,
    };
    const INTERRUPTIBLE: Self = Self {
        on_kill: true,
        on_signal: true,
    };

    #[inline]
    fn probe(self, bk: &dyn WaitQueueBackend) -> Option<WaitAbort> {
        if self.on_kill && bk.current_task_is_killed() {
            return Some(WaitAbort::Killed);
        }
        if self.on_signal && bk.current_task_has_deliverable_signal() {
            return Some(WaitAbort::Interrupted);
        }
        None
    }
}

/// Longest single sleep a timed wait takes before re-evaluating, so a lost
/// wake costs a bounded delay rather than the whole remaining budget.
const TIMEOUT_CHUNK_MS: u64 = 500;

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

    /// Wake a previously-blocked task, named by id. Returns 0 on success or a
    /// negative errno-shaped value on failure — including the case where the
    /// task is already gone, which is not an error the queue can act on.
    ///
    /// Safe, and deliberately so: the id is resolved through the task registry,
    /// so an implementation cannot be handed a stale reference. The pointer
    /// form this replaced carried an unenforceable "must still refer to a live
    /// task" obligation that the wait queue had no way to discharge — a waiter
    /// killed while parked never unwinds its own stack, so its node outlives
    /// the task it names.
    fn unblock_task(&self, task: WaitTaskHandle) -> i32;

    /// Current monotonic time in milliseconds.
    fn get_time_ms(&self) -> u64;

    /// Record which queue the *current* task is parked on, returning the
    /// previous value. Null means "parked on nothing".
    ///
    /// The queue is erased to `*mut c_void` because neither side can name the
    /// other's type. Teardown reads the recorded pointer back and hands it to
    /// [`purge_parked_wait_node`], which is the only thing that can reach a
    /// stack-pinned node belonging to a task that will never run again.
    ///
    /// Must be a no-op returning null when there is no current task.
    fn swap_parked_queue(&self, queue: *mut c_void) -> *mut c_void;

    /// Whether the task running on this CPU has been marked for death.
    ///
    /// Must return `false` when there is no current task: a wait on an idle
    /// CPU or a pre-init probe path is not killed, it is `NoRuntime`, and
    /// conflating the two would fail every boot-time acquire instead of
    /// degrading it to a spin.
    fn current_task_is_killed(&self) -> bool;

    /// Whether the task running on this CPU has a signal it can act on.
    /// Consulted only by the interruptible wait tier.
    fn current_task_has_deliverable_signal(&self) -> bool;
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
    fn unblock_task(&self, _task: WaitTaskHandle) -> i32 {
        0
    }
    fn get_time_ms(&self) -> u64 {
        0
    }
    fn swap_parked_queue(&self, _queue: *mut c_void) -> *mut c_void {
        core::ptr::null_mut()
    }
    fn current_task_is_killed(&self) -> bool {
        false
    }
    fn current_task_has_deliverable_signal(&self) -> bool {
        false
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
    pub unblock_task: fn(WaitTaskHandle) -> i32,
    /// See [`WaitQueueBackend::get_time_ms`].
    pub get_time_ms: fn() -> u64,
    /// See [`WaitQueueBackend::swap_parked_queue`].
    pub swap_parked_queue: fn(*mut c_void) -> *mut c_void,
    /// See [`WaitQueueBackend::current_task_is_killed`].
    pub current_task_is_killed: fn() -> bool,
    /// See [`WaitQueueBackend::current_task_has_deliverable_signal`].
    pub current_task_has_deliverable_signal: fn() -> bool,
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
    fn unblock_task(&self, task: WaitTaskHandle) -> i32 {
        (self.0.unblock_task)(task)
    }
    fn get_time_ms(&self) -> u64 {
        (self.0.get_time_ms)()
    }
    fn swap_parked_queue(&self, queue: *mut c_void) -> *mut c_void {
        (self.0.swap_parked_queue)(queue)
    }
    fn current_task_is_killed(&self) -> bool {
        (self.0.current_task_is_killed)()
    }
    fn current_task_has_deliverable_signal(&self) -> bool {
        (self.0.current_task_has_deliverable_signal)()
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

/// Wake a task by id through the registered wait-queue backend.
///
/// Crate-visible so the kill path can issue the wake half of a kill without a
/// second registration point. This is the call `wake_one` and `wake_all`
/// already make, with the same contract: id-keyed, registry-resolved, tolerant
/// of an id whose task is already gone, and a no-op on a task that is not
/// blocked.
#[inline]
pub(crate) fn unblock_task_by_id(task: WaitTaskHandle) -> i32 {
    backend().unblock_task(task)
}

/// Publishes "the current task is parked on this queue" for as long as a
/// stack-pinned node may be linked into it.
///
/// A guard rather than a matched pair of calls because a `wait_event*` body has
/// many exits and one of them is an unwind. The previous value is restored
/// rather than cleared so a nested wait — a predicate that itself blocks —
/// leaves the outer park visible again on the way out.
struct ParkedQueueScope {
    previous: *mut c_void,
}

impl ParkedQueueScope {
    #[inline]
    fn enter(queue: &WaitQueue) -> Self {
        let previous = backend().swap_parked_queue(queue as *const WaitQueue as *mut c_void);
        Self { previous }
    }
}

impl Drop for ParkedQueueScope {
    #[inline]
    fn drop(&mut self) {
        let _ = backend().swap_parked_queue(self.previous);
    }
}

/// A parked wait node the queue does not own, which is the shape a
/// `wait_event*` caller's stack-pinned node has.
///
/// Heap-backed rather than pinned at the call site because `core::pin::pin!`
/// expands to `unsafe`, and no crate outside this one may contain `unsafe`
/// even by macro expansion. The node is deliberately *not* flagged heap-owned,
/// so the queue's wake and purge paths treat it exactly as they treat a
/// stack-pinned one and this guard remains its sole owner.
#[cfg(any(test, feature = "test-helpers"))]
pub struct ParkedTestNode {
    node: NonNull<WaitNode>,
}

#[cfg(any(test, feature = "test-helpers"))]
impl Drop for ParkedTestNode {
    fn drop(&mut self) {
        // SAFETY: minted in `park_unowned_node_for_test` from `KBox::into_raw`
        // and never handed out; no queue path reclaims it, because the node is
        // not flagged heap-owned. `Drop for WaitNode` unlinks it first if it is
        // somehow still linked.
        unsafe {
            drop(KBox::from_raw(self.node.as_ptr()));
        }
    }
}

/// Unlink every node naming `task` from the queue `queue` points at, including
/// stack-pinned ones, and report how many were found.
///
/// The teardown counterpart to [`WaitQueue::remove_task`], which declines
/// stack-pinned nodes because in steady state their owner unlinks them on the
/// way out of `wait_event*`. This exists for the one case where the owner never
/// will: a task torn down from another CPU without unwinding its stack. The
/// node's memory is then recycled with the stack slot while still linked, and
/// the next wake writes through it.
///
/// `queue` must be a pointer previously published by
/// [`WaitQueueBackend::swap_parked_queue`], or null. A queue that has had a
/// waiter linked into it is required to outlive that waiter — the same contract
/// `Drop for WaitNode` already relies on to reach `drop_unlink`.
///
/// Reaches the innermost wait only. A predicate that itself blocks would park a
/// second node and overwrite the back-pointer, hiding the outer one; no
/// predicate in the tree does, because every lock they take is a `SpinLock`
/// rather than the sleeping [`Mutex`](super::mutex::Mutex).
pub fn purge_parked_wait_node(queue: *mut c_void, task: WaitTaskHandle) -> usize {
    if queue.is_null() || task == NULL_HANDLE {
        return 0;
    }
    // SAFETY: `swap_parked_queue` is only ever handed
    // `&WaitQueue as *const _ as *mut c_void`, and the queue outlives any node
    // linked into it.
    let queue = unsafe { &*(queue as *const WaitQueue) };
    queue.purge_task(task)
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
    ///
    /// The class comes from the caller because `wait_core` reaches
    /// `TASK_MANAGER` while holding this lock: one class shared by every
    /// queue in the kernel would make any wait-under-manager path close a
    /// cycle that is an artefact of the merge rather than a real inversion.
    pub const fn new(class: &'static LockClassKey) -> Self {
        Self {
            inner: SpinLock::new(WaitQueueInner::new(), class),
            generation: AtomicU32::new(0),
        }
    }

    // ---------------------------------------------------------------
    // Stack-pinned `wait_event` family.
    // ---------------------------------------------------------------

    /// Block the current task until `condition()` returns `true`.
    ///
    /// Killable: aborts with [`WaitAbort::Killed`] if the task is marked for
    /// death, and otherwise only when the condition holds. See
    /// [`wait_event_interruptible`](Self::wait_event_interruptible) for the
    /// tier that also aborts on a signal.
    ///
    /// # Race-close protocol
    ///
    /// The closure is **deliberately never** evaluated while the queue's
    /// internal `SpinLock` is held. Linux's `wait_event` works this way too
    /// (see `___wait_event` + `prepare_to_wait_event` in `kernel/sched/wait.c`);
    /// re-checking the condition under the lock leaks whatever lock the closure
    /// takes — typically a per-resource data lock such as the pipe slot lock —
    /// into the wait queue's lock hierarchy. Combined with any producer that
    /// briefly holds the same data lock around its `wake_*` call (e.g.
    /// `PipeWriteOps::write` doing `slot.lock(); write_from; wake_one()`), the
    /// two paths form a classical AB-BA (`PS -> WQ` on the wake side,
    /// `WQ -> PS` on the wait side) that freezes two CPUs on busy-spinning
    /// ticket locks with interrupts disabled.
    ///
    /// The protocol per iteration is documented on [`wait_core`](Self::wait_core).
    ///
    /// # Memory-ordering proof
    ///
    /// The producer's contract is: store the wake-signal data, then call
    /// `wake_*()`, which acquires this queue's `SpinLock`. Two cases for the
    /// producer's acquire relative to our enqueue:
    ///
    /// - Producer **before** our enqueue: the lock pair establishes
    ///   producer-store -> our acquire -> our release -> our condition recheck.
    ///   The closure observes the producer's data; any per-resource lock the
    ///   closure also takes only sharpens the happens-before.
    /// - Producer **after** our enqueue: it sees our queued node and `Blocked`
    ///   state, and its `unblock_task` CAS `Blocked -> Ready` succeeds. Either
    ///   our recheck sees the data and we cancel the block, or it does not and
    ///   the yield is a no-op (our state is `Ready`, not `Blocked`), we loop,
    ///   and the next pre-check sees the data because the producer's
    ///   acquire/release pair preceded it.
    ///
    /// Producers do not need an explicit fence between their data store and
    /// `wake_*()`.
    #[inline]
    pub fn wait_event<F: FnMut() -> bool>(&self, mut condition: F) -> WaitResult<()> {
        self.wait_core(|| condition().then_some(()), None, AbortMask::KILLABLE)
    }

    /// Block until `condition()` returns `Some(R)`, carrying the value out.
    /// Killable; see [`wait_event`](Self::wait_event).
    #[inline]
    pub fn wait_event_until<F, R>(&self, condition: F) -> WaitResult<R>
    where
        F: FnMut() -> Option<R>,
    {
        self.wait_core(condition, None, AbortMask::KILLABLE)
    }

    /// Block until `condition()` returns `true` or the deadline elapses.
    /// Killable; see [`wait_event`](Self::wait_event).
    #[inline]
    pub fn wait_event_timeout<F: FnMut() -> bool>(
        &self,
        mut condition: F,
        timeout_ms: u64,
    ) -> WaitResult<()> {
        self.wait_core(
            || condition().then_some(()),
            Some(timeout_ms),
            AbortMask::KILLABLE,
        )
    }

    /// Block until `condition()` returns `Some(R)` or the deadline elapses.
    /// Killable; see [`wait_event`](Self::wait_event).
    #[inline]
    pub fn wait_event_timeout_until<F, R>(&self, condition: F, timeout_ms: u64) -> WaitResult<R>
    where
        F: FnMut() -> Option<R>,
    {
        self.wait_core(condition, Some(timeout_ms), AbortMask::KILLABLE)
    }

    /// Block until `condition()` returns `true`, aborting on a kill or on any
    /// deliverable signal.
    ///
    /// A caller that returns [`WaitAbort::Interrupted`] to userland owes it an
    /// `EINTR` or a restart; a caller that cannot express either wants
    /// [`wait_event`](Self::wait_event) instead.
    #[inline]
    pub fn wait_event_interruptible<F: FnMut() -> bool>(&self, mut condition: F) -> WaitResult<()> {
        self.wait_core(|| condition().then_some(()), None, AbortMask::INTERRUPTIBLE)
    }

    /// Generic-return [`wait_event_interruptible`](Self::wait_event_interruptible).
    #[inline]
    pub fn wait_event_interruptible_until<F, R>(&self, condition: F) -> WaitResult<R>
    where
        F: FnMut() -> Option<R>,
    {
        self.wait_core(condition, None, AbortMask::INTERRUPTIBLE)
    }

    /// Timed [`wait_event_interruptible`](Self::wait_event_interruptible).
    #[inline]
    pub fn wait_event_interruptible_timeout<F: FnMut() -> bool>(
        &self,
        mut condition: F,
        timeout_ms: u64,
    ) -> WaitResult<()> {
        self.wait_core(
            || condition().then_some(()),
            Some(timeout_ms),
            AbortMask::INTERRUPTIBLE,
        )
    }

    /// Timed generic-return
    /// [`wait_event_interruptible`](Self::wait_event_interruptible).
    #[inline]
    pub fn wait_event_interruptible_timeout_until<F, R>(
        &self,
        condition: F,
        timeout_ms: u64,
    ) -> WaitResult<R>
    where
        F: FnMut() -> Option<R>,
    {
        self.wait_core(condition, Some(timeout_ms), AbortMask::INTERRUPTIBLE)
    }

    /// The one wait loop. `timeout: None` is the unbounded flavour.
    ///
    /// Per iteration:
    ///
    /// 1. **Abort probe.** On the first iteration this is the pre-wait probe;
    ///    on every later one it is the post-wake probe, because a return from
    ///    `yield_blocked_task*` lands here. It runs before the predicate, so a
    ///    task that is dying never re-enters the caller's closure.
    /// 2. Pre-check the condition outside every lock.
    /// 3. Check that a runtime and a current task exist; compute the remaining
    ///    budget against a deadline fixed on the first pass, so a wait that
    ///    loops does not silently extend itself.
    /// 4. Under the queue lock: unlink any stale node, push a fresh one, and
    ///    commit `Running -> Blocked`. The condition is **not** evaluated here.
    /// 5. Re-check the condition, probe again, and load `has_woken`, all
    ///    outside the lock. Any of the three cancels the committed block via
    ///    `set_current_runnable` — an abort observed here must cancel it just
    ///    as a satisfied condition does, or the caller returns marked `Blocked`
    ///    and the next `schedule()` strands it in no runqueue.
    /// 6. Otherwise yield, with or without a deadline.
    ///
    /// Every exit leaves through the single `unlink_if_linked` below the loop;
    /// there is no `return` inside it. That is what makes a task woken by its
    /// own kill unlink its node on its own stack, before the frame returns.
    fn wait_core<F, R>(
        &self,
        mut condition: F,
        timeout: Option<u64>,
        aborts: AbortMask,
    ) -> WaitResult<R>
    where
        F: FnMut() -> Option<R>,
    {
        let bk = backend();
        let _parked = ParkedQueueScope::enter(self);
        let node = core::pin::pin!(WaitNode::new());
        let mut deadline_ms: Option<u64> = None;

        let result = loop {
            if let Some(abort) = aborts.probe(bk) {
                break Err(abort);
            }

            if let Some(r) = condition() {
                break Ok(r);
            }

            if !bk.is_runtime_initialised() {
                break Err(WaitAbort::NoRuntime);
            }
            let task = bk.current_task_handle();
            if task == NULL_HANDLE {
                break Err(WaitAbort::NoRuntime);
            }

            // Sampled after the runtime check so the clock is never read
            // through the unregistered backend, which answers 0 and would make
            // every deadline instantly expired.
            let sleep_ms = match timeout {
                None => None,
                Some(budget) => {
                    let deadline =
                        *deadline_ms.get_or_insert_with(|| bk.get_time_ms().saturating_add(budget));
                    let now = bk.get_time_ms();
                    if now >= deadline {
                        break Err(WaitAbort::Timeout);
                    }
                    Some(deadline.saturating_sub(now).min(TIMEOUT_CHUNK_MS) as u32)
                }
            };

            let marked_blocked = {
                let inner = self.inner.lock();
                self.unlink_locked(node.as_ref(), &inner);
                node.as_ref().set_task(task);
                self.push_node(&inner, node.as_ref());
                bk.mark_current_blocked()
            };

            let condition_ready = condition();
            let abort = aborts.probe(bk);
            let woke = node.as_ref().has_woken_load();
            if condition_ready.is_some() || abort.is_some() || woke {
                bk.set_current_runnable();
                // Unlinked here as well as at the funnel: a node left linked
                // across the retry could absorb a wake aimed at another waiter.
                self.unlink_if_linked(node.as_ref());
                if let Some(r) = condition_ready {
                    break Ok(r);
                }
                if let Some(abort) = abort {
                    break Err(abort);
                }
                continue;
            }

            if !marked_blocked {
                // The CAS failed and neither the condition nor this iteration's
                // wake bit explains it: the Linux "wake before schedule" case,
                // where a prior wake made this task Ready while it is still
                // executing. Consume it and retry, or the task runs on as Ready
                // and later looks stranded to the idle rescue backstop.
                bk.set_current_runnable();
                self.unlink_if_linked(node.as_ref());
                continue;
            }

            match sleep_ms {
                None => bk.yield_blocked_task(),
                Some(ms) => bk.yield_blocked_task_with_timeout(ms),
            }
        };

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
        if task == NULL_HANDLE {
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
        if task == NULL_HANDLE {
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
                // An id, so a waiter reaped while parked resolves to nothing
                // rather than to freed memory. Already-woken ids are a no-op.
                let _ = backend().unblock_task(task);
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
                    // See `wake_one`: an id cannot name freed memory.
                    let _ = backend().unblock_task(task);
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
        if task == NULL_HANDLE {
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

    /// Remove every node naming `task`, whatever its flavour, and report how
    /// many were found. See [`purge_parked_wait_node`] for why this exists
    /// where [`remove_task`](Self::remove_task) declines.
    fn purge_task(&self, task: WaitTaskHandle) -> usize {
        // The lock is dropped between nodes so a heap-owned one can be freed
        // outside it, which leaves a window for the task to link a fresh node.
        // A correct waiter holds one node per queue, so the second pass is
        // already the defensive one; the cap is what stops a task that is still
        // running from trading pushes with a killer spinning here with
        // preemption disabled.
        const MAX_NODES_PER_TASK: usize = 8;
        let mut purged = 0usize;
        while purged < MAX_NODES_PER_TASK {
            let removed = {
                let inner = self.inner.lock();
                let mut found: Option<(NonNull<WaitNode>, bool)> = None;
                for nn in inner.list.iter() {
                    // SAFETY: `nn` is a live element of `inner.list`; the
                    // `Linked` contract guarantees address stability for the
                    // duration of its membership, which we hold via the
                    // SpinLock.
                    let n = unsafe { nn.as_ref() };
                    if n.task() == task {
                        found = Some((nn, n.is_heap_owned()));
                        break;
                    }
                }
                if let Some((nn, _)) = found {
                    // SAFETY: `nn` came from `iter()` on this same list, held
                    // under the lock for the whole sequence.
                    let _ = inner.list.remove(nn);
                    let n = unsafe { nn.as_ref() };
                    let _ = n.has_woken_swap_true();
                    n.queue_clear();
                }
                found
            };

            let Some((nn, heap_owned)) = removed else {
                return purged;
            };
            purged += 1;
            if heap_owned {
                // SAFETY: `is_heap_owned` was true under the lock, so this node
                // came from `enqueue_current` via `KBox::into_raw`.
                unsafe {
                    drop(KBox::from_raw(nn.as_ptr()));
                }
            }
        }
        debug_assert!(
            false,
            "wait-queue purge hit its per-task node cap; a task is linking nodes \
             as fast as teardown removes them"
        );
        purged
    }

    /// Park a node for the current task and publish the park back-pointer —
    /// the two steps `wait_event*` takes before it yields, without yielding.
    ///
    /// Lets a test build the state a task torn down mid-wait leaves behind
    /// without the test also having to *be* that task. The back-pointer is
    /// deliberately not restored: an abandoned wait is exactly one that never
    /// reached its own cleanup.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn park_unowned_node_for_test(&self) -> Option<ParkedTestNode> {
        let bk = backend();
        let task = bk.current_task_handle();
        if task == NULL_HANDLE {
            return None;
        }
        let raw = KBox::into_raw(KBox::try_new(WaitNode::new()).ok()?);
        let nn = NonNull::new(raw)?;
        let _ = bk.swap_parked_queue(self as *const WaitQueue as *mut c_void);
        {
            let inner = self.inner.lock();
            // SAFETY: the allocation was just made here, is owned by the
            // `ParkedTestNode` this returns, and is never moved out of it.
            let node = unsafe { Pin::new_unchecked(nn.as_ref()) };
            node.set_task(task);
            self.push_node(&inner, node);
        }
        Some(ParkedTestNode { node: nn })
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
            // kept alive by an owning `KArc`, etc.) into the back-pointer.
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
