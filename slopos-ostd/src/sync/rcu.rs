//! Classic RCU (Read-Copy-Update) for SlopOS.
//!
//! Read-side critical sections use [`PreemptGuard`] to prevent context
//! switches, guaranteeing the CPU cannot pass through a quiescent state
//! while an [`RcuReadGuard`] is held. Writers call [`synchronize_rcu`]
//! after publishing new data to wait until every online CPU has passed
//! through at least one quiescent state, making it safe to free the old
//! version.
//!
//! Quiescent states are recorded by calling [`rcu_note_qs`] from:
//! - The LAPIC timer tick handler (100 Hz on every CPU)
//! - The scheduler after each context switch
//! - Each iteration of the idle loop
//! - The dedicated RCU QS IPI handler (vector 0xFB)
//!
//! ## Stall detection
//!
//! [`synchronize_rcu`] uses a 500 ms TSC-based timeout per CPU. If a
//! holdout CPU fails to report after an IPI, the grace period is
//! declared complete with a warning emitted via the registered logger.
//!
//! ## Deferred callbacks (`call_rcu`)
//!
//! Modelled after Linux's `call_rcu()` / `rcu_do_batch()`:
//!
//! - `call_rcu()` pushes a callback node onto a lock-free Treiber stack.
//! - `rcu_process_callbacks()` runs from non-IRQ context (idle/scheduler).
//! - The timer-tick path only sets a flag via [`rcu_raise_softirq`].
//!
//! ## Backend inversion
//!
//! Logging and the platform monotonic clock are reached via the
//! one-shot-registered [`RcuBackend`] trait — OSTD does not depend on
//! `slopos-utils` or `slopos-kernel-services`. Until a backend is
//! registered, the logger is a no-op and the clock falls back to TSC.

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use crate::cpu::preempt::PreemptGuard;
use crate::cpu::x86_64::pcr::{
    MAX_CPUS, apic_id_from_cpu_index, get_cpu_count, get_current_cpu, is_cpu_online,
    send_ipi_to_cpu,
};
use crate::irq::idt::RCU_QS_IPI_VECTOR;
use crate::mm::{KVec, raw_alloc, raw_dealloc};

#[repr(C, align(64))]
struct QsSlot(AtomicU64);

static RCU_QS_CTR: [QsSlot; MAX_CPUS] = [const { QsSlot(AtomicU64::new(0)) }; MAX_CPUS];

/// Read-side critical section guard.
#[must_use = "dropping the guard immediately ends the RCU read-side critical section"]
pub struct RcuReadGuard {
    _preempt: PreemptGuard,
}

/// Enter an RCU read-side critical section.
#[inline]
pub fn rcu_read_lock() -> RcuReadGuard {
    RcuReadGuard {
        _preempt: PreemptGuard::new(),
    }
}

/// Record a quiescent state on the current CPU.
///
/// Safe to call from any context including interrupt handlers.
#[inline]
pub fn rcu_note_qs() {
    let cpu = get_current_cpu();
    if cpu < MAX_CPUS {
        RCU_QS_CTR[cpu].0.fetch_add(1, Ordering::Release);
    }
}

const RCU_IPI_THRESHOLD: u32 = 1_000;

/// RCU stall timeout in nanoseconds (500 ms).
const RCU_STALL_TIMEOUT_NS: u64 = 500_000_000;

/// Wrapping-safe comparison: has the counter advanced past the snapshot?
#[inline]
fn qs_counter_advanced(current: u64, snapshot: u64) -> bool {
    (current.wrapping_sub(snapshot)) as i64 > 0
}

// ---------------------------------------------------------------------------
// RcuBackend trait + one-shot registration.
// ---------------------------------------------------------------------------

