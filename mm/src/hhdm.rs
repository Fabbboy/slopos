//! Higher Half Direct Map (HHDM) translation.
//!
//! Single source of truth for the HHDM offset; all HHDM translation goes
//! through this module.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::sync::InitFlag;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);
static HHDM_INIT: InitFlag = InitFlag::new();

pub fn init(offset: u64) {
    HHDM_OFFSET.store(offset, Ordering::Release);

    if !HHDM_INIT.init_once() {
        panic!("HHDM already initialized - init() called twice!");
    }
}

#[inline]
pub fn is_available() -> bool {
    HHDM_INIT.is_set()
}

/// # Panics
///
/// Debug-panics if HHDM has not been initialized. In release builds,
/// returns 0 (which will cause incorrect translations).
#[inline]
pub fn offset() -> u64 {
    debug_assert!(
        is_available(),
        "HHDM not initialized - call hhdm::init() first"
    );
    HHDM_OFFSET.load(Ordering::Acquire)
}

#[inline]
pub fn try_offset() -> Option<u64> {
    if is_available() {
        Some(HHDM_OFFSET.load(Ordering::Acquire))
    } else {
        None
    }
}

pub trait PhysAddrHhdm {
    /// Returns `VirtAddr::NULL` for null physical addresses.
    ///
    /// # Panics
    ///
    /// Panics if HHDM has not been initialized.
    fn to_virt(self) -> VirtAddr;

    /// Returns `None` if:
    /// - Physical address is null
    /// - HHDM is not available
    fn try_to_virt(self) -> Option<VirtAddr>;

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
            MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT, MM_RESERVATION_FLAG_MMIO,
            mm_reservations_find_option,
        };

        if self.is_null() {
            return None;
        }

        let hhdm = try_offset()?;

        if let Some(region) = mm_reservations_find_option(self.as_u64()) {
            let allowed = region.flags
                & (MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT | MM_RESERVATION_FLAG_MMIO);
            if allowed == 0 {
                return None;
            }
        }

        if self.as_u64() >= hhdm {
            return Some(VirtAddr::new(self.as_u64()));
        }

        let virt = self.as_u64().checked_add(hhdm)?;
        Some(VirtAddr::new(virt))
    }
}

pub trait VirtAddrHhdm {
    /// Arithmetic only: assumes the address came from HHDM translation. Use
    /// `to_phys_walk()` for arbitrary virtual addresses.
    ///
    /// Returns `PhysAddr::NULL` for null virtual addresses.
    fn to_phys_hhdm(self) -> PhysAddr;

    /// Walks the page tables, so it works for any mapped virtual address.
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
        if phys.is_null() { None } else { Some(phys) }
    }
}
