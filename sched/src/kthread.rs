#![allow(dead_code)]

use core::ffi::{c_char, c_int, c_void};

use slopos_lib::klog::{klog_printf, KlogLevel};

use crate::scheduler;
use crate::scheduler::task_wait_for;
use crate::task::{task_create, TaskEntry, INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_PRIORITY_NORMAL};

pub type KthreadId = u32;

#[no_mangle]
pub extern "C" fn kthread_spawn(
    name: *const c_char,
    entry_point: Option<TaskEntry>,
    arg: *mut c_void,
) -> KthreadId {
    kthread_spawn_ex(name, entry_point, arg, TASK_PRIORITY_NORMAL, 0)
}

#[no_mangle]
pub extern "C" fn kthread_spawn_ex(
    name: *const c_char,
    entry_point: Option<TaskEntry>,
    arg: *mut c_void,
    priority: u8,
    flags: u16,
) -> KthreadId {
    if name.is_null() || entry_point.is_none() {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"kthread_spawn_ex: invalid parameters\n\0".as_ptr() as *const c_char,
            );
        }
        return INVALID_TASK_ID;
    }

    let combined_flags = flags | TASK_FLAG_KERNEL_MODE;
    let id = unsafe { task_create(name, entry_point.unwrap(), arg, priority, combined_flags) };

    if id == INVALID_TASK_ID {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"kthread_spawn_ex: failed to create thread '%s'\n\0".as_ptr() as *const c_char,
                name,
            );
        }
    }

    id
}

#[no_mangle]
pub extern "C" fn kthread_yield() {
    scheduler::r#yield();
}

#[no_mangle]
pub extern "C" fn kthread_join(thread_id: KthreadId) -> c_int {
    unsafe { task_wait_for(thread_id) }
}

#[no_mangle]
pub extern "C" fn kthread_exit() -> ! {
    scheduler::scheduler_task_exit();
}

