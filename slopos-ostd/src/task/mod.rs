//! Kernel task control block + context-switch primitives.
//!
//! The live task control block is [`kernel_task::TaskInner`] (aliased
//! `Task` in the `slopos-sched` crate, which drives scheduling).
//! OSTD owns the context-switch register snapshot ([`TaskContext`]),
//! the switch assembly, the kernel-thread spawner, and the
//! [`LinkProvider`] hook that absorbs the unsafe intrusive-list
//! `Linked` impls into the trusted core.

pub mod abi;
pub mod accessors;
pub mod drop_context;
pub mod exit_info;
pub mod fpu;
pub mod handles;
pub mod job_control;
pub mod kernel_task;
pub mod link_roles;
pub mod spawner;
pub mod state;
pub mod switch;
pub mod task;
pub mod test_reports;

pub use drop_context::{assert_task_drop_context, drop_off_lock, run_off_lock};
pub use exit_info::ExitInfo;
pub use job_control::{ProcessGroup, Session, new_group_in_session, new_session_group};
pub use kernel_task::SchedPlacement;
pub use link_roles::{ReadyQueueRole, RemoteWakeRole, ZombieListRole};
pub use state::{TaskState, TaskStateView};
pub use test_reports::{
    PendingDrain, TestReport, TestReportRing, alloc_ring, consume_pending_drain, empty_report,
    pending_drain_present, stash_pending_drain,
};

pub use fpu::{FPU_STATE_SIZE, FXSAVE_AREA_SIZE, FpuState, MXCSR_DEFAULT, fpu_xrstor, fpu_xsave};
pub use handles::LinkProvider;
pub use spawner::{
    KernelThreadEntry, KernelThreadSpawner, SpawnError, SpawnedTaskId,
    current_kernel_thread_spawner, register_kernel_thread_spawner, spawn,
};
pub use switch::{
    TaskExitHook, init_current_context, register_task_exit_hook, switch_context, switch_registers,
    task_entry_trampoline,
};
pub use task::TaskContext;
