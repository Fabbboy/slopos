//! Higher Half Direct Map (HHDM) translation.
//!
//! This module is the **SINGLE source of truth** for HHDM offset storage.
//! All HHDM translation should go through this module.
//!
//! # Usage
//!
//! ```ignore
//! use slopos_abi::addr::PhysAddr;
//! use slopos_mm::hhdm::{self, PhysAddrHhdm};
//!
//! // Initialize once during boot
//! hhdm::init(limine_hhdm_offset);
//!
//! // Convert physical to virtual
//! let phys = PhysAddr::new(0x1000);
//! let virt = phys.to_virt();  // Panics if HHDM not initialized
//!
//! // Or check availability first
//! if let Some(virt) = phys.try_to_virt() {
//!     // use virt
//! }
//!
//! // Or with full validation (reservation checks, overflow detection)
//! if let Some(virt) = phys.to_virt_checked() {
//!     // use virt
//! }
//! ```

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};

// =============================================================================
// HHDM Storage (Single Source of Truth)
// =============================================================================

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);
static HHDM_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize HHDM with offset from bootloader.
///
/// This must be called **once** during early boot before any HHDM translations.
///
/// # Panics
///
/// Panics if called more than once. This catches double-init bugs at runtime.
pub fn init(offset: u64) {
    if HHDM_INITIALIZED.swap(true, Ordering::SeqCst) {
        panic!("HHDM already initialized - init() called twice!");
    }
    HHDM_OFFSET.store(offset, Ordering::Release);
}

/// Check if HHDM has been initialized.
///
/// Returns `true` after `init()` has been called.
#[inline]
pub fn is_available() -> bool {
    HHDM_INITIALIZED.load(Ordering::Acquire)
}

/// Get the raw HHDM offset value.
///
/// # Panics
///
/// Debug-panics if HHDM has not been initialized. In release builds,
/// returns 0 (which will cause incorrect translations).
#[inline]
pub fn offset() -> u64 {
    debug_assert!(is_available(), "HHDM not initialized - call hhdm::init() first");
    HHDM_OFFSET.load(Ordering::Acquire)
}

/// Get the HHDM offset, returning None if not initialized.
#[inline]
pub fn try_offset() -> Option<u64> {
    if is_available() {
        Some(HHDM_OFFSET.load(Ordering::Acquire))
    } else {
        None
    }
}

// =============================================================================
// PhysAddr Extension Trait
// =============================================================================

/// Extension trait adding HHDM translation methods to `PhysAddr`.
pub trait PhysAddrHhdm {
    /// Convert physical address to virtual address via HHDM.
    ///
    /// Returns `VirtAddr::NULL` for null physical addresses.
    ///
    /// # Panics
    ///
    /// Panics if HHDM has not been initialized.
    fn to_virt(self) -> VirtAddr;

    /// Try to convert physical to virtual.
    ///
    /// Returns `None` if:
    /// - Physical address is null
    /// - HHDM is not available
    fn try_to_virt(self) -> Option<VirtAddr>;

    /// Convert with full validation.
    ///
    /// Returns `None` if:
    /// - Physical address is null
    /// - HHDM is not available
    /// - Address is in a reserved region that doesn't allow translation
    /// - Translation would overflow
    ///
    /// Also handles already-translated addresses (idempotent).
    fn to_virt_checked(self) -> Option<VirtAddr>;
}

impl PhysAddrHhdm for PhysAddr {
    #[inline]
    fn to_virt(self) -> VirtAddr {
        if self.is_null() {
            return VirtAddr::NULL;
        }
        assert!(is_available(), "HHDM not initialized");
        VirtAddr::new(self.as_u64() + offset())
    }

    #[inline]
    fn try_to_virt(self) -> Option<VirtAddr> {
        if self.is_null() {
            return None;
        }
        let off = try_offset()?;
        Some(VirtAddr::new(self.as_u64() + off))
    }

    fn to_virt_checked(self) -> Option<VirtAddr> {
        use crate::memory_reservations::{
            mm_reservations_find_option, MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT,
            MM_RESERVATION_FLAG_MMIO,
        };

        if self.is_null() {
            return None;
        }

        // Check HHDM availability
        let hhdm = try_offset()?;

        // Check reservation database
        if let Some(region) = mm_reservations_find_option(self.as_u64()) {
            let allowed = region.flags
                & (MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT | MM_RESERVATION_FLAG_MMIO);
            if allowed == 0 {
                return None;
            }
        }

        // If address is already in higher-half, return as-is (idempotent)
        if self.as_u64() >= hhdm {
            return Some(VirtAddr::new(self.as_u64()));
        }

        // Check for overflow
        if self.as_u64() > u64::MAX - hhdm {
            return None;
        }

        Some(VirtAddr::new(self.as_u64() + hhdm))
    }
}

// =============================================================================
// VirtAddr Extension Trait
// =============================================================================

/// Extension trait adding HHDM reverse translation to `VirtAddr`.
pub trait VirtAddrHhdm {
    /// Convert virtual address back to physical assuming HHDM mapping.
    ///
    /// This is a simple arithmetic operation - it assumes the virtual address
    /// was created via HHDM translation. For arbitrary virtual addresses,
    /// use `to_phys_walk()` instead.
    ///
    /// Returns `PhysAddr::NULL` for null virtual addresses.
    fn to_phys_hhdm(self) -> PhysAddr;

    /// Convert virtual address to physical via page table walk.
    ///
    /// This performs an actual page table lookup and works for any mapped
    /// virtual address, not just HHDM-translated ones.
    ///
    /// Returns `None` if:
    /// - Virtual address is null
    /// - Address is not mapped in page tables
    fn to_phys_walk(self) -> Option<PhysAddr>;
}

impl VirtAddrHhdm for VirtAddr {
    #[inline]
    fn to_phys_hhdm(self) -> PhysAddr {
        if self.is_null() {
            return PhysAddr::NULL;
        }
        PhysAddr::new(self.as_u64().wrapping_sub(offset()))
    }

    fn to_phys_walk(self) -> Option<PhysAddr> {
        if self.is_null() {
            return None;
        }
        let phys = crate::paging::virt_to_phys(self);
        if phys.is_null() {
            None
        } else {
            Some(phys)
        }
    }
}

// =============================================================================
// Convenience Functions
// =============================================================================

/// Quick physical-to-virtual translation using the typed API.
///
/// Equivalent to `PhysAddr::new(phys).to_virt().as_u64()`.
#[inline]
pub fn phys_to_virt(phys: u64) -> u64 {
    PhysAddr::new(phys).to_virt().as_u64()
}

/// Quick virtual-to-physical translation assuming HHDM mapping.
///
/// Equivalent to `VirtAddr::new(virt).to_phys_hhdm().as_u64()`.
#[inline]
pub fn virt_to_phys(virt: u64) -> u64 {
    VirtAddr::new(virt).to_phys_hhdm().as_u64()
}
