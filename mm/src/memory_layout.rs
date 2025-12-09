#![allow(dead_code)]

use core::ffi::c_void;
use core::ptr;

use crate::mm_constants::{
    BOOT_STACK_PHYS_ADDR, BOOT_STACK_SIZE, HHDM_VIRT_BASE, KERNEL_HEAP_SIZE, KERNEL_HEAP_VBASE,
    KERNEL_VIRTUAL_BASE, PAGE_SIZE_1GB, USER_SPACE_END_VA, USER_SPACE_START_VA,
    PROCESS_CODE_START_VA, PROCESS_DATA_START_VA, PROCESS_HEAP_MAX_VA, PROCESS_HEAP_START_VA,
    PROCESS_STACK_SIZE_BYTES, PROCESS_STACK_TOP_VA,
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KernelMemoryLayout {
    pub kernel_start_phys: u64,
    pub kernel_end_phys: u64,
    pub kernel_start_virt: u64,
    pub kernel_end_virt: u64,
    pub kernel_heap_start: u64,
    pub kernel_heap_end: u64,
    pub kernel_stack_start: u64,
    pub kernel_stack_end: u64,
    pub identity_map_end: u64,
    pub user_space_start: u64,
    pub user_space_end: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ProcessMemoryLayout {
    pub code_start: u64,
    pub data_start: u64,
    pub heap_start: u64,
    pub heap_max: u64,
    pub stack_top: u64,
    pub stack_size: u64,
    pub user_space_start: u64,
    pub user_space_end: u64,
}

static mut KERNEL_LAYOUT: KernelMemoryLayout = KernelMemoryLayout {
    kernel_start_phys: 0,
    kernel_end_phys: 0,
    kernel_start_virt: 0,
    kernel_end_virt: 0,
    kernel_heap_start: 0,
    kernel_heap_end: 0,
    kernel_stack_start: 0,
    kernel_stack_end: 0,
    identity_map_end: 0,
    user_space_start: 0,
    user_space_end: 0,
};
static mut LAYOUT_INITIALIZED: bool = false;

static PROCESS_LAYOUT: ProcessMemoryLayout = ProcessMemoryLayout {
    code_start: PROCESS_CODE_START_VA,
    data_start: PROCESS_DATA_START_VA,
    heap_start: PROCESS_HEAP_START_VA,
    heap_max: PROCESS_HEAP_MAX_VA,
    stack_top: PROCESS_STACK_TOP_VA,
    stack_size: PROCESS_STACK_SIZE_BYTES,
    user_space_start: USER_SPACE_START_VA,
    user_space_end: USER_SPACE_END_VA,
};

extern "C" {
    static _kernel_start: c_void;
    static _kernel_end: c_void;
    fn klog_debug(fmt: *const u8, ...);
}

fn ptr_as_u64(p: *const c_void) -> u64 {
    p as usize as u64
}

#[no_mangle]
pub extern "C" fn init_kernel_memory_layout() {
    unsafe {
        let start_phys = ptr_as_u64(&_kernel_start);
        let end_phys = ptr_as_u64(&_kernel_end);

        KERNEL_LAYOUT.kernel_start_phys = start_phys;
        KERNEL_LAYOUT.kernel_end_phys = end_phys;

        KERNEL_LAYOUT.kernel_start_virt = KERNEL_VIRTUAL_BASE;
        KERNEL_LAYOUT.kernel_end_virt = KERNEL_VIRTUAL_BASE + (end_phys - start_phys);

        KERNEL_LAYOUT.kernel_heap_start = KERNEL_HEAP_VBASE;
        KERNEL_LAYOUT.kernel_heap_end = KERNEL_HEAP_VBASE + KERNEL_HEAP_SIZE;

        KERNEL_LAYOUT.kernel_stack_start = BOOT_STACK_PHYS_ADDR;
        KERNEL_LAYOUT.kernel_stack_end = BOOT_STACK_PHYS_ADDR + BOOT_STACK_SIZE;

        KERNEL_LAYOUT.identity_map_end = PAGE_SIZE_1GB;
        KERNEL_LAYOUT.user_space_start = USER_SPACE_START_VA;
        KERNEL_LAYOUT.user_space_end = USER_SPACE_END_VA;

        LAYOUT_INITIALIZED = true;

        klog_debug(b"SlopOS: Kernel memory layout initialized\0".as_ptr());
    }
}

#[no_mangle]
pub extern "C" fn get_kernel_memory_layout() -> *const KernelMemoryLayout {
    unsafe {
        if LAYOUT_INITIALIZED {
            &KERNEL_LAYOUT as *const KernelMemoryLayout
        } else {
            ptr::null()
        }
    }
}

#[no_mangle]
pub extern "C" fn mm_get_kernel_phys_start() -> u64 {
    unsafe { KERNEL_LAYOUT.kernel_start_phys }
}

#[no_mangle]
pub extern "C" fn mm_get_kernel_phys_end() -> u64 {
    unsafe { KERNEL_LAYOUT.kernel_end_phys }
}

#[no_mangle]
pub extern "C" fn mm_get_kernel_virt_start() -> u64 {
    unsafe { KERNEL_LAYOUT.kernel_start_virt }
}

#[no_mangle]
pub extern "C" fn mm_get_identity_map_limit() -> u64 {
    unsafe { KERNEL_LAYOUT.identity_map_end }
}

#[no_mangle]
pub extern "C" fn mm_get_kernel_heap_start() -> u64 {
    unsafe { KERNEL_LAYOUT.kernel_heap_start }
}

#[no_mangle]
pub extern "C" fn mm_get_kernel_heap_end() -> u64 {
    unsafe { KERNEL_LAYOUT.kernel_heap_end }
}

#[no_mangle]
pub extern "C" fn mm_get_user_space_start() -> u64 {
    unsafe { KERNEL_LAYOUT.user_space_start }
}

#[no_mangle]
pub extern "C" fn mm_get_user_space_end() -> u64 {
    unsafe { KERNEL_LAYOUT.user_space_end }
}

#[no_mangle]
pub extern "C" fn mm_get_process_layout() -> *const ProcessMemoryLayout {
    &PROCESS_LAYOUT as *const ProcessMemoryLayout
}

