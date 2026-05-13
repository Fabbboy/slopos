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

/// `INVPCID` descriptor per SDM Vol 2A §3.2.
#[repr(C)]
struct InvpcidDescriptor {
    pcid: u64,
    linear: u64,
}

/// Issue `INVPCID` with the given type, PCID, and linear address.
///
/// SDM Vol 2A §3.2 "INVPCID":
///   - type 0: individual address invalidation within `pcid`
///   - type 1: single-context invalidation (non-globals for `pcid`)
///   - type 2: all-context invalidation including globals
///   - type 3: all-context invalidation excluding globals
///
/// Caller must have already gated on CPUID-leaf availability (e.g.
/// the `invpcid_available()` check in `mm/src/mmu/asid.rs`).
#[inline(always)]
pub fn invpcid(kind: u64, pcid: u16, linear: u64) {
    let desc = InvpcidDescriptor {
        pcid: pcid as u64,
        linear,
    };
    unsafe {
        asm!(
            "invpcid {kind}, [{desc}]",
            kind = in(reg) kind,
            desc = in(reg) &desc,
            options(nostack, preserves_flags, readonly),
        );
    }
}
