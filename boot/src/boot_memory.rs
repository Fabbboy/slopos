use slopos_hermetic::BootCtx;
use slopos_utils::klog::{self, KlogLevel};
use slopos_utils::{klog_debug, klog_info};

use crate::early_init::{boot_get_hhdm_offset, boot_get_memmap, boot_init_priority};

use slopos_arch::cpu::security::{SupervisorFeatures, enable_supervisor_features};
use slopos_mm::memory_init::init_memory_system;
use slopos_mm::memory_layout_defs::KERNEL_VIRTUAL_BASE;

fn boot_step_supervisor_features_fn(_ctx: &mut BootCtx) {
    let SupervisorFeatures { pge, smep, smap } = enable_supervisor_features();
    klog_info!(
        "CPU: supervisor features enabled (PGE={}, SMEP={}, SMAP={})",
        pge,
        smep,
        smap
    );
}

fn boot_step_mmu_asid_init_fn(_ctx: &mut BootCtx) {
    let pcid_live = slopos_mm::mmu::init_bsp();
    klog_debug!("MMU: PCID {}", if pcid_live { "live" } else { "disabled" });
}

fn boot_step_memory_init(_ctx: &mut BootCtx) -> i32 {
    let memmap = boot_get_memmap();
    if memmap.is_null() {
        klog_info!("ERROR: Memory map not available");
        return -1;
    }

    let hhdm = boot_get_hhdm_offset();
    let hhdm_available = crate::limine_protocol::is_hhdm_available() != 0;
    let boot_fb = crate::limine_protocol::boot_info().framebuffer;
    let framebuffer = boot_fb.as_ref().map(|bf| (bf.address as u64, &bf.info));

    klog_debug!("Initializing memory management from Limine data...");
    let rc = init_memory_system(memmap, hhdm, hhdm_available, framebuffer);
    if rc != 0 {
        klog_info!("ERROR: Memory system initialization failed");
        return -1;
    }

    klog_debug!("Memory management initialized.");
    0
}

fn boot_step_memory_verify(_ctx: &mut BootCtx) {
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

crate::boot_init!(
    BOOT_STEP_SUPERVISOR_FEATURES,
    memory,
    b"cpu supervisor features\0",
    boot_step_supervisor_features_fn,
    flags = boot_init_priority(1)
);
crate::boot_init!(
    BOOT_STEP_MEMORY_INIT,
    memory,
    b"memory init\0",
    boot_step_memory_init,
    fallible,
    flags = boot_init_priority(10)
);
crate::boot_init!(
    BOOT_STEP_MEMORY_VERIFY,
    memory,
    b"address verification\0",
    boot_step_memory_verify,
    flags = boot_init_priority(20)
);
crate::boot_init!(
    BOOT_STEP_MMU_ASID_INIT,
    memory,
    b"mmu asid init\0",
    boot_step_mmu_asid_init_fn,
    flags = boot_init_priority(30)
);
