use slopos_lib::cpu;
use slopos_mm::mm_constants::{KERNEL_VIRTUAL_BASE, PAGE_SIZE_1GB, PAGE_SIZE_4KB};

const MSR_EFER: u32 = 0xC000_0080;

pub fn verify_cpu_state() {
    let cr0 = cpu::read_cr0();
    let cr4 = cpu::read_cr4();
    let efer = cpu::read_msr(MSR_EFER);

    if (cr0 & cpu::CR0_PG) == 0 {
        panic!("Paging not enabled in CR0");
    }
    if (cr0 & cpu::CR0_PE) == 0 {
        panic!("Protected mode not enabled in CR0");
    }
    if (cr4 & cpu::CR4_PAE) == 0 {
        panic!("PAE not enabled in CR4");
    }
    if (efer & (1 << 8)) == 0 {
        panic!("Long mode not enabled in EFER");
    }
    if (efer & (1 << 10)) == 0 {
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

    unsafe extern "C" {
        static _start: u8;
    }
    let _ = unsafe { core::ptr::read_volatile(&_start) };
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
    let (_, _, _, edx1) = cpu::cpuid(1);
    if (edx1 & (1 << 6)) == 0 {
        panic!("CPU does not support PAE");
    }
    if (edx1 & (1 << 13)) == 0 {
        panic!("CPU does not support PGE");
    }

    let (_, _, _, edx2) = cpu::cpuid(0x8000_0001);
    if (edx2 & (1 << 29)) == 0 {
        panic!("CPU does not support long mode");
    }
}

pub fn complete_system_verification() {
    verify_cpu_state();
    verify_memory_layout();
    check_stack_health();
    verify_cpu_features();
}
