//! Bare kernel task primitive + context-switch primitives.
//!
//! Hosts the OSTD-side [`Task`], [`TaskContext`], and [`Scheduler`] /
//! [`RunQueue`] trait surface. Until consumers are migrated, the
//! kernel scheduler in `core::scheduler` continues to drive execution
//! through its own types; the OSTD primitives compile but are unwired.

pub mod abi;
pub mod accessors;
pub mod exit_info;
pub mod fpu;
pub mod handles;
pub mod idle_factory;
pub mod kernel_task;
pub mod link_roles;
pub mod scheduler;
pub mod scheduler_registry;
pub mod spawner;
pub mod state;
pub mod switch;
pub mod task;
pub mod test_reports;

pub use exit_info::ExitInfo;
pub use link_roles::{ReadyQueueRole, ZombieListRole};
pub use state::{TaskState, TaskStateView};
pub use test_reports::{
    PendingDrain, TestReport, TestReportRing, alloc_ring, consume_pending_drain, empty_report,
    pending_drain_present, stash_pending_drain,
};

pub use fpu::{FPU_STATE_SIZE, FXSAVE_AREA_SIZE, FpuState, MXCSR_DEFAULT, fpu_xrstor, fpu_xsave};
pub use handles::{LinkProvider, OwnedTaskHandle, SharedTaskHandle, TaskOps, task_state};
pub use idle_factory::{IdleTaskFactory, current_idle_task_factory, register_idle_task_factory};
pub use scheduler::{RunQueue, Scheduler, TaskRef};
pub use scheduler_registry::{current_scheduler, register_scheduler};
pub use spawner::{
    KernelThreadEntry, KernelThreadSpawner, SpawnError, SpawnedTaskId,
    current_kernel_thread_spawner, register_kernel_thread_spawner, spawn,
};
pub use switch::{
    TaskExitHook, init_current_context, register_task_exit_hook, switch_context, switch_registers,
    task_entry_trampoline,
};
pub use task::{
    CurrentTask, DEFAULT_TASK_RUNTIME_BACKEND, KernelStack, PcrTaskRuntimeBackend, Task,
    TaskContext, TaskId, TaskRuntimeBackend, current, register_task_runtime_backend,
};
