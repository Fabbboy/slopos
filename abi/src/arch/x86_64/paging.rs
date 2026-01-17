//! Page table flags and paging constants.
//!
//! This module provides type-safe bitflags for x86_64 page table entries
//! and related paging constants like page sizes.

use bitflags::bitflags;

bitflags! {
    /// x86_64 page table entry flags.
    ///
    /// These flags control page permissions, caching behavior, and
    /// hardware-maintained access/dirty bits. Combine with `|` operator.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use slopos_abi::arch::x86_64::PageFlags;
    ///
    /// let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
    /// let pte = phys_addr | flags.bits();
    ///
    /// if flags.contains(PageFlags::USER) {
    ///     // User-accessible page
    /// }
    /// ```
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct PageFlags: u64 {
        /// Page is present in memory (bit 0).
        const PRESENT       = 1 << 0;
        /// Page is writable (otherwise read-only) (bit 1).
        const WRITABLE      = 1 << 1;
        /// Page is accessible from user mode (ring 3) (bit 2).
        const USER          = 1 << 2;
        /// Write-through caching (vs write-back) (bit 3).
        const WRITE_THROUGH = 1 << 3;
        /// Disable caching for this page (bit 4).
        const CACHE_DISABLE = 1 << 4;
        /// Set by hardware when page is accessed (bit 5).
        const ACCESSED      = 1 << 5;
        /// Set by hardware when page is written (bit 6).
        const DIRTY         = 1 << 6;
        /// Page is 2MB (PDE) or 1GB (PDPTE) huge page (bit 7).
        const HUGE          = 1 << 7;
        /// Page is global (not flushed on CR3 change) (bit 8).
        const GLOBAL        = 1 << 8;
        /// Disable instruction fetch from this page (bit 63).
        /// Requires NX bit enabled in EFER MSR.
        const NO_EXECUTE    = 1 << 63;

        // =====================================================================
        // Software-defined flags (bits 9-11 are available for OS use)
        // =====================================================================

        /// Copy-on-Write marker (bit 9).
        /// When set with !WRITABLE, a write fault triggers COW resolution.
        const COW           = 1 << 9;

        // =====================================================================
        // Convenience Combinations
        // =====================================================================

        /// Kernel read-write page (PRESENT | WRITABLE).
        const KERNEL_RW = Self::PRESENT.bits() | Self::WRITABLE.bits();
        /// Kernel read-only page (PRESENT only).
        const KERNEL_RO = Self::PRESENT.bits();
        /// User read-write page (PRESENT | WRITABLE | USER).
        const USER_RW = Self::PRESENT.bits() | Self::WRITABLE.bits() | Self::USER.bits();
        /// User read-only page (PRESENT | USER).
        const USER_RO = Self::PRESENT.bits() | Self::USER.bits();
        /// Large kernel page (PRESENT | WRITABLE | HUGE).
        const LARGE_KERNEL_RW = Self::PRESENT.bits() | Self::WRITABLE.bits() | Self::HUGE.bits();
        const MMIO = Self::PRESENT.bits() | Self::WRITABLE.bits() | Self::CACHE_DISABLE.bits() | Self::NO_EXECUTE.bits();
    }
}

impl PageFlags {
    /// Address mask for extracting physical frame address from PTE.
    /// Bits 12-51 contain the 4KB-aligned physical address.
    pub const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    /// Extract physical address from a page table entry.
    #[inline]
    pub const fn extract_address(pte: u64) -> u64 {
        pte & Self::ADDRESS_MASK
    }
}

// =============================================================================
// Page Sizes
// =============================================================================

/// 4KB page size (standard).
pub const PAGE_SIZE_4KB: u64 = 0x1000;

/// 2MB page size (huge page via PDE).
pub const PAGE_SIZE_2MB: u64 = 0x20_0000;

/// 1GB page size (huge page via PDPTE).
pub const PAGE_SIZE_1GB: u64 = 0x4000_0000;

/// Page alignment (same as PAGE_SIZE_4KB).
pub const PAGE_ALIGN: u64 = PAGE_SIZE_4KB;

/// Stack alignment requirement.
pub const STACK_ALIGN: u64 = 16;

/// Number of entries per page table (512 for 4KB pages).
pub const ENTRIES_PER_PAGE_TABLE: usize = 512;

/// EFI page size (same as 4KB).
pub const EFI_PAGE_SIZE: u64 = PAGE_SIZE_4KB;

/// EFI conventional memory type.
pub const EFI_CONVENTIONAL_MEMORY: u64 = 7;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_flags_combinations() {
        let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
        assert!(flags.contains(PageFlags::PRESENT));
        assert!(flags.contains(PageFlags::WRITABLE));
        assert!(flags.contains(PageFlags::USER));
        assert!(!flags.contains(PageFlags::HUGE));
    }

    #[test]
    fn page_flags_bits() {
        assert_eq!(PageFlags::PRESENT.bits(), 0x001);
        assert_eq!(PageFlags::WRITABLE.bits(), 0x002);
        assert_eq!(PageFlags::USER.bits(), 0x004);
        assert_eq!(PageFlags::KERNEL_RW.bits(), 0x003);
        assert_eq!(PageFlags::USER_RW.bits(), 0x007);
    }

    #[test]
    fn address_extraction() {
        let pte = 0x0000_1234_5678_9003u64; // Address with flags
        let addr = PageFlags::extract_address(pte);
        assert_eq!(addr, 0x0000_1234_5678_9000);
    }
}
