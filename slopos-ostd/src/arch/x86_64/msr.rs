//! Model-Specific Register (MSR) addresses and read/write instructions.
//!
//! This module provides the type-safe `Msr` newtype and RDMSR/WRMSR
//! instruction wrappers.

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

/// Write a 64-bit value to the specified MSR.
#[inline(always)]
pub fn write_msr(msr: Msr, value: u64) {
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
