use crate::kernel_panic::kernel_panic;

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

#[no_mangle]
pub extern "C" fn verify_cpu_state() {
    let cr0 = read_cr0();
    let cr4 = read_cr4();
    let efer = read_efer();

    if (cr0 & (1 << 31)) == 0 {
        kernel_panic("Paging not enabled in CR0");
    }
    if (cr0 & 1) == 0 {
        kernel_panic("Protected mode not enabled in CR0");
    }
    if (cr4 & (1 << 5)) == 0 {
        kernel_panic("PAE not enabled in CR4");
    }
    if (efer & (1 << 8)) == 0 {
        kernel_panic("Long mode not enabled in EFER");
    }
    if (efer & (1 << 10)) == 0 {
        kernel_panic("Long mode not active in EFER");
    }
}

#[no_mangle]
pub extern "C" fn verify_memory_layout() {
    let addr = verify_memory_layout as *const () as u64;
    if addr < 0xFFFFFFFF80000000 {
        kernel_panic("Kernel not running in higher-half virtual memory");
    }
    if addr < 0xFFFF800000000000 {
        kernel_panic("Kernel running in user space address range");
    }

    extern "C" {
        static _start: u8;
    }
    let _ = unsafe { core::ptr::read_volatile(&_start) };
}

#[no_mangle]
pub extern "C" fn check_stack_health() {
    let rsp = get_stack_pointer();
    if rsp == 0 {
        kernel_panic("Stack pointer is null");
    }
    if (rsp & 0xF) != 0 {
        kernel_panic("Stack pointer not properly aligned");
    }
    if rsp < 0x1000 {
        kernel_panic("Stack pointer too low (possible corruption)");
    }
    if rsp >= 0x4000_0000 && rsp < 0xFFFF_8000_0000_0000 {
        kernel_panic("Stack pointer in invalid memory region");
    }
}

#[no_mangle]
pub extern "C" fn verify_cpu_features() {
    let (eax1, _ebx1, _ecx1, edx1) = slopos_lib::cpu::cpuid(1);
    let _ = eax1;
    if (edx1 & (1 << 6)) == 0 {
        kernel_panic("CPU does not support PAE");
    }
    if (edx1 & (1 << 13)) == 0 {
        kernel_panic("CPU does not support PGE");
    }

    let (_eax2, _ebx2, _ecx2, edx2) = slopos_lib::cpu::cpuid(0x8000_0001);
    if (edx2 & (1 << 29)) == 0 {
        kernel_panic("CPU does not support long mode");
    }
}

#[no_mangle]
pub extern "C" fn complete_system_verification() {
    verify_cpu_state();
    verify_memory_layout();
    check_stack_health();
    verify_cpu_features();
}
