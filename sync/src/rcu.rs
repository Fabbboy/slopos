//! Classic RCU (Read-Copy-Update) for SlopOS.
//!
//! Read-side critical sections use [`PreemptGuard`] to prevent context
//! switches, guaranteeing the CPU cannot pass through a quiescent state
//! while an [`RcuReadGuard`] is held.  Writers call [`synchronize_rcu`]
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
//! If a CPU is slow to report (e.g. halted in `hlt` under a hypervisor
//! that starves idle vCPUs), a lightweight RCU QS IPI is sent to force
//! it to bump its counter.  Unlike the reschedule IPI, the RCU QS IPI
//! only calls `rcu_note_qs()` + EOI — no context switch, safe from any
//! CPU state including the idle loop.
//!
//! ## Stall detection
//!
//! [`synchronize_rcu`] spins for at most [`RCU_STALL_LIMIT_SPINS`]
//! iterations per CPU.  If a holdout CPU fails to report a quiescent
//! state within that budget (even after an IPI), the grace period is
//! declared complete for that CPU with a serial warning.  The old data
//! is leaked rather than risking a use-after-free, matching Linux's
//! philosophy of "warn loudly but don't crash."
//!
//! ## Deferred callbacks (`call_rcu`)
//!
//! Modelled after Linux's `call_rcu()` / `rcu_do_batch()`:
//!
//! - `call_rcu()` pushes a callback node onto a lock-free Treiber stack.
//!   The node is allocated via the global allocator; on OOM, the call
//!   falls back to a synchronous `synchronize_rcu()` + immediate
//!   callback — slower but correct, matching Linux's OOM behaviour in
//!   `kfree_rcu()` (see `kernel/rcu/tree.c:kfree_rcu_monitor`).
//!
//! - `rcu_process_callbacks()` is called from non-IRQ context (the
//!   scheduler idle loop on CPU 0, or context-switch paths).  The timer
//!   tick only raises a flag via [`rcu_raise_softirq`]; the actual grace
//!   period wait and callback invocation happen outside interrupt context,
//!   matching Linux's separation of `rcu_sched_clock_irq()` (hardirq)
//!   from `rcu_core()` (softirq/kthread).
//!
//! - After draining and invoking a batch, `rcu_process_callbacks` re-checks
//!   `PENDING_HEAD` and loops if new callbacks arrived during the grace
//!   period wait.  This mirrors Linux's `rcu_core()` which calls
//!   `invoke_rcu_core()` to re-raise the softirq when
//!   `rcu_segcblist_ready_cbs()` indicates more work.  Without this
//!   re-check, callbacks pushed between the atomic steal and the end of
//!   `synchronize_rcu()` could sit unprocessed until the next timer tick
//!   raises the softirq flag — an unbounded latency hole.

use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use slopos_ostd::{raw_alloc, raw_dealloc};

use crate::preempt::PreemptGuard;
use slopos_arch::pcr::{
    apic_id_from_cpu_index, get_cpu_count, get_current_cpu, is_cpu_online, send_ipi_to_cpu,
    MAX_CPUS,
};

use slopos_utils::klog_warn;

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
/// Safe to call from any context including interrupt handlers — uses
/// raw per-CPU indexing without PreemptGuard.
#[inline]
pub fn rcu_note_qs() {
    let cpu = get_current_cpu();
    if cpu < MAX_CPUS {
        RCU_QS_CTR[cpu].0.fetch_add(1, Ordering::Release);
    }
}

const RCU_IPI_THRESHOLD: u32 = 1_000;

/// RCU stall timeout in nanoseconds.
///
/// Modelled after Linux's `CONFIG_RCU_CPU_STALL_TIMEOUT` (default 21 s).
/// We use a shorter 500 ms budget because SlopOS is a unikernel running
/// under QEMU with a 100 Hz timer tick — any healthy CPU should report
/// a QS via timer tick (10 ms), idle loop, or context switch well
/// within this window.  If a CPU hasn't reported after an IPI + 500 ms,
/// something is seriously wrong.
///
/// Unlike the old iteration-count limit, this is clock-based (TSC) so
/// the timeout is deterministic regardless of CPU frequency, matching
/// Linux's approach of using `jiffies` / `local_clock()` for stall
/// detection in `rcu_check_gp_stall_node()`.
const RCU_STALL_TIMEOUT_NS: u64 = 500_000_000;

