use core::ffi::c_char;
use core::ptr;

use slopos_boot::early_init::{BootInitStep, boot_init_priority};
use slopos_core::syscall::register_spawn_task_callback;
use slopos_core::{
    INVALID_TASK_ID, TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_STATE_BLOCKED,
    TASK_STATE_RUNNING, Task, TaskEntry, schedule_task, task_get_info, task_set_state,
    task_terminate,
};
use slopos_lib::klog_info;
use slopos_mm::process_vm::process_vm_load_elf;

use crate::loader::user_spawn_program_with_flags;

#[unsafe(link_section = ".user_text")]
fn log_info(msg: &str) {
    klog_info!("{msg}");
}

#[unsafe(link_section = ".user_text")]
fn userland_spawn_with_flags(name: &[u8], priority: u8, flags: u16) -> i32 {
    let dummy_entry: TaskEntry = unsafe { core::mem::transmute(0x400000usize) };
    let task_id = user_spawn_program_with_flags(
        name.as_ptr() as *const c_char,
        dummy_entry,
        ptr::null_mut(),
        priority,
        flags,
    );
    if task_id == INVALID_TASK_ID {
        return -1;
    }

    let mut task_info: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_info) != 0 || task_info.is_null() {
        return -1;
    }

    let pid = unsafe { (*task_info).process_id };
    let mut new_entry: u64 = 0;

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
    } else if name == b"compositor\0" {
        const COMPOSITOR_ELF: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../builddir/compositor.elf"
        ));
        COMPOSITOR_ELF
    } else if name == b"file_manager\0" {
        const FILE_MANAGER_ELF: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../builddir/file_manager.elf"
        ));
        FILE_MANAGER_ELF
    } else if name == b"sysinfo\0" {
        const SYSINFO_ELF: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../builddir/sysinfo.elf"
        ));
        SYSINFO_ELF
    } else {
        log_info("USERLAND: Unknown task name, cannot load ELF\n");
        task_terminate(task_id);
        return -1;
    };

    if process_vm_load_elf(pid, elf_data.as_ptr(), elf_data.len(), &mut new_entry) != 0
        || new_entry == 0
    {
        log_info("USERLAND: ELF load failed\n");
        task_terminate(task_id);
        return -1;
    }

    unsafe {
        (*task_info).entry_point = new_entry;
        ptr::write_unaligned(ptr::addr_of_mut!((*task_info).context.rip), new_entry);
    }

    task_id as i32
}

#[unsafe(link_section = ".user_text")]
pub fn spawn_task_by_name(name: &[u8]) -> i32 {
    let task_id = userland_spawn_with_flags(name, 5, 0);
    if task_id <= 0 {
        return task_id;
    }

    let mut task_info: *mut Task = ptr::null_mut();
    if task_get_info(task_id as u32, &mut task_info) != 0 || task_info.is_null() {
        task_terminate(task_id as u32);
        return -1;
    }

    if schedule_task(task_info) != 0 {
        task_terminate(task_id as u32);
        return -1;
    }

    task_id
}

#[unsafe(link_section = ".user_text")]
fn block_task_on(task_id: u32, task_info: *mut Task, wait_on: u32) -> i32 {
    if task_info.is_null() {
        return -1;
    }
    unsafe {
        (*task_info).waiting_on_task_id = wait_on;
    }
    if task_set_state(task_id, TASK_STATE_RUNNING) != 0 {
        return -1;
    }
    if task_set_state(task_id, TASK_STATE_BLOCKED) != 0 {
        return -1;
    }
    0
}

#[unsafe(link_section = ".user_text")]
fn boot_step_userland_preinit() -> i32 {
    register_spawn_task_callback(spawn_task_by_name);

    let shell_id = userland_spawn_with_flags(b"shell\0", 5, 0);
    if shell_id <= 0 {
        log_info("USERLAND: Failed to create shell init task\n");
        return -1;
    }

    let compositor_id = userland_spawn_with_flags(b"compositor\0", 4, TASK_FLAG_COMPOSITOR);
    if compositor_id <= 0 {
        log_info("USERLAND: Failed to create compositor task\n");
        task_terminate(shell_id as u32);
        return -1;
    }

    let roulette_id = userland_spawn_with_flags(b"roulette\0", 5, TASK_FLAG_DISPLAY_EXCLUSIVE);
    if roulette_id <= 0 {
        log_info("USERLAND: Failed to create roulette task\n");
        task_terminate(shell_id as u32);
        task_terminate(compositor_id as u32);
        return -1;
    }

    let mut shell_info: *mut Task = ptr::null_mut();
    if task_get_info(shell_id as u32, &mut shell_info) != 0 {
        log_info("USERLAND: Failed to fetch shell init task info\n");
        task_terminate(shell_id as u32);
        task_terminate(compositor_id as u32);
        task_terminate(roulette_id as u32);
        return -1;
    }

    let mut compositor_info: *mut Task = ptr::null_mut();
    if task_get_info(compositor_id as u32, &mut compositor_info) != 0 {
        log_info("USERLAND: Failed to fetch compositor task info\n");
        task_terminate(shell_id as u32);
        task_terminate(compositor_id as u32);
        task_terminate(roulette_id as u32);
        return -1;
    }

    if block_task_on(shell_id as u32, shell_info, roulette_id as u32) != 0 {
        log_info("USERLAND: Failed to block shell init task\n");
        task_terminate(shell_id as u32);
        task_terminate(compositor_id as u32);
        task_terminate(roulette_id as u32);
        return -1;
    }

    if block_task_on(compositor_id as u32, compositor_info, roulette_id as u32) != 0 {
        log_info("USERLAND: Failed to block compositor task\n");
        task_terminate(shell_id as u32);
        task_terminate(compositor_id as u32);
        task_terminate(roulette_id as u32);
        return -1;
    }

    let mut roulette_info: *mut Task = ptr::null_mut();
    if task_get_info(roulette_id as u32, &mut roulette_info) != 0 || roulette_info.is_null() {
        log_info("USERLAND: Failed to fetch roulette task info\n");
        task_terminate(shell_id as u32);
        task_terminate(compositor_id as u32);
        task_terminate(roulette_id as u32);
        return -1;
    }

    if schedule_task(roulette_info) != 0 {
        log_info("USERLAND: Failed to schedule roulette task\n");
        task_terminate(shell_id as u32);
        task_terminate(compositor_id as u32);
        task_terminate(roulette_id as u32);
        return -1;
    }

    0
}

#[used]
#[unsafe(link_section = ".boot_init_services")]
static BOOT_STEP_USERLAND_HOOK: BootInitStep = BootInitStep::new(
    b"userland pre-init\0",
    boot_step_userland_preinit,
    boot_init_priority(35),
);
