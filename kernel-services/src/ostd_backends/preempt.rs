//! `PreemptBackend` impl that proxies to the per-CPU preempt count
//! living in `slopos_arch::pcr::ProcessorControlRegion::preempt_count`.

use core::sync::atomic::Ordering;

use slopos_ostd::cpu::preempt::PreemptBackend;

pub struct PcrPreemptBackend;

pub static PCR_PREEMPT: PcrPreemptBackend = PcrPreemptBackend;

impl PreemptBackend for PcrPreemptBackend {
    fn enter(&self) {
        // SAFETY: `register_preempt_backend` is only invoked after
        // `pcr.install()` has run on the BSP and on each AP, so
        // `current_pcr()` returns a valid `&'static ProcessorControlRegion`.
        unsafe {
            slopos_arch::pcr::current_pcr()
                .preempt_count
                .fetch_add(1, Ordering::AcqRel);
        }
    }

    fn leave(&self) {
        // No reschedule-callback dispatch yet — that wiring lands with
        // the scheduler migration. Maintaining the count is sufficient
        // for now; legacy paths still drive scheduling.
        unsafe {
            slopos_arch::pcr::current_pcr()
                .preempt_count
                .fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn count(&self) -> u32 {
        unsafe {
            slopos_arch::pcr::current_pcr()
                .preempt_count
                .load(Ordering::Acquire)
        }
    }
}
