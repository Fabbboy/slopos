//! VMA Flags and Backing Type Definitions
//!
//! This module provides type-safe bitflags for Virtual Memory Area (VMA) properties
//! and an enum for tracking the backing type of memory regions. These are used for
//! demand paging, copy-on-write, and memory protection.
//!
//! # Design
//!
//! The flags are split into two categories:
//! - **Protection flags** (bits 0-3): Mirror x86_64 PTE flags for easy conversion
//! - **VMA state flags** (bits 16-23): Track memory region state (CoW, lazy, etc.)
//!
//! # Example
//!
//! ```ignore
//! use slopos_mm::vma_flags::{VmaFlags, VmaBacking};
//!
//! // Create flags for a demand-paged anonymous heap region
//! let heap_flags = VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER | VmaFlags::LAZY;
//!
//! // Check if a region is demand-paged
//! if flags.contains(VmaFlags::LAZY) {
//!     // Handle demand fault
//! }
//! ```

use slopos_abi::arch::x86_64::paging::PageFlags;

/// Backing type for a VMA - describes where page content comes from
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VmaBacking {
    /// Anonymous memory - zero-filled on demand
    Anonymous = 0,
    /// File-backed memory - loaded from file on demand (future)
    FileBacked = 1,
    /// Shared memory region - backed by shared memory object
    Shared = 2,
    /// Device memory - direct physical mapping (no demand paging)
    Device = 3,
}

impl Default for VmaBacking {
    fn default() -> Self {
        Self::Anonymous
    }
}

/// VMA flags using bitflags for type safety
///
/// Bits 0-3 mirror x86_64 PTE protection flags for easy conversion.
/// Bits 16-23 are VMA-specific state flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct VmaFlags(u32);

impl VmaFlags {
    // =========================================================================
    // Protection flags (bits 0-3) - mirror x86_64 PTE flags
    // =========================================================================

    /// Page is readable (always true for mapped pages on x86_64)
    pub const READ: Self = Self(1 << 0);
    /// Page is writable
    pub const WRITE: Self = Self(1 << 1);
    /// Page is executable (inverse of NX bit)
    pub const EXEC: Self = Self(1 << 2);
    /// Page is accessible from user mode (Ring 3)
    pub const USER: Self = Self(1 << 3);

    // =========================================================================
    // VMA state flags (bits 16-23)
    // =========================================================================

    /// Copy-on-Write: page is shared read-only, copy on write fault
    pub const COW: Self = Self(1 << 16);
    /// Anonymous memory: zero-fill on demand
    pub const ANON: Self = Self(1 << 17);
    /// File-backed memory: load from file on demand
    pub const BACKED: Self = Self(1 << 18);
    /// Lazy/demand-paged: physical pages not yet allocated
    pub const LAZY: Self = Self(1 << 19);
    /// Shared memory region
    pub const SHARED: Self = Self(1 << 20);
    /// Guard page: triggers stack growth or fault
    pub const GUARD: Self = Self(1 << 21);
    /// Stack region: grows downward
    pub const STACK: Self = Self(1 << 22);
    /// Heap region: grows upward via brk()
    pub const HEAP: Self = Self(1 << 23);

    // =========================================================================
    // Convenience combinations
    // =========================================================================

    /// Empty flags (no permissions, not lazy)
    pub const NONE: Self = Self(0);

    /// Standard user read-write heap flags (demand-paged anonymous)
    pub const USER_HEAP: Self = Self(
        Self::READ.0 | Self::WRITE.0 | Self::USER.0 | Self::ANON.0 | Self::LAZY.0 | Self::HEAP.0,
    );

    /// Standard user read-write stack flags (demand-paged anonymous)
    pub const USER_STACK: Self = Self(
        Self::READ.0 | Self::WRITE.0 | Self::USER.0 | Self::ANON.0 | Self::LAZY.0 | Self::STACK.0,
    );

    /// Standard user read-only code flags (not lazy - code should be pre-loaded)
    pub const USER_CODE: Self = Self(Self::READ.0 | Self::EXEC.0 | Self::USER.0);

    /// Standard user read-write data flags (not lazy - data should be pre-loaded)
    pub const USER_DATA: Self = Self(Self::READ.0 | Self::WRITE.0 | Self::USER.0);

    // =========================================================================
    // Methods
    // =========================================================================

    /// Create flags from raw u32 value
    #[inline]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Get raw u32 value
    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Check if these flags contain all of the given flags
    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Check if these flags contain any of the given flags
    #[inline]
    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    /// Check if flags are empty
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Insert additional flags
    #[inline]
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Remove flags
    #[inline]
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// Toggle flags
    #[inline]
    pub fn toggle(&mut self, other: Self) {
        self.0 ^= other.0;
    }

    /// Set or clear flags based on value
    #[inline]
    pub fn set(&mut self, other: Self, value: bool) {
        if value {
            self.insert(other);
        } else {
            self.remove(other);
        }
    }

    /// Check if this VMA is demand-paged (lazy allocation)
    #[inline]
    pub const fn is_demand_paged(self) -> bool {
        self.contains(Self::LAZY)
    }

