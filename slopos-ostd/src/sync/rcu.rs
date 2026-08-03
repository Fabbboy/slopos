//! Classic RCU (Read-Copy-Update) for SlopOS.
//!
//! Read-side critical sections use [`PreemptGuard`] to prevent context
//! switches, guaranteeing the CPU cannot pass through a quiescent state
//! while an [`RcuReadGuard`] is held. Writers call [`synchronize_rcu`]
//! after publishing new data to wait until every online CPU has passed
//! through at least one quiescent state, making it safe to free the old
//! version.
//!
//! Quiescent states are recorded from four sites, and the two that run in
//! interrupt context report *conditionally*:
//! - The scheduler after each context switch — [`rcu_note_qs`], unconditional:
//!   a read-side section holds a [`PreemptGuard`], so reaching a switch proves
//!   the section has ended.
//! - Each iteration of the idle loop — [`rcu_note_qs`], same reasoning.
//! - The LAPIC timer tick handler (100 Hz on every CPU) —
//!   [`rcu_note_qs_from_interrupt`]. A section disables preemption but not
//!   interrupts, so the tick can land inside one.
//! - The dedicated RCU QS IPI handler (vector 0xFB) — likewise, and more
//!   sharply: [`synchronize_rcu`] sends that IPI to break a stall, so an
//!   unconditional report there would fake a quiescent state on the very CPU
//!   the grace period is waiting for.
//!
//! The two unconditional sites are what carry liveness: a tick that declines
//! only delays a grace period, because the next switch reports regardless.
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
use crate::sync::BspToken;

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
/// Call this only from a site that is a quiescent state *by construction* — a
/// context switch or the idle loop. A switch qualifies because a read-side
/// section holds a [`PreemptGuard`], so a CPU cannot switch away from inside
/// one; reaching a switch therefore proves the section has ended.
///
/// **Not safe from an interrupt handler.** A section disables preemption but
/// **not** interrupts, so an ISR can land in the middle of one; reporting a
/// quiescent state from there tells [`synchronize_rcu`] a reader has finished
/// when it has not, and the object it is reading can then be freed underneath
/// it. Interrupt context wants [`rcu_note_qs_from_interrupt`].
#[inline]
pub fn rcu_note_qs() {
    let cpu = get_current_cpu();
    if cpu < MAX_CPUS {
        RCU_QS_CTR[cpu].0.fetch_add(1, Ordering::Release);
    }
}

