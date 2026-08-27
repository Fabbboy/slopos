//! Wait queue primitive for blocking/waking kernel tasks: an intrusive list of
//! [`WaitNode`]s under a [`SpinLock`](super::spin::SpinLock), talking to the
//! task runtime through a one-shot-registered [`WaitQueueBackend`] because OSTD
//! cannot depend on the scheduler crate.
//!
//! Two node flavours share one list: stack-pinned ones owned by the waiter's
//! frame ([`WaitQueue::wait_event`]), and heap-owned ones reclaimed by whichever
//! side dequeues them ([`WaitQueue::enqueue_current`]).
//!
//! Producers need no fence between the condition update and `wake_*`: the
//! queue's SpinLock supplies the release-acquire pair.
//!
//! `condition()` is never evaluated under the queue lock. Doing so would pull
//! whatever data lock the closure takes into the queue's lock hierarchy and
//! AB-BA against any producer that wakes while holding that same data lock. A
//! wake landing between the consumer's unlock and its recheck is absorbed
//! instead: `set_current_runnable` cancels the committed block, and
//! `yield_blocked_task` is state-aware and returns without switching.

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
use crate::sync::wait_node::NO_POLL_ERA;
use crate::sync::wait_node::WaitNode;
use crate::sync::wait_node::WaitQueueRole;

/// Identifier of a task parked on a wait queue: its registry id.
///
/// An id rather than a task pointer: a blocked task that is killed never
/// unwinds its own stack, so its node can still be linked here after the task
/// has been reaped and its allocation returned to the heap. Resolving an id is
/// a registry weak upgrade, which answers "already gone" instead of dangling.
pub type WaitTaskHandle = u32;

/// Sentinel for a node with no task claim yet, and what
/// [`WaitQueueBackend::current_task_handle`] returns with no current task.
const NULL_HANDLE: WaitTaskHandle = INVALID_TASK_ID;

