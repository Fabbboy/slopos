//! `KernelIoToken` — compile-time witness that the holder runs at
//! [`TaskPriority::KernelIo`] (Phase-1 scheduler refactor).
//!
//! Phase-1 of the rip-and-replace introduces a strict priority tier
//! above `Normal` for kernel I/O kthreads (NAPI, net-timer, deferred
//! driver work). The runtime invariant — "these tasks must run on
//! their sleep deadlines regardless of user-task load" — is enforced
//! by three layers:
//!
//! 1. **Priority class.** `TaskPriority::KernelIo = 1` lies between
//!    `High` and `Normal`. The scheduler picks higher-priority work
//!    first; a user task at `Normal` cannot starve a `KernelIo`
//!    kthread. The syscall boundary in
//!    `core::syscall::process_handlers` rejects user-supplied
//!    `KernelIo` with `EINVAL`, so the tier is kernel-only by ABI.
//!
//! 2. **Spawn surface.** The only sanctioned way to create a
//!    `KernelIo`-priority task is the [`spawn_kernel_io!`] macro
//!    below. It generates a hidden `fn()` trampoline that constructs
//!    a [`KernelIoToken`] inside the new task's stack frame and
//!    hands it to the user entry. Plain
//!    `slopos_ostd::task::spawn(.., TaskPriority::KernelIo.as_u8())`
//!    is rejected at runtime by the scheduler-side validator
//!    (`SpawnError::PriorityReserved`) — see `sched::kthread`.
//!
//! 3. **Yield obligation.** Once a kthread holds a
//!    [`KernelIoToken`], the only sleep API it has access to is
//!    [`yield_with_deadline`], which forces every caller to name a
//!    [`Deadline`]. "Sleep indefinitely" is still allowed (via
//!    `Deadline::Indefinite`) but is now an explicit, grep-able
//!    decision rather than the easy-to-reach `sleep_current_task_ms(1)`
//!    that produced the original starvation regression.
//!
//! The build gate `scripts/check_wait_predicate_purity.sh` (added in
//! Phase 1.11) additionally bans `napi::kick`, `force_napi_poll`, and
//! `sleep_current_task_ms` from any closure passed to
//! `WaitQueue::wait_event{,_timeout,_until}` — closing the loop on
//! the original workaround. Together these layers make "kernel I/O
//! task got starved" a compile-time / CI failure, not a runtime
//! debugging exercise.

use crate::sync::lock_tracking::LOCK_LEVEL_REGISTRY;
use core::marker::PhantomData;

/// Witness type carried by every `KernelIo`-priority kthread.
///
/// The constructor is `pub(crate)` to slopos-ostd; only
/// [`spawn_kernel_io!`] expansions (which are emitted inside this
/// crate via the macro's `$crate::...` path) reach it. The
/// `'cpu` lifetime is informational only (the same task always
/// runs on the same CPU between yields); use `'static` for the
/// long-lived case.
///
/// `KernelIoToken` is `!Send + !Sync` via the `*const ()` zero-cost
/// marker. The token must stay on the thread that received it from
/// the trampoline — handing it to a worker thread (or any other
/// kernel task) would break the "this caller is `KernelIo`-priority"
/// witness. The marker also prevents accidentally storing the token
/// in a global.
#[derive(Debug)]
pub struct KernelIoToken<'cpu> {
    _not_send: PhantomData<*const ()>,
    _lifetime: PhantomData<&'cpu ()>,
}

impl<'cpu> KernelIoToken<'cpu> {
    /// Construct a fresh witness.
    ///
    /// **Internal — call only from the [`spawn_kernel_io!`]
    /// trampoline.** The trampoline runs as the first userland frame
    /// of a freshly-spawned `KernelIo` task, so by the time it
    /// constructs a token the scheduler has already validated the
    /// priority class.
    #[doc(hidden)]
    #[inline]
    pub const fn __new_for_trampoline_only() -> Self {
        Self {
            _not_send: PhantomData,
            _lifetime: PhantomData,
        }
    }
}

