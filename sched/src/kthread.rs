use core::ffi::{c_char, c_int, c_void};

use slopos_lib::klog_info;
use slopos_lib::string::cstr_to_str;

use crate::scheduler;
use crate::scheduler::task_wait_for;
use crate::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_PRIORITY_NORMAL, TaskEntry, task_create,
};

pub type KthreadId = u32;
pub fn kthread_spawn(
    name: *const c_char,
    entry_point: Option<TaskEntry>,
    arg: *mut c_void,
) -> KthreadId {
    kthread_spawn_ex(name, entry_point, arg, TASK_PRIORITY_NORMAL, 0)
}
pub fn kthread_spawn_ex(
    name: *const c_char,
    entry_point: Option<TaskEntry>,
    arg: *mut c_void,
    priority: u8,
    flags: u16,
) -> KthreadId {
    if name.is_null() || entry_point.is_none() {
        klog_info!("kthread_spawn_ex: invalid parameters");
        return INVALID_TASK_ID;
    }

    let combined_flags = flags | TASK_FLAG_KERNEL_MODE;
    let id = task_create(name, entry_point.unwrap(), arg, priority, combined_flags);

    if id == INVALID_TASK_ID {
        klog_info!("kthread_spawn_ex: failed to create thread '{}'", unsafe {
            cstr_to_str(name)
        });
    }

    id
}
pub fn kthread_yield() {
    scheduler::r#yield();
}
pub fn kthread_join(thread_id: KthreadId) -> c_int {
    task_wait_for(thread_id)
}
pub fn kthread_exit() -> ! {
    crate::ffi_boundary::scheduler_task_exit();
}
