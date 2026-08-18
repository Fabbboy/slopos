//! Classic RCU (Read-Copy-Update) for SlopOS.
//!
//! Read-side critical sections hold a [`PreemptGuard`], so a CPU cannot pass
//! through a quiescent state while an [`RcuReadGuard`] is live. Writers call
//! [`synchronize_rcu`] to wait for a grace period, or [`call_rcu`] to defer the
//! free until after one.
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
use crate::mm::{raw_alloc, raw_dealloc};
use crate::sync::BspToken;

#[repr(C, align(64))]
struct QsSlot(AtomicU64);

static RCU_QS_CTR: [QsSlot; MAX_CPUS] = [const { QsSlot(AtomicU64::new(0)) }; MAX_CPUS];

#[must_use = "dropping the guard immediately ends the RCU read-side critical section"]
pub struct RcuReadGuard {
    _preempt: PreemptGuard,
}

#[inline]
pub fn rcu_read_lock() -> RcuReadGuard {
    RcuReadGuard {
        _preempt: PreemptGuard::new(),
    }
}

/// Record a quiescent state on the current CPU.
///
/// Only from a site that is a quiescent state *by construction* — a context
/// switch or the idle loop, where a read-side section's [`PreemptGuard`] proves
/// the section has ended.
///
/// **Not safe from an interrupt handler.** A section disables preemption but
/// **not** interrupts, so an ISR can land in the middle of one; interrupt
/// context wants [`rcu_note_qs_from_interrupt`].
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
/// does not depend on the tick.
#[inline]
pub fn rcu_note_qs_from_interrupt() -> bool {
    if PreemptGuard::is_active() {
        return false;
    }
    rcu_note_qs();
    true
}

/// Whether each CPU is parked in its idle loop with interrupts enabled.
///
/// A halted CPU stops reporting, and reaching the halt means passing through
/// the idle loop, so it is provably not inside a read-side critical section.
/// Marking the state lets a period complete across it instead of stalling.
static RCU_CPU_IDLE: [QsSlot; MAX_CPUS] = [const { QsSlot(AtomicU64::new(0)) }; MAX_CPUS];

/// Enter the extended quiescent state: this CPU is about to halt.
#[inline]
pub fn rcu_note_cpu_idle_enter() {
    let cpu = get_current_cpu();
    if cpu < MAX_CPUS {
        RCU_CPU_IDLE[cpu].0.store(1, Ordering::Release);
    }
}

/// Leave the extended quiescent state, and report: the stretch just ended was
/// one, and a period that starts before the next report must still see it.
#[inline]
pub fn rcu_note_cpu_idle_exit() {
    let cpu = get_current_cpu();
    if cpu < MAX_CPUS {
        RCU_CPU_IDLE[cpu].0.store(0, Ordering::Release);
    }
    rcu_note_qs();
}

/// Has `cpu` satisfied the in-flight period — by reporting, or by being asleep?
#[inline]
fn cpu_is_quiescent(cpu: usize) -> bool {
    if RCU_CPU_IDLE[cpu].0.load(Ordering::Acquire) != 0 {
        return true;
    }
    qs_counter_advanced(
        RCU_QS_CTR[cpu].0.load(Ordering::Acquire),
        GP_SNAP[cpu].0.load(Ordering::Acquire),
    )
}

const RCU_IPI_THRESHOLD: u32 = 1_000;

/// RCU stall timeout in nanoseconds (500 ms).
const RCU_STALL_TIMEOUT_NS: u64 = 500_000_000;

/// Wrapping-safe comparison: has the counter advanced past the snapshot?
#[inline]
fn qs_counter_advanced(current: u64, snapshot: u64) -> bool {
    (current.wrapping_sub(snapshot)) as i64 > 0
}

/// Grace-period sequence. Bit 0 set means a period is in flight; the rest
/// counts completions.
static GP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-CPU quiescent-state counters as of the in-flight period's start.
///
/// A static rather than a stack array or a `KVec`: `[u64; MAX_CPUS]` is exactly
/// the stack gate's 2 KiB threshold, and reclamation must not be able to fail
/// an allocation. Written only by whichever CPU wins the claim in
/// [`gp_start_if_idle`].
static GP_SNAP: [QsSlot; MAX_CPUS] = [const { QsSlot(AtomicU64::new(0)) }; MAX_CPUS];

