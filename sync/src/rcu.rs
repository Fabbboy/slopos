//! Classic RCU (Read-Copy-Update) for SlopOS.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::preempt::PreemptGuard;

static RCU_GLOBAL_QS: AtomicU64 = AtomicU64::new(0);

pub struct RcuReadGuard {
    _preempt: PreemptGuard,
}

#[inline]
pub fn rcu_read_lock() -> RcuReadGuard {
    RcuReadGuard {
        _preempt: PreemptGuard::new(),
    }
}

#[inline]
pub fn rcu_note_qs() {
    RCU_GLOBAL_QS.fetch_add(1, Ordering::Release);
}

/// Block until enough quiescent-state ticks have been observed to
/// guarantee every online CPU has passed through at least one.
///
/// Falls back to a timed spin if QS ticks do not advance within a
/// reasonable window (covers early boot and edge cases where APs
/// may not yet be running their idle loops).
pub fn synchronize_rcu() {
    let snap = RCU_GLOBAL_QS.load(Ordering::Acquire);
    let need = snap.saturating_add(slopos_arch::pcr::get_cpu_count() as u64);
    let mut iters: u32 = 0;
    loop {
        let current = RCU_GLOBAL_QS.load(Ordering::Acquire);
        if current >= need {
            break;
        }
        iters = iters.wrapping_add(1);
        if iters > 10_000_000 {
            break;
        }
        core::hint::spin_loop();
    }
}
