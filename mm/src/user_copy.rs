#![allow(dead_code)]

use core::ffi::{c_int, c_void};
use core::ptr;

use crate::memory_layout::mm_get_kernel_heap_start;
use crate::paging::paging_is_user_accessible;
use crate::process_vm::process_vm_get_page_dir;

#[repr(C)]
pub struct Task {
    pub process_id: u32,
}

unsafe extern "C" {
    fn scheduler_get_current_task() -> *mut Task;
}

static mut KERNEL_GUARD_CHECKED: bool = false;

fn current_process_dir() -> *mut crate::paging::ProcessPageDir {
    unsafe {
        let task = scheduler_get_current_task();
        if task.is_null() || (*task).process_id == crate::mm_constants::INVALID_PROCESS_ID {
            return ptr::null_mut();
        }
        process_vm_get_page_dir((*task).process_id)
    }
}

fn validate_user_buffer(user_ptr: u64, len: usize, dir: *mut crate::paging::ProcessPageDir) -> c_int {
    if len == 0 {
        return 0;
    }
    if dir.is_null() {
        return -1;
    }

    let start = user_ptr;
    let end = start.wrapping_add(len as u64);
    if end < start {
        return -1;
    }

    unsafe {
        if !KERNEL_GUARD_CHECKED {
            let kernel_probe = mm_get_kernel_heap_start();
            if paging_is_user_accessible(dir, kernel_probe) != 0 {
                return -1;
            }
            KERNEL_GUARD_CHECKED = true;
        }
    }

    let mut page = start & !(crate::mm_constants::PAGE_SIZE_4KB - 1);
    while page < end {
        if paging_is_user_accessible(dir, page) == 0 {
            return -1;
        }
        page = page.wrapping_add(crate::mm_constants::PAGE_SIZE_4KB);
    }
    0
}

#[unsafe(no_mangle)]
pub fn user_copy_from_user(kernel_dst: *mut c_void, user_src: *const c_void, len: usize) -> c_int {
    let dir = current_process_dir();
    if kernel_dst.is_null() || user_src.is_null() {
        return -1;
    }
    if validate_user_buffer(user_src as u64, len, dir) != 0 {
        return -1;
    }
    unsafe {
        ptr::copy_nonoverlapping(user_src, kernel_dst, len);
    }
    0
}

#[unsafe(no_mangle)]
pub fn user_copy_to_user(user_dst: *mut c_void, kernel_src: *const c_void, len: usize) -> c_int {
    let dir = current_process_dir();
    if user_dst.is_null() || kernel_src.is_null() {
        return -1;
    }
    if validate_user_buffer(user_dst as u64, len, dir) != 0 {
        return -1;
    }
    unsafe {
        ptr::copy_nonoverlapping(kernel_src, user_dst, len);
    }
    0
}
