//! TLB and cache management instructions.
//!
//! Outside `target_os = "none"` — host and `cargo miri test` builds — they
//! are no-ops.

use super::control_regs::{read_cr3, write_cr3};
#[allow(unused_imports)]
use core::arch::asm;

#[inline(always)]
pub fn flush_tlb_all() {
    let cr3 = read_cr3();
    write_cr3(cr3);
}

#[inline(always)]
pub fn invlpg(vaddr: u64) {
    #[cfg(target_os = "none")]
    unsafe {
        asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = vaddr;
    }
}

#[inline(always)]
pub fn wbinvd() {
    #[cfg(target_os = "none")]
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

/// SDM Vol 2A §3.2 "INVPCID" types:
///   - 0: individual address invalidation within `pcid`
///   - 1: single-context invalidation (non-globals for `pcid`)
///   - 2: all-context invalidation including globals
///   - 3: all-context invalidation excluding globals
///
/// Caller must have already gated on CPUID-leaf availability.
#[inline(always)]
pub fn invpcid(kind: u64, pcid: u16, linear: u64) {
    #[cfg(target_os = "none")]
    {
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
    #[cfg(not(target_os = "none"))]
    {
        let _ = (kind, pcid, linear);
        // Keeps InvpcidDescriptor off the dead_code path on host builds.
        let _ = core::mem::size_of::<InvpcidDescriptor>();
    }
}
