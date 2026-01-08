//! Model-Specific Register (MSR) addresses.
//!
//! MSRs are accessed via RDMSR/WRMSR instructions using a 32-bit address.
//! This module provides a type-safe `Msr` newtype that prevents accidentally
//! using an MSR address where a port number or other value is expected.

/// Model-Specific Register address.
///
/// MSRs are accessed via RDMSR/WRMSR instructions using a 32-bit address.
/// This newtype prevents accidentally using an MSR address where a port
/// number or other value is expected.
///
/// # Example
///
/// ```ignore
/// use slopos_abi::arch::x86_64::Msr;
///
/// let apic_base = read_msr(Msr::APIC_BASE);
/// // read_msr(0x1B);  // Compile error: expected Msr, found integer
/// ```
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