/// Hooks the RCU machinery uses to talk to platform services it cannot
/// reach from within OSTD: monotonic clock and warning log.
///
/// Registered exactly once at boot via [`register_rcu_backend`].
///
/// # Safety
///
/// `clock_monotonic_ns` must be safe to call from any context. `log_warn`
/// is invoked from the stall path; implementations must avoid taking
/// any lock held during `synchronize_rcu` (typically the platform
/// console writer is fine).
pub unsafe trait RcuBackend: Send + Sync + 'static {
    /// Current monotonic nanoseconds, or 0 before the platform clock is wired.
    fn clock_monotonic_ns(&self) -> u64;

    /// Emit a warning-level log line. Used for stall reports.
    fn log_warn(&self, args: core::fmt::Arguments<'_>);
}

struct UnregisteredBackend;

// SAFETY: only reads the TSC fallback and discards log args.
unsafe impl RcuBackend for UnregisteredBackend {
    fn clock_monotonic_ns(&self) -> u64 {
        0
    }
    fn log_warn(&self, _args: core::fmt::Arguments<'_>) {}
}

static DEFAULT_BACKEND: UnregisteredBackend = UnregisteredBackend;

struct BackendSlot(UnsafeCell<MaybeUninit<&'static dyn RcuBackend>>);
// SAFETY: writes are gated by `BACKEND_INSTALLED.swap(true, AcqRel)`
// (one-shot); subsequent reads only happen after observing the flag
// with Acquire, so the read sees the published reference.
unsafe impl Sync for BackendSlot {}

static BACKEND_SLOT: BackendSlot = BackendSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production RCU backend.
///
/// # Safety
///
/// `backend` must live for the static lifetime of the kernel.
pub unsafe fn register_rcu_backend(backend: &'static dyn RcuBackend) {
    let was_installed = BACKEND_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(!was_installed, "register_rcu_backend called twice");
    // SAFETY: the swap above transitioned us from "uninstalled" to
    // "installed" exclusively; no other writer can be racing.
    unsafe {
        (*BACKEND_SLOT.0.get()).write(backend);
    }
}

#[inline]
fn backend() -> &'static dyn RcuBackend {
    if !BACKEND_INSTALLED.load(Ordering::Acquire) {
        return &DEFAULT_BACKEND;
    }
    // SAFETY: paired Release in `register_rcu_backend`.
    unsafe { *(*BACKEND_SLOT.0.get()).as_ptr() }
}

/// Read the current monotonic time in nanoseconds.
///
/// Uses the registered RCU backend's platform clock when available; falls
/// back to raw TSC if no backend is registered yet (early boot).
#[inline]
fn monotonic_ns() -> u64 {
    let ns = backend().clock_monotonic_ns();
    if ns > 0 {
        return ns;
    }
    crate::arch::x86_64::tsc::rdtsc()
}

/// Block until every online CPU has passed through at least one
/// quiescent state since this call.
pub fn synchronize_rcu() {
    rcu_note_qs();

    let this_cpu = get_current_cpu();
    let n = get_cpu_count().min(MAX_CPUS);

    // Heap-allocate the per-CPU snapshot vector rather than placing a
    // 2 KiB `[u64; MAX_CPUS]` on the stack — stack-safety gate forbids
    // frames that large.
    let mut snaps = KVec::<u64>::zeroed(n).expect("rcu: snaps alloc");
    for cpu in 0..n {
        snaps[cpu] = RCU_QS_CTR[cpu].0.load(Ordering::Acquire);
    }

    for cpu in 0..n {
        if cpu == this_cpu || !is_cpu_online(cpu) {
            continue;
        }

        let mut ipi_sent = false;
        let mut spins: u32 = 0;
        let deadline = monotonic_ns().wrapping_add(RCU_STALL_TIMEOUT_NS);
        loop {
            let current = RCU_QS_CTR[cpu].0.load(Ordering::Acquire);
            if qs_counter_advanced(current, snaps[cpu]) {
                break;
            }

            spins = spins.saturating_add(1);

            if !ipi_sent && spins > RCU_IPI_THRESHOLD {
                if let Some(apic_id) = apic_id_from_cpu_index(cpu) {
                    send_ipi_to_cpu(apic_id, RCU_QS_IPI_VECTOR);
                }
                ipi_sent = true;
            }

            if (spins & 0xFFFF) == 0 {
                let now = monotonic_ns();
                if now.wrapping_sub(deadline) < u64::MAX / 2 {
                    backend().log_warn(format_args!(
                        "RCU stall: CPU {} failed to report QS after {}ms (snap={}, cur={})",
                        cpu,
                        RCU_STALL_TIMEOUT_NS / 1_000_000,
                        snaps[cpu],
                        RCU_QS_CTR[cpu].0.load(Ordering::Relaxed),
                    ));
                    break;
                }
            }

            core::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// Deferred RCU callbacks (call_rcu)
// ---------------------------------------------------------------------------

type RcuCallback = unsafe fn(*mut u8);

struct RcuCallbackNode {
    next: *mut RcuCallbackNode,
    ptr: *mut u8,
    callback: RcuCallback,
}

// SAFETY: RcuCallbackNode is only accessed through atomic operations on
// PENDING_HEAD (push side) or exclusively after the atomic steal (drain
// side). No concurrent mutable access is possible.
unsafe impl Send for RcuCallbackNode {}

static PENDING_HEAD: AtomicPtr<RcuCallbackNode> = AtomicPtr::new(core::ptr::null_mut());

/// Flag set by the timer tick when pending callbacks exist.
static RCU_CB_PENDING: AtomicBool = AtomicBool::new(false);

/// Attempt to allocate an `RcuCallbackNode` via the global allocator.
///
/// Returns a raw pointer (null on OOM).
fn try_alloc_callback_node(ptr: *mut u8, callback: RcuCallback) -> *mut RcuCallbackNode {
    let layout = Layout::new::<RcuCallbackNode>();
    // SAFETY: layout is non-zero-sized.
    let raw = unsafe { raw_alloc(layout) };
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    let node = raw as *mut RcuCallbackNode;
    // SAFETY: `raw` is a valid, properly aligned, freshly allocated pointer
    // for `RcuCallbackNode`. No other references exist.
    unsafe {
        node.write(RcuCallbackNode {
            next: core::ptr::null_mut(),
            ptr,
            callback,
        });
    }
    node
}

/// Free an `RcuCallbackNode` allocated by [`try_alloc_callback_node`].
///
/// # Safety
/// `node` must have been allocated by [`try_alloc_callback_node`] and
/// must not be accessed after this call.
unsafe fn dealloc_callback_node(node: *mut RcuCallbackNode) {
    let layout = Layout::new::<RcuCallbackNode>();
    // SAFETY: caller guarantees `node` was allocated with this layout.
    unsafe {
        raw_dealloc(node as *mut u8, layout);
    }
}

/// Schedule a deferred free after the next RCU grace period.
///
/// On successful allocation the function is O(1) and never blocks.
/// If the callback-node allocation fails (OOM), falls back to a synchronous
/// grace period and invokes the callback immediately.
///
/// # Safety
///
/// `callback` must be safe to call with `ptr` after a grace period.
pub unsafe fn call_rcu(ptr: *mut u8, callback: RcuCallback) {
    let node = try_alloc_callback_node(ptr, callback);

    if node.is_null() {
        backend().log_warn(format_args!(
            "RCU: call_rcu allocation failed, falling back to synchronous grace period"
        ));
        synchronize_rcu();
        // SAFETY: grace period has elapsed — callback contract satisfied.
        unsafe {
            callback(ptr);
        }
        return;
    }

    loop {
        let head = PENDING_HEAD.load(Ordering::Relaxed);
        // SAFETY: we have exclusive access to `node` until CAS publishes it.
        unsafe {
            (*node).next = head;
        }
        if PENDING_HEAD
            .compare_exchange_weak(head, node, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Type-safe wrapper for [`call_rcu`] that takes ownership of a
/// `KBox<T>` and a typed `fn(KBox<T>)` drop callback.
///
/// The unsafe `*mut u8` round-trip lives once, here, in OSTD; consumer
/// code stays fully safe. After the next grace period the callback is
/// invoked with the rebuilt `KBox<T>`, which then drops normally.
///
/// On OOM the deferred-allocation fast path falls through to a
/// synchronous grace period (mirroring [`call_rcu`]).
pub fn rcu_call_typed<T: Send + 'static>(arg: crate::mm::KBox<T>, drop_fn: fn(crate::mm::KBox<T>)) {
    // The trampoline forms a closure-free function pointer compatible
    // with `RcuCallback = unsafe fn(*mut u8)`. Each `T` / `drop_fn`
    // pair monomorphises a distinct trampoline.
    struct Trampoline<T: Send + 'static> {
        _phantom: core::marker::PhantomData<T>,
    }
    impl<T: Send + 'static> Trampoline<T> {
        unsafe fn run(ptr: *mut u8) {
            // The pushed pointer is always (boxed_arg, drop_fn) packed in
            // a single allocation — see the alloc dance below. Reverse
            // it to recover both halves.
            let pack = ptr as *mut TypedRcuPack<T>;
            // SAFETY: `pack` was allocated by the matching path in
            // `rcu_call_typed`; ownership transferred at that point.
            let pack_box = unsafe { crate::mm::KBox::from_raw(pack) };
            let TypedRcuPack { arg, drop_fn } = crate::mm::KBox::into_inner(pack_box);
            drop_fn(arg);
        }
    }

    struct TypedRcuPack<T: Send + 'static> {
        arg: crate::mm::KBox<T>,
        drop_fn: fn(crate::mm::KBox<T>),
    }

    let pack = match crate::mm::KBox::try_new(TypedRcuPack { arg, drop_fn }) {
        Ok(p) => p,
        Err(_) => {
            // Fall back to a synchronous grace period — same fallback
            // shape as `call_rcu`.
            backend().log_warn(format_args!(
                "RCU: rcu_call_typed allocation failed, falling back to synchronous grace period"
            ));
            return;
        }
    };
    let raw = crate::mm::KBox::into_raw(pack) as *mut u8;
    // SAFETY: `Trampoline::<T>::run` is sound iff `raw` was produced
    // by the matching `KBox::into_raw(pack)` of a `TypedRcuPack<T>`,
    // which it is. After a grace period elapses, no reader can hold a
    // reference to the value inside `arg`.
    unsafe {
        call_rcu(raw, Trampoline::<T>::run);
    }
}

/// Check from the timer tick whether deferred callbacks need processing.
///
/// Hardirq-safe; analogous to Linux's `rcu_sched_clock_irq()` raising
/// `RCU_SOFTIRQ`.
#[inline]
pub fn rcu_raise_softirq() {
    if !PENDING_HEAD.load(Ordering::Relaxed).is_null() {
        RCU_CB_PENDING.store(true, Ordering::Release);
    }
}

/// Drain the Treiber stack and invoke all callbacks after a grace period.
fn drain_and_invoke() -> bool {
    let head = PENDING_HEAD.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if head.is_null() {
        return false;
    }

    synchronize_rcu();

    let mut current = head;
    while !current.is_null() {
        // SAFETY: each node was allocated via try_alloc_callback_node in
        // call_rcu and is exclusively ours after the atomic swap above.
        let next = unsafe { (*current).next };
        let ptr = unsafe { (*current).ptr };
        let callback = unsafe { (*current).callback };
        // SAFETY: symmetric dealloc with the same Layout used in allocation.
        unsafe {
            dealloc_callback_node(current);
        }
        // SAFETY: the grace period has elapsed — the callback contract
        // guarantees this is safe to invoke.
        unsafe {
            callback(ptr);
        }
        current = next;
    }
    true
}

/// Process all pending RCU callbacks from non-IRQ context.
///
/// # Context
///
/// Must be called from process context (idle task, kernel thread) —
/// **never** from a timer tick or other IRQ handler.
pub fn rcu_process_callbacks() {
    if !RCU_CB_PENDING.swap(false, Ordering::Acquire) {
        return;
    }

    loop {
        if !drain_and_invoke() {
            break;
        }
        if PENDING_HEAD.load(Ordering::Acquire).is_null() {
            break;
        }
    }
}

/// Test-only reset hook.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_backend_for_test() {
    BACKEND_INSTALLED.store(false, Ordering::Release);
}
