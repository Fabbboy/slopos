//! CR3 write helper + [`Pcid`] newtype.
//!
//! A thin `Pcid(u16)` (only 12 bits architecturally) and a
//! `write_cr3_pcid` that writes the (PCID-encoded) physical address
//! of the PML4 plus the no-flush bit.
//!
//! # Soundness
//!
//! The single `unsafe` block wraps a `mov %rax, %cr3`. The PML4 phys
//! + PCID encoding are typed via [`PhysAddr`] / [`Pcid`], so callers
//! cannot smuggle arbitrary u64s into CR3.

use slopos_abi::addr::PhysAddr;

/// Process-context identifier. 12 bits architecturally; we accept a
/// `u16` and mask on construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pcid(u16);

impl Pcid {
    /// PCID 0 — kernel-only address space (the kernel page-dir).
    pub const KERNEL: Pcid = Pcid(0);

    /// Construct a `Pcid`. Mask to 12 bits.
    #[inline]
    pub const fn new(raw: u16) -> Self {
        Pcid(raw & 0x0FFF)
    }

    /// Raw 12-bit value.
    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }
}

impl Default for Pcid {
    fn default() -> Self {
        Self::KERNEL
    }
}

/// Write CR3 to `(pml4_phys, pcid)`. When `no_flush == true`,
/// architecturally bit 63 is set and the CPU skips flushing the
/// previous PCID's TLB entries. Set to `false` if you need a flush.
///
/// # Safety
///
/// - `pml4_phys` must point to a 4 KiB-aligned, well-formed PML4
///   that is reachable from the kernel HHDM mapping.
/// - The kernel-half mappings (indices 256..512) of the PML4 must
///   match the canonical kernel master, otherwise the running CPU
///   loses access to its own code segment immediately after the
///   write.
/// - PCID semantics: if `pcid == Pcid::KERNEL` and another PCID is
///   currently loaded, choose `no_flush == false` or guarantee the
///   kernel-mapping invariant by other means.
pub unsafe fn write_cr3_pcid(pml4_phys: PhysAddr, pcid: Pcid, no_flush: bool) {
    let mut value = pml4_phys.as_u64() & !0xFFF_u64;
    value |= pcid.raw() as u64;
    if no_flush {
        value |= 1u64 << 63;
    }
    #[cfg(target_os = "none")]
    {
        // SAFETY: caller's contract above. The value is built entirely
        // from typed inputs (`PhysAddr`, `Pcid`), so no arbitrary u64
        // can sneak in. The single assembly statement clobbers no
        // general-purpose register.
        unsafe {
            core::arch::asm!(
                "mov {value}, %cr3",
                value = in(reg) value,
                options(nostack, preserves_flags, att_syntax),
            );
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        // Host / cargo-miri-test: no real CR3 to write to. Stash the
        // would-be value in an atomic so any caller that immediately
        // re-reads observes the write.
        use core::sync::atomic::{AtomicU64, Ordering};
        static MOCK_CR3: AtomicU64 = AtomicU64::new(0);
        MOCK_CR3.store(value, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcid_masks_to_12_bits() {
        assert_eq!(Pcid::new(0xFFFF).raw(), 0x0FFF);
        assert_eq!(Pcid::new(0x1234).raw(), 0x0234);
        assert_eq!(Pcid::KERNEL.raw(), 0);
    }
}