/// Sleep deadline for [`yield_with_deadline`].
///
/// Every `KernelIo` task must name one when descheduling. The
/// pre-refactor `sleep_current_task_ms(1)` polling loop in
/// `drivers/src/virtio_net.rs::napi_thread_entry` is replaced by a
/// `Deadline::Indefinite` wait on a [`crate::sync::WaitQueue`]
/// guarded by an IRQ-armed `NapiWaker` — explicit, deliberate, and
/// grep-able as `Deadline::Indefinite`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deadline {
    /// Yield to the scheduler immediately. Used after exhausting a
    /// NAPI burst budget: re-arm the wakeup and let the scheduler
    /// pick the next task in case it's higher priority.
    Immediate,
    /// Wake no later than `ms` milliseconds from now. Resolution is
    /// bounded by the LAPIC tick (10 ms by default) plus the
    /// tickless-idle one-shot (see
    /// `sched::scheduler::arm_tickless_idle_if_due`).
    AtMs(u32),
    /// Sleep until explicitly woken (e.g. via a [`crate::sync::WaitQueue`]
    /// `wake_all`). The task does not consume a sleep-queue entry; the
    /// wake path runs the predicate and unblocks. Used by the threaded
    /// NAPI kthread parking on an IRQ-armed `NapiWaker`.
    Indefinite,
}

/// The only sleep API available to a [`KernelIoToken`] holder.
///
/// Forcing every yield to name a deadline turns starvation
/// avoidance into a compile-time obligation: a forgotten
/// `Deadline::AtMs(_)` is a missing match arm, not a runtime
/// performance puzzle. The backend ([`yield_backend`]) is registered
/// from outside slopos-ostd at boot (`sched::runtime`) so this
/// primitive does not pull a scheduler dependency into the trusted
/// core.
///
/// `_token` is taken by reference rather than by value so the same
/// witness can drive many yields in a single kthread loop.
#[inline]
pub fn yield_with_deadline<'cpu>(_token: &KernelIoToken<'cpu>, deadline: Deadline) {
    if let Some(yield_fn) = current_yield_backend() {
        yield_fn(deadline);
    }
    // If no backend is registered (pre-boot, test fixture without
    // scheduler), the yield is a no-op. The kthread continues; this
    // matches `WaitQueue`'s pre-runtime no-op contract.
}

// ---------------------------------------------------------------------------
// Backend registration (yield function)
// ---------------------------------------------------------------------------

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::sync::BspToken;

/// Function-pointer backend for [`yield_with_deadline`]. Registered
/// from outside slopos-ostd at boot (`sched::runtime`) so the
/// trusted core does not depend on the scheduler crate. Mirrors the
/// existing one-shot-registration pattern used by
/// [`crate::sync::rcu::register_rcu_backend`].
pub type YieldBackend = fn(Deadline);

static YIELD_BACKEND: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the production yield backend. BSP-only via the
/// `&BspToken<'brand>` witness produced by [`crate::sync::run_bsp_init`].
pub fn register_yield_backend<'brand>(_token: &BspToken<'brand>, backend: YieldBackend) {
    let raw = backend as *const () as *mut ();
    let prev = YIELD_BACKEND.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::sync::kernel_io_task::register_yield_backend called twice"
    );
}