/// Why a wait ended without its predicate being satisfied.
///
/// Carried in a `Result` because that is `#[must_use]` where `bool`/`Option`
/// are not: adding a variant is a compile error at every blocking call site
/// rather than a silent new path.
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
    /// No wait-queue backend is registered, or there is no current task. There
    /// is no blocking surface, so the caller must poll or fail rather than park.
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
    fn is_runtime_initialised(&self) -> bool;

    /// Opaque handle for the task currently running on this CPU, or null if
    /// there is no current task.
    fn current_task_handle(&self) -> WaitTaskHandle;

    /// Mark the *current* task `Blocked` without yielding. Returns `false` if
    /// the task was not `Running` (a wake already won the race).
    ///
    /// Committed under the queue's SpinLock so a producer's `wake_one` observed
    /// after this call necessarily sees `Blocked`; the yield happens after the
    /// lock is released, via [`yield_blocked_task`](Self::yield_blocked_task).
    fn mark_current_blocked(&self) -> bool;

    /// Yield a task already CAS-flipped to `Blocked` by
    /// [`mark_current_blocked`](Self::mark_current_blocked). Must be called
    /// *outside* any SpinLock — `schedule()` is not reentrant under our locks.
    ///
    /// A producer's `wake_*` may CAS `Blocked → Ready` between that call and
    /// this one. Implementations MUST observe that window: if the task is no
    /// longer `Blocked` at entry, restore `Running`, remove any spurious
    /// runqueue presence and return without context-switching, or the racing
    /// wake is silently dropped.
    fn yield_blocked_task(&self);

    /// [`yield_blocked_task`] plus a millisecond-resolution timeout: the
    /// sleep-queue entry CAS-flips `Blocked → Ready` at the deadline, and a
    /// `wake_*` arriving first cancels it. Same out-of-lock and state-aware
    /// contract as [`yield_blocked_task`].
    fn yield_blocked_task_with_timeout(&self, timeout_ms: u32);

    /// Force the current task's state back to `Running`, cancelling a committed
    /// `Running → Blocked` when the condition is observed true *after* the
    /// queue's SpinLock has been released.
    ///
    /// Unconditional, not a CAS: a wake that fired in that window may have left
    /// the task `Ready` and enqueued on a runqueue, and implementations must
    /// strip that stale presence so it is not double-dispatched.
    fn set_current_runnable(&self);

    /// Wake a previously-blocked task, named by id. Returns 0 on success or a
    /// negative errno-shaped value on failure — including the case where the
    /// task is already gone, which is not an error the queue can act on.
    fn unblock_task(&self, task: WaitTaskHandle) -> i32;

    /// Current monotonic time in milliseconds.
    fn get_time_ms(&self) -> u64;

    /// Record which queue the *current* task is parked on, returning the
    /// previous value; null means "parked on nothing". Must be a no-op
    /// returning null when there is no current task.
    ///
    /// Erased to `*mut c_void` because neither side can name the other's type.
    /// Teardown hands the recorded pointer to [`purge_parked_wait_node`], the
    /// only thing that reaches a stack-pinned node whose task will never run
    /// again.
    fn swap_parked_queue(&self, queue: *mut c_void) -> *mut c_void;

    /// Whether the task running on this CPU has been marked for death.
    ///
    /// Must return `false` when there is no current task: that case is
    /// `NoRuntime`, and conflating the two would fail every boot-time acquire
    /// instead of degrading it to a spin.
    fn current_task_is_killed(&self) -> bool;

    /// Whether the task running on this CPU has a signal it can act on.
    /// Consulted only by the interruptible wait tier.
    fn current_task_has_deliverable_signal(&self) -> bool;

    /// Claim the current task's poll-waiter slot, yielding the new token's
    /// era, or `None` because one is already live or there is no current task.
    fn poll_arm_current(&self) -> Option<u8>;

    /// The current task's live poll-token era, or `None` when none is armed or
    /// there is no current task.
    fn poll_era_current(&self) -> Option<u8>;

    /// Release the current task's poll-waiter slot, discarding any unconsumed
    /// wake.
    fn poll_disarm_current(&self);

    /// Clear an unconsumed wake on the current task, keeping the token armed.
    fn poll_clear_pending_current(&self);

    /// Record a wake against `task`'s poll token of generation `era`, reporting
    /// whether that token was live to take it. `false` obliges the caller to
    /// fall back to its ordinary wake path.
    fn poll_set_pending(&self, task: WaitTaskHandle, era: u32) -> bool;

    /// Consume a pending poll wake or park the current task for at most
    /// `timeout_ms`, in a single state compare-exchange. Same out-of-lock
    /// contract as [`yield_blocked_task`](Self::yield_blocked_task).
    fn poll_block_current_timeout(&self, timeout_ms: u32);
}

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
    fn poll_arm_current(&self) -> Option<u8> {
        None
    }
    fn poll_era_current(&self) -> Option<u8> {
        None
    }
    fn poll_disarm_current(&self) {}
    fn poll_clear_pending_current(&self) {}
    fn poll_set_pending(&self, _task: WaitTaskHandle, _era: u32) -> bool {
        false
    }
    fn poll_block_current_timeout(&self, _timeout_ms: u32) {}
}

static DEFAULT_BACKEND: UnregisteredBackend = UnregisteredBackend;

/// Function-pointer table wiring up the production wait-queue backend without
/// exposing the OSTD-internal [`WaitQueueBackend`] trait shape. Every entry
/// must honour the contract of the like-named method on that trait.
pub struct WaitQueueOps {
    pub is_runtime_initialised: fn() -> bool,
    pub current_task_handle: fn() -> WaitTaskHandle,
    pub mark_current_blocked: fn() -> bool,
    pub yield_blocked_task: fn(),
    pub yield_blocked_task_with_timeout: fn(u32),
    pub set_current_runnable: fn(),
    pub unblock_task: fn(WaitTaskHandle) -> i32,
    pub get_time_ms: fn() -> u64,
    pub swap_parked_queue: fn(*mut c_void) -> *mut c_void,
    pub current_task_is_killed: fn() -> bool,
    pub current_task_has_deliverable_signal: fn() -> bool,
    pub poll_arm_current: fn() -> u32,
    pub poll_era_current: fn() -> u32,
    pub poll_disarm_current: fn(),
    pub poll_clear_pending_current: fn(),
    pub poll_set_pending: fn(WaitTaskHandle, u32) -> bool,
    pub poll_block_current_timeout: fn(u32),
}

