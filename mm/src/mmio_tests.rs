use core::ffi::c_int;

use slopos_abi::addr::PhysAddr;
use slopos_lib::klog_info;

use crate::mmio::MmioRegion;

pub fn test_mmio_empty_region_state() -> c_int {
    let region = MmioRegion::empty();

    if region.is_mapped() {
        klog_info!("MMIO_TEST: Empty region should not be mapped");
        return -1;
    }

    if region.size() != 0 {
        klog_info!("MMIO_TEST: Empty region size should be 0");
        return -1;
    }

    if region.virt_base() != 0 {
        klog_info!("MMIO_TEST: Empty region virt_base should be 0");
        return -1;
    }

    if !region.phys_base().is_null() {
        klog_info!("MMIO_TEST: Empty region phys_base should be null");
        return -1;
    }

    0
}

pub fn test_mmio_is_valid_offset_overflow() -> c_int {
    let region = MmioRegion::empty();

    if region.is_valid_offset(usize::MAX, 1) {
        klog_info!("MMIO_TEST: usize::MAX offset should be invalid");
        return -1;
    }

    if region.is_valid_offset(usize::MAX - 10, 20) {
        klog_info!("MMIO_TEST: Large offset + size overflow should be invalid");
        return -1;
    }

    if !region.is_valid_offset(0, 0) {
        klog_info!("MMIO_TEST: Zero offset/size on empty region should be valid");
        return -1;
    }

    if region.is_valid_offset(1, 0) {
        klog_info!("MMIO_TEST: Non-zero offset on empty region should be invalid");
        return -1;
    }

    0
}

pub fn test_mmio_sub_region_overflow() -> c_int {
    let region = MmioRegion::empty();

    if region.sub_region(usize::MAX, 1).is_some() {
        klog_info!("MMIO_TEST: sub_region with overflow should return None");
        return -1;
    }

    if region.sub_region(usize::MAX - 5, 10).is_some() {
        klog_info!("MMIO_TEST: sub_region with size overflow should return None");
        return -1;
    }

    if region.sub_region(0, 1).is_some() {
        klog_info!("MMIO_TEST: sub_region exceeding parent size should return None");
        return -1;
    }

    0
}

pub fn test_mmio_empty_region_invalid_reads() -> c_int {
    let region = MmioRegion::empty();

    let would_be_oob = !region.is_valid_offset(0, 4);
    if !would_be_oob {
        klog_info!("MMIO_TEST: Empty region should report all reads as OOB");
        return -1;
    }

    0
}

pub fn test_mmio_map_zero_size() -> c_int {
    let result = MmioRegion::map(PhysAddr::new(0x1000), 0);
    if result.is_some() {
        klog_info!("MMIO_TEST: Mapping zero size should fail");
        return -1;
    }

    0
}

pub fn test_mmio_map_null_addr() -> c_int {
    let result = MmioRegion::map(PhysAddr::NULL, 0x1000);
    if result.is_some() {
        klog_info!("MMIO_TEST: Mapping null address should fail");
        return -1;
    }

    0
}

pub fn test_mmio_map_large_size() -> c_int {
    let result = MmioRegion::map(PhysAddr::new(0x1000), usize::MAX);

    if result.is_some() {
        klog_info!("MMIO_TEST: Mapping with huge size should fail");
        return -1;
    }

    0
}

pub fn test_mmio_map_near_phys_limit() -> c_int {
    let near_max = PhysAddr::MAX.as_u64() - 0x1000;
    let result = MmioRegion::map(PhysAddr::new(near_max), 0x3000);

    if result.is_some() {
        klog_info!("MMIO_TEST: Mapping near PhysAddr::MAX should fail gracefully");
        return -1;
    }

    0
}