#[inline]
fn current_yield_backend() -> Option<YieldBackend> {
    let raw = YIELD_BACKEND.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` was produced by `register_yield_backend` from a
    // function pointer of matching ABI; the round-trip is sound by
    // the same logic as the RCU-backend registration.
    Some(unsafe { core::mem::transmute::<*mut (), YieldBackend>(raw) })
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_yield_backend_for_test() {
    YIELD_BACKEND.store(core::ptr::null_mut(), Ordering::Release);
}

// ---------------------------------------------------------------------------
// Spawn macro
// ---------------------------------------------------------------------------

/// Spawn a `KernelIo`-priority kthread.
///
/// `name` is a `'static` string baked into the binary. `entry` is a
/// `fn(KernelIoToken<'static>)` — typically a plain top-level
/// function. The macro generates a hidden `fn()` trampoline that
/// constructs the token and calls `entry`, then dispatches to
/// [`crate::task::spawn`] at [`slopos_abi::task::TaskPriority::KernelIo`].
///
/// Example:
/// ```ignore
/// use slopos_ostd::sync::kernel_io_task::{KernelIoToken, Deadline, yield_with_deadline};
/// use slopos_ostd::spawn_kernel_io;
///
/// fn napi_thread_entry(token: KernelIoToken<'static>) {
///     loop {
///         NAPI_WAKER.wait();
///         run_napi_burst();
///         yield_with_deadline(&token, Deadline::Immediate);
///     }
/// }
///
/// spawn_kernel_io!("netpoll", napi_thread_entry).expect("spawn");
/// ```
///
/// The hidden trampoline construction (and only that path)
/// instantiates a [`KernelIoToken`] — direct constructor calls
/// outside the macro show up in the grep-gate
/// `scripts/check_wait_predicate_purity.sh` as a build failure.
#[macro_export]
macro_rules! spawn_kernel_io {
    ($name:expr, $entry:path $(,)?) => {{
        fn __slopos_kernel_io_trampoline() {
            // SAFETY: the trampoline is only reachable from a
            // task slot the scheduler validated as KernelIo priority
            // — the `spawn_kernel_io!` macro is the only emitter
            // and it always pairs this trampoline with the
            // `TaskPriority::KernelIo` argument below.
            let token =
                $crate::sync::kernel_io_task::KernelIoToken::<'static>::__new_for_trampoline_only();
            $entry(token);
        }

        $crate::task::spawn(
            $name,
            __slopos_kernel_io_trampoline,
            ::slopos_abi::task::TaskPriority::KernelIo.as_u8(),
        )
    }};
}

// ---------------------------------------------------------------------------
// Cooperative stop
// ---------------------------------------------------------------------------

use crate::sync::lock_graph::LockClassKey;
use crate::sync::spin::SpinLock;
use crate::sync::wait_queue::WaitQueue;
use core::sync::atomic::AtomicBool;

/// Cooperative stop signal for one kernel-I/O thread.
///
/// Kernel tasks are structurally excluded from signals, and the resources
/// these threads drive — a NIC's RX path, a touchpad's interrupt line, an
/// ext2 writeback cache — outlive any one user process, so they are not
/// killable. What they need instead is a stop they can *finish* on: the ext2
/// flusher's last act is a full sync, which a kill semantic would discard.
///
/// The queue lives inside the token rather than beside it so the park and the
/// stop-wake cannot drift into two objects that disagree about which queue is
/// the live one.
pub struct KernelIoStop {
    name: &'static str,
    requested: AtomicBool,
    exited: AtomicBool,
    wq: WaitQueue,
}

impl KernelIoStop {
    /// The lock class comes from the caller for the same reason `name` does:
    /// these threads serve unrelated subsystems, and a class minted here
    /// would merge all of them under whichever name was written down first.
    pub const fn new(name: &'static str, class: &'static LockClassKey) -> Self {
        Self {
            name,
            requested: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            wq: WaitQueue::new(class),
        }
    }

    /// The thread's name, for the shutdown report.
    #[inline]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The queue the thread parks on. Producers wake it to hand over work.
    #[inline]
    pub const fn queue(&self) -> &WaitQueue {
        &self.wq
    }

    /// Whether a stop has been asked for.
    #[inline]
    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Ask the thread to stop, and wake it so it notices.
    ///
    /// Fused for the same reason the kill flag is: the flag alone leaves a
    /// parked thread that never re-evaluates it.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        let _ = self.wq.wake_all();
    }

    /// Called by the thread once its loop has ended and its final work is done.
    #[inline]
    pub fn note_exited(&self) {
        self.exited.store(true, Ordering::Release);
    }

    /// Whether the thread has reported that it finished.
    #[inline]
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

/// Why a kernel-I/O park ended.
///
/// `#[must_use]` so adding a stop to a thread turns each of its parks into a
/// compile error until the `Stop` arm is written.
#[must_use = "a kernel-I/O park that ignores Stop cannot be shut down"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KthreadWait {
    /// The caller's condition held.
    Ready,
    /// The deadline elapsed, or there was no blocking surface.
    Timeout,
    /// A stop was requested. Finish what must be finished, then return.
    Stop,
}

