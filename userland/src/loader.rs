
use core::ffi::{c_char, c_void};

use slopos_sched::{task_create, TaskEntry, INVALID_TASK_ID, TASK_FLAG_USER_MODE};

#[unsafe(no_mangle)]
#[unsafe(link_section = ".user_text")]
pub fn user_spawn_program(
    name: *const c_char,
    entry_point: TaskEntry,
    arg: *mut c_void,
    priority: u8,
) -> u32 {
    if entry_point as usize == 0 {
        return INVALID_TASK_ID;
    }
    task_create(name, entry_point, arg, priority, TASK_FLAG_USER_MODE)
}
