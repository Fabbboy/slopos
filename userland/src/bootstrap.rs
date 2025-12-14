#![allow(dead_code)]

use core::ffi::{c_char, CStr};
use core::ptr;

use slopos_boot::early_init::{boot_init_priority, BootInitStep};
use slopos_drivers::wl_currency;
use slopos_lib::klog_info;
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

unsafe extern "C" {
    fn fate_register_outcome_hook(cb: FateHook);
    fn process_vm_load_elf(process_id: u32, payload: *const u8, payload_len: usize, entry_out: *mut u64) -> i32;
}

#[inline(always)]
fn is_win(res: &FateResult) -> bool {
    res.value & 1 == 1
}

#[unsafe(link_section = ".user_text")]
fn log_info(msg: &str) {
    klog_info!("{msg}");
}

#[unsafe(link_section = ".user_text")]
fn with_task_name(name: *const c_char, f: impl FnOnce(&str)) {
    let task_name = unsafe {
        if name.is_null() {
            "<null>"
        } else {
            CStr::from_ptr(name)
                .to_str()
                .unwrap_or("<invalid utf-8>")
        }
    };

    f(task_name);
}

#[unsafe(link_section = ".user_text")]
fn userland_spawn_and_schedule(name: &[u8], entry: TaskEntry, priority: u8) -> i32 {
    let task_id = user_spawn_program(name.as_ptr() as *const c_char, entry, ptr::null_mut(), priority);
    if task_id == INVALID_TASK_ID {
        with_task_name(name.as_ptr() as *const c_char, |task_name| {
            klog_info!("USERLAND: Failed to create task '{}'\n", task_name);
        });
        wl_currency::award_loss();
        return -1;
    }

    let mut task_info: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_info) != 0 || task_info.is_null() {
        with_task_name(name.as_ptr() as *const c_char, |task_name| {
            klog_info!("USERLAND: Failed to fetch task info for '{}'\n", task_name);
        });
        wl_currency::award_loss();
        return -1;
    }

    // Load bundled user payload ELF into the new process address space and repoint entry.
    let mut new_entry: u64 = 0;
    const ROULETTE_ELF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../builddir/roulette_payload.elf"));
    let pid = unsafe { (*task_info).process_id };
    if unsafe { process_vm_load_elf(pid, ROULETTE_ELF.as_ptr(), ROULETTE_ELF.len(), &mut new_entry) } != 0 || new_entry == 0 {
        with_task_name(name.as_ptr() as *const c_char, |task_name| {
            klog_info!("USERLAND: Failed to load roulette payload ELF for '{}'\n", task_name);
        });
        wl_currency::award_loss();
        task_terminate(task_id);
        return -1;
    }

    unsafe {
        (*task_info).entry_point = new_entry;
        (*task_info).context.rip = new_entry;
    }

    if schedule_task(task_info) != 0 {
        with_task_name(name.as_ptr() as *const c_char, |task_name| {
            klog_info!("USERLAND: Failed to schedule task '{}'\n", task_name);
        });
        wl_currency::award_loss();
        task_terminate(task_id);
        return -1;
    }

    wl_currency::award_win();
    0
}

#[unsafe(link_section = ".user_bss")]
static mut SHELL_SPAWNED: bool = false;

#[unsafe(link_section = ".user_text")]
fn userland_launch_shell_once() -> i32 {
    unsafe {
        if SHELL_SPAWNED {
            return 0;
        }
    }
    if userland_spawn_and_schedule(b"shell\0", shell_user_main, 5) != 0 {
        log_info("USERLAND: Shell failed to start after roulette win\n");
        return -1;
    }
    unsafe {
        SHELL_SPAWNED = true;
    }
    0
}

#[unsafe(link_section = ".user_text")]
extern "C" fn userland_fate_hook(res: *const FateResult) {
    if res.is_null() {
        return;
    }
    let result = unsafe { *res };
    if !is_win(&result) {
        return;
    }
    if userland_launch_shell_once() != 0 {
        log_info("USERLAND: Shell bootstrap hook failed\n");
    }
}

#[unsafe(link_section = ".user_text")]
extern "C" fn boot_step_userland_hook() -> i32 {
    unsafe {
        fate_register_outcome_hook(userland_fate_hook);
    }
    0
}

#[unsafe(link_section = ".user_text")]
extern "C" fn boot_step_roulette_task() -> i32 {
    userland_spawn_and_schedule(b"roulette\0", roulette_user_main, 5)
}

#[used]
#[unsafe(link_section = ".boot_init_services")]
static BOOT_STEP_USERLAND_HOOK: BootInitStep = BootInitStep::new(
    b"userland fate hook\0",
    boot_step_userland_hook,
    boot_init_priority(35),
);

#[used]
#[unsafe(link_section = ".boot_init_services")]
static BOOT_STEP_ROULETTE_TASK: BootInitStep = BootInitStep::new(
    b"roulette task\0",
    boot_step_roulette_task,
    boot_init_priority(40),
);
