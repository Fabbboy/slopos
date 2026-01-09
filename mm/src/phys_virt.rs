//! Physical/Virtual Address Translation
//!
//! This module provides safe, validated address translation functions.
//! Use these instead of raw HHDM arithmetic for most kernel code.
//!
//! # API Layers
//!
//! SlopOS provides three levels of address translation:
//!
//! 1. **Typed HHDM** (`hhdm` module + `PhysAddrHhdm` trait)
//!    - Type-safe `PhysAddr::to_virt()` translation
//!    - Checks HHDM availability
//!    - Preferred for new code
//!
//! 2. **Safe Wrappers** (`mm_phys_to_virt`, `mm_virt_to_phys` in this module)
//!    - Zero-address handling
//!    - Overflow detection
//!    - Reserved region permission checks
//!    - Already-translated detection
//!    - **Preferred for most kernel code**
//!
//! 3. **Page Table Walk** (`paging::virt_to_phys*`)
//!    - Actual page table translation
//!    - Use when you need to verify mappings exist
//!
//! # Usage
//!
//! ```ignore
//! use slopos_abi::addr::PhysAddr;
//! use slopos_mm::hhdm::PhysAddrHhdm;
//!
//! // Typed translation (preferred for new code)
//! let phys = PhysAddr::new(0x1000);
//! let virt = phys.to_virt();
//!
//! // Safe wrapper (legacy, still supported)
//! let virt = mm_phys_to_virt(PhysAddr::new(phys_addr));
//! if virt.is_null() { /* handle error */ }
//! ```

use core::ffi::{CStr, c_int, c_void};
use core::ptr;

use slopos_lib::{klog_debug, klog_info};
use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::hhdm;
use crate::memory_reservations::{
    MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT, MM_RESERVATION_FLAG_MMIO, MmRegion,
    mm_reservations_find_option,
};

use crate::mm_constants::PAGE_SIZE_4KB;

use crate::memory_reservations::mm_reservation_type_name;
use crate::paging::virt_to_phys;

#[inline]
fn hhdm_available() -> bool {
    hhdm::is_available()
}
pub fn mm_init_phys_virt_helpers() {
    if !hhdm_available() {
        static MSG: &[u8] = b"MM: HHDM unavailable; cannot translate physical addresses\0";
        panic!(
            "{}",
            core::str::from_utf8(MSG).unwrap_or("MM: HHDM unavailable")
        );
    }
}
pub fn mm_phys_to_virt(phys_addr: PhysAddr) -> VirtAddr {
    if phys_addr.is_null() {
        return VirtAddr::NULL;
    }

    let reservation: Option<&'static MmRegion> = mm_reservations_find_option(phys_addr.as_u64());
    if let Some(region) = reservation {
        let allowed =
            region.flags & (MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT | MM_RESERVATION_FLAG_MMIO);
        if allowed == 0 {
            let type_name = mm_reservation_type_name(region.type_);
            let type_name_str = unsafe { CStr::from_ptr(type_name) }
                .to_str()
                .unwrap_or("<invalid utf-8>");
            klog_debug!(
                "mm_phys_to_virt: rejected reserved phys 0x{:x} ({})",
                phys_addr.as_u64(),
                type_name_str
            );
            return VirtAddr::NULL;
        }
    }

    if !hhdm_available() {
        klog_info!(
            "mm_phys_to_virt: HHDM unavailable for 0x{:x}",
            phys_addr.as_u64()
        );
        return VirtAddr::NULL;
    }

    let hhdm = hhdm::offset();

    // If we were handed something already in the higher-half window, treat it
    // as translated and return it directly rather than overflowing the add.
    if phys_addr.as_u64() >= hhdm {
        return VirtAddr::new(phys_addr.as_u64());
    }

    if phys_addr.as_u64() > u64::MAX - hhdm {
        klog_info!(
            "mm_phys_to_virt: overflow translating phys 0x{:x} with hhdm 0x{:x}",
            phys_addr.as_u64(),
            hhdm
        );
        return VirtAddr::NULL;
    }

    VirtAddr::new(phys_addr.as_u64() + hhdm)
}
pub fn mm_virt_to_phys(virt_addr: VirtAddr) -> PhysAddr {
    if virt_addr.is_null() {
        return PhysAddr::NULL;
    }
    virt_to_phys(virt_addr)
}
pub fn mm_zero_physical_page(phys_addr: PhysAddr) -> c_int {
    if phys_addr.is_null() {
        return -1;
    }

    let virt = mm_phys_to_virt(phys_addr);
    if virt.is_null() {
        return -1;
    }

    unsafe {
        ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, PAGE_SIZE_4KB as usize);
    }
    0
}
pub fn mm_map_mmio_region(phys_addr: PhysAddr, size: usize) -> *mut c_void {
    if phys_addr.is_null() || size == 0 {
        return ptr::null_mut();
    }

    let end_addr = phys_addr
        .as_u64()
        .wrapping_add(size as u64)
        .wrapping_sub(1);
    if end_addr < phys_addr.as_u64() {
        klog_info!("MM: mm_map_mmio_region overflow detected");
        return ptr::null_mut();
    }

    if !hhdm_available() {
        klog_info!("MM: mm_map_mmio_region requires HHDM (unavailable)");
        return ptr::null_mut();
    }

    (phys_addr.as_u64() + hhdm::offset()) as *mut c_void
}
pub fn mm_unmap_mmio_region(_virt_addr: *mut c_void, _size: usize) -> c_int {
    /* HHDM mappings are static; nothing to do yet. */
    0
}