impl KernelIoToken<'_> {
    /// Park until `condition` holds or a stop is requested.
    ///
    /// The stop probe is folded into the predicate so a `request` issued
    /// between the caller's last check and the park is not lost: `request`
    /// stores the flag before waking, and the predicate reads it under the
    /// same queue-lock pairing the wait protocol already establishes.
    pub fn park<F: FnMut() -> bool>(&self, stop: &KernelIoStop, mut condition: F) -> KthreadWait {
        if stop.requested() {
            return KthreadWait::Stop;
        }
        let outcome = stop.wq.wait_event(|| stop.requested() || condition());
        Self::classify(stop, outcome.is_ok())
    }

    /// Park until `condition` holds, a stop is requested, or `timeout_ms`
    /// elapses.
    pub fn park_timeout<F: FnMut() -> bool>(
        &self,
        stop: &KernelIoStop,
        mut condition: F,
        timeout_ms: u64,
    ) -> KthreadWait {
        if stop.requested() {
            return KthreadWait::Stop;
        }
        let outcome = stop
            .wq
            .wait_event_timeout(|| stop.requested() || condition(), timeout_ms);
        Self::classify(stop, outcome.is_ok())
    }

    #[inline]
    fn classify(stop: &KernelIoStop, satisfied: bool) -> KthreadWait {
        if stop.requested() {
            KthreadWait::Stop
        } else if satisfied {
            KthreadWait::Ready
        } else {
            KthreadWait::Timeout
        }
    }
}

/// Every kernel-I/O thread that can be asked to stop.
///
/// A fixed array rather than a linker registry: the set is small, known at
/// boot, and a registry would put a new section in `link.ld` for four entries.
const MAX_KERNEL_IO_STOPS: usize = 8;

struct StopRegistry {
    entries: [Option<&'static KernelIoStop>; MAX_KERNEL_IO_STOPS],
    count: usize,
}

static STOP_REGISTRY: SpinLock<StopRegistry> = SpinLock::new(
    StopRegistry {
        entries: [None; MAX_KERNEL_IO_STOPS],
        count: 0,
    },
    crate::lock_class!("STOP_REGISTRY", LOCK_LEVEL_REGISTRY),
);

/// Make `stop` visible to [`request_kernel_io_stop_all`]. A thread that never
/// registers simply cannot be asked to stop, which shutdown reports by name.
pub fn register_kernel_io_stop(stop: &'static KernelIoStop) {
    let mut registry = STOP_REGISTRY.lock();
    if registry.count >= MAX_KERNEL_IO_STOPS {
        crate::klog_info!(
            "kernel-io: stop registry full, '{}' cannot be stopped",
            stop.name()
        );
        return;
    }
    let idx = registry.count;
    registry.entries[idx] = Some(stop);
    registry.count += 1;
}

/// Ask every registered kernel-I/O thread to stop, in reverse registration
/// order so a later thread that feeds an earlier one drains first. Returns how
/// many were asked.
///
/// Only asks. The caller owns the bounded wait, because waiting needs a
/// scheduler and this crate sits below it.
pub fn request_kernel_io_stop_all() -> usize {
    let stops = {
        let registry = STOP_REGISTRY.lock();
        let mut copy: [Option<&'static KernelIoStop>; MAX_KERNEL_IO_STOPS] =
            [None; MAX_KERNEL_IO_STOPS];
        copy[..registry.count].copy_from_slice(&registry.entries[..registry.count]);
        copy
    };
    let mut asked = 0usize;
    for stop in stops.iter().rev().flatten() {
        stop.request();
        asked += 1;
    }
    asked
}

/// Registered kernel-I/O threads that have not yet reported that they
/// finished. Zero means every one of them ran its own exit path.
pub fn kernel_io_stops_pending() -> usize {
    let registry = STOP_REGISTRY.lock();
    registry.entries[..registry.count]
        .iter()
        .flatten()
        .filter(|stop| !stop.has_exited())
        .count()
}

/// Names of the kernel-I/O threads that have not finished, for the shutdown
/// report. Calls `report` once per outstanding thread.
pub fn for_each_unstopped_kernel_io(mut report: impl FnMut(&'static str)) {
    let registry = STOP_REGISTRY.lock();
    for stop in registry.entries[..registry.count].iter().flatten() {
        if !stop.has_exited() {
            report(stop.name());
        }
    }
}
