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
//! - [`MmioAddr`]: An MMIO device address. Must use volatile operations only.
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

/// An MMIO (Memory-Mapped I/O) address.
///
/// MMIO addresses are special virtual addresses that map to device registers.
/// They must be accessed using volatile operations only - regular loads/stores
/// may be incorrectly optimized by the compiler.
///
/// This type is equivalent to Linux's `__iomem *` annotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct MmioAddr(pub u64);

// =============================================================================
// PhysAddr implementation
// =============================================================================

impl PhysAddr {
    /// The null physical address.
    pub const NULL: Self = Self(0);

    /// Maximum valid physical address on x86_64 (52-bit physical address space).
    pub const MAX: Self = Self((1 << 52) - 1);

    /// Create a new physical address from a raw u64 value.
    #[inline]
    pub const fn new(addr: u64) -> Self {
        Self(addr)
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

    /// Returns the page-aligned base address (4KB pages).
    #[inline]
    pub const fn page_base(self) -> Self {
        self.align_down(4096)
    }

    /// Returns the offset within a 4KB page.
    #[inline]
    pub const fn page_offset(self) -> u64 {
        self.0 & 0xFFF
    }
}

// =============================================================================
// VirtAddr implementation
// =============================================================================

impl VirtAddr {
    /// The null virtual address.
    pub const NULL: Self = Self(0);

    /// Create a new virtual address from a raw u64 value.
    #[inline]
    pub const fn new(addr: u64) -> Self {
        Self(addr)
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

    /// Returns the page-aligned base address (4KB pages).
    #[inline]
    pub const fn page_base(self) -> Self {
        self.align_down(4096)
    }

    /// Returns the offset within a 4KB page.
    #[inline]
    pub const fn page_offset(self) -> u64 {
        self.0 & 0xFFF
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
}

// =============================================================================
// MmioAddr implementation
// =============================================================================

impl MmioAddr {
    /// The null MMIO address.
    pub const NULL: Self = Self(0);

    /// Create a new MMIO address from a raw u64 value.
    #[inline]
    pub const fn new(addr: u64) -> Self {
        Self(addr)
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

    /// Add an offset to this address.
    #[inline]
    pub const fn offset(self, off: u64) -> Self {
        Self(self.0.wrapping_add(off))
    }
}

// =============================================================================
// Conversions
// =============================================================================

impl From<u64> for PhysAddr {
    #[inline]
    fn from(addr: u64) -> Self {
        Self(addr)
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
        Self(addr)
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
        Self(ptr as u64)
    }
}

impl<T> From<*mut T> for VirtAddr {
    #[inline]
    fn from(ptr: *mut T) -> Self {
        Self(ptr as u64)
    }
}

impl From<u64> for MmioAddr {
    #[inline]
    fn from(addr: u64) -> Self {
        Self(addr)
    }
}

impl From<MmioAddr> for u64 {
    #[inline]
    fn from(addr: MmioAddr) -> Self {
        addr.0
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

impl core::fmt::LowerHex for MmioAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::LowerHex::fmt(&self.0, f)
    }
}

impl core::fmt::UpperHex for MmioAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::UpperHex::fmt(&self.0, f)
    }
}