/// Record a quiescent state from an interrupt handler, if this CPU is in one.
///
/// Reports only when the preemption count is zero, which is exactly the
/// condition "this interrupt did not land inside an RCU read-side critical
/// section". Returns whether it reported.
///
/// Declining is always safe: it delays a grace period, never shortens one. The
/// switch and idle sites remain unconditional quiescent states, so liveness
/// does not depend on the tick — and a CPU that is preempt-disabled for some
/// unrelated reason (a spinlock, say) simply reports at its next switch.
///
/// This is the distinction Linux draws between a context switch, which is a
/// quiescent state outright, and a clock interrupt, which is one only if it did
/// not interrupt a reader.
#[inline]
pub fn rcu_note_qs_from_interrupt() -> bool {
    if PreemptGuard::is_active() {
        return false;
    }
    rcu_note_qs();
    true
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

// ---------------------------------------------------------------------------
// Function-pointer ops table — production-backend registration shape.
// ---------------------------------------------------------------------------

/// Function-pointer table that consumers use to wire up the production
/// RCU backend without taking a dependency on the OSTD-internal
/// [`RcuBackend`] trait shape. Every fn pointer must honour the
/// equivalent method's contract on [`RcuBackend`].
pub struct RcuOps {
    /// See [`RcuBackend::clock_monotonic_ns`].
    pub clock_monotonic_ns: fn() -> u64,
    /// See [`RcuBackend::log_warn`].
    pub log_warn: fn(args: core::fmt::Arguments<'_>),
}

struct OpsBackend(&'static RcuOps);

// SAFETY: every method delegates to the registered ops table; the
// caller of `register_rcu_backend` certifies the table honours the
// `RcuBackend` contract documented on each fn pointer.
unsafe impl RcuBackend for OpsBackend {
    fn clock_monotonic_ns(&self) -> u64 {
        (self.0.clock_monotonic_ns)()
    }
    fn log_warn(&self, args: core::fmt::Arguments<'_>) {
        (self.0.log_warn)(args)
    }
}

struct BackendSlot(UnsafeCell<MaybeUninit<OpsBackend>>);
// SAFETY: writes are gated by `BACKEND_INSTALLED.swap(true, AcqRel)`
// (one-shot); subsequent reads only happen after observing the flag
// with Acquire, so the read sees the published reference.
unsafe impl Sync for BackendSlot {}

static BACKEND_SLOT: BackendSlot = BackendSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the production RCU backend. The
/// `&BspToken<'brand>` witnesses BSP-only init; `ops` must live for
/// the static lifetime of the kernel.
pub fn register_rcu_backend<'brand>(_token: &BspToken<'brand>, ops: &'static RcuOps) {
    let was_installed = BACKEND_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(!was_installed, "register_rcu_backend called twice");
    // SAFETY: the swap above transitioned us from "uninstalled" to
    // "installed" exclusively; no other writer can be racing.
    unsafe {
        (*BACKEND_SLOT.0.get()).write(OpsBackend(ops));
    }
}

#[inline]
fn backend() -> &'static dyn RcuBackend {
    if !BACKEND_INSTALLED.load(Ordering::Acquire) {
        return &DEFAULT_BACKEND;
    }
    // SAFETY: paired Release in `register_rcu_backend`; the Acquire
    // load above synchronises with the publishing write.
    unsafe { (*BACKEND_SLOT.0.get()).assume_init_ref() }
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
    // frames that large. It is per-call because concurrent callers are
    // waiting on different instants.
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

            // A CPU spinning on a peer must keep acknowledging that peer's
            // shootdowns; `spin_relax` is the service call and carries no
            // `pause` of its own.
            crate::sync::spin_relax();
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
/// On OOM this takes a synchronous grace period *before* releasing `arg`,
/// mirroring [`call_rcu`]. The ordering is the whole point: a reader that
/// entered its read-side section before the publication must be allowed to
/// leave it before the old value dies, and an allocation failure is not a
/// licence to skip that.
pub fn rcu_call_typed<T: Send + 'static>(arg: crate::mm::KBox<T>, drop_fn: fn(crate::mm::KBox<T>)) {
    // The trampoline forms a closure-free function pointer compatible
    // with `RcuCallback = unsafe fn(*mut u8)`. Each `T` / `drop_fn`
    // pair monomorphises a distinct trampoline.
    struct Trampoline<T: Send + 'static> {
        _phantom: core::marker::PhantomData<T>,
    }
    impl<T: Send + 'static> Trampoline<T> {
        unsafe fn run(ptr: *mut u8) {
            // The pushed pointer is the (arg, drop_fn) pack the reservation
            // below allocated. Recover both halves, release the pack's own
            // storage symmetrically with `raw_alloc`, then run the callback.
            let pack = ptr as *mut TypedRcuPack<T>;
            // SAFETY: `pack` was allocated by the matching path in
            // `rcu_call_typed`; ownership transferred at that point, and this
            // trampoline runs exactly once per allocation.
            let TypedRcuPack { arg, drop_fn } = unsafe { pack.read() };
            // SAFETY: symmetric with the `raw_alloc` below, same layout.
            unsafe { raw_dealloc(pack.cast::<u8>(), Layout::new::<TypedRcuPack<T>>()) };
            drop_fn(arg);
        }
    }

    struct TypedRcuPack<T: Send + 'static> {
        arg: crate::mm::KBox<T>,
        drop_fn: fn(crate::mm::KBox<T>),
    }

    // Reserve the pack's storage *before* taking ownership of `arg`. Moving
    // `arg` into a fallible constructor instead would hand it to a callee that
    // drops it on failure — freeing the payload with no grace period at all,
    // which is exactly the use-after-free the deferral exists to prevent, and
    // is invisible because the failure path still logs that it waited.
    let layout = Layout::new::<TypedRcuPack<T>>();
    // SAFETY: `TypedRcuPack<T>` holds a `KBox` and a fn pointer, so its layout
    // is non-zero-sized.
    let pack = unsafe { raw_alloc(layout) }.cast::<TypedRcuPack<T>>();
    if pack.is_null() {
        backend().log_warn(format_args!(
            "RCU: rcu_call_typed allocation failed, falling back to synchronous grace period"
        ));
        synchronize_rcu();
        drop_fn(arg);
        return;
    }
    // SAFETY: freshly allocated, correctly aligned for `TypedRcuPack<T>`, and
    // uniquely owned until `call_rcu` publishes it.
    unsafe { pack.write(TypedRcuPack { arg, drop_fn }) };

    // SAFETY: `Trampoline::<T>::run` is sound iff the pointer is the matching
    // `TypedRcuPack<T>` allocation, which it is. After a grace period elapses,
    // no reader can hold a reference to the value inside `arg`. `call_rcu`'s
    // own OOM path takes the grace period before invoking the callback, so the
    // ordering holds even when the node cannot be allocated either.
    unsafe {
        call_rcu(pack.cast::<u8>(), Trampoline::<T>::run);
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

// ---------------------------------------------------------------------------
// RcuCell<T> — safe RCU-protected atomic-pointer cell.
// ---------------------------------------------------------------------------

/// RCU-protected single-slot cell of `KBox<T>`.
///
/// Stores at most one heap-owned `T`. Readers obtain an
/// [`RcuCellGuard`] which holds an [`RcuReadGuard`] for its lifetime
/// and derefs to `&T`. Writers replace the contents with another
/// `KBox<T>`; the displaced value is freed after a grace period via
/// [`rcu_call_typed`].
///
/// Designed for the global glyph atlas / per-namespace policy table
/// pattern: hot-path readers are wait-free, writers are rare and
/// pay for the grace period.
///
/// `T: Send + 'static` is required because writers may need to free
/// the displaced box from a different CPU after the grace period.
///
/// Use [`RcuCell::store_unchecked`] for pre-scheduler init when no
/// concurrent reader can exist — the displaced box is dropped
/// synchronously, sidestepping the deferred-free fast path.
pub struct RcuCell<T: Send + 'static> {
    ptr: AtomicPtr<T>,
}

