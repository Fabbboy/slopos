use core::ffi::{CStr, c_char};
use core::ptr;

use slopos_boot::early_init::{BootInitStep, boot_init_priority};
use slopos_drivers::{fate, wl_currency};
use slopos_lib::klog_info;
use slopos_mm::process_vm::process_vm_load_elf;
use slopos_sched::{
    INVALID_TASK_ID, Task, TaskEntry, schedule_task, task_get_info, task_terminate,
};

use crate::loader::user_spawn_program;

pub type FateResult = slopos_drivers::fate::FateResult;

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
            CStr::from_ptr(name).to_str().unwrap_or("<invalid utf-8>")
        }
    };

    f(task_name);
}

#[unsafe(link_section = ".user_text")]
fn userland_spawn_and_schedule(name: &[u8], priority: u8) -> i32 {
    // Create task with dummy entry point - will be replaced by ELF loader
    // Use a dummy function pointer that points to 0x400000 (user code base)
    let dummy_entry: TaskEntry = unsafe { core::mem::transmute(0x400000usize) };
    let task_id = user_spawn_program(
        name.as_ptr() as *const c_char,
        dummy_entry,
        ptr::null_mut(),
        priority,
    );
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

    // Load user program ELF into the new process address space and repoint entry.
    let mut new_entry: u64 = 0;
    let pid = unsafe { (*task_info).process_id };
    
    // Determine which ELF binary to load based on task name
    let elf_data: &[u8] = if name == b"roulette\0" {
        const ROULETTE_ELF: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../builddir/roulette.elf"
        ));
        ROULETTE_ELF
    } else if name == b"shell\0" {
        const SHELL_ELF: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../builddir/shell.elf"
        ));
        SHELL_ELF
    } else {
        with_task_name(name.as_ptr() as *const c_char, |task_name| {
            klog_info!("USERLAND: Unknown task name '{}', cannot load ELF\n", task_name);
        });
        wl_currency::award_loss();
        task_terminate(task_id);
        return -1;
    };

    if process_vm_load_elf(
        pid,
        elf_data.as_ptr(),
        elf_data.len(),
        &mut new_entry,
    ) != 0
        || new_entry == 0
    {
        with_task_name(name.as_ptr() as *const c_char, |task_name| {
            klog_info!(
                "USERLAND: Failed to load ELF for '{}' (new_entry=0x{:x})\n",
                task_name,
                new_entry
            );
        });
        wl_currency::award_loss();
        task_terminate(task_id);
        return -1;
    }

    with_task_name(name.as_ptr() as *const c_char, |task_name| {
        klog_info!(
            "USERLAND: Loaded ELF for '{}', entry point=0x{:x}\n",
            task_name,
            new_entry
        );
    });

    unsafe {
        (*task_info).entry_point = new_entry;
        (*task_info).context.rip = new_entry;
        // Copy to local variables to avoid unaligned access
        let entry_point = (*task_info).entry_point;
        let context_rip = (*task_info).context.rip;
        klog_info!("USERLAND: Updated task entry_point=0x{:x} context.rip=0x{:x}\n", 
                   entry_point, context_rip);
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
    if userland_spawn_and_schedule(b"shell\0", 5) != 0 {
        log_info("USERLAND: Shell failed to start after roulette win\n");
        return -1;
    }
    unsafe {
        SHELL_SPAWNED = true;
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn userland_fate_hook(res: *const FateResult) {
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
fn boot_step_userland_hook() -> i32 {
    fate::fate_register_outcome_hook(userland_fate_hook);
    0
}

#[unsafe(link_section = ".user_text")]
fn boot_step_roulette_task() -> i32 {
    userland_spawn_and_schedule(b"roulette\0", 5)
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