/// Decode an ops-table era. The table is a plain `fn` table, so `Option<u8>`
/// travels as a `u32` with [`NO_POLL_ERA`] standing for `None` rather than as a
/// layout the table cannot state.
#[inline]
fn era_from_abi(era: u32) -> Option<u8> {
    (era != NO_POLL_ERA).then_some(era as u8)
}

struct OpsBackend(&'static WaitQueueOps);

// SAFETY: every method delegates to the registered ops table, which the caller
// of `register_wait_queue_backend` certifies honours the contract.
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
    fn poll_arm_current(&self) -> Option<u8> {
        era_from_abi((self.0.poll_arm_current)())
    }
    fn poll_era_current(&self) -> Option<u8> {
        era_from_abi((self.0.poll_era_current)())
    }
    fn poll_disarm_current(&self) {
        (self.0.poll_disarm_current)()
    }
    fn poll_clear_pending_current(&self) {
        (self.0.poll_clear_pending_current)()
    }
    fn poll_set_pending(&self, task: WaitTaskHandle, era: u32) -> bool {
        (self.0.poll_set_pending)(task, era)
    }
    fn poll_block_current_timeout(&self, timeout_ms: u32) {
        (self.0.poll_block_current_timeout)(timeout_ms)
    }
}

struct BackendSlot(UnsafeCell<MaybeUninit<OpsBackend>>);
// SAFETY: writes are gated by the one-shot `BACKEND_INSTALLED.swap(true,
// AcqRel)`; reads happen only after observing that flag with Acquire.
unsafe impl Sync for BackendSlot {}

static BACKEND_SLOT: BackendSlot = BackendSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production wait-queue backend; the
/// `&BspToken<'brand>` witnesses BSP-only init. The table's entries must honour
/// the [`WaitQueueBackend`] contract — a caller invariant on the
/// kernel-services bridge wiring rather than on the registration itself.
pub fn register_wait_queue_backend<'brand>(_token: &BspToken<'brand>, ops: &'static WaitQueueOps) {
    let was_installed = BACKEND_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(!was_installed, "register_wait_queue_backend called twice");
    // SAFETY: the swap above claimed the slot exclusively; no writer can race.
    unsafe {
        (*BACKEND_SLOT.0.get()).write(OpsBackend(ops));
    }
}

#[inline]
pub(crate) fn backend() -> &'static dyn WaitQueueBackend {
    if !BACKEND_INSTALLED.load(Ordering::Acquire) {
        return &DEFAULT_BACKEND;
    }
    // SAFETY: the Acquire load above synchronises with the publishing write in
    // `register_wait_queue_backend`.
    unsafe { (*BACKEND_SLOT.0.get()).assume_init_ref() }
}

/// Wake a task by id through the registered wait-queue backend.
///
/// Crate-visible so the kill path can issue the wake half of a kill without a
/// second registration point.
#[inline]
pub(crate) fn unblock_task_by_id(task: WaitTaskHandle) -> i32 {
    backend().unblock_task(task)
}

