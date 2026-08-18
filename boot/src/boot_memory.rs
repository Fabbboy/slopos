use slopos_hermetic::{BootCtx, BspInit};
use slopos_ostd::klog::{self, KlogLevel};
use slopos_ostd::lock_class;
use slopos_ostd::{klog_debug, klog_info};

use crate::early_init::{boot_get_hhdm_offset, boot_get_memmap, boot_init_priority};

use slopos_arch::cpu::security::{SupervisorFeatures, enable_supervisor_features};
use slopos_mm::memory_init::{init_memory_system_post_typestate, init_memory_system_pre_typestate};
use slopos_mm::memory_layout_defs::KERNEL_VIRTUAL_BASE;

fn boot_step_supervisor_features_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    let SupervisorFeatures { pge, smep, smap } = enable_supervisor_features();
    klog_info!(
        "CPU: supervisor features enabled (PGE={}, SMEP={}, SMAP={})",
        pge,
        smep,
        smap
    );
}

fn boot_step_mmu_asid_init_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    let pcid_live = slopos_mm::mmu::init_bsp();
    klog_debug!("MMU: PCID {}", if pcid_live { "live" } else { "disabled" });
}

fn boot_step_init_meta_slots_fn(ctx: &mut BootCtx<'_, BspInit>) {
    // Must run after the buddy allocator and before the frame allocator is
    // registered: `install_meta_slots` bootstraps META_SLOTS itself, so the
    // typestate path physically cannot work yet.
    let n_slots = slopos_mm::kernel_meta::install_meta_slots(&ctx.bsp_token());
    klog_info!("OSTD: meta_slots installed ({} entries)", n_slots);
}

fn boot_step_register_frame_alloc_fn(ctx: &mut BootCtx<'_, BspInit>) {
    // Must follow the buddy allocator and the meta_slots install; after it the
    // typestate `Frame::<KernelMeta>::alloc` path is live.
    slopos_ostd::mm::frame_alloc::register_frame_allocator(
        &ctx.bsp_token(),
        slopos_mm::page_alloc::frame_alloc_handle(),
    );
    klog_info!("OSTD: frame_allocator registered (BuddyAllocator)");
}

fn boot_step_install_kernel_vm_space_fn(ctx: &mut BootCtx<'_, BspInit>) {
    // Must follow meta_slots and frame_alloc — `wrap_kernel_master`'s only
    // prerequisites — and precede the first kernel-half mapping, because every
    // kernel-half page-table write goes through this VmSpace's cursor.
    //
    // CR3 has not been swapped since early init, so it still names the live
    // kernel master PML4 and is safe to wrap.
    use slopos_kernel_services::kernel_vm_space::KERNEL_VM_SPACE;
    use slopos_ostd::mm::vm_space::{VmSpace, prepopulate_kernel_half};
    use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

    let bsp = ctx.bsp_token();
    let cr3 = slopos_arch::cpu::control_regs::read_cr3();
    let pml4_phys = slopos_abi::addr::PhysAddr::new(cr3 & 0x000F_FFFF_FFFF_F000);
    assert!(
        !KERNEL_VM_SPACE.is_completed(),
        "boot_step_install_kernel_vm_space_fn called twice"
    );
    KERNEL_VM_SPACE.call_once(|| {
        let space = VmSpace::wrap_kernel_master(&bsp, pml4_phys).expect(
            "boot_step_install_kernel_vm_space_fn: wrap_existing failed (pml4 slot already TYPED?)",
        );
        // Registry level: the kernel master PML4 is shared across every address
        // space, so its lock sits between the resource and allocator levels.
        SpinLock::new(space, lock_class!("KERNEL_VM_SPACE", LOCK_LEVEL_REGISTRY))
    });

    // Two roots name the kernel half — the one `slopos_mm::paging` walks and the
    // one the cursor writes. A CR3 read that kept its PCID or PWT/PCD bits would
    // make them differ, and both are dereferenced as a table base.
    assert_eq!(
        slopos_mm::paging::kernel_pml4_phys(),
        pml4_phys,
        "kernel master PML4 disagreement: mm walker root vs. CR3",
    );

    // Link all 256 kernel-half PDPTs before any address space is built
    // from this master, so a fresh `VmSpace`'s one-shot copy of the top
    // level can never go stale.
    let linked = prepopulate_kernel_half(&bsp)
        .expect("boot_step_install_kernel_vm_space_fn: kernel-half prepopulation failed");

    klog_info!(
        "OSTD: KERNEL_VM_SPACE installed (pml4_phys=0x{:x}, pcid=0, {} kernel-half PDPTs linked)",
        pml4_phys.as_u64(),
        linked
    );
}

