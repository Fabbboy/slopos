//! Re-export task types from abi for backwards compatibility with syscall macros.

// Re-export all task types and constants from abi
pub use slopos_abi::task::{
    Task, TaskContext, TaskExitReason, TaskFaultReason, INVALID_PROCESS_ID, INVALID_TASK_ID,
    TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT,
    TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_STATE_BLOCKED, TASK_STATE_INVALID,
    TASK_STATE_READY, TASK_STATE_RUNNING, TASK_STATE_TERMINATED,
};

// Re-export InterruptFrame from lib
pub use slopos_lib::InterruptFrame;
