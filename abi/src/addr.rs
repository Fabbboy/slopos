//! Physical and Virtual address types for type-safe memory operations.
//!
//! These newtypes prevent accidentally confusing physical addresses with virtual
//! addresses, which is a common source of bugs in OS development. The types are
//! zero-cost abstractions (`#[repr(transparent)]`) that compile to raw u64 values.
//!
//! # Address Types
//!
//! - [`PhysAddr`]: A physical memory address. Cannot be directly dereferenced.
//! - [`VirtAddr`]: A virtual memory address in kernel or user space.
//!
//! # Example
//!
//! ```ignore
//! use slopos_abi::addr::{PhysAddr, VirtAddr};
//!
//! let phys = PhysAddr::new(0x1000);
//! let virt = VirtAddr::new(0xFFFF_8000_0000_1000);
//!
//! // Type system prevents mistakes:
//! // map_page(virt, phys);  // OK
//! // map_page(phys, virt);  // Compile error!
//! ```

use crate::arch::x86_64::paging::PAGE_SIZE_4KB;

/// A physical memory address.
///
/// Physical addresses cannot be directly dereferenced - they must first be
/// translated to virtual addresses via the HHDM (Higher Half Direct Map) or
/// by looking up the page tables.
///
/// On x86_64, physical addresses are up to 52 bits (4 PB addressable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PhysAddr(pub u64);

/// A virtual memory address.
///
/// Virtual addresses can be kernel-space (higher half) or user-space (lower half).
/// On x86_64, virtual addresses must be "canonical" - bits 48-63 must be copies
/// of bit 47 (sign extension).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct VirtAddr(pub u64);

// =============================================================================
// PhysAddr implementation
// =============================================================================

impl PhysAddr {
    /// The null physical address.
    pub const NULL: Self = Self(0);

    /// Maximum valid physical address on x86_64 (52-bit physical address space).
    pub const MAX: Self = Self((1 << 52) - 1);

    /// Create a new physical address from a raw u64 value.
    ///
    /// # Panics
    ///
    /// Panics if the address exceeds the 52-bit physical address limit.
    #[inline]
    pub fn new(addr: u64) -> Self {
        assert!(addr <= Self::MAX.0, "PhysAddr out of range: 0x{:x}", addr);
        Self(addr)
    }

    /// Create a new physical address if it is in range.
    #[inline]
    pub const fn try_new(addr: u64) -> Option<Self> {
        if addr <= Self::MAX.0 {
            Some(Self(addr))
        } else {
            None
        }
    }

    /// Returns the raw u64 value of this address.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Returns true if this is the null address.
    #[inline]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// Add an offset to this address (wrapping on overflow).
    #[inline]
    pub const fn offset(self, off: u64) -> Self {
        Self(self.0.wrapping_add(off))
    }

    /// Add an offset, returning None on overflow.
    #[inline]
    pub const fn checked_offset(self, off: u64) -> Option<Self> {
        match self.0.checked_add(off) {
            Some(addr) => Some(Self(addr)),
            None => None,
        }
    }

    /// Align address down to the given alignment.
    ///
    /// # Panics
    ///
    /// Debug-panics if `align` is not a power of two.
    #[inline]
    pub const fn align_down(self, align: u64) -> Self {
        debug_assert!(align.is_power_of_two(), "align must be power of two");
        Self(self.0 & !(align - 1))
    }

    /// Align address up to the given alignment.
    ///
    /// # Panics
    ///
    /// Debug-panics if `align` is not a power of two.
    #[inline]
    pub const fn align_up(self, align: u64) -> Self {
        debug_assert!(align.is_power_of_two(), "align must be power of two");
        Self((self.0 + align - 1) & !(align - 1))
    }

    /// Check if address is aligned to the given alignment.
    #[inline]
    pub const fn is_aligned(self, align: u64) -> bool {
        self.0 & (align - 1) == 0
    }

    #[inline]
    pub const fn page_base(self) -> Self {
        self.align_down(PAGE_SIZE_4KB)
    }

    #[inline]
    pub const fn page_offset(self) -> u64 {
        self.0 & (PAGE_SIZE_4KB - 1)
    }
}

// =============================================================================
// VirtAddr implementation
// =============================================================================

impl VirtAddr {
    /// The null virtual address.
    pub const NULL: Self = Self(0);

