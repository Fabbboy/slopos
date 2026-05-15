//! Model-Specific Register (MSR) addresses and read/write instructions.
//!
//! This module provides the type-safe `Msr` newtype and RDMSR/WRMSR
//! instruction wrappers.
//!
//! Host build behaviour (`cfg(not(target_os = "none"))`, including
//! `cargo miri test`): MSR reads / writes go to a tiny in-process
//! key/value store (`MOCK_MSRS`). Sufficient for any caller that
//! only reads back what it wrote (EFER toggle + SYSCALL MSRs).

#[allow(unused_imports)]
use core::arch::asm;

// =============================================================================
// MSR Address Type
// =============================================================================

/// Model-Specific Register address.
///
/// MSRs are accessed via RDMSR/WRMSR instructions using a 32-bit address.
/// This newtype prevents accidentally using an MSR address where a port
/// number or other value is expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Msr(pub u32);

impl Msr {
    // =========================================================================
    // IA32 MSRs (0x00 - 0x1FF)
    // =========================================================================

    /// APIC Base MSR - contains physical base address and enable flags.
    pub const APIC_BASE: Self = Self(0x1B);

    /// Memory Type Range Register capabilities.
    pub const MTRR_CAP: Self = Self(0xFE);

    /// SYSENTER CS selector.
    pub const SYSENTER_CS: Self = Self(0x174);

    /// SYSENTER ESP (stack pointer).
    pub const SYSENTER_ESP: Self = Self(0x175);

    /// SYSENTER EIP (instruction pointer).
    pub const SYSENTER_EIP: Self = Self(0x176);

    /// Page Attribute Table.
    pub const PAT: Self = Self(0x277);

    // =========================================================================
    // AMD64/Intel 64 MSRs (0xC000_0000+)
    // =========================================================================

    /// Extended Feature Enable Register.
    pub const EFER: Self = Self(0xC000_0080);

    /// SYSCALL target CS/SS and return CS/SS.
    pub const STAR: Self = Self(0xC000_0081);

    /// SYSCALL target RIP (64-bit mode).
    pub const LSTAR: Self = Self(0xC000_0082);

    /// SYSCALL target RIP (compatibility mode).
    pub const CSTAR: Self = Self(0xC000_0083);

    /// SYSCALL RFLAGS mask.
    pub const SFMASK: Self = Self(0xC000_0084);

    /// FS segment base address.
    pub const FS_BASE: Self = Self(0xC000_0100);

    /// GS segment base address.
    pub const GS_BASE: Self = Self(0xC000_0101);

    /// Kernel GS base (swapped on SWAPGS).
    pub const KERNEL_GS_BASE: Self = Self(0xC000_0102);

    // =========================================================================
    // Methods
    // =========================================================================

    /// Returns the raw MSR address for use with RDMSR/WRMSR.
    #[inline]
    pub const fn address(self) -> u32 {
        self.0
    }

    /// Creates a new MSR from a raw address.
    ///
    /// Use this for MSRs not defined as constants.
    #[inline]
    pub const fn new(address: u32) -> Self {
        Self(address)
    }
}

// =============================================================================
// EFER (Extended Feature Enable Register) Bit Definitions
// =============================================================================

/// System Call Extensions — enables SYSCALL/SYSRET instructions.
pub const EFER_SCE: u64 = 1 << 0;

/// Long Mode Enable — activates IA-32e paging when set with CR0.PG.
pub const EFER_LME: u64 = 1 << 8;

/// Long Mode Active — read-only; set by hardware when long mode is active.
pub const EFER_LMA: u64 = 1 << 10;

/// No-Execute Enable — enables the NX (execute-disable) page protection bit.
pub const EFER_NXE: u64 = 1 << 11;

// =============================================================================
// RDMSR / WRMSR Instructions
// =============================================================================

/// Read a 64-bit value from the specified MSR.
#[inline(always)]
pub fn read_msr(msr: Msr) -> u64 {
    #[cfg(target_os = "none")]
    {
        let low: u32;
        let high: u32;
        unsafe {
            asm!(
                "rdmsr",
                out("eax") low,
                out("edx") high,
                in("ecx") msr.address(),
                options(nomem, nostack, preserves_flags)
            );
        }
        ((high as u64) << 32) | (low as u64)
    }
    #[cfg(not(target_os = "none"))]
    {
        host_mock::read(msr.address())
    }
}

