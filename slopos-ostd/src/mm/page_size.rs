//! Type-level page sizes for the cursor map/unmap/protect API.
//!
//! Each impl pins a concrete leaf level at compile time, and cursor methods
//! take `S: PageSize` by turbofish, so a 2 MiB map cannot produce a 4 KiB leaf
//! or vice versa.
//!
//! # Example
//!
//! ```ignore
//! cursor.map::<Size4Kb, _>(uframe, PageProperty::USER_RW)?;
//! cursor.map::<Size2Mb, _>(huge_uframe, PageProperty::USER_RW)?;
//! ```

use crate::mm::page_table::PageTableLevel;

mod sealed {
    pub trait Sealed {}
}

/// Marker trait identifying a leaf page-size at the type level.
///
/// Sealed: the architectural set of leaf sizes is closed at 4 KiB / 2 MiB /
/// 1 GiB, and a fourth impl would walk the cursor to a non-existent level.
pub trait PageSize: sealed::Sealed {
    const LEVEL: PageTableLevel;
    const BYTES: u64;
    const HUGE_BIT: bool;
}

pub struct Size4Kb;
impl sealed::Sealed for Size4Kb {}
impl PageSize for Size4Kb {
    const LEVEL: PageTableLevel = PageTableLevel::One;
    const BYTES: u64 = 0x1000;
    const HUGE_BIT: bool = false;
}

pub struct Size2Mb;
impl sealed::Sealed for Size2Mb {}
impl PageSize for Size2Mb {
    const LEVEL: PageTableLevel = PageTableLevel::Two;
    const BYTES: u64 = 0x20_0000;
    const HUGE_BIT: bool = true;
}

pub struct Size1Gb;
impl sealed::Sealed for Size1Gb {}
impl PageSize for Size1Gb {
    const LEVEL: PageTableLevel = PageTableLevel::Three;
    const BYTES: u64 = 0x4000_0000;
    const HUGE_BIT: bool = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_constants_match_legacy() {
        assert_eq!(Size4Kb::BYTES, 0x1000);
        assert_eq!(Size2Mb::BYTES, 0x20_0000);
        assert_eq!(Size1Gb::BYTES, 0x4000_0000);
    }

    #[test]
    fn level_pairings() {
        assert_eq!(Size4Kb::LEVEL, PageTableLevel::One);
        assert_eq!(Size2Mb::LEVEL, PageTableLevel::Two);
        assert_eq!(Size1Gb::LEVEL, PageTableLevel::Three);
    }

    #[test]
    fn huge_bit_assignment() {
        assert!(!Size4Kb::HUGE_BIT);
        assert!(Size2Mb::HUGE_BIT);
        assert!(Size1Gb::HUGE_BIT);
    }
}