    /// Create a new virtual address from a raw u64 value.
    ///
    /// # Panics
    ///
    /// Panics if the address is not canonical.
    #[inline]
    pub fn new(addr: u64) -> Self {
        assert!(
            Self::is_canonical(addr),
            "VirtAddr not canonical: 0x{:x}",
            addr
        );
        Self(addr)
    }

    /// Create a new virtual address if it is canonical.
    #[inline]
    pub const fn try_new(addr: u64) -> Option<Self> {
        if Self::is_canonical(addr) {
            Some(Self(addr))
        } else {
            None
        }
    }

    /// Returns the raw u64 value of this address.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Returns true if this is the null address.
    #[inline]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// Convert to a const pointer of type T.
    #[inline]
    pub const fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    /// Convert to a mut pointer of type T.
    #[inline]
    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }

    /// Add an offset to this address (wrapping on overflow).
    #[inline]
    pub const fn offset(self, off: u64) -> Self {
        Self(self.0.wrapping_add(off))
    }

    /// Add an offset, returning None on overflow.
    #[inline]
    pub const fn checked_offset(self, off: u64) -> Option<Self> {
        match self.0.checked_add(off) {
            Some(addr) => Some(Self(addr)),
            None => None,
        }
    }

    /// Align address down to the given alignment.
    #[inline]
    pub const fn align_down(self, align: u64) -> Self {
        Self(self.0 & !(align - 1))
    }

    /// Align address up to the given alignment.
    #[inline]
    pub const fn align_up(self, align: u64) -> Self {
        Self((self.0 + align - 1) & !(align - 1))
    }

    /// Check if address is aligned to the given alignment.
    #[inline]
    pub const fn is_aligned(self, align: u64) -> bool {
        self.0 & (align - 1) == 0
    }

    #[inline]
    pub const fn page_base(self) -> Self {
        self.align_down(PAGE_SIZE_4KB)
    }

    #[inline]
    pub const fn page_offset(self) -> u64 {
        self.0 & (PAGE_SIZE_4KB - 1)
    }

    /// Check if this address is in kernel space (higher half).
    ///
    /// On x86_64 with typical HHDM layout, kernel addresses have bit 47 set.
    #[inline]
    pub const fn is_kernel_space(self) -> bool {
        self.0 >= 0xFFFF_8000_0000_0000
    }

    /// Check if this address is in user space (lower half).
    #[inline]
    pub const fn is_user_space(self) -> bool {
        self.0 < 0x0000_8000_0000_0000
    }

    /// Returns true if the raw address is canonical on x86_64.
    #[inline]
    pub const fn is_canonical(addr: u64) -> bool {
        let sign = (addr >> 47) & 1;
        let upper = addr >> 48;
        if sign == 0 {
            upper == 0
        } else {
            upper == 0xFFFF
        }
    }
}

// =============================================================================
// Conversions
// =============================================================================

impl From<u64> for PhysAddr {
    #[inline]
    fn from(addr: u64) -> Self {
        Self::new(addr)
    }
}

impl From<PhysAddr> for u64 {
    #[inline]
    fn from(addr: PhysAddr) -> Self {
        addr.0
    }
}

impl From<u64> for VirtAddr {
    #[inline]
    fn from(addr: u64) -> Self {
        Self::new(addr)
    }
}

impl From<VirtAddr> for u64 {
    #[inline]
    fn from(addr: VirtAddr) -> Self {
        addr.0
    }
}

impl<T> From<*const T> for VirtAddr {
    #[inline]
    fn from(ptr: *const T) -> Self {
        Self::new(ptr as u64)
    }
}

impl<T> From<*mut T> for VirtAddr {
    #[inline]
    fn from(ptr: *mut T) -> Self {
        Self::new(ptr as u64)
    }
}

// =============================================================================
// Display implementations
// =============================================================================

impl core::fmt::LowerHex for PhysAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::LowerHex::fmt(&self.0, f)
    }
}

impl core::fmt::UpperHex for PhysAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::UpperHex::fmt(&self.0, f)
    }
}

impl core::fmt::LowerHex for VirtAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::LowerHex::fmt(&self.0, f)
    }
}

impl core::fmt::UpperHex for VirtAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::UpperHex::fmt(&self.0, f)
    }
}
