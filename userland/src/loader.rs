use core::ffi::{c_char, c_void};

use slopos_core::{TASK_FLAG_USER_MODE, TaskEntry, task_create};
#[unsafe(link_section = ".user_text")]
pub fn user_spawn_program(
    name: *const c_char,
    entry_point: TaskEntry,
    arg: *mut c_void,
    priority: u8,
) -> u32 {
    user_spawn_program_with_flags(name, entry_point, arg, priority, TASK_FLAG_USER_MODE)
}

#[unsafe(link_section = ".user_text")]
pub fn user_spawn_program_with_flags(
    name: *const c_char,
    entry_point: TaskEntry,
    arg: *mut c_void,
    priority: u8,
    flags: u16,
) -> u32 {
    // Allow null entry point - it will be set by ELF loader
    // Use a dummy function pointer if null to satisfy task_create
    let entry = if entry_point as usize == 0 {
        // Dummy entry point - will be replaced by ELF loader
        unsafe { core::mem::transmute(0x400000usize) }
    } else {
        entry_point
    };
    let final_flags = if flags & TASK_FLAG_USER_MODE == 0 {
        flags | TASK_FLAG_USER_MODE
    } else {
        flags
    };
    task_create(name, entry, arg, priority, final_flags)
}