/// Write a 64-bit value to the specified MSR.
#[inline(always)]
pub fn write_msr(msr: Msr, value: u64) {
    #[cfg(target_os = "none")]
    {
        let low = value as u32;
        let high = (value >> 32) as u32;
        unsafe {
            asm!(
                "wrmsr",
                in("eax") low,
                in("edx") high,
                in("ecx") msr.address(),
                options(nomem, nostack, preserves_flags)
            );
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        host_mock::write(msr.address(), value);
    }
}

#[cfg(not(target_os = "none"))]
mod host_mock {
    //! Tiny fixed-size MSR mock backing store for host tests / Miri.
    //!
    //! Bounded slot table indexed by address mod CAP. Stores
    //! `(address, value)` so unrelated MSRs that collide on the same
    //! slot do not silently corrupt each other's stored value.
    use core::sync::atomic::{AtomicU64, Ordering};

    const CAP: usize = 32;
    // Use NONE = u64::MAX as the empty sentinel for `address` (no real
    // MSR address comes near 2^32, so the high 32 bits being all-1 is
    // safe as a vacant marker).
    const VACANT: u64 = u64::MAX;
    static SLOTS_ADDR: [AtomicU64; CAP] = [const { AtomicU64::new(VACANT) }; CAP];
    static SLOTS_VALUE: [AtomicU64; CAP] = [const { AtomicU64::new(0) }; CAP];

    fn slot_index(address: u32) -> usize {
        (address as usize) % CAP
    }

    pub(super) fn write(address: u32, value: u64) {
        let i = slot_index(address);
        SLOTS_ADDR[i].store(address as u64, Ordering::Relaxed);
        SLOTS_VALUE[i].store(value, Ordering::Relaxed);
    }

    pub(super) fn read(address: u32) -> u64 {
        let i = slot_index(address);
        if SLOTS_ADDR[i].load(Ordering::Relaxed) == address as u64 {
            SLOTS_VALUE[i].load(Ordering::Relaxed)
        } else {
            0
        }
    }
}

// =============================================================================
// SYSCALL fast-path MSR install
// =============================================================================

use crate::arch::x86_64::gdt::SegmentSelector;

/// Build the STAR MSR value from kernel-code and user-data segment
/// selectors, following the AMD64 SYSRET convention.
///
/// Bits 47:32 hold `kernel_code` (used by SYSCALL to load CS).
/// Bits 63:48 hold `user_data - 8` (used by SYSRET to derive USER_CODE
/// at `user_data + 8` and USER_DATA at `user_data + 0`).
///
/// Lower 32 bits are reserved (zero in long mode).
pub const fn star_from_selectors(kernel_code: SegmentSelector, user_data: SegmentSelector) -> u64 {
    ((user_data.bits() as u64 - 8) << 48) | ((kernel_code.bits() as u64) << 32)
}

/// Program the SYSCALL fast-path MSRs.
///
/// Sets `EFER.SCE` (System Call Extensions enable) if not already set,
/// then writes `STAR`, `LSTAR`, and `SFMASK`. After this returns the
/// CPU honours the SYSCALL instruction with `lstar` as the target RIP
/// and `sfmask` as the RFLAGS clear-mask.
///
/// The `&W: CpuInitWitness` parameter authorises mutation of the
/// current CPU's MSRs — both BSP-init and per-AP-bringup paths call
/// this. Inv. 2: `lstar` must reference a properly-aligned kernel-mode
/// trampoline that swaps GS on SYSCALL entry and saves the user
/// context; `STAR` selectors must match the active GDT layout. The
/// MSR writes themselves stay inside a safe surface — only callers
/// staging the trampoline contract carry the soundness burden.
pub fn install_syscall_msrs<W: crate::sync::CpuInitWitness>(
    _witness: &W,
    star: u64,
    lstar: u64,
    sfmask: u64,
) {
    let efer = read_msr(Msr::EFER);
    if (efer & EFER_SCE) == 0 {
        write_msr(Msr::EFER, efer | EFER_SCE);
    }
    write_msr(Msr::STAR, star);
    write_msr(Msr::LSTAR, lstar);
    write_msr(Msr::SFMASK, sfmask);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_layout_matches_amd64_convention() {
        let star = star_from_selectors(SegmentSelector::KERNEL_CODE, SegmentSelector::USER_DATA);
        // KERNEL_CODE = 0x08 in bits 47:32
        assert_eq!((star >> 32) & 0xFFFF, 0x08);
        // USER_DATA - 8 = 0x13 in bits 63:48
        assert_eq!((star >> 48) & 0xFFFF, 0x13);
        // Lower 32 bits unused
        assert_eq!(star & 0xFFFF_FFFF, 0);
    }
}
