use slopos_arch::cpu;
use slopos_arch::cpu::cpuid::{
    CPUID_EXT_FEAT_EDX_LM, CPUID_FEAT_EDX_PAE, CPUID_FEAT_EDX_PGE, CPUID_LEAF_EXT_INFO,
    CPUID_LEAF_FEATURES,
};
use slopos_arch::cpu::msr::{EFER_LMA, EFER_LME, Msr};
use slopos_mm::memory_layout_defs::KERNEL_VIRTUAL_BASE;
use slopos_mm::paging_defs::{PAGE_SIZE_1GB, PAGE_SIZE_4KB};

pub fn verify_cpu_state() {
    let cr0 = cpu::read_cr0();
    let cr4 = cpu::read_cr4();
    let efer = cpu::read_msr(Msr::EFER);

    if (cr0 & cpu::CR0_PG) == 0 {
        panic!("Paging not enabled in CR0");
    }
    if (cr0 & cpu::CR0_PE) == 0 {
        panic!("Protected mode not enabled in CR0");
    }
    if (cr4 & cpu::CR4_PAE) == 0 {
        panic!("PAE not enabled in CR4");
    }
    if (efer & EFER_LME) == 0 {
        panic!("Long mode not enabled in EFER");
    }
    if (efer & EFER_LMA) == 0 {
        panic!("Long mode not active in EFER");
    }
}

pub fn verify_memory_layout() {
    let addr = verify_memory_layout as *const () as u64;
    if addr < KERNEL_VIRTUAL_BASE {
        panic!("Kernel not running in higher-half virtual memory");
    }
    if let Some(hhdm_base) = slopos_mm::hhdm::try_offset() {
        if addr < hhdm_base {
            panic!("Kernel running in user space address range");
        }
    }

    // Sanity-check the kernel-image mapping via OSTD's safe probe
    // surface — the unsafe `read_volatile` of the linker `_start`
    // byte lives inside OSTD's `verify_kernel_entry_alive`.
    slopos_ostd::arch::x86_64::kernel_ptr::verify_kernel_entry_alive();
}

pub fn check_stack_health() {
    let rsp = cpu::read_rsp();
    if rsp == 0 {
        panic!("Stack pointer is null");
    }
    if (rsp & 0xF) != 0 {
        panic!("Stack pointer not properly aligned");
    }
    if rsp < PAGE_SIZE_4KB {
        panic!("Stack pointer too low (possible corruption)");
    }
    if let Some(hhdm_base) = slopos_mm::hhdm::try_offset() {
        if rsp >= PAGE_SIZE_1GB && rsp < hhdm_base {
            panic!("Stack pointer in invalid memory region");
        }
    }
}

pub fn verify_cpu_features() {
    let (_, _, _, edx1) = cpu::cpuid(CPUID_LEAF_FEATURES);
    if (edx1 & CPUID_FEAT_EDX_PAE) == 0 {
        panic!("CPU does not support PAE");
    }
    if (edx1 & CPUID_FEAT_EDX_PGE) == 0 {
        panic!("CPU does not support PGE");
    }

    let (_, _, _, edx2) = cpu::cpuid(CPUID_LEAF_EXT_INFO);
    if (edx2 & CPUID_EXT_FEAT_EDX_LM) == 0 {
        panic!("CPU does not support long mode");
    }
}

pub fn complete_system_verification() {
    verify_cpu_state();
    verify_memory_layout();
    check_stack_health();
    verify_cpu_features();
}