/// RAII guard returned by [`RcuCell::load`]. Derefs to `&T` and holds
/// the RCU read-side critical section open for its lifetime.
#[must_use = "dropping the guard immediately ends the RCU read-side critical section"]
pub struct RcuCellGuard<T: 'static> {
    _rcu: RcuReadGuard,
    ptr: *const T,
    _not_send_sync: core::marker::PhantomData<*mut ()>,
}

impl<T: 'static> core::ops::Deref for RcuCellGuard<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `ptr` is non-null (guarded by `RcuCell::load`'s
        // null-check) and remains valid for the duration of the
        // embedded `RcuReadGuard` — writers do not free until the
        // next grace period boundary, which cannot be observed by
        // this CPU while preemption is disabled inside `_rcu`.
        unsafe { &*self.ptr }
    }
}

impl<T: Send + 'static> RcuCell<T> {
    /// Construct an empty cell.
    #[inline]
    pub const fn empty() -> Self {
        Self {
            ptr: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// Load the current value under an RCU read-side critical section.
    /// Returns `None` if the cell is empty.
    #[inline]
    pub fn load(&self) -> Option<RcuCellGuard<T>> {
        let rcu = rcu_read_lock();
        let ptr = self.ptr.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            Some(RcuCellGuard {
                _rcu: rcu,
                ptr: ptr as *const T,
                _not_send_sync: core::marker::PhantomData,
            })
        }
    }

    /// Replace the cell contents with `new_value`. The displaced
    /// `KBox<T>` (if any) is scheduled for deferred drop via
    /// [`rcu_call_typed`] so concurrent readers can complete safely.
    ///
    /// Returns the raw `*mut T` of the displaced value (may be null
    /// on first publish).
    pub fn replace(&self, new_value: crate::mm::KBox<T>) -> *mut T {
        let new_ptr = crate::mm::KBox::into_raw(new_value);
        let old = self.ptr.swap(new_ptr, Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: `old` was produced by a previous `into_raw` on
            // a `KBox<T>` we owned exclusively; reclaim ownership for
            // the deferred drop.
            let old_box = unsafe { crate::mm::KBox::from_raw(old) };
            rcu_call_typed::<T>(old_box, drop_typed::<T>);
        }
        old
    }

    /// Pre-scheduler-only store: replace the cell contents and drop
    /// the displaced value immediately. Sound only when the caller
    /// can prove no concurrent reader exists (e.g. the call happens
    /// before the scheduler starts).
    pub fn store_pre_scheduler(&self, new_value: crate::mm::KBox<T>) {
        let new_ptr = crate::mm::KBox::into_raw(new_value);
        let old = self.ptr.swap(new_ptr, Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: caller asserts no reader is observing `old`.
            drop(unsafe { crate::mm::KBox::from_raw(old) });
        }
    }
}