    /// Check if this VMA is anonymous (zero-fill on fault)
    #[inline]
    pub const fn is_anonymous(self) -> bool {
        self.contains(Self::ANON)
    }

    /// Check if this VMA is copy-on-write
    #[inline]
    pub const fn is_cow(self) -> bool {
        self.contains(Self::COW)
    }

    /// Check if this VMA is writable
    #[inline]
    pub const fn is_writable(self) -> bool {
        self.contains(Self::WRITE)
    }

    /// Check if this VMA is user-accessible
    #[inline]
    pub const fn is_user(self) -> bool {
        self.contains(Self::USER)
    }

    /// Convert VMA protection flags to x86_64 PTE flags
    ///
    /// This converts the protection bits to the corresponding PageFlags,
    /// taking into account:
    /// - READ is implicit (all present pages are readable on x86_64)
    /// - WRITE maps to PageFlags::WRITABLE
    /// - USER maps to PageFlags::USER
    /// - EXEC maps to absence of PageFlags::NO_EXECUTE
    /// - COW sets !WRITABLE + PageFlags::COW
    #[inline]
    pub const fn to_page_flags(self) -> PageFlags {
        let mut pf = PageFlags::PRESENT;

        // User accessible
        if self.contains(Self::USER) {
            pf = pf.union(PageFlags::USER);
        }

        // Writable (unless COW, which makes it read-only with COW marker)
        if self.contains(Self::COW) {
            // COW pages are read-only with COW bit set
            pf = pf.union(PageFlags::COW);
        } else if self.contains(Self::WRITE) {
            pf = pf.union(PageFlags::WRITABLE);
        }

        // No-execute (inverse of EXEC flag)
        if !self.contains(Self::EXEC) {
            pf = pf.union(PageFlags::NO_EXECUTE);
        }

        pf
    }

    /// Create VMA flags from x86_64 PTE flags
    ///
    /// This is the inverse of `to_page_flags()`.
    #[inline]
    pub const fn from_page_flags(pf: PageFlags) -> Self {
        let mut vf = Self::READ; // READ is always implied on x86_64

        if pf.contains(PageFlags::USER) {
            vf = Self(vf.0 | Self::USER.0);
        }

        if pf.contains(PageFlags::WRITABLE) {
            vf = Self(vf.0 | Self::WRITE.0);
        }

        if pf.contains(PageFlags::COW) {
            vf = Self(vf.0 | Self::COW.0);
        }

        if !pf.contains(PageFlags::NO_EXECUTE) {
            vf = Self(vf.0 | Self::EXEC.0);
        }

        vf
    }

    /// Get protection-only flags (mask out state flags)
    #[inline]
    pub const fn protection_only(self) -> Self {
        Self(self.0 & 0x0000_FFFF)
    }

    /// Get state-only flags (mask out protection flags)
    #[inline]
    pub const fn state_only(self) -> Self {
        Self(self.0 & 0xFFFF_0000)
    }
}

// Implement bitwise operators for ergonomic flag combination
impl core::ops::BitOr for VmaFlags {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for VmaFlags {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl core::ops::BitAnd for VmaFlags {
    type Output = Self;
    #[inline]
    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl core::ops::BitAndAssign for VmaFlags {
    #[inline]
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl core::ops::Not for VmaFlags {
    type Output = Self;
    #[inline]
    fn not(self) -> Self::Output {
        Self(!self.0)
    }
}

/// Legacy constant for backward compatibility during migration
/// TODO: Remove once all callers are migrated to VmaFlags::COW
pub const VMA_FLAG_COW: u32 = VmaFlags::COW.bits();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vma_flags_contains() {
        let flags = VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER;
        assert!(flags.contains(VmaFlags::READ));
        assert!(flags.contains(VmaFlags::WRITE));
        assert!(flags.contains(VmaFlags::USER));
        assert!(!flags.contains(VmaFlags::EXEC));
        assert!(!flags.contains(VmaFlags::LAZY));
    }

    #[test]
    fn test_vma_flags_to_page_flags() {
        // User read-write
        let vma = VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER;
        let pf = vma.to_page_flags();
        assert!(pf.contains(PageFlags::PRESENT));
        assert!(pf.contains(PageFlags::WRITABLE));
        assert!(pf.contains(PageFlags::USER));
        assert!(pf.contains(PageFlags::NO_EXECUTE));

        // COW page (read-only even if WRITE set)
        let vma_cow = VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER | VmaFlags::COW;
        let pf_cow = vma_cow.to_page_flags();
        assert!(pf_cow.contains(PageFlags::PRESENT));
        assert!(!pf_cow.contains(PageFlags::WRITABLE));
        assert!(pf_cow.contains(PageFlags::COW));
        assert!(pf_cow.contains(PageFlags::USER));
    }

    #[test]
    fn test_demand_paging_flags() {
        let heap = VmaFlags::USER_HEAP;
        assert!(heap.is_demand_paged());
        assert!(heap.is_anonymous());
        assert!(heap.is_writable());
        assert!(heap.is_user());
        assert!(!heap.is_cow());
    }
}
