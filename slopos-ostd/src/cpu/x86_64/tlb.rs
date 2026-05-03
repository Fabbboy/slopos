//! TLB and cache management instructions.

use super::control_regs::{read_cr3, write_cr3};
use core::arch::asm;

/// Flush the entire TLB by reloading CR3.
#[inline(always)]
pub fn flush_tlb_all() {
    let cr3 = read_cr3();
    write_cr3(cr3);
}

/// Invalidate TLB entry for a single virtual address.
#[inline(always)]
pub fn invlpg(vaddr: u64) {
    unsafe {
        asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags));
    }
}

/// Write-back and invalidate all cache lines.
#[inline(always)]
pub fn wbinvd() {
    unsafe {
        asm!("wbinvd", options(nostack, preserves_flags));
    }
}