// SAFETY: `RcuCell<T>` shares `T` across threads via the embedded
// `AtomicPtr`. `T: Send + 'static` ensures the publishable value can
// cross thread boundaries; the RCU machinery serialises observation.
unsafe impl<T: Send + 'static> Send for RcuCell<T> {}
unsafe impl<T: Send + Sync + 'static> Sync for RcuCell<T> {}

/// RCU-protected slot holding at most one [`KArc<T>`].
///
/// The refcounted sibling of [`RcuCell`]. Where `RcuCell` owns a `KBox<T>` and
/// lends readers a guard that borrows it, this owns one *strong reference* and
/// gives each reader one of their own — so a reader's handle stays valid after
/// the slot has moved on. That is what a shared object with independent
/// lifetime, like a process group, needs from a field that a *different* task
/// may replace at any moment.
///
/// - **Read** — preemption off, one acquire load, one refcount increment.
///   Wait-free: no lock, no allocation, and no ordering against the writer
///   beyond the load itself.
/// - **Write** — one swap, then the displaced reference is released after a
///   grace period. So the destructor never runs on the writer's stack, never
///   under the writer's locks, and never with interrupts off.
///
/// # Why the read side holds the section across the increment
///
/// Loading the pointer and incrementing its refcount are two steps, and a
/// writer may swap the slot between them. What makes the increment sound is
/// not that it beat the writer, but that the writer's *release* cannot run
/// until every CPU has passed through a quiescent state — and this CPU cannot,
/// because [`rcu_read_lock`] holds preemption off across both steps. Shrinking
/// the section to cover only the load would reintroduce exactly the
/// resurrection race the deferral exists to prevent.
///
/// `T: Send + Sync` because a reference published here can be cloned, read and
/// finally released by a CPU other than the one that stored it.
pub struct RcuArcSlot<T: Send + Sync + 'static> {
    /// A strong reference parked as a raw pointer (`KArc::into_raw`), or null.
    ptr: AtomicPtr<T>,
}

