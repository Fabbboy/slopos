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
pub mod addr;
pub mod cell;
pub mod diag;
pub mod drop_context;
pub mod exit_info;
pub mod fpu;
pub mod fpu_owner;
pub mod handles;
pub mod job_control;
pub mod kernel_task;
pub mod link_roles;
pub mod placement;
pub mod spawner;
pub mod state;
pub mod switch;
pub mod task;
pub mod test_reports;

pub use addr::TaskAddr;
pub use cell::{CurrentTask, SwitchWindow, TaskExclusive, TaskOwnCell};
pub use diag::{TaskDiag, current_task_diag};
pub use drop_context::{assert_task_drop_context, drop_off_lock, run_off_lock};
pub use exit_info::ExitInfo;
pub use job_control::{ProcessGroup, Session, new_group_in_session, new_session_group};
pub use kernel_task::SchedPlacement;
pub use link_roles::{ReadyQueueRole, RemoteWakeRole, SiblingRole};
pub use placement::{
    ParkedTask, task_destroy_parked, task_existence_is_parked, task_existence_park,
    task_existence_parked_count, task_existence_release, task_parked_leak, task_parked_reclaim,
    task_placement_clone, task_placement_leak, task_placement_reclaim, task_placement_retain,
    task_placement_strong_count, task_release_strong, with_parked,
};
pub use state::{TaskState, TaskStateView};
pub use test_reports::{
    PendingDrain, TestReport, TestReportRing, alloc_ring, consume_pending_drain, empty_report,
    pending_drain_present, stash_pending_drain,
};

pub use fpu::{FPU_STATE_SIZE, FXSAVE_AREA_SIZE, FpuState, MXCSR_DEFAULT, fpu_xrstor, fpu_xsave};
pub use fpu_owner::{
    FPU_CPU_NONE, fpu_current_cpu, fpu_owner_assert_may_take, fpu_owner_forget, fpu_owner_is,
    fpu_owner_may_take, fpu_owner_on, fpu_owner_take, fpu_owner_yield_after_save, fpu_state_valid,
};
pub use handles::{DLinkProvider, LinkProvider};
pub use spawner::{
    KernelThreadEntry, KernelThreadSpawner, SpawnError, SpawnedTaskId,
    current_kernel_thread_spawner, register_kernel_thread_spawner, spawn,
};
pub use switch::{
    TaskExitHook, init_current_context, register_task_exit_hook, run_switch, switch_context,
    switch_registers, task_entry_trampoline,
};
pub use task::TaskContext;
