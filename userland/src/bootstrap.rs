#![allow(dead_code)]

use core::ffi::c_char;
use core::ptr;

use slopos_boot::early_init::{boot_init_priority, BootInitStep};
use slopos_drivers::wl_currency;
use slopos_lib::{klog_printf, KlogLevel};
use slopos_sched::{
    schedule_task, task_get_info, task_terminate, Task, TaskEntry, INVALID_TASK_ID,
};

use crate::loader::user_spawn_program;
use crate::roulette::roulette_user_main;
use crate::shell::shell_user_main;

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct FateResult {
    pub token: u32,
    pub value: u32,
}

type FateHook = extern "C" fn(*const FateResult);

extern "C" {
    fn fate_register_outcome_hook(cb: FateHook);
}

#[inline(always)]
fn is_win(res: &FateResult) -> bool {
    res.value & 1 == 1
}

#[link_section = ".user_text"]
fn log_info(msg: &[u8]) {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            msg.as_ptr() as *const c_char,
        );
    }
}

#[link_section = ".user_text"]
fn log_info_name(msg: &[u8], name: *const c_char) {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            msg.as_ptr() as *const c_char,
            name,
        );
    }
}

#[link_section = ".user_text"]
fn userland_spawn_and_schedule(name: &[u8], entry: TaskEntry, priority: u8) -> i32 {
    let task_id = user_spawn_program(name.as_ptr() as *const c_char, entry, ptr::null_mut(), priority);
    if task_id == INVALID_TASK_ID {
        log_info_name(b"USERLAND: Failed to create task '%s'\n\0", name.as_ptr() as *const c_char);
        wl_currency::award_loss();
        return -1;
    }

    let mut task_info: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_info) != 0 || task_info.is_null() {
        log_info_name(b"USERLAND: Failed to fetch task info for '%s'\n\0", name.as_ptr() as *const c_char);
        wl_currency::award_loss();
        return -1;
    }

    if schedule_task(task_info) != 0 {
        log_info_name(b"USERLAND: Failed to schedule task '%s'\n\0", name.as_ptr() as *const c_char);
        wl_currency::award_loss();
        task_terminate(task_id);
        return -1;
    }

    wl_currency::award_win();
    0
}

static mut SHELL_SPAWNED: bool = false;

#[link_section = ".user_text"]
fn userland_launch_shell_once() -> i32 {
    unsafe {
        if SHELL_SPAWNED {
            return 0;
        }
    }
    if userland_spawn_and_schedule(b"shell\0", shell_user_main, 5) != 0 {
        log_info(b"USERLAND: Shell failed to start after roulette win\n\0");
        return -1;
    }
    unsafe {
        SHELL_SPAWNED = true;
    }
    0
}

#[link_section = ".user_text"]
extern "C" fn userland_fate_hook(res: *const FateResult) {
    if res.is_null() {
        return;
    }
    let result = unsafe { *res };
    if !is_win(&result) {
        return;
    }
    if userland_launch_shell_once() != 0 {
        log_info(b"USERLAND: Shell bootstrap hook failed\n\0");
    }
}

#[link_section = ".user_text"]
extern "C" fn boot_step_userland_hook() -> i32 {
    unsafe {
        fate_register_outcome_hook(userland_fate_hook);
    }
    0
}

#[link_section = ".user_text"]
extern "C" fn boot_step_roulette_task() -> i32 {
    userland_spawn_and_schedule(b"roulette\0", roulette_user_main, 5)
}

#[used]
#[link_section = ".boot_init_services"]
static BOOT_STEP_USERLAND_HOOK: BootInitStep = BootInitStep::new(
    b"userland fate hook\0",
    boot_step_userland_hook,
    boot_init_priority(35),
);

#[used]
#[link_section = ".boot_init_services"]
static BOOT_STEP_ROULETTE_TASK: BootInitStep = BootInitStep::new(
    b"roulette task\0",
    boot_step_roulette_task,
    boot_init_priority(40),
);
