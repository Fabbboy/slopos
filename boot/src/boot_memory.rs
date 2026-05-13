use slopos_hermetic::BootCtx;
use slopos_utils::klog::{self, KlogLevel};
use slopos_utils::{klog_debug, klog_info};

use crate::early_init::{boot_get_hhdm_offset, boot_get_memmap, boot_init_priority};

use slopos_arch::cpu::security::{SupervisorFeatures, enable_supervisor_features};
use slopos_mm::memory_init::{init_memory_system_post_typestate, init_memory_system_pre_typestate};
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

fn boot_step_init_meta_slots_fn(_ctx: &mut BootCtx) {
    // Memory phase priority 5 — runs after the buddy allocator is up
    // (BOOT_STEP_MEMORY_PRE_TYPESTATE, priority 2) and before
    // BOOT_STEP_REGISTER_FRAME_ALLOC at priority 6. `install_meta_slots`
    // itself uses the raw bootstrap escape (`__alloc_page_frames_raw`)
    // because it is bootstrapping META_SLOTS — the typestate path
    // physically cannot work yet.
    let n_slots = slopos_mm::kernel_meta::install_meta_slots();
    klog_info!("OSTD: meta_slots installed ({} entries)", n_slots);
}

fn boot_step_register_frame_alloc_fn(_ctx: &mut BootCtx) {
    // SAFETY: Memory phase priority 6 — runs after the buddy allocator
    // (priority 2) and after meta_slots install (priority 5), which
    // is what the OSTD frame allocator interface needs. After this
    // step the typestate `Frame::<KernelMeta>::alloc` path is live;
    // the post-typestate boot step at priority 10 then performs every
    // remaining kernel page allocation through the typestate gate.
    // Single registration site.
    unsafe {
        slopos_mm::frame_alloc_shim::register_with_ostd();
    }
    klog_info!("OSTD: frame_allocator registered (LegacyFrameAllocShim)");
}

fn boot_step_install_kernel_vm_space_fn(_ctx: &mut BootCtx) {
    // SAFETY: Memory phase priority 55 — runs after meta_slots
    // (priority 5) and frame_alloc (priority 6). The kernel master
    // PML4 paddr was registered with OSTD at early-init time
    // (`register_kernel_master_pml4(read_cr3())` in early_init.rs);
    // CR3 has not been swapped since, so the same paddr is still the
    // live PML4 and is safe to wrap.
    let cr3 = slopos_arch::cpu::control_regs::read_cr3();
    let pml4_phys = slopos_abi::addr::PhysAddr::new(cr3 & 0x000F_FFFF_FFFF_F000);
    unsafe {
        slopos_kernel_services::kernel_vm_space::install_kernel_vm_space(pml4_phys);
    }

    // Stamp GLOBAL onto every kernel-half leaf via the OSTD cursor.
    // This used to live inside `init_paging` (priority 10, legacy
    // walker); routing through the cursor here exercises the
    // huge-leaf-aware `protect::<S>` path on every 2 MiB HHDM entry.
    // CR4.PGE is enabled at priority 1, so the bit is already
    // meaningful on the leaves we're stamping.
    slopos_mm::paging::paging_mark_kernel_global();

    // Force the TLB to re-walk and re-tag kernel entries as global.
    // Intel SDM §4.10.2.4: the CPU may have cached kernel entries
    // before the GLOBAL bit was set; a CR3 reload drops those
    // non-global entries. `VmSpace::activate` writes CR3 and is the
    // sanctioned post-framekernel CR3 entry point.
    // SAFETY: irqs are off in this boot step; KERNEL_VM_SPACE was
    // installed two lines above; kernel-half invariant holds.
    unsafe {
        slopos_kernel_services::kernel_vm_space::kernel_vm_space()
            .lock()
            .activate();
    }

    klog_info!(
        "OSTD: KERNEL_VM_SPACE installed (pml4_phys=0x{:x}, pcid=0)",
        pml4_phys.as_u64()
    );
}

fn boot_step_register_luf_hook_fn(_ctx: &mut BootCtx) {
    // Memory phase priority 56 — runs after KERNEL_VM_SPACE is installed
    // (priority 55) and before any per-process VmSpace cursor mutation
    // can occur (those happen via `create_process_vm`, which is far
    // post-boot). Installing the hook now means every subsequent
    // `CursorMut::unmap` over a USER-flagged leaf and every
    // `VmSpace::activate` routes into slopos-mm's LUF.
    //
    // SAFETY: one-shot install; OSTD's `register_cursor_unmap_hook`
    // asserts on double-call.
    unsafe {
        slopos_mm::mmu::luf_hook::register_with_ostd();
    }
    klog_info!("OSTD: cursor_unmap_hook registered (LufHook)");
}

fn boot_step_memory_pre_typestate(_ctx: &mut BootCtx) -> i32 {
    let memmap = boot_get_memmap();
    if memmap.is_null() {
        klog_info!("ERROR: Memory map not available");
        return -1;
    }

    let hhdm = boot_get_hhdm_offset();
    let hhdm_available = crate::limine_protocol::is_hhdm_available() != 0;
    let boot_fb = crate::limine_protocol::boot_info().framebuffer;
    let framebuffer = boot_fb.as_ref().map(|bf| (*bf.address as u64, &bf.info));

    klog_debug!("Initializing memory management from Limine data...");
    let rc = init_memory_system_pre_typestate(memmap, hhdm, hhdm_available, framebuffer);
    if rc != 0 {
        klog_info!("ERROR: Memory system pre-typestate init failed");
        return -1;
    }

    klog_debug!("Memory pre-typestate phase complete.");
    0
}

fn boot_step_memory_post_typestate(_ctx: &mut BootCtx) -> i32 {
    let rc = init_memory_system_post_typestate();
    if rc != 0 {
        klog_info!("ERROR: Memory system post-typestate init failed");
        return -1;
    }
    klog_debug!("Memory management initialized.");
    0
}

fn boot_step_memory_verify(_ctx: &mut BootCtx) {
    let stack_ptr = slopos_ostd::cpu::x86_64::stack::read_rsp();

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
    BOOT_STEP_MEMORY_PRE_TYPESTATE,
    memory,
    b"memory init pre-typestate\0",
    boot_step_memory_pre_typestate,
    fallible,
    flags = boot_init_priority(2)
);
crate::boot_init!(
    BOOT_STEP_INIT_META_SLOTS,
    memory,
    b"ostd meta_slots\0",
    boot_step_init_meta_slots_fn,
    flags = boot_init_priority(5)
);
crate::boot_init!(
    BOOT_STEP_REGISTER_FRAME_ALLOC,
    memory,
    b"ostd frame_allocator\0",
    boot_step_register_frame_alloc_fn,
    flags = boot_init_priority(6)
);
crate::boot_init!(
    BOOT_STEP_MEMORY_POST_TYPESTATE,
    memory,
    b"memory init post-typestate\0",
    boot_step_memory_post_typestate,
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
crate::boot_init!(
    BOOT_STEP_INSTALL_KERNEL_VM_SPACE,
    memory,
    b"ostd kernel_vm_space\0",
    boot_step_install_kernel_vm_space_fn,
    flags = boot_init_priority(55)
);
crate::boot_init!(
    BOOT_STEP_REGISTER_LUF_HOOK,
    memory,
    b"ostd cursor_unmap_hook\0",
    boot_step_register_luf_hook_fn,
    flags = boot_init_priority(56)
);
