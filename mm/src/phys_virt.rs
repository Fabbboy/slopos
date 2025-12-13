#![allow(dead_code)]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use crate::memory_reservations::{
    mm_reservations_find_option, MmRegion, MmReservationType, MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT,
    MM_RESERVATION_FLAG_MMIO,
};

const PAGE_SIZE_4KB: usize = 0x1000;

extern "C" {
    fn kernel_panic(msg: *const c_char) -> !;
    fn get_hhdm_offset() -> u64;
    fn is_hhdm_available() -> c_int;
    fn virt_to_phys(vaddr: u64) -> u64;
    fn klog_printf(level: slopos_lib::klog::KlogLevel, fmt: *const c_char, ...) -> c_int;
    fn mm_reservation_type_name(type_: MmReservationType) -> *const c_char;
}

#[inline]
fn hhdm_available() -> bool {
    unsafe { is_hhdm_available() != 0 }
}

#[no_mangle]
pub extern "C" fn mm_init_phys_virt_helpers() {
    if !hhdm_available() {
        static MSG: &[u8] = b"MM: HHDM unavailable; cannot translate physical addresses\0";
        unsafe {
            kernel_panic(MSG.as_ptr() as *const c_char);
        }
    }
}

#[no_mangle]
pub extern "C" fn mm_phys_to_virt(phys_addr: u64) -> u64 {
    if phys_addr == 0 {
        return 0;
    }

    let reservation: Option<&'static MmRegion> = mm_reservations_find_option(phys_addr);
    if let Some(region) = reservation {
        let allowed = region.flags & (MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT | MM_RESERVATION_FLAG_MMIO);
        if allowed == 0 {
            let type_name = unsafe { mm_reservation_type_name(region.type_) };
            unsafe {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Debug,
                    b"mm_phys_to_virt: rejected reserved phys 0x%llx (%s)\n\0".as_ptr()
                        as *const c_char,
                    phys_addr,
                    type_name,
                );
            }
            return 0;
        }
    }

    if !hhdm_available() {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"mm_phys_to_virt: HHDM unavailable for 0x%llx\n\0".as_ptr() as *const c_char,
                phys_addr,
            );
        }
        return 0;
    }

    let hhdm = unsafe { get_hhdm_offset() };

    // If we were handed something already in the higher-half window, treat it
    // as translated and return it directly rather than overflowing the add.
    if phys_addr >= hhdm {
        return phys_addr;
    }

    if phys_addr > u64::MAX - hhdm {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"mm_phys_to_virt: overflow translating phys 0x%llx with hhdm 0x%llx\n\0".as_ptr()
                    as *const c_char,
                phys_addr,
                hhdm,
            );
        }
        return 0;
    }

    phys_addr + hhdm
}

#[no_mangle]
pub extern "C" fn mm_virt_to_phys(virt_addr: u64) -> u64 {
    if virt_addr == 0 {
        return 0;
    }
    unsafe { virt_to_phys(virt_addr) }
}

#[no_mangle]
pub extern "C" fn mm_zero_physical_page(phys_addr: u64) -> c_int {
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

#[no_mangle]
pub extern "C" fn mm_map_mmio_region(phys_addr: u64, size: usize) -> *mut c_void {
    if phys_addr == 0 || size == 0 {
        return ptr::null_mut();
    }

    let end_addr = phys_addr.wrapping_add(size as u64).wrapping_sub(1);
    if end_addr < phys_addr {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"MM: mm_map_mmio_region overflow detected\n\0".as_ptr() as *const c_char,
            );
        }
        return ptr::null_mut();
    }

    if !hhdm_available() {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"MM: mm_map_mmio_region requires HHDM (unavailable)\n\0".as_ptr()
                    as *const c_char,
            );
        }
        return ptr::null_mut();
    }

    unsafe { (phys_addr + get_hhdm_offset()) as *mut c_void }
}

#[no_mangle]
pub extern "C" fn mm_unmap_mmio_region(_virt_addr: *mut c_void, _size: usize) -> c_int {
    /* HHDM mappings are static; nothing to do yet. */
    0
}
