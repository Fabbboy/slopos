//! Re-export of the OSTD-owned PCR-backed `PreemptBackend` impl.
//!
//! The implementation moved into `slopos_ostd::cpu::preempt` so the
//! `current_pcr()` `unsafe` block lives inside OSTD where it belongs.
//! `kernel-services` only forwards the static reference for the
//! boot-side registration call.

pub use slopos_ostd::cpu::preempt::{PcrPreemptBackend, DEFAULT_PCR_PREEMPT as PCR_PREEMPT};
