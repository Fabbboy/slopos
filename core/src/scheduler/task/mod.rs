use core::ffi::c_void;

mod task_accessors;
mod task_cleanup_hooks;
mod task_lifecycle;
mod task_session;
mod task_state;
mod task_stats;
mod task_table;

pub use super::task_struct::{FpuState, Task, TaskContext};
pub use slopos_abi::task::{
    BlockReason, INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_COMPOSITOR,
    TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM,
    TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN, TASK_STACK_SIZE,
    TASK_UNSAFE_STACK_SIZE, TaskExitReason, TaskExitRecord, TaskFaultReason, TaskPriority,
    TaskStatus,
};
pub use slopos_arch::arch::idt::IdtEntry;

pub use task_accessors::*;
pub use task_cleanup_hooks::*;
pub use task_lifecycle::*;
pub use task_session::*;
pub use task_state::*;
pub use task_stats::*;
pub use task_table::*;

pub type TaskIterateCb = Option<fn(*mut Task, *mut c_void)>;
pub type TaskEntry = fn(*mut c_void);

/// Build a [`TaskEntry`] from a kernel-half virtual address.
///
/// Used by the exec path to convert `PROCESS_CODE_START_VA` (a usize
/// constant) into the function-pointer shape the scheduler expects.
/// The transmute is sound because `TaskEntry` and `usize` have the
/// same size + layout on x86_64; the value is a kernel-mapped
/// instruction-pointer that the user-mode round-trip will jump to.
///
/// SAFETY (caller, weakly): `addr` must point at a valid user/kernel
/// instruction sequence reachable on next dispatch. All current
/// callers pass a kernel-defined constant.
#[inline]
pub fn task_entry_from_kernel_va(addr: u64) -> TaskEntry {
    // SAFETY: see doc comment; transmuting between two same-sized
    // function-pointer-equivalent types.
    unsafe { core::mem::transmute(addr as usize) }
}
