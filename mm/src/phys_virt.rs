
use core::ffi::{c_char, c_int, CStr, c_void};
use core::ptr;

use slopos_lib::{klog_debug, klog_info};

use crate::memory_reservations::{
    mm_reservations_find_option, MmRegion, MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT,
    MM_RESERVATION_FLAG_MMIO,
};

const PAGE_SIZE_4KB: usize = 0x1000;

use crate::paging::virt_to_phys;
use crate::memory_reservations::mm_reservation_type_name;
// Keep extern "C" for boot functions to break circular dependency
unsafe extern "C" {
    fn kernel_panic(msg: *const c_char) -> !;
    fn get_hhdm_offset() -> u64;
    fn is_hhdm_available() -> c_int;
}

#[inline]
fn hhdm_available() -> bool {
    unsafe { is_hhdm_available() != 0 }
}

#[unsafe(no_mangle)]
pub fn mm_init_phys_virt_helpers() {
    if !hhdm_available() {
        static MSG: &[u8] = b"MM: HHDM unavailable; cannot translate physical addresses\0";
        unsafe {
            kernel_panic(MSG.as_ptr() as *const c_char);
        }
    }
}

#[unsafe(no_mangle)]
pub fn mm_phys_to_virt(phys_addr: u64) -> u64 {
    if phys_addr == 0 {
        return 0;
    }

    let reservation: Option<&'static MmRegion> = mm_reservations_find_option(phys_addr);
    if let Some(region) = reservation {
        let allowed = region.flags & (MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT | MM_RESERVATION_FLAG_MMIO);
        if allowed == 0 {
            let type_name = mm_reservation_type_name(region.type_);
            let type_name_str = unsafe { CStr::from_ptr(type_name) }
                .to_str()
                .unwrap_or("<invalid utf-8>");
            klog_debug!("mm_phys_to_virt: rejected reserved phys 0x{:x} ({})", phys_addr, type_name_str);
            return 0;
        }
    }

    if !hhdm_available() {
        klog_info!("mm_phys_to_virt: HHDM unavailable for 0x{:x}", phys_addr);
        return 0;
    }

    let hhdm = unsafe { get_hhdm_offset() };

    // If we were handed something already in the higher-half window, treat it
    // as translated and return it directly rather than overflowing the add.
    if phys_addr >= hhdm {
        return phys_addr;
    }

    if phys_addr > u64::MAX - hhdm {
        klog_info!("mm_phys_to_virt: overflow translating phys 0x{:x} with hhdm 0x{:x}", phys_addr, hhdm);
        return 0;
    }

    phys_addr + hhdm
}

#[unsafe(no_mangle)]
pub fn mm_virt_to_phys(virt_addr: u64) -> u64 {
    if virt_addr == 0 {
        return 0;
    }
    virt_to_phys(virt_addr)
}

#[unsafe(no_mangle)]
pub fn mm_zero_physical_page(phys_addr: u64) -> c_int {
    if phys_addr == 0 {
        return -1;
    }

    let virt = mm_phys_to_virt(phys_addr);
    if virt == 0 {
        return -1;
    }

    unsafe {
        ptr::write_bytes(virt as *mut u8, 0, PAGE_SIZE_4KB);
    }
    0
}

#[unsafe(no_mangle)]
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

    unsafe { (phys_addr + get_hhdm_offset()) as *mut c_void }
}

#[unsafe(no_mangle)]
pub fn mm_unmap_mmio_region(_virt_addr: *mut c_void, _size: usize) -> c_int {
    /* HHDM mappings are static; nothing to do yet. */
    0
}
