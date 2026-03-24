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

use core::sync::atomic::{AtomicU64, Ordering};

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

/// Block until every online CPU has passed through at least one
/// quiescent state since this call.
///
/// Two-phase approach:
/// 1. Spin briefly waiting for natural QS reports (timer tick, idle loop)
/// 2. Send a dedicated RCU QS IPI (vector 0xFB) to holdout CPUs — the
///    handler just calls `rcu_note_qs()` + EOI, safe from any state
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

            spins = spins.wrapping_add(1);

            if !ipi_sent && spins > 1000 {
                if let Some(apic_id) = apic_id_from_cpu_index(cpu) {
                    send_ipi_to_cpu(apic_id, slopos_arch::arch::idt::RCU_QS_IPI_VECTOR);
                }
                ipi_sent = true;
            }

            core::hint::spin_loop();
        }
    }
}