impl<T: Send + Sync + 'static> RcuArcSlot<T> {
    /// An empty slot. `const`, so it can initialise a struct field.
    #[inline]
    pub const fn empty() -> Self {
        Self {
            ptr: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// Take an owning handle on the current contents, or `None` if empty.
    #[inline]
    pub fn load(&self) -> Option<crate::mm::KArc<T>> {
        let _rcu = rcu_read_lock();
        let raw = self.ptr.load(Ordering::Acquire);
        if raw.is_null() {
            return None;
        }
        // Reconstruct the slot's parked reference without taking it, clone one
        // fresh reference from it, then hand the borrow back untouched — the
        // same balanced borrow the task placement primitives use.
        // SAFETY: `raw` was produced by `KArc::into_raw` on a reference this
        // slot still owns. A concurrent `store` cannot have released it yet:
        // that release is deferred past a grace period, which cannot complete
        // while `_rcu` holds preemption off on this CPU.
        let borrowed = unsafe { crate::mm::KArc::from_raw(raw.cast_const()) };
        let cloned = borrowed.clone();
        let _ = crate::mm::KArc::into_raw(borrowed);
        Some(cloned)
    }

    /// Publish `value`, releasing the displaced reference after a grace period.
    ///
    /// Must not be called with interrupts disabled or a lock held that the RCU
    /// machinery could need — this is a writer path, and writers are rare.
    pub fn store(&self, value: Option<crate::mm::KArc<T>>) {
        let new_raw = match value {
            Some(arc) => crate::mm::KArc::into_raw(arc).cast_mut(),
            None => core::ptr::null_mut(),
        };
        let old = self.ptr.swap(new_raw, Ordering::AcqRel);
        if old.is_null() {
            return;
        }
        // SAFETY: `old` is the reference this slot owned until the swap, and
        // the swap made this call its unique owner, so the deferred release
        // happens exactly once. `drop_karc::<T>` is the matching release, and
        // deferring it past a grace period is what lets a concurrent `load`
        // finish its increment first. `call_rcu`'s own OOM path takes a
        // synchronous grace period before invoking the callback, so the
        // ordering holds even when the deferral cannot be allocated.
        unsafe { call_rcu(old.cast::<u8>(), drop_karc::<T>) };
    }

    /// Replace the contents with exclusivity proven by `&mut self` rather than
    /// by a grace period, returning the displaced handle to the caller.
    ///
    /// Reachable only where no reader can exist — construction, before the
    /// containing object is published, and [`Drop`].
    #[inline]
    pub fn replace_exclusive(
        &mut self,
        value: Option<crate::mm::KArc<T>>,
    ) -> Option<crate::mm::KArc<T>> {
        let new_raw = match value {
            Some(arc) => crate::mm::KArc::into_raw(arc).cast_mut(),
            None => core::ptr::null_mut(),
        };
        let old = *self.ptr.get_mut();
        *self.ptr.get_mut() = new_raw;
        if old.is_null() {
            return None;
        }
        // SAFETY: `old` is the reference this slot owned, and `&mut self`
        // proves no other observer exists, so taking it back is balanced.
        Some(unsafe { crate::mm::KArc::from_raw(old.cast_const()) })
    }

    /// Whether the slot currently holds a reference. Racy by nature; for
    /// diagnostics and assertions, never for deciding to dereference.
    #[inline]
    pub fn is_empty_racy(&self) -> bool {
        self.ptr.load(Ordering::Relaxed).is_null()
    }
}

impl<T: Send + Sync + 'static> Default for RcuArcSlot<T> {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

impl<T: Send + Sync + 'static> Drop for RcuArcSlot<T> {
    fn drop(&mut self) {
        // The slot owns one reference; releasing it here is what keeps a
        // dropped container from leaking its member. `&mut self` proves no
        // reader can be mid-clone, so no grace period is needed.
        drop(self.replace_exclusive(None));
    }
}

/// Release one `KArc<T>` parked by [`RcuArcSlot::store`].
///
/// # Safety
/// `raw` must be the `KArc::into_raw` pointer a single `RcuArcSlot::store`
/// displaced, and a grace period must have elapsed since that swap.
unsafe fn drop_karc<T: Send + Sync + 'static>(raw: *mut u8) {
    // SAFETY: forwarded from the caller contract above.
    drop(unsafe { crate::mm::KArc::<T>::from_raw(raw.cast::<T>().cast_const()) });
}

fn drop_typed<T: Send + 'static>(_b: crate::mm::KBox<T>) {
    // `KBox::drop` releases the heap allocation.
}

/// A CPU's quiescent-state counter.
///
/// Diagnostics, and the only way to tell a *declined* report from a silent one:
/// asserting `rcu_note_qs_from_interrupt` returned `false` would pass just as
/// happily against a version that always declined and never reported, which is
/// a liveness bug rather than a soundness one but still a bug a test should be
/// able to see. Ungated rather than `test-helpers`-gated because it is a plain
/// atomic load with no more privilege than `synchronize_rcu` already exercises,
/// and `slopos-ostd/test-helpers` is not in the kernel test build's feature
/// chain.
pub fn rcu_qs_counter(cpu: usize) -> u64 {
    if cpu >= MAX_CPUS {
        return 0;
    }
    RCU_QS_CTR[cpu].0.load(Ordering::Acquire)
}

/// Test-only reset hook.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_backend_for_test() {
    BACKEND_INSTALLED.store(false, Ordering::Release);
}
