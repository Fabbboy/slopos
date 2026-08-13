use core::ffi::c_void;

mod pending_spawn;
mod task_cleanup_hooks;
mod task_family;
mod task_lifecycle;
mod task_ops;
pub mod task_quota;
mod task_reclaim;
mod task_session;
mod task_state;
mod task_stats;
mod task_table;
mod user_ctx_init;

pub use super::task_struct::{FpuState, Task, TaskContext};
pub use slopos_abi::task::{
    BlockReason, INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_COMPOSITOR,
    TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM,
    TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN, TASK_STACK_SIZE,
    TASK_UNSAFE_STACK_SIZE, TaskExitReason, TaskExitRecord, TaskFaultReason, TaskPriority,
    TaskStatus,
};
pub use slopos_arch::arch::idt::IdtEntry;

pub use pending_spawn::SpawnGuard;
pub use task_cleanup_hooks::*;
pub use task_family::*;
pub use task_lifecycle::*;
pub use task_ops::*;
pub use task_reclaim::*;
pub use task_session::*;
pub use task_state::*;
pub use task_stats::*;
pub use task_table::*;
pub use user_ctx_init::init_user_ctx_for_new_task;

/// Kernel-task entry-point function pointer.
///
/// Always `extern "C"` so caller side (driver-spawned kernel
/// threads, idle tasks, exec'd user-mode round-trippers) can hand a
/// bare `extern "C" fn(*mut c_void)` straight to the scheduler
/// without a transmute. Mirrors the `extern "sysv64" fn() -> !`
/// pattern used by the OSTD task-exit hook.
pub type TaskEntry = extern "C" fn(*mut c_void);

/// Build a [`TaskEntry`] from a kernel-half virtual address.
///
/// Used by the exec path to convert `PROCESS_CODE_START_VA` (a usize
/// constant) into the function-pointer shape the scheduler expects.
/// The reinterpretation is sound because `TaskEntry` and `usize` have
/// the same size + layout on x86_64; the value is a kernel-mapped
/// instruction-pointer that the user-mode round-trip will jump to.
///
/// Caller invariant: `addr` names a valid instruction sequence
/// reachable on next dispatch. Every caller passes a kernel-defined
/// constant.
///
/// # Panics
///
/// Panics if `addr` is zero. A null entry point would dispatch a task
/// straight into address zero, so it is a programming error rather than
/// a value to propagate.
#[inline]
pub fn task_entry_from_kernel_va(addr: u64) -> TaskEntry {
    slopos_ostd::util::fn_ptr::fn_ptr_decode_opt::<TaskEntry>(addr as *mut ())
        .expect("task entry VA must be non-null")
}