/// Deliver a wake to `task`, recording it against the poll token of generation
/// `poll_era` when the registration named one.
///
/// The lost-wake fix. A poll/select-style caller registers on N queues and only
/// afterwards parks, so a wake can arrive while it is still `Running` — where
/// `unblock_task` finds nothing to do and returns, dropping the wake. Setting
/// the durable token first means the caller's block CAS consumes it instead of
/// sleeping out its budget.
///
/// `poll_era` is the generation the *registration* was made under, read off the
/// node rather than from the task at delivery time. This runs after the queue
/// lock is released, and in that window the waiter can finish its poll, disarm,
/// and a fresh poll on the same task can arm; addressing "whichever token is
/// armed now" would hand that new poll a wake it was never owed, and its first
/// block would return without parking. `poll_set_pending` refuses an era that
/// is not the live one.
///
/// It reports `false` for a stack-pinned `wait_event*` node (its own
/// `has_woken` covers that case), for a registration made with no token, and
/// for a stale era. All three want the ordinary wake, so the unblock is
/// unconditional: a wake must never be delivered by neither path.
#[inline]
fn deliver_wake(task: WaitTaskHandle, poll_era: u32) {
    let bk = backend();
    // Token first, then the unblock. A poll waiter already parked is `Blocked`
    // with its token armed, and the unblock is what moves it; one still
    // `Running` has the wake carried by the token instead. Setting the token
    // after the unblock would leave a window where neither carries it.
    if poll_era != NO_POLL_ERA {
        let _ = bk.poll_set_pending(task, poll_era);
    }
    let _ = bk.unblock_task(task);
}

/// Publishes "the current task is parked on this queue" while a stack-pinned
/// node may be linked into it.
///
/// A guard because a `wait_event*` body has many exits, one of them an unwind.
/// The previous value is restored rather than cleared so a nested wait leaves
/// the outer park visible again on the way out.
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

/// A parked wait node the queue does not own — the shape a `wait_event*`
/// caller's stack-pinned node has.
///
/// Heap-backed rather than pinned at the call site because `core::pin::pin!`
/// expands to `unsafe`, which no crate outside this one may contain even by
/// macro expansion. It is deliberately *not* flagged heap-owned, so the wake
/// and purge paths leave it to this guard.
#[cfg(any(test, feature = "test-helpers"))]
pub struct ParkedTestNode {
    node: NonNull<WaitNode>,
}

#[cfg(any(test, feature = "test-helpers"))]
impl Drop for ParkedTestNode {
    fn drop(&mut self) {
        // SAFETY: minted in `park_unowned_node_for_test` from `KBox::into_raw`
        // and never handed out; no queue path reclaims it, because the node is
        // not flagged heap-owned.
        unsafe {
            drop(KBox::from_raw(self.node.as_ptr()));
        }
    }
}

/// Unlink every node naming `task` from the queue `queue` points at, including
/// stack-pinned ones, and report how many were found.
///
/// The teardown counterpart to [`WaitQueue::remove_task`], which declines
/// stack-pinned nodes because their owner normally unlinks them on the way out
/// of `wait_event*`. Here the owner never will: a task torn down from another
/// CPU without unwinding its stack leaves its node linked into memory that is
/// then recycled with the stack slot, and the next wake writes through it.
///
/// `queue` must be a pointer previously published by
/// [`WaitQueueBackend::swap_parked_queue`], or null; a queue with a waiter
/// linked into it must outlive that waiter.
///
/// Reaches the innermost wait only — a predicate that itself blocked would park
/// a second node over the back-pointer, but none in the tree does, every lock
/// they take being a `SpinLock` rather than the sleeping
/// [`Mutex`](super::mutex::Mutex).
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

// SAFETY: every method takes the surrounding `SpinLock` before touching
// `list`. The node pointers it shuttles are either stack-pinned by the waiter
// (alive until the waiter unlinks under the same lock) or heap-owned by the
// queue (reclaimed via `KBox::from_raw` on dequeue).
unsafe impl Send for WaitQueueInner {}

/// A wait queue for blocking and waking kernel tasks.
pub struct WaitQueue {
    inner: SpinLock<WaitQueueInner>,
    generation: AtomicU32,
}

// SAFETY: cross-CPU access to the wait nodes is mediated by the `SpinLock`; the
// nodes carry their own memory-ownership discriminant (`heap_owned`).
unsafe impl Sync for WaitQueue {}
unsafe impl Send for WaitQueue {}

impl WaitQueue {
    /// Create a new empty wait queue.
    ///
    /// The lock class comes from the caller because `wait_core` reaches
    /// `TASK_MANAGER` under this lock: one class shared by every queue would
    /// make any wait-under-manager path close a cycle that is an artefact of
    /// the merge rather than a real inversion.
    pub const fn new(class: &'static LockClassKey) -> Self {
        Self {
            inner: SpinLock::new(WaitQueueInner::new(), class),
            generation: AtomicU32::new(0),
        }
    }

