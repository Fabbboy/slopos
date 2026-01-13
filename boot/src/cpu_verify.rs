use slopos_mm::mm_constants::{KERNEL_VIRTUAL_BASE, PAGE_SIZE_1GB, PAGE_SIZE_4KB};

#[inline(always)]
fn read_cr0() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

#[inline(always)]
fn read_cr4() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

#[inline(always)]
fn read_efer() -> u64 {
    slopos_lib::cpu::read_msr(0xC000_0080)
}

#[inline(always)]
fn get_stack_pointer() -> u64 {
    let rsp: u64;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    rsp
}

pub fn verify_cpu_state() {
    let cr0 = read_cr0();
    let cr4 = read_cr4();
    let efer = read_efer();

    if (cr0 & (1 << 31)) == 0 {
        panic!("Paging not enabled in CR0");
    }
    if (cr0 & 1) == 0 {
        panic!("Protected mode not enabled in CR0");
    }
    if (cr4 & (1 << 5)) == 0 {
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
    let rsp = get_stack_pointer();
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
    let (_eax1, _ebx1, _ecx1, edx1) = slopos_lib::cpu::cpuid(1);
    if (edx1 & (1 << 6)) == 0 {
        panic!("CPU does not support PAE");
    }
    if (edx1 & (1 << 13)) == 0 {
        panic!("CPU does not support PGE");
    }

    let (_eax2, _ebx2, _ecx2, edx2) = slopos_lib::cpu::cpuid(0x8000_0001);
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