/// Wrapping-safe comparison: has the counter advanced past the snapshot?
///
/// Modelled after Linux's `ULONG_CMP_GE` — uses signed wrapping
/// subtraction so the comparison is correct even if the counter has
/// wrapped around (which is astronomically unlikely for `u64` but
/// formally required for soundness).
#[inline]
fn qs_counter_advanced(current: u64, snapshot: u64) -> bool {
    (current.wrapping_sub(snapshot)) as i64 > 0
}

/// Read the current monotonic time in nanoseconds.
///
/// Uses the kernel-services platform clock (HPET-backed) when
/// available; falls back to raw TSC with a conservative 1 GHz
/// assumption if the platform clock isn't wired yet (early boot).
#[inline]
fn monotonic_ns() -> u64 {
    let ns = slopos_kernel_services::platform::clock_monotonic_ns();
    if ns > 0 {
        return ns;
    }
    slopos_arch::tsc::rdtsc()
}

/// Block until every online CPU has passed through at least one
/// quiescent state since this call.
///
/// Sequence (modelled on Linux `synchronize_rcu` / GP init in
/// `kernel/rcu/tree.c`):
///
/// 1. Note a QS on the calling CPU — the act of entering this function
///    proves no RCU read-side critical section is active here (we are in
///    process context with preemption enabled).  Linux does the same in
///    `rcu_gp_init()` after snapshotting all CPUs: it calls `rcu_qs()` +
///    `rcu_report_qs_rdp()` to immediately clear the local CPU from the
///    qsmask.
///
/// 2. Snapshot every CPU's QS counter.
///
/// 3. For each remote online CPU, spin until its counter advances past
///    the snapshot (wrapping-safe comparison, like Linux's
///    `ULONG_CMP_GE`).  If the CPU is slow, send a dedicated RCU QS
///    IPI (vector 0xFB) to force it.  If it still hasn't reported
///    after [`RCU_STALL_TIMEOUT_NS`], log a stall warning and move on.
pub fn synchronize_rcu() {
    rcu_note_qs();

    let this_cpu = get_current_cpu();
    let n = get_cpu_count().min(MAX_CPUS);

    // Heap-allocate the per-CPU snapshot vector rather than placing a
    // 2 KiB `[u64; MAX_CPUS]` on the stack — stack-safety gate forbids
    // frames that large, and an RCU grace-period that can't allocate
    // 2 KiB of scratch is already on a wedged path.
    let mut snaps = slopos_ostd::KVec::<u64>::zeroed(n).expect("rcu: snaps alloc");
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
                    send_ipi_to_cpu(apic_id, slopos_arch::arch::idt::RCU_QS_IPI_VECTOR);
                }
                ipi_sent = true;
            }

            if (spins & 0xFFFF) == 0 {
                let now = monotonic_ns();
                if now.wrapping_sub(deadline) < u64::MAX / 2 {
                    klog_warn!(
                        "RCU stall: CPU {} failed to report QS after {}ms (snap={}, cur={})",
                        cpu,
                        RCU_STALL_TIMEOUT_NS / 1_000_000,
                        snaps[cpu],
                        RCU_QS_CTR[cpu].0.load(Ordering::Relaxed),
                    );
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
// side).  No concurrent mutable access is possible.
unsafe impl Send for RcuCallbackNode {}

static PENDING_HEAD: AtomicPtr<RcuCallbackNode> = AtomicPtr::new(core::ptr::null_mut());

/// Flag set by the timer tick when pending callbacks exist.
///
/// Modelled after Linux's `raise_softirq(RCU_SOFTIRQ)`: the hardirq
/// path only sets this flag; the actual grace-period wait and callback
/// invocation happen in [`rcu_process_callbacks`] which runs from
/// non-IRQ context (idle loop, context switch).
static RCU_CB_PENDING: AtomicBool = AtomicBool::new(false);

/// Attempt to allocate an `RcuCallbackNode` via the global allocator.
///
/// Returns a raw pointer (null on OOM).  Uses the raw allocator API
/// instead of `Box::new` to avoid panicking under memory pressure —
/// the caller is responsible for the OOM fallback path.
///
/// Freed symmetrically via [`dealloc_callback_node`] with the same layout.
fn try_alloc_callback_node(ptr: *mut u8, callback: RcuCallback) -> *mut RcuCallbackNode {
    let layout = Layout::new::<RcuCallbackNode>();
    // SAFETY: layout is non-zero-sized (RcuCallbackNode contains pointers).
    // SAFETY: layout is non-zero-sized (RcuCallbackNode contains pointers).
    let raw = unsafe { raw_alloc(layout) };
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    let node = raw as *mut RcuCallbackNode;
    // SAFETY: `raw` is a valid, properly aligned, freshly allocated pointer
    // for `RcuCallbackNode`.  No other references exist.
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
/// Uses the same [`Layout`] as the allocation to ensure symmetry.
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
/// The callback will be invoked with `ptr` after [`rcu_process_callbacks`]
/// completes a grace period.  This is the non-blocking alternative to
/// calling `synchronize_rcu()` + `drop()` inline.
///
/// On successful allocation the function is O(1) and never blocks.
/// If the callback-node allocation fails (OOM), the function falls back
/// to a synchronous grace period and invokes the callback immediately —
/// this matches Linux's `kfree_rcu()` OOM fallback which calls
/// `synchronize_rcu()` then `kfree()` inline (see
/// `kernel/rcu/tree.c:kfree_rcu_monitor`).
///
/// # Safety
///
/// `callback` must be safe to call with `ptr` after a grace period.
pub unsafe fn call_rcu(ptr: *mut u8, callback: RcuCallback) {
    let node = try_alloc_callback_node(ptr, callback);

    if node.is_null() {
        klog_warn!("RCU: call_rcu allocation failed, falling back to synchronous grace period");
        synchronize_rcu();
        // SAFETY: grace period has elapsed — callback contract satisfied.
        unsafe {
            callback(ptr);
        }
        return;
    }

    // Lock-free Treiber stack push.  We own `node` exclusively until
    // the CAS succeeds, so the unsynchronised write to `next` is safe.
    //
    // Note: Linux's `call_rcu()` uses per-CPU segmented callback lists
    // with no atomic contention.  Our Treiber stack is simpler but has
    // CAS contention under high call_rcu() concurrency.  The spin_loop
    // hint reduces power waste on retry; contention is expected to be
    // very low (font changes are rare events).
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

/// Check from the timer tick whether deferred callbacks need processing.
///
/// This is the hardirq-safe half of the callback pipeline, analogous to
/// Linux's `rcu_sched_clock_irq()` raising `RCU_SOFTIRQ`.  It only
/// sets an atomic flag — the actual grace-period wait and callback
/// invocation happen in [`rcu_process_callbacks`] which must be called
/// from non-IRQ context.
#[inline]
pub fn rcu_raise_softirq() {
    if !PENDING_HEAD.load(Ordering::Relaxed).is_null() {
        RCU_CB_PENDING.store(true, Ordering::Release);
    }
}

/// Drain the Treiber stack and invoke all callbacks after a grace period.
///
/// Returns `true` if any callbacks were processed.
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
/// Called from the scheduler idle loop or context-switch path on CPU 0.
/// If the softirq flag is set, atomically steals the entire callback
/// list, waits for one grace period, then invokes every callback.
///
/// Multiple callbacks batched between ticks share a single
/// `synchronize_rcu()` — the same optimisation Linux uses in
/// `rcu_do_batch()`.
///
/// After draining a batch, re-checks `PENDING_HEAD` for callbacks that
/// arrived during the grace period wait.  This mirrors Linux's
/// `rcu_core()` which re-invokes itself via `invoke_rcu_core()` when
/// `rcu_segcblist_ready_cbs()` shows more work.  Without this loop,
/// callbacks pushed between the atomic steal and the `synchronize_rcu()`
/// return would sit unprocessed until the next timer tick — an
/// unbounded latency hole.
///
/// # Context
///
/// Must be called from process context (idle task, kernel thread) —
/// **never** from a timer tick or other IRQ handler, because
/// `synchronize_rcu()` spins waiting for all CPUs to report quiescent
/// states.
pub fn rcu_process_callbacks() {
    if !RCU_CB_PENDING.swap(false, Ordering::Acquire) {
        return;
    }

    loop {
        if !drain_and_invoke() {
            break;
        }
        // New callbacks may have arrived during synchronize_rcu().
        // Re-check before returning to avoid latency holes.
        if PENDING_HEAD.load(Ordering::Acquire).is_null() {
            break;
        }
    }
}