    /// Block the current task until `condition()` returns `true`.
    ///
    /// Killable: aborts with [`WaitAbort::Killed`] if the task is marked for
    /// death, and otherwise only when the condition holds. See
    /// [`wait_event_interruptible`](Self::wait_event_interruptible) for the
    /// tier that also aborts on a signal, and [`wait_core`](Self::wait_core)
    /// for the per-iteration protocol.
    ///
    /// The closure is never evaluated under the queue's `SpinLock`, and
    /// producers need no fence between their data store and `wake_*()`; the
    /// module doc carries both arguments.
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
    /// The abort probe runs before the predicate, so a dying task never
    /// re-enters the caller's closure. The deadline is fixed on the first pass,
    /// so a wait that loops does not silently extend itself. `Running ->
    /// Blocked` is committed under the queue lock, and the recheck outside it
    /// must cancel that block on an abort just as on a satisfied condition, or
    /// the caller returns marked `Blocked` and the next `schedule()` strands it
    /// in no runqueue.
    ///
    /// Every exit leaves through the single `unlink_if_linked` below the loop,
    /// so a task woken by its own kill unlinks its node on its own stack.
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

            // Sampled after the runtime check: the unregistered backend answers
            // 0, which would make every deadline instantly expired.
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
                // Neither the condition nor this iteration's wake bit explains
                // the failed CAS: a prior wake left this task Ready while it is
                // still executing. Consume it and retry, or the task runs on as
                // Ready and later looks stranded to the idle rescue backstop.
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

    /// Enqueue the current task without blocking, for poll/select-style callers
    /// that register on several queues and then block once. The heap-owned
    /// [`WaitNode`] is freed by [`WaitQueue::remove_current`] or by whichever
    /// wake reaches it first.
    ///
    /// The node records the poll token's era, if one is armed, so a wake
    /// delivered after this poll has finished cannot be applied to the next
    /// one's token. With no token armed the registration still queues — the
    /// token makes a wake *durable* across the register-block gap, it is not a
    /// precondition for being woken — and the wake takes the ordinary path.
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
        node.set_poll_era(bk.poll_era_current().map_or(NO_POLL_ERA, u32::from));
        let raw = KBox::into_raw(node);
        // SAFETY: `into_raw` returns a non-null pointer to the allocation we
        // just produced.
        let nn = unsafe { NonNull::new_unchecked(raw) };

        let pushed = {
            let inner = self.inner.lock();
            inner.list.push(nn).is_ok()
        };

        if !pushed {
            // SAFETY: `raw` was just produced via `KBox::into_raw` and never
            // published, so nothing else holds a reference to it.
            unsafe {
                drop(KBox::from_raw(raw));
            }
            return false;
        }

