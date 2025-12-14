use slopos_lib::klog::{self, KlogLevel};
use slopos_lib::{klog_debug, klog_info};

use crate::boot_init_step;
use crate::early_init::{boot_get_hhdm_offset, boot_get_memmap};
use crate::limine_protocol::LimineMemmapResponse;

unsafe extern "C" {
    fn init_memory_system(memmap: *const LimineMemmapResponse, hhdm_offset: u64) -> i32;
}

const KERNEL_VIRTUAL_BASE: u64 = 0xFFFFFFFF80000000;

extern "C" fn boot_step_memory_init() -> i32 {
    let memmap = boot_get_memmap();
    if memmap.is_null() {
        klog_info!("ERROR: Memory map not available");
        return -1;
    }

    let hhdm = boot_get_hhdm_offset();

    klog_debug!("Initializing memory management from Limine data...");
    let rc = unsafe { init_memory_system(memmap, hhdm) };
    if rc != 0 {
        klog_info!("ERROR: Memory system initialization failed");
        return -1;
    }

    klog_debug!("Memory management initialized.");
    0
}

extern "C" fn boot_step_memory_verify() {
    let stack_ptr: u64;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) stack_ptr, options(nomem, preserves_flags));
    }

    if klog::is_enabled_level(KlogLevel::Debug) {
        klog_debug!("Stack pointer read successfully!");
        klog_info!("Current Stack Pointer: 0x{:x}", stack_ptr);

        let current_ip = boot_step_memory_verify as *const () as usize as u64;
        klog_info!("Kernel Code Address: 0x{:x}", current_ip);

        if current_ip >= KERNEL_VIRTUAL_BASE {
            klog_debug!("Running in higher-half virtual memory - CORRECT");
        } else {
            klog_info!("WARNING: Not running in higher-half virtual memory");
        }
    }
}

boot_init_step!(
    BOOT_STEP_MEMORY_INIT,
    memory,
    b"memory init\0",
    boot_step_memory_init
);
crate::boot_init_step_unit!(
    BOOT_STEP_MEMORY_VERIFY,
    memory,
    b"address verification\0",
    boot_step_memory_verify
);
