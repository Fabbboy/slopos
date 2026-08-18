//! Re-export of the OSTD-owned PCR-backed `PreemptBackend` for the boot-side
//! registration call; the impl lives in OSTD because `current_pcr()` needs
//! `unsafe`.

pub use slopos_ostd::cpu::preempt::{PcrPreemptBackend, DEFAULT_PCR_PREEMPT as PCR_PREEMPT};
