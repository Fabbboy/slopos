//! Kernel task control block + context-switch primitives.
//!
//! The live control block is [`kernel_task::TaskInner`], aliased `Task` in the
//! scheduling crate. OSTD owns [`TaskContext`], the switch assembly, the
//! kernel-thread spawner, and the [`LinkProvider`] hook that keeps the unsafe
//! intrusive-list `Linked` impls inside the trusted core.

pub mod abi;
pub mod addr;
pub mod bootstrap;
pub mod borrowed;
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
pub mod ops;
pub mod pcr_ty;
pub mod placement;
pub mod spawner;
pub mod state;
pub mod switch;
pub mod task;
pub mod test_reports;

pub use addr::TaskAddr;
pub use cell::{CurrentTask, IdleTask, SwitchWindow, TaskExclusive, TaskOwnCell};
pub use diag::{TaskDiag, current_task_diag};
pub use drop_context::{
    assert_task_drop_context, drop_context_is_safe, drop_off_lock, run_off_lock,
};
pub use exit_info::ExitInfo;
pub use job_control::{ProcessGroup, Session, new_group_in_session, new_session_group};
pub use kernel_task::SchedPlacement;
pub use link_roles::{ReadyQueueRole, RemoteWakeRole, SiblingRole};
#[cfg(any(test, feature = "test-helpers"))]
pub use pcr_ty::HostStack;
pub use pcr_ty::{PcrStackTy, PcrTaskType};
pub use placement::{
    ParkedTask, parked_task_has_exited, reverse_detached_chain, task_destroy_parked,
    task_existence_is_parked, task_existence_park, task_existence_parked_count,
    task_existence_release, task_parked_leak, task_parked_reclaim, task_placement_clone,
    task_placement_leak, task_placement_reclaim, task_placement_retain,
    task_placement_strong_count, task_release_strong, with_parked, with_parked_node,
};
pub use state::{TaskState, TaskStateView};
pub use test_reports::{TestReport, TestReportRing, alloc_ring, empty_report};

pub use fpu::{
    FPU_STATE_SIZE, FXSAVE_AREA_SIZE, FpuState, MXCSR_DEFAULT, XCOMP_BV_OFFSET, XSTATE_BV_OFFSET,
    XSTATE_RESERVED_OFFSET, XsaveImageError, fpu_xrstor, fpu_xsave, validate_xsave_image,
};
pub use fpu_owner::{
    FPU_CPU_NONE, fpu_current_cpu, fpu_owner_assert_may_take, fpu_owner_forget, fpu_owner_take,
    fpu_owner_yield_after_save, fpu_state_valid,
};
pub use handles::{DLinkProvider, LinkProvider};
pub use spawner::{
    KernelThreadEntry, KernelThreadSpawner, SpawnError, SpawnedTaskId,
    current_kernel_thread_spawner, register_kernel_thread_spawner, spawn,
};
pub use switch::{
    TaskExitHook, UserTaskEntry, register_task_exit_hook, register_user_task_entry, run_switch,
    switch_context, switch_registers, task_entry_trampoline, user_task_entry_addr,
};
pub use task::TaskContext;