        true
    }

    /// Remove the current task from this wait queue, pairing with
    /// [`enqueue_current`]. Heap-owned nodes only; stack-pinned `wait_event`
    /// nodes manage themselves.
    pub fn remove_current(&self) {
        let bk = backend();
        let task = bk.current_task_handle();
        if task == NULL_HANDLE {
            return;
        }
        self.remove_task(task);
    }

    /// Wake one waiting task. Returns `true` if a task was woken.
    ///
    /// Every node mutation happens under the WQ critical section, so the
    /// consumer's `Drop` and post-unlock recheck observe a consistent state.
    /// `unblock_task` happens outside it: it may take scheduler runqueue locks,
    /// which must never be taken under ours.
    pub fn wake_one(&self) -> bool {
        let popped = {
            let inner = self.inner.lock();
            inner.list.pop().map(|nn| {
                // SAFETY: `nn` was just popped from the list and is alive:
                // stack waiters block before freeing their frame, and heap
                // nodes are queue-owned until we drop them.
                let n = unsafe { nn.as_ref() };
                let task = n.task();
                let heap_owned = n.is_heap_owned();
                // Read under the lock, before the node can be freed below: the
                // delivery happens outside it and must name the era this
                // registration was made under, not the one live on arrival.
                let poll_era = n.poll_era();
                // Both stores land before the lock is released: the wake signal
                // for the consumer's post-unlock recheck, and the back-pointer
                // clear that a heap-owned node's `Drop` reads outside the lock.
                let _ = n.has_woken_swap_true();
                n.queue_clear();
                (nn, task, heap_owned, poll_era)
            })
        };

        match popped {
            None => false,
            Some((nn, task, heap_owned, poll_era)) => {
                self.generation.fetch_add(1, Ordering::Relaxed);
                if heap_owned {
                    // SAFETY: `heap_owned` implies `enqueue_current` produced
                    // the node via `KBox::into_raw`, and we now own it. The
                    // back-pointer was cleared under the WQ lock above, so
                    // `Drop for WaitNode` sees null and short-circuits.
                    unsafe {
                        drop(KBox::from_raw(nn.as_ptr()));
                    }
                }
                deliver_wake(task, poll_era);
                true
            }
        }
    }

    /// Wake all waiting tasks. Returns the number woken.
    ///
    /// Drains one node at a time, releasing the inner `SpinLock` between
    /// iterations so `unblock_task` is never invoked under it.
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
                    let poll_era = n.poll_era();
                    let _ = n.has_woken_swap_true();
                    n.queue_clear();
                    (nn, task, heap_owned, poll_era)
                })
            };

            match popped {
                None => break,
                Some((nn, task, heap_owned, poll_era)) => {
                    if heap_owned {
                        // SAFETY: see `wake_one`.
                        unsafe {
                            drop(KBox::from_raw(nn.as_ptr()));
                        }
                    }
                    deliver_wake(task, poll_era);
                    woken += 1;
                }
            }
        }

        if woken > 0 {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        woken
    }

    /// Whether any waiter is queued. Lock-free, so a producer can use it as a
    /// pre-filter without paying the spinlock acquire.
    ///
    /// Exposed here rather than hidden inside `wake_*` as a fast path: that
    /// would burden every producer with an unstated ordering contract (hold and
    /// release the same data lock the consumer's condition closure takes,
    /// around the data store), so the decision is left at the call site.
    ///
    /// The `Acquire` head read pairs with the `Release` store in `push_node`'s
    /// critical section: `true` means that locked push happened-before. `false`
    /// may race with a just-committed push and is therefore advisory — the miss
    /// costs one extra spin-loop iteration on the consumer side.
    pub fn has_waiters(&self) -> bool {
        // SAFETY: `as_ptr` skips the lock, but the only field read through it
        // is the `AtomicPtr` list head, and the `WaitQueue` outlives all its
        // waiters by API contract.
        let inner = unsafe { &*self.inner.as_ptr() };
        !inner.list.is_empty()
    }

    pub fn waiter_count(&self) -> usize {
        let inner = self.inner.lock();
        inner.list.len()
    }

    /// Wake generation counter, for debugging and tests.
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Remove a specific task from the wait queue. Heap-owned nodes (those
    /// created via [`enqueue_current`]) only; stack-pinned `wait_event` nodes
    /// manage their own lifecycle.
    pub fn remove_task(&self, task: WaitTaskHandle) {
        if task == NULL_HANDLE {
            return;
        }

        // Scan and remove under the lock so a concurrent `wake_*` cannot free
        // the same node out from under us.
        let removed = {
            let inner = self.inner.lock();
            let mut found: Option<NonNull<WaitNode>> = None;
            for nn in inner.list.iter() {
                // SAFETY: `nn` is a live element of `inner.list`; the `Linked`
                // contract guarantees address stability for the duration of its
                // membership, which we hold via the SpinLock.
                let n = unsafe { nn.as_ref() };
                if n.task() == task && n.is_heap_owned() {
                    found = Some(nn);
                    break;
                }
            }
            if let Some(nn) = found {
                let _ = inner.list.remove(nn);
                // SAFETY: as above — we hold the WQ lock, so no concurrent
                // reclaimer can have run.
                let n = unsafe { nn.as_ref() };
                let _ = n.has_woken_swap_true();
                n.queue_clear();
            }
            found
        };

        if let Some(nn) = removed {
            // SAFETY: `is_heap_owned` was true under the lock, so the node came
            // from `enqueue_current` via `KBox::into_raw`.
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
        // The cap stops a still-running task from trading pushes with a killer
        // spinning here with preemption disabled.
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
                    let _ = inner.list.remove(nn);
                    // SAFETY: `nn` came from `iter()` on this same list, held
                    // under the lock for the whole sequence.
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
    /// what `wait_event*` does before it yields, without yielding, so a test
    /// can build the state a task torn down mid-wait leaves behind.
    ///
    /// The back-pointer is deliberately not restored: an abandoned wait is
    /// exactly one that never reached its own cleanup.
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

    /// Push a node onto the list under the caller's lock; the node must outlive
    /// its list membership. Also resets `has_woken`, so a previous iteration's
    /// wake does not false-positive the next check, and stores the queue
    /// back-pointer `Drop for WaitNode` unlinks through.
    fn push_node(&self, inner: &WaitQueueInner, node: Pin<&WaitNode>) {
        // SAFETY: `node` is pinned, so its address is stable, and the waiter
        // must unlink before returning from the function that created the pin.
        let nn = NonNull::from(node.get_ref());
        // Both stores precede the push: by the time the node is observable to a
        // wake, has_woken must be reset and the back-pointer set.
        node.get_ref().has_woken_reset();
        node.get_ref()
            .queue_store(self as *const WaitQueue as *mut c_void);
        // Already-linked refusal is unreachable: the loop unlinks before it
        // pushes.
        let _ = inner.list.push(nn);
    }

    /// Take the queue's lock and unlink `node` if it is a member of the list.
    fn unlink_if_linked(&self, node: Pin<&WaitNode>) {
        let inner = self.inner.lock();
        self.unlink_locked(node, &inner);
    }

    /// Unlink `node` under an already-held lock, clearing the queue
    /// back-pointer so `Drop for WaitNode` sees null and short-circuits.
    fn unlink_locked(&self, node: Pin<&WaitNode>, inner: &WaitQueueInner) {
        // SAFETY: we hold the queue lock, and the node's address is the pinned
        // one, valid for the duration of this call. A non-member yields
        // `Err(NotPresent)`, which is ignored.
        let nn = NonNull::from(node.get_ref());
        let _ = inner.list.remove(nn);
        node.get_ref().queue_clear();
    }

    /// Called from `Drop for WaitNode` when the node still carries a queue
    /// back-pointer at drop time — some path failed to unlink it before its
    /// owning stack frame or `KBox` was destroyed.
    ///
    /// `try_lock` rather than `lock()` so a refactor that drops a `WaitNode`
    /// while holding the WQ lock surfaces as a `debug_assert!` rather than a
    /// permanent deadlock; release builds leak the entry instead.
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
                // `remove` is idempotent on unlinked nodes and touches only the
                // embedded `Link`, never user data, so it is callable on a node
                // whose user data is mid-Drop.
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
                // Release builds leak the entry rather than deadlock.
            }
        }
    }
}

// Defined here rather than in `wait_node.rs` so it can reach `drop_unlink`
// without exposing the queue internals.
impl Drop for WaitNode {
    fn drop(&mut self) {
        let q = self.queue_load();
        if !q.is_null() {
            // SAFETY: the back-pointer only ever holds a `*const WaitQueue`,
            // and a queue must outlive every node linked into it (see the
            // `wait_node.rs` module doc for the lifetime invariant).
            let queue = unsafe { &*(q as *const WaitQueue) };
            let nn = NonNull::from(&*self);
            // SAFETY: `nn` points at `self`, which Drop runs before freeing.
            unsafe { queue.drop_unlink(nn) };
        }
        // A wake that pops a node this Drop missed sees an already-woken
        // sentinel and elides the spurious unblock.
        let _ = self.has_woken_swap_true();
    }
}
