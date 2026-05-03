//! Stack and frame pointer register reads.

use core::arch::asm;

/// Read the current RBP (frame pointer).
#[inline(always)]
pub fn read_rbp() -> u64 {
    let rbp: u64;
    unsafe {
        asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags));
    }
    rbp
}

/// Read the current RSP (stack pointer).
#[inline(always)]
pub fn read_rsp() -> u64 {
    let rsp: u64;
    unsafe {
        asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    rsp
}

/// Read the current R15 register.
#[inline(always)]
pub fn read_r15() -> u64 {
    let r15: u64;
    unsafe {
        asm!("mov {}, r15", out(reg) r15, options(nomem, nostack, preserves_flags));
    }
    r15
}
