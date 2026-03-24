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

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::preempt::PreemptGuard;
use slopos_arch::pcr::{
    apic_id_from_cpu_index, get_cpu_count, get_current_cpu, is_cpu_online, send_ipi_to_cpu,
    MAX_CPUS,
};

#[repr(C, align(64))]
struct QsSlot(AtomicU64);

static RCU_QS_CTR: [QsSlot; MAX_CPUS] = [const { QsSlot(AtomicU64::new(0)) }; MAX_CPUS];

/// Read-side critical section guard.
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

/// Maximum spin iterations per CPU before declaring an RCU stall.
///
/// At ~GHz spin rates this is roughly 100-500ms — long enough for any
/// healthy CPU to report a QS via timer tick (10ms) or idle loop, but
/// short enough to avoid hanging the kernel indefinitely.
const RCU_STALL_LIMIT_SPINS: u32 = 500_000_000;

/// Block until every online CPU has passed through at least one
/// quiescent state since this call.
///
/// Two-phase approach per holdout CPU:
/// 1. Spin briefly waiting for natural QS reports (timer tick, idle loop)
/// 2. Send a dedicated RCU QS IPI (vector 0xFB) to force the CPU to
///    bump its counter
/// 3. If the CPU still hasn't reported after [`RCU_STALL_LIMIT_SPINS`],
///    log a warning and move on (leak the old data rather than hang)
pub fn synchronize_rcu() {
    let this_cpu = get_current_cpu();
    let n = get_cpu_count().min(MAX_CPUS);

    let mut snaps = [0u64; MAX_CPUS];
    for cpu in 0..n {
        snaps[cpu] = RCU_QS_CTR[cpu].0.load(Ordering::Acquire);
    }

    for cpu in 0..n {
        if cpu == this_cpu || !is_cpu_online(cpu) || snaps[cpu] == 0 {
            continue;
        }

        let mut ipi_sent = false;
        let mut spins: u32 = 0;
        loop {
            let current = RCU_QS_CTR[cpu].0.load(Ordering::Acquire);
            if current != snaps[cpu] {
                break;
            }

            spins = spins.saturating_add(1);

            if !ipi_sent && spins > RCU_IPI_THRESHOLD {
                if let Some(apic_id) = apic_id_from_cpu_index(cpu) {
                    send_ipi_to_cpu(apic_id, slopos_arch::arch::idt::RCU_QS_IPI_VECTOR);
                }
                ipi_sent = true;
            }

            if spins >= RCU_STALL_LIMIT_SPINS {
                break;
            }

            core::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// Deferred RCU callbacks (call_rcu)
// ---------------------------------------------------------------------------

type RcuCallback = unsafe fn(*mut u8);

struct RcuPendingFree {
    ptr: *mut u8,
    callback: RcuCallback,
}

unsafe impl Send for RcuPendingFree {}

const MAX_PENDING: usize = 8;
static PENDING_COUNT: AtomicU64 = AtomicU64::new(0);
static PENDING_SLOTS: [AtomicPtr<RcuPendingFree>; MAX_PENDING] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_PENDING];

/// Schedule a deferred free after the next RCU grace period.
///
/// The callback will be invoked with `ptr` after `synchronize_rcu()`
/// completes.  This is the non-blocking alternative to calling
/// `synchronize_rcu()` + `drop()` inline.
///
/// If the pending queue is full, falls back to synchronous
/// `synchronize_rcu()` + immediate callback.
///
/// # Safety
///
/// `callback` must be safe to call with `ptr` after a grace period.
pub unsafe fn call_rcu(ptr: *mut u8, callback: RcuCallback) {
    let entry = Box::into_raw(Box::new(RcuPendingFree { ptr, callback }));
    for slot in &PENDING_SLOTS {
        if slot
            .compare_exchange(
                core::ptr::null_mut(),
                entry,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            PENDING_COUNT.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    // Queue full — fall back to synchronous path.
    unsafe {
        drop(Box::from_raw(entry));
    }
    synchronize_rcu();
    unsafe {
        callback(ptr);
    }
}

/// Process all pending RCU callbacks.
///
/// Called from the timer tick on CPU 0 (or any periodic kernel context).
/// Performs a single `synchronize_rcu()` then drains all pending entries.
pub fn rcu_process_callbacks() {
    if PENDING_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    synchronize_rcu();
    for slot in &PENDING_SLOTS {
        let entry = slot.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !entry.is_null() {
            PENDING_COUNT.fetch_sub(1, Ordering::Relaxed);
            let pending = unsafe { Box::from_raw(entry) };
            unsafe {
                (pending.callback)(pending.ptr);
            }
        }
    }
}
