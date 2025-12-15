use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use crate::memory_layout::mm_get_kernel_heap_start;
use crate::paging::paging_is_user_accessible;
use crate::process_vm::process_vm_get_page_dir;

#[repr(C)]
pub struct Task {
    pub process_id: u32,
}

static KERNEL_GUARD_CHECKED: AtomicBool = AtomicBool::new(false);
static CURRENT_TASK_PROVIDER: Mutex<Option<fn() -> u32>> = Mutex::new(None);

pub fn register_current_task_provider(provider: fn() -> u32) {
    *CURRENT_TASK_PROVIDER.lock() = Some(provider);
}

fn current_process_id() -> u32 {
    let guard = CURRENT_TASK_PROVIDER.lock();
    if let Some(cb) = *guard {
        cb()
    } else {
        crate::mm_constants::INVALID_PROCESS_ID
    }
}

fn current_process_dir() -> *mut crate::paging::ProcessPageDir {
    let pid = current_process_id();
    if pid == crate::mm_constants::INVALID_PROCESS_ID {
        return ptr::null_mut();
    }
    process_vm_get_page_dir(pid)
}

fn validate_user_buffer(
    user_ptr: u64,
    len: usize,
    dir: *mut crate::paging::ProcessPageDir,
) -> c_int {
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

    if !KERNEL_GUARD_CHECKED.load(Ordering::Acquire) {
        let kernel_probe = mm_get_kernel_heap_start();
        if paging_is_user_accessible(dir, kernel_probe) != 0 {
            return -1;
        }
        KERNEL_GUARD_CHECKED.store(true, Ordering::Release);
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
