//! CPU HAL surface: preemption control, instruction wrappers, MSR /
//! CPUID access, per-CPU storage. Subsystem-prefixed re-exports kept
//! at the [`crate::cpu`] level for ergonomic access.

pub mod preempt;
pub mod x86_64;

pub use preempt::{
    DisabledPreemptGuard, NoOpBackend, PreemptBackend, is_preempt_disabled, preempt_count,
    register_preempt_backend,
};
