//! `KernelIoToken` — compile-time witness that the holder runs at
//! `TaskPriority::KernelIo`, the tier above `Normal` reserved for kernel I/O
//! kthreads (NAPI, net-timer, deferred driver work).
//!
//! The tier is kernel-only: the syscall boundary rejects a user-supplied
//! `KernelIo` priority with `EINVAL`, and a plain `task::spawn` at it is
//! refused by the scheduler-side validator (`SpawnError::PriorityReserved`),
//! leaving [`spawn_kernel_io!`] as the only way in. A token holder's only
//! sleep API is [`yield_with_deadline`], so every deschedule names a
//! [`Deadline`] — "sleep indefinitely" stays available but is explicit.

use crate::sync::lock_tracking::LOCK_LEVEL_REGISTRY;
use core::marker::PhantomData;

/// Witness type carried by every `KernelIo`-priority kthread.
///
/// Only [`spawn_kernel_io!`] expansions reach the constructor. `!Send + !Sync`
/// via the `*const ()` marker: the witness holds only for the thread the
/// trampoline handed it to, so it can neither move to another task nor be
/// stored in a global. The `'cpu` lifetime is informational; use `'static` for
/// the long-lived case.
#[derive(Debug)]
pub struct KernelIoToken<'cpu> {
    _not_send: PhantomData<*const ()>,
    _lifetime: PhantomData<&'cpu ()>,
}

impl<'cpu> KernelIoToken<'cpu> {
    /// Construct a fresh witness.
    ///
    /// **Internal — call only from the [`spawn_kernel_io!`] trampoline**,
    /// which runs as the first frame of a task whose priority class the
    /// scheduler has already validated.
    #[doc(hidden)]
    #[inline]
    pub const fn __new_for_trampoline_only() -> Self {
        Self {
            _not_send: PhantomData,
            _lifetime: PhantomData,
        }
    }
}

/// Sleep deadline for [`yield_with_deadline`]; every `KernelIo` task must name
/// one when descheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deadline {
    Immediate,
    /// Wake no later than `ms` milliseconds from now. Resolution is bounded by
    /// the LAPIC tick (10 ms by default) plus the tickless-idle one-shot.
    AtMs(u32),
    /// Sleep until explicitly woken. Consumes no sleep-queue entry; the wake
    /// path runs the predicate and unblocks.
    Indefinite,
}

/// The only sleep API available to a [`KernelIoToken`] holder. `_token` is
/// taken by reference so one witness can drive many yields in a kthread loop.
#[inline]
pub fn yield_with_deadline<'cpu>(_token: &KernelIoToken<'cpu>, deadline: Deadline) {
    if let Some(yield_fn) = current_yield_backend() {
        yield_fn(deadline);
    }
    // With no backend registered (pre-boot, or a test fixture without a
    // scheduler) the yield is a no-op, matching `WaitQueue`'s pre-runtime
    // contract.
}

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::sync::BspToken;

/// Function-pointer backend for [`yield_with_deadline`], registered from
/// outside slopos-ostd at boot (`sched::runtime`) so the trusted core does not
/// depend on the scheduler crate.
pub type YieldBackend = fn(Deadline);

static YIELD_BACKEND: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the production yield backend. BSP-only via the `&BspToken<'brand>`
/// witness produced by [`crate::sync::run_bsp_init`].
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
    // SAFETY: `raw` was produced by `register_yield_backend` from a function
    // pointer of matching ABI.
    Some(unsafe { core::mem::transmute::<*mut (), YieldBackend>(raw) })
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_yield_backend_for_test() {
    YIELD_BACKEND.store(core::ptr::null_mut(), Ordering::Release);
}

/// Spawn a `KernelIo`-priority kthread.
///
/// `name` is a `'static` string baked into the binary; `entry` is a
/// `fn(KernelIoToken<'static>)`. The macro generates a hidden `fn()`
/// trampoline that constructs the token and calls `entry`, then dispatches to
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
/// That trampoline is the only path that instantiates a [`KernelIoToken`];
/// direct constructor calls fail the grep gate
/// `scripts/check_wait_predicate_purity.sh`.
#[macro_export]
macro_rules! spawn_kernel_io {
    ($name:expr, $entry:path $(,)?) => {{
        fn __slopos_kernel_io_trampoline() {
            // The trampoline is only reachable from a task slot the scheduler
            // validated as KernelIo priority: this macro is its sole emitter
            // and always pairs it with the priority argument below.
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

use crate::sync::lock_graph::LockClassKey;
use crate::sync::spin::SpinLock;
use crate::sync::wait_queue::WaitQueue;
use core::sync::atomic::AtomicBool;

/// Cooperative stop signal for one kernel-I/O thread.
///
/// Kernel tasks take no signals and are not killable: they need a stop they
/// can *finish* on — the ext2 flusher's last act is a full sync, which a kill
/// would discard. The queue lives inside the signal so the park and the
/// stop-wake cannot drift onto two different queues.
pub struct KernelIoStop {
    name: &'static str,
    requested: AtomicBool,
    exited: AtomicBool,
    wq: WaitQueue,
}

impl KernelIoStop {
    /// The lock class comes from the caller: these threads serve unrelated
    /// subsystems, and a class minted here would merge all of them.
    pub const fn new(name: &'static str, class: &'static LockClassKey) -> Self {
        Self {
            name,
            requested: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            wq: WaitQueue::new(class),
        }
    }

    #[inline]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The queue the thread parks on; producers wake it to hand over work.
    #[inline]
    pub const fn queue(&self) -> &WaitQueue {
        &self.wq
    }

    #[inline]
    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Ask the thread to stop, and wake it: the flag alone leaves a parked
    /// thread that never re-evaluates it.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        let _ = self.wq.wake_all();
    }

    /// Called by the thread once its loop has ended and its final work is done.
    #[inline]
    pub fn note_exited(&self) {
        self.exited.store(true, Ordering::Release);
    }

    #[inline]
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

/// Why a kernel-I/O park ended.
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
    /// between the caller's last check and the park is not lost.
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

/// A fixed array rather than a linker registry: the set is small and known at
/// boot, and a registry would add a `link.ld` section for four entries.
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

/// Make `stop` visible to [`request_kernel_io_stop_all`]; a thread that never
/// registers cannot be asked to stop.
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
/// order so a later thread that feeds an earlier one drains first.
///
/// Only asks: the bounded wait belongs to the caller, because waiting needs a
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

/// Registered kernel-I/O threads that have not yet reported finishing; zero
/// means every one of them ran its own exit path.
pub fn kernel_io_stops_pending() -> usize {
    let registry = STOP_REGISTRY.lock();
    registry.entries[..registry.count]
        .iter()
        .flatten()
        .filter(|stop| !stop.has_exited())
        .count()
}

/// Names of the kernel-I/O threads that have not finished, for the shutdown
/// report.
pub fn for_each_unstopped_kernel_io(mut report: impl FnMut(&'static str)) {
    let registry = STOP_REGISTRY.lock();
    for stop in registry.entries[..registry.count].iter().flatten() {
        if !stop.has_exited() {
            report(stop.name());
        }
    }
}