/// Which sequence [`GP_SNAP`] currently describes.
///
/// The claim and the snapshot cannot be one atomic step, so this is what says
/// the snapshot is ready. Without it a peer polling between the two would read
/// the *previous* period's snapshot, find every counter long since advanced, and
/// declare a period complete that never ran — freeing under live readers.
static GP_SNAP_SEQ: AtomicU64 = AtomicU64::new(0);

const GP_IN_FLIGHT: u64 = 1;

/// The sequence at which a period started *now* would be complete.
///
/// Rounds past an in-flight period: that one may have begun before the caller
/// unpublished its object, so waiting for it is not sufficient.
#[inline]
fn gp_snap(seq: u64) -> u64 {
    (seq + 3) & !GP_IN_FLIGHT
}

/// Wrapping-safe: has `seq` reached `target`?
#[inline]
fn gp_done(seq: u64, target: u64) -> bool {
    seq.wrapping_sub(target) as i64 >= 0
}

/// Start a grace period if none is in flight.
///
/// Interrupts stay off across claim and publish so the window in which peers
/// decline to poll is a few hundred cycles rather than a scheduling quantum.
/// Correctness rests on [`GP_SNAP_SEQ`], not on this.
fn gp_start_if_idle() {
    crate::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        let seq = GP_SEQ.load(Ordering::Acquire);
        if seq & GP_IN_FLIGHT != 0 {
            return;
        }
        if GP_SEQ
            .compare_exchange(seq, seq + 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return; // a peer started one; its snapshot is as good as ours
        }
        for cpu in 0..get_cpu_count().min(MAX_CPUS) {
            GP_SNAP[cpu]
                .0
                .store(RCU_QS_CTR[cpu].0.load(Ordering::Acquire), Ordering::Relaxed);
        }
        GP_SNAP_SEQ.store(seq + 1, Ordering::Release);
    });
}

/// Advance the grace-period state machine if every online CPU has reported.
///
/// Loads plus at most one compare-exchange: no lock, no allocation, no wait, so
/// it is legal from a hard IRQ handler by the same argument
/// [`crate::sync`]'s other tick-driven pollers make. Driven from every CPU's
/// timer tick, so a period completes on whichever CPU notices first.
pub fn rcu_gp_poll() {
    let seq = GP_SEQ.load(Ordering::Acquire);
    if seq & GP_IN_FLIGHT == 0 {
        return;
    }
    if GP_SNAP_SEQ.load(Ordering::Acquire) != seq {
        return; // claimed, snapshot not yet published
    }
    for cpu in 0..get_cpu_count().min(MAX_CPUS) {
        if is_cpu_online(cpu) && !cpu_is_quiescent(cpu) {
            return;
        }
    }
    let _ = GP_SEQ.compare_exchange(seq, seq + 1, Ordering::AcqRel, Ordering::Relaxed);
}

/// The grace-period sequence: completions in the high bits, in-flight in bit 0.
#[inline]
pub fn rcu_gp_seq() -> u64 {
    GP_SEQ.load(Ordering::Acquire)
}

/// Nudge CPUs that have not yet reported for the in-flight period.
///
/// The IPI handler reports a quiescent state if it did not land inside a
/// reader, which is what breaks a CPU that is busy in a long kernel path with
/// no context switch of its own.
fn kick_holdouts() {
    let this_cpu = get_current_cpu();
    for cpu in 0..get_cpu_count().min(MAX_CPUS) {
        if cpu == this_cpu || !is_cpu_online(cpu) {
            continue;
        }
        if cpu_is_quiescent(cpu) {
            continue;
        }
        if let Some(apic_id) = apic_id_from_cpu_index(cpu) {
            send_ipi_to_cpu(apic_id, RCU_QS_IPI_VECTOR);
        }
    }
}

