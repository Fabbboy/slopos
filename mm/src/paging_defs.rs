//! Page table flags and paging constants.

use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct PageFlags: u64 {
        const PRESENT       = 1 << 0;
        const WRITABLE      = 1 << 1;
        /// Accessible from ring 3.
        const USER          = 1 << 2;
        const WRITE_THROUGH = 1 << 3;
        const CACHE_DISABLE = 1 << 4;
        /// Set by hardware on access.
        const ACCESSED      = 1 << 5;
        /// Set by hardware on write.
        const DIRTY         = 1 << 6;
        /// 2MB in a PDE, 1GB in a PDPTE.
        const HUGE          = 1 << 7;
        /// Not flushed on a CR3 change.
        const GLOBAL        = 1 << 8;
        /// Requires the NX bit enabled in the EFER MSR.
        const NO_EXECUTE    = 1 << 63;

        /// Copy-on-Write marker; with `!WRITABLE` a write fault triggers COW
        /// resolution.
        const COW           = 1 << 9;

        const KERNEL_RW = Self::PRESENT.bits() | Self::WRITABLE.bits();
        const KERNEL_RO = Self::PRESENT.bits();
        const USER_RW = Self::PRESENT.bits() | Self::WRITABLE.bits() | Self::USER.bits();
        const USER_RO = Self::PRESENT.bits() | Self::USER.bits();
        const LARGE_KERNEL_RW = Self::PRESENT.bits() | Self::WRITABLE.bits() | Self::HUGE.bits();
        const MMIO = Self::PRESENT.bits() | Self::WRITABLE.bits() | Self::CACHE_DISABLE.bits() | Self::NO_EXECUTE.bits();
    }
}

impl PageFlags {
    /// Bits 12-51 hold the 4KB-aligned physical address.
    pub const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    #[inline]
    pub const fn extract_address(pte: u64) -> u64 {
        pte & Self::ADDRESS_MASK
    }
}

pub const PAGE_SIZE_4KB: u64 = 0x1000;

pub const PAGE_SIZE_4KB_USIZE: usize = PAGE_SIZE_4KB as usize;

pub const PAGE_SIZE_2MB: u64 = 0x20_0000;

pub const PAGE_SIZE_1GB: u64 = 0x4000_0000;

// The kernel-half mapping helpers convert `PageFlags` into
// `slopos_ostd::mm::page_table::PteFlags` on every call, so a drifted bit
// silently changes a mapping's permissions.
const _: () = {
    use slopos_ostd::mm::page_table::PteFlags;
    assert!(PageFlags::PRESENT.bits() == PteFlags::PRESENT.bits());
    assert!(PageFlags::WRITABLE.bits() == PteFlags::WRITABLE.bits());
    assert!(PageFlags::USER.bits() == PteFlags::USER.bits());
    assert!(PageFlags::WRITE_THROUGH.bits() == PteFlags::WRITE_THROUGH.bits());
    assert!(PageFlags::CACHE_DISABLE.bits() == PteFlags::CACHE_DISABLE.bits());
    assert!(PageFlags::ACCESSED.bits() == PteFlags::ACCESSED.bits());
    assert!(PageFlags::DIRTY.bits() == PteFlags::DIRTY.bits());
    assert!(PageFlags::HUGE.bits() == PteFlags::HUGE.bits());
    assert!(PageFlags::GLOBAL.bits() == PteFlags::GLOBAL.bits());
    assert!(PageFlags::NO_EXECUTE.bits() == PteFlags::NO_EXECUTE.bits());
    assert!(PageFlags::ADDRESS_MASK == PteFlags::ADDRESS_MASK);
    assert!(PageFlags::COW.bits() == 1 << 9);
    assert!(PageFlags::COW.bits() & PteFlags::SOFTWARE_BITS_MASK == PageFlags::COW.bits());
};

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
