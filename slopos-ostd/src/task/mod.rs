//! Bare kernel task primitive + context-switch primitives.
//!
//! Hosts the OSTD-side [`Task`], [`TaskContext`], and [`Scheduler`] /
//! [`RunQueue`] trait surface. Until consumers are migrated, the
//! kernel scheduler in `core::scheduler` continues to drive execution
//! through its own types; the OSTD primitives compile but are unwired.

pub mod abi;
pub mod fpu;
pub mod scheduler;
pub mod switch;
pub mod task;

pub use fpu::{FPU_STATE_SIZE, FXSAVE_AREA_SIZE, FpuState, MXCSR_DEFAULT, fpu_xrstor, fpu_xsave};
pub use scheduler::{RoundRobinRq, RoundRobinScheduler, RunQueue, Scheduler, TaskRef};
pub use switch::{
    TaskExitHook, init_current_context, register_task_exit_hook, switch_registers,
    task_entry_trampoline,
};
pub use task::{
    CurrentTask, KernelStack, Task, TaskContext, TaskId, TaskRuntimeBackend, current,
    register_task_runtime_backend,
};