/// Name a CPU that has not reported, for the stall report.
fn holdout_cpu() -> Option<usize> {
    (0..get_cpu_count().min(MAX_CPUS)).find(|&cpu| is_cpu_online(cpu) && !cpu_is_quiescent(cpu))
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

/// Block until a grace period that began after this call has elapsed.
///
/// Snap the target first, then poll: concurrent callers land on the same target
/// and share one period rather than each forcing their own. Allocates nothing.
///
/// A stall is reported and waited through rather than declared complete. The
/// alternative — giving up and letting the caller free — hands memory back while
/// a reader may still be dereferencing it, and says so only through a logger
/// that is a no-op in production. A CPU that never reports is a bug the watchdog
/// can name; a use-after-free is not.
pub fn synchronize_rcu() {
    let target = gp_snap(GP_SEQ.load(Ordering::Acquire));
    gp_start_if_idle();

    let mut spins: u32 = 0;
    let mut ipi_sent = false;
    let mut next_warn = monotonic_ns().wrapping_add(RCU_STALL_TIMEOUT_NS);

    while !gp_done(GP_SEQ.load(Ordering::Acquire), target) {
        // Report unconditionally. A CPU spinning here reaches no switch of its
        // own, so a declined report makes the caller its own permanent holdout;
        // and the guarded variant declines for any preemption guard, not just a
        // read-side section, so an ordinary spinlock held across this call would
        // wedge the machine. Waiting for a grace period from inside a read-side
        // section is a self-deadlock the caller must not write, which is the
        // same assumption that lets the poll below treat this CPU as quiescent.
        rcu_note_qs();

        // The period this caller is waiting for may not have started yet: its
        // target rounds past whichever period was in flight at the snap.
        gp_start_if_idle();
        rcu_gp_poll();

        spins = spins.saturating_add(1);
        if !ipi_sent && spins > RCU_IPI_THRESHOLD {
            kick_holdouts();
            ipi_sent = true;
        }

        if (spins & 0xFFFF) == 0 {
            let now = monotonic_ns();
            if now.wrapping_sub(next_warn) < u64::MAX / 2 {
                match holdout_cpu() {
                    Some(cpu) => backend().log_warn(format_args!(
                        "RCU stall: CPU {} has not reported a quiescent state in {}ms (seq={}, target={})",
                        cpu,
                        RCU_STALL_TIMEOUT_NS / 1_000_000,
                        GP_SEQ.load(Ordering::Relaxed),
                        target,
                    )),
                    None => backend().log_warn(format_args!(
                        "RCU stall: no holdout but seq={} has not reached target={}",
                        GP_SEQ.load(Ordering::Relaxed),
                        target,
                    )),
                }
                next_warn = now.wrapping_add(RCU_STALL_TIMEOUT_NS);
                ipi_sent = false;
            }
        }

        // A CPU spinning on a peer must keep acknowledging that peer's
        // shootdowns; `spin_relax` is the service call and carries no
        // `pause` of its own.
        crate::sync::spin_relax();
        core::hint::spin_loop();
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
            // One byte store. This path runs from under cli-spinlocks, so it
            // cannot do anything that takes a lock or allocates.
            crate::sync::bh::raise();
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

/// Callbacks are parked in [`CB_BATCHES`] that a drain has not finished with.
///
/// The tick cannot take the batch lock to find that out, so the drain publishes
/// it here. Without it a backlog left by a limited pass would rest entirely on
/// that pass having re-armed, and one dropped arm would strand it until the next
/// `call_rcu`.
static CB_BACKLOG: AtomicBool = AtomicBool::new(false);

/// Check from the timer tick whether deferred callbacks need processing.
///
/// Hardirq-safe; analogous to Linux's `rcu_sched_clock_irq()` raising
/// `RCU_SOFTIRQ`.
#[inline]
pub fn rcu_raise_softirq() {
    if !PENDING_HEAD.load(Ordering::Relaxed).is_null() || CB_BACKLOG.load(Ordering::Relaxed) {
        RCU_CB_PENDING.store(true, Ordering::Release);
    }
}

/// The two segments only the drain touches.
///
/// [`PENDING_HEAD`] is the third and holds newly queued callbacks; it stays a
/// lock-free stack so `call_rcu` gains no context constraint from any of this.
///
/// `wait` is a separate list from the pending stack because it carries a
/// sequence number, and a single list has nowhere to record which grace period
/// its callbacks are waiting for. That missing field is what forced the
/// invocation step to take a grace period inline.
struct CbBatches {
    /// Queued before `wait_seq` was chosen; not yet invocable.
    wait: *mut RcuCallbackNode,
    wait_seq: u64,
    /// Past their grace period, waiting only for a drain to pick them up.
    done: *mut RcuCallbackNode,
}

// SAFETY: every access goes through `CB_BATCHES`, and a node reaches `wait`
// only after the atomic swap that detached it from `PENDING_HEAD` gave this
// caller exclusive ownership of the chain.
unsafe impl Send for CbBatches {}

static CB_BATCHES: crate::sync::SpinLock<CbBatches> = crate::sync::SpinLock::new(
    CbBatches {
        wait: core::ptr::null_mut(),
        wait_seq: 0,
        done: core::ptr::null_mut(),
    },
    crate::lock_class!("RCU_CB_BATCHES", crate::sync::LOCK_LEVEL_RESOURCE),
);

/// Callbacks invoked per drain pass.
///
/// A pass runs on a CPU that has other things to do next, so the remainder goes
/// back on `done` and the drain re-arms rather than running the whole backlog at
/// once. Linux calls this `blimit`.
const RCU_BLIMIT: usize = 64;

/// Detach at most `limit` nodes from `*head`.
fn split_off(head: &mut *mut RcuCallbackNode, limit: usize) -> *mut RcuCallbackNode {
    let taken = *head;
    if taken.is_null() {
        return taken;
    }
    let mut tail = taken;
    let mut count = 1;
    loop {
        // SAFETY: `tail` is a node this caller owns exclusively; the chain is
        // null-terminated.
        let next = unsafe { (*tail).next };
        if next.is_null() || count == limit {
            *head = next;
            // SAFETY: as above; cutting the chain at the node this pass keeps.
            unsafe { (*tail).next = core::ptr::null_mut() };
            return taken;
        }
        tail = next;
        count += 1;
    }
}

/// Invoke and release every node in `chain`.
fn invoke_chain(chain: *mut RcuCallbackNode) {
    let mut current = chain;
    while !current.is_null() {
        // SAFETY: each node was allocated by `try_alloc_callback_node` and is
        // exclusively owned by this caller — it left every shared list before
        // reaching here.
        let next = unsafe { (*current).next };
        let ptr = unsafe { (*current).ptr };
        let callback = unsafe { (*current).callback };
        // SAFETY: symmetric dealloc with the same Layout used in allocation.
        unsafe { dealloc_callback_node(current) };
        // SAFETY: the node reached `done` only after its grace period elapsed,
        // which is the callback's contract.
        unsafe { callback(ptr) };
        current = next;
    }
}

/// One drain pass: retire an elapsed batch, tag a fresh one, invoke up to
/// `limit` callbacks. Returns whether anything remains.
///
/// Never waits. That is the invariant the segments exist to establish: a pass
/// invokes exactly those callbacks already observed to be past their grace
/// period, and returns. A drain that took the grace period itself could not run
/// anywhere a CPU has other work to get back to.
///
/// The lock is `try_lock`ed and released before any callback runs, so a second
/// CPU skips rather than spins, and a callback that itself calls `call_rcu` only
/// pushes to the lock-free stack.
fn drain_ready(limit: usize) -> bool {
    let Some(mut batches) = CB_BATCHES.try_lock() else {
        // Hand the flag back. The CPU holding the lock re-arms for what it
        // leaves behind, but it may have computed that before this swap, and a
        // dropped arm is a backlog nothing comes back for.
        RCU_CB_PENDING.store(true, Ordering::Release);
        return false;
    };

    // Retire an elapsed batch. Only into an empty `done`, which keeps this
    // O(1): a non-empty `done` means the previous pass hit its limit, and the
    // caller comes straight back for it.
    if batches.done.is_null()
        && !batches.wait.is_null()
        && gp_done(GP_SEQ.load(Ordering::Acquire), batches.wait_seq)
    {
        batches.done = core::mem::replace(&mut batches.wait, core::ptr::null_mut());
    }

    // Tag a fresh batch with the period it has to outlive.
    if batches.wait.is_null() {
        let head = PENDING_HEAD.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !head.is_null() {
            batches.wait = head;
            batches.wait_seq = gp_snap(GP_SEQ.load(Ordering::Acquire));
        }
    }

    let batch = split_off(&mut batches.done, limit);
    let ready = !batches.done.is_null();
    let more = ready || !batches.wait.is_null();
    CB_BACKLOG.store(more, Ordering::Release);
    drop(batches);

    // Outside the lock: callbacks free to the heap, and one of them may queue
    // another.
    invoke_chain(batch);

    // A batch was tagged but its period may not have started, and nothing else
    // starts one on the callback path.
    gp_start_if_idle();

    if more || !PENDING_HEAD.load(Ordering::Acquire).is_null() {
        RCU_CB_PENDING.store(true, Ordering::Release);
    }
    ready
}

/// Invoke every callback whose grace period has already elapsed.
///
/// Called from a CPU that has found nothing to dispatch, so it keeps going while
/// callbacks are ready rather than leaving a backlog for the next pass: the
/// batch limit is there to bound one pass, not to ration a CPU that has nothing
/// else to do. Callbacks whose period has *not* elapsed are left alone — this
/// never waits for one.
pub fn rcu_process_callbacks() {
    if !RCU_CB_PENDING.swap(false, Ordering::Acquire) {
        return;
    }
    while drain_ready(RCU_BLIMIT) {}
}

/// Invoke one bounded batch from the bottom-half point.
///
/// The witness is what distinguishes this from [`rcu_process_callbacks`]: the
/// caller is a CPU on its way back to real work rather than one that has run
/// out of it, so this takes a single pass and re-arms for the rest.
pub(crate) fn invoke_callbacks(_bh: &crate::sync::bh::BhContext<'_>) {
    if !RCU_CB_PENDING.swap(false, Ordering::Acquire) {
        return;
    }
    drain_ready(RCU_BLIMIT);
}

/// Wait until every callback queued before this call has been *invoked*.
///
/// [`synchronize_rcu`] says a grace period elapsed; once invocation is
/// asynchronous that no longer says the callbacks ran, and those are two
/// different facts a caller tearing something down needs to keep apart.
///
/// Drives the drain itself rather than waiting on a peer, so it makes progress
/// from a context that will not go idle.
pub fn rcu_barrier() {
    // Everything queued before this point is either on the pending stack or in
    // a batch, and one full drain of both is what the caller is waiting for.
    while !PENDING_HEAD.load(Ordering::Acquire).is_null() || CB_BACKLOG.load(Ordering::Acquire) {
        RCU_CB_PENDING.store(true, Ordering::Release);
        rcu_process_callbacks();
        // Unconditional, for the reason [`synchronize_rcu`] gives: a spinning
        // CPU reaches no switch of its own, so a declined report leaves it
        // waiting on itself.
        rcu_note_qs();
        rcu_gp_poll();
        crate::sync::spin_relax();
        core::hint::spin_loop();
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

#[cfg(test)]
mod tests {
    use super::{GP_IN_FLIGHT, gp_done, gp_snap};

    /// From an idle sequence, the next period to run is the one to wait for.
    #[test]
    fn snap_from_idle_targets_the_next_completion() {
        assert_eq!(gp_snap(0), 2);
        assert_eq!(gp_snap(2), 4);
        assert_eq!(gp_snap(100), 102);
    }

    /// From an in-flight sequence, the running period does not count: it may
    /// have begun before the caller unpublished, so a reader that entered
    /// before the caller's store can still be inside it.
    #[test]
    fn snap_rounds_past_an_in_flight_period() {
        assert_eq!(gp_snap(1), 4);
        assert_eq!(gp_snap(3), 6);
        assert_eq!(gp_snap(101), 104);
    }

    /// Every target names a completion, never a period in flight.
    #[test]
    fn snap_is_always_a_completed_sequence() {
        for seq in 0u64..64 {
            assert_eq!(gp_snap(seq) & GP_IN_FLIGHT, 0, "seq={seq}");
        }
    }

    /// A target is always strictly ahead, so no caller returns without waiting.
    #[test]
    fn snap_is_strictly_ahead_of_the_snapped_sequence() {
        for seq in 0u64..64 {
            assert!(gp_snap(seq) > seq, "seq={seq}");
        }
    }

    #[test]
    fn done_is_reached_at_and_after_the_target() {
        assert!(!gp_done(0, 2));
        assert!(!gp_done(1, 2));
        assert!(gp_done(2, 2));
        assert!(gp_done(4, 2));
    }

    /// The sequence is a wrapping counter, so the comparison has to be a signed
    /// difference. A plain `>=` reads a wrapped counter as permanently behind
    /// and every waiter hangs.
    #[test]
    fn done_survives_wrapping() {
        let target = gp_snap(u64::MAX - 3);
        assert!(!gp_done(u64::MAX - 3, target));
        assert!(gp_done(target, target));
        assert!(gp_done(target.wrapping_add(2), target));
    }
}