fn boot_step_mark_kernel_global_fn(ctx: &mut BootCtx<'_, BspInit>) {
    // Must run after every kernel-half mapping: this stamps GLOBAL onto leaves
    // that already exist, so anything mapped afterwards would miss it.
    use slopos_kernel_services::kernel_vm_space::kernel_vm_space;

    let bsp = ctx.bsp_token();

    // CR4.PGE is enabled earlier in this phase, so the bit is already
    // meaningful on the leaves being stamped.
    slopos_mm::paging::paging_mark_kernel_global();

    // Intel SDM §4.10.2.4: entries cached before the GLOBAL bit was set stay
    // non-global, so a CR3 reload is what drops them.
    kernel_vm_space().lock().activate_kernel_master_bsp(&bsp);

    klog_debug!("OSTD: kernel-half leaves stamped GLOBAL");
}

fn boot_step_register_luf_hook_fn(ctx: &mut BootCtx<'_, BspInit>) {
    // Must precede any per-process VmSpace cursor mutation, so that every
    // `CursorMut::unmap` over a USER-flagged leaf and every `VmSpace::activate`
    // routes into slopos-mm's LUF.
    slopos_ostd::mm::vm_space::register_cursor_unmap_hook(
        &ctx.bsp_token(),
        &slopos_mm::mmu::luf_hook::LUF_HOOK_REF,
    );
    klog_info!("OSTD: cursor_unmap_hook registered (LufHook)");
}

/// Registration order is policy: the quarantine goes first because its frames
/// are already free, so a small shortfall is met without dropping a cache that
/// costs disk I/O to refill.
fn boot_step_register_reclaimers_fn(ctx: &mut BootCtx<'_, BspInit>) {
    let token = ctx.bsp_token();
    slopos_mm::page_alloc::register_reclaim(&token);
    slopos_fs::ext2_vfs::register_reclaim(&token);
    klog_info!(
        "OSTD: reclaim tier armed ({} pages currently reclaimable)",
        slopos_ostd::mm::reclaim::reclaimable_pages()
    );
}

fn boot_step_memory_pre_typestate(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
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

fn boot_step_memory_post_typestate(ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    let rc = init_memory_system_post_typestate(&ctx.bsp_token());
    if rc != 0 {
        klog_info!("ERROR: Memory system post-typestate init failed");
        return -1;
    }
    klog_debug!("Memory management initialized.");
    0
}

fn boot_step_memory_verify(_ctx: &mut BootCtx<'_, BspInit>) {
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
    BOOT_STEP_INSTALL_KERNEL_VM_SPACE,
    memory,
    b"ostd kernel_vm_space\0",
    boot_step_install_kernel_vm_space_fn,
    flags = boot_init_priority(7)
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
    BOOT_STEP_MARK_KERNEL_GLOBAL,
    memory,
    b"kernel-half GLOBAL stamp\0",
    boot_step_mark_kernel_global_fn,
    flags = boot_init_priority(55)
);
crate::boot_init!(
    BOOT_STEP_REGISTER_LUF_HOOK,
    memory,
    b"ostd cursor_unmap_hook\0",
    boot_step_register_luf_hook_fn,
    flags = boot_init_priority(56)
);
crate::boot_init!(
    BOOT_STEP_REGISTER_RECLAIMERS,
    memory,
    b"ostd reclaim tier\0",
    boot_step_register_reclaimers_fn,
    flags = boot_init_priority(57)
);
