//! Physical/Virtual Address Translation
//!
//! This module provides safe, validated address translation functions.
//! Use these instead of raw HHDM arithmetic for most kernel code.
//!
//! # API Layers
//!
//! SlopOS provides three levels of address translation:
//!
//! 1. **Raw HHDM** (`hhdm_phys_to_virt`, `hhdm_virt_to_phys` in mm/lib.rs)
//!    - Bare arithmetic with HHDM offset
//!    - No safety checks
//!    - Use only when you're certain the address is valid
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
//! // Safe translation (preferred)
//! let virt = mm_phys_to_virt(phys_addr);
//! if virt == 0 { /* handle error */ }
//!
//! // Raw translation (only when performance-critical and address is known-safe)
//! let virt = hhdm_phys_to_virt(phys_addr);
//! ```

use core::ffi::{CStr, c_int, c_void};
use core::ptr;

use slopos_lib::{klog_debug, klog_info};

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
pub fn mm_phys_to_virt(phys_addr: u64) -> u64 {
    if phys_addr == 0 {
        return 0;
    }

    let reservation: Option<&'static MmRegion> = mm_reservations_find_option(phys_addr);
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
                phys_addr,
                type_name_str
            );
            return 0;
        }
    }

    if !hhdm_available() {
        klog_info!("mm_phys_to_virt: HHDM unavailable for 0x{:x}", phys_addr);
        return 0;
    }

    let hhdm = hhdm::offset();

    // If we were handed something already in the higher-half window, treat it
    // as translated and return it directly rather than overflowing the add.
    if phys_addr >= hhdm {
        return phys_addr;
    }

    if phys_addr > u64::MAX - hhdm {
        klog_info!(
            "mm_phys_to_virt: overflow translating phys 0x{:x} with hhdm 0x{:x}",
            phys_addr,
            hhdm
        );
        return 0;
    }

    phys_addr + hhdm
}
pub fn mm_virt_to_phys(virt_addr: u64) -> u64 {
    if virt_addr == 0 {
        return 0;
    }
    virt_to_phys(virt_addr)
}
pub fn mm_zero_physical_page(phys_addr: u64) -> c_int {
    if phys_addr == 0 {
        return -1;
    }

    let virt = mm_phys_to_virt(phys_addr);
    if virt == 0 {
        return -1;
    }

    unsafe {
        ptr::write_bytes(virt as *mut u8, 0, PAGE_SIZE_4KB as usize);
    }
    0
}
pub fn mm_map_mmio_region(phys_addr: u64, size: usize) -> *mut c_void {
    if phys_addr == 0 || size == 0 {
        return ptr::null_mut();
    }

    let end_addr = phys_addr.wrapping_add(size as u64).wrapping_sub(1);
    if end_addr < phys_addr {
        klog_info!("MM: mm_map_mmio_region overflow detected");
        return ptr::null_mut();
    }

    if !hhdm_available() {
        klog_info!("MM: mm_map_mmio_region requires HHDM (unavailable)");
        return ptr::null_mut();
    }

    (phys_addr + hhdm::offset()) as *mut c_void
}
pub fn mm_unmap_mmio_region(_virt_addr: *mut c_void, _size: usize) -> c_int {
    /* HHDM mappings are static; nothing to do yet. */
    0
}
