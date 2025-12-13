use core::ffi::c_char;

use slopos_lib::{klog_is_enabled, klog_printf, KlogLevel};

use crate::early_init::{boot_get_hhdm_offset, boot_get_memmap, BootInitStep};
use crate::limine_protocol::LimineMemmapResponse;

extern "C" {
    fn init_memory_system(memmap: *const LimineMemmapResponse, hhdm_offset: u64) -> i32;
}

const KERNEL_VIRTUAL_BASE: u64 = 0xFFFFFFFF80000000;

fn log(level: KlogLevel, msg: &[u8]) {
    unsafe { klog_printf(level, msg.as_ptr() as *const c_char) };
}

fn log_info(msg: &[u8]) {
    log(KlogLevel::Info, msg);
}

fn log_debug(msg: &[u8]) {
    log(KlogLevel::Debug, msg);
}

extern "C" fn boot_step_memory_init() -> i32 {
    let memmap = boot_get_memmap();
    if memmap.is_null() {
        log_info(b"ERROR: Memory map not available\n\0");
        return -1;
    }

    let hhdm = boot_get_hhdm_offset();

    log_debug(b"Initializing memory management from Limine data...\0");
    let rc = unsafe { init_memory_system(memmap, hhdm) };
    if rc != 0 {
        log_info(b"ERROR: Memory system initialization failed\n\0");
        return -1;
    }

    log_debug(b"Memory management initialized.\0");
    0
}

extern "C" fn boot_step_memory_verify() -> i32 {
    let stack_ptr: u64;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) stack_ptr, options(nomem, preserves_flags));
    }

    if unsafe { klog_is_enabled(KlogLevel::Debug) } != 0 {
        log_debug(b"Stack pointer read successfully!\0");
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Current Stack Pointer: 0x%lx\n\0".as_ptr() as *const c_char,
                stack_ptr,
            );
        }

        let current_ip = boot_step_memory_verify as *const () as usize as u64;
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Kernel Code Address: 0x%lx\n\0".as_ptr() as *const c_char,
                current_ip,
            );
        }

        if current_ip >= KERNEL_VIRTUAL_BASE {
            log_debug(b"Running in higher-half virtual memory - CORRECT\0");
        } else {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"WARNING: Not running in higher-half virtual memory\n\0".as_ptr()
                        as *const c_char,
                );
            }
        }
    }

    0
}

#[used]
#[link_section = ".boot_init_memory"]
static BOOT_STEP_MEMORY_INIT: BootInitStep =
    BootInitStep::new(b"memory init\0", boot_step_memory_init, 0);
#[used]
#[link_section = ".boot_init_memory"]
static BOOT_STEP_MEMORY_VERIFY: BootInitStep =
    BootInitStep::new(b"address verification\0", boot_step_memory_verify, 0);
