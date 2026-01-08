//! Global Descriptor Table (GDT) definitions.
//!
//! This module provides type-safe segment selectors and GDT-related constants.
//! Segment selectors encode the descriptor table index, table indicator (GDT/LDT),
//! and requested privilege level (RPL).

/// x86_64 segment selector.
///
/// Layout (16 bits):
/// - Bits 0-1: Requested Privilege Level (RPL)
/// - Bit 2: Table Indicator (0 = GDT, 1 = LDT)
/// - Bits 3-15: Descriptor index
///
/// # Example
///
/// ```ignore
/// use slopos_abi::arch::x86_64::SegmentSelector;
///
/// // Use predefined selectors
/// let cs = SegmentSelector::KERNEL_CODE;
///
/// // Or create custom selectors
/// let custom = SegmentSelector::new(6, false, 0);
///
/// // Get the raw value for assembly
/// let raw = cs.bits();
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SegmentSelector(pub u16);

impl SegmentSelector {
    // =========================================================================
    // Standard Selectors
    // =========================================================================

    /// Null selector (index 0, GDT, RPL 0).
    pub const NULL: Self = Self(0);

    /// Kernel code segment (GDT index 1, RPL 0) = 0x08.
    pub const KERNEL_CODE: Self = Self::new(1, false, 0);

    /// Kernel data segment (GDT index 2, RPL 0) = 0x10.
    pub const KERNEL_DATA: Self = Self::new(2, false, 0);

    /// User data segment (GDT index 3, RPL 3) = 0x1B.
    ///
    /// Note: Must come before user code in GDT for SYSRET compatibility.
    pub const USER_DATA: Self = Self::new(3, false, 3);

    /// User code segment (GDT index 4, RPL 3) = 0x23.
    pub const USER_CODE: Self = Self::new(4, false, 3);

    /// TSS segment (GDT index 5, RPL 0) = 0x28.
    pub const TSS: Self = Self::new(5, false, 0);

    // =========================================================================
    // Constructor and Accessors
    // =========================================================================

    /// Create a new segment selector.
    ///
    /// # Arguments
    /// * `index` - Descriptor table index (0-8191)
    /// * `ldt` - Use LDT instead of GDT
    /// * `rpl` - Requested privilege level (0-3)
    #[inline]
    pub const fn new(index: u16, ldt: bool, rpl: u8) -> Self {
        let ti = if ldt { 1 << 2 } else { 0 };
        Self((index << 3) | ti | (rpl as u16 & 0x3))
    }

    /// Get the descriptor table index.
    #[inline]
    pub const fn index(self) -> u16 {
        self.0 >> 3
    }

    /// Check if this selector references the LDT.
    #[inline]
    pub const fn is_ldt(self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// Get the requested privilege level (0-3).
    #[inline]
    pub const fn rpl(self) -> u8 {
        (self.0 & 0x3) as u8
    }

    /// Get the raw selector value for loading into segment register.
    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

// =========================================================================
// GDT Descriptor Access Byte Fields
// =========================================================================

/// Present bit in GDT access byte (bit 7).
pub const GDT_ACCESS_PRESENT: u8 = 1 << 7;

/// DPL = 0 (Ring 0 / Kernel) in GDT access byte (bits 5-6).
pub const GDT_ACCESS_DPL_KERNEL: u8 = 0 << 5;

/// DPL = 3 (Ring 3 / User) in GDT access byte (bits 5-6).
pub const GDT_ACCESS_DPL_USER: u8 = 3 << 5;

/// Segment type bit (bit 4) - 1 for code/data segment.
pub const GDT_ACCESS_SEGMENT: u8 = 1 << 4;

/// Code segment type: executable, readable, non-conforming.
pub const GDT_ACCESS_CODE_TYPE: u8 = 0b1010;

/// Data segment type: writable, expand-up.
pub const GDT_ACCESS_DATA_TYPE: u8 = 0b0010;

// =========================================================================
// GDT Flags (bits 52-55 of descriptor)
// =========================================================================

/// Granularity flag (G=1) - limit in 4KB units.
pub const GDT_FLAG_GRANULARITY: u8 = 1 << 3;

/// Long mode flag (L=1) - 64-bit code segment.
pub const GDT_FLAG_LONG_MODE: u8 = 1 << 1;

/// Combined flags for 64-bit segments: G=1, D/B=0, L=1, AVL=0 = 0xA.
pub const GDT_FLAGS_64BIT: u8 = GDT_FLAG_GRANULARITY | GDT_FLAG_LONG_MODE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_selector_values() {
        assert_eq!(SegmentSelector::KERNEL_CODE.bits(), 0x08);
        assert_eq!(SegmentSelector::KERNEL_DATA.bits(), 0x10);
        assert_eq!(SegmentSelector::USER_DATA.bits(), 0x1B);
        assert_eq!(SegmentSelector::USER_CODE.bits(), 0x23);
        assert_eq!(SegmentSelector::TSS.bits(), 0x28);
    }

    #[test]
    fn segment_selector_decomposition() {
        let sel = SegmentSelector::USER_CODE;
        assert_eq!(sel.index(), 4);
        assert_eq!(sel.rpl(), 3);
        assert!(!sel.is_ldt());
    }
}
