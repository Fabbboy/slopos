use core::ffi::c_void;

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
    TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN, TASK_PRIORITY_HIGH,
    TASK_PRIORITY_IDLE, TASK_PRIORITY_LOW, TASK_PRIORITY_NORMAL, TASK_STACK_SIZE, TaskExitReason,
    TaskExitRecord, TaskFaultReason, TaskStatus,
};
pub use slopos_lib::arch::idt::IdtEntry;

pub use task_cleanup_hooks::*;
pub use task_lifecycle::*;
pub use task_session::*;
pub use task_state::*;
pub use task_stats::*;
pub use task_table::*;

pub type TaskIterateCb = Option<fn(*mut Task, *mut c_void)>;
pub type TaskEntry = fn(*mut c_void);
