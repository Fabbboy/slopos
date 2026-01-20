//! TLB Shootdown Tests - Finding Real Bugs in SMP TLB Invalidation
//!
//! These tests target dangerous edge cases in TLB management:
//! - flush_page/flush_range/flush_all with invalid addresses
//! - TlbFlushBatch overflow behavior
//! - SMP state consistency (active_cpu_count, register_cpu edge cases)
//! - handle_shootdown_ipi with invalid cpu_idx
//! - Race conditions in broadcast_flush_request

use core::ffi::c_int;

use slopos_abi::addr::VirtAddr;
use slopos_lib::klog_info;

use crate::tlb::{
    flush_all, flush_asid, flush_page, flush_range, get_active_cpu_count, handle_shootdown_ipi,
    has_invpcid, has_pcid, is_smp_active, set_bsp_apic_id, FlushType, TlbFlushBatch, MAX_CPUS,
    TLB_SHOOTDOWN_VECTOR,
};

// =============================================================================
// BASIC FLUSH OPERATION TESTS
// =============================================================================

pub fn test_flush_page_null_address() -> c_int {
    flush_page(VirtAddr::NULL);
    0
}

pub fn test_flush_page_kernel_address() -> c_int {
    let kernel_addr = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    flush_page(kernel_addr);
    0
}

pub fn test_flush_page_user_max_address() -> c_int {
    let user_max = VirtAddr::new(0x0000_7FFF_FFFF_F000);
    flush_page(user_max);
    0
}

pub fn test_flush_page_high_kernel_address() -> c_int {
    // High canonical kernel address (valid but unusual)
    let high_kernel = VirtAddr::new(0xFFFF_FFFF_FFFF_0000);
    flush_page(high_kernel);
    0
}

pub fn test_flush_range_empty() -> c_int {
    let addr = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    // Start == end means empty range
    flush_range(addr, addr);
    0
}

pub fn test_flush_range_inverted() -> c_int {
    // End < start - should be handled gracefully (probably no-op)
    let start = VirtAddr::new(0xFFFF_FFFF_8001_0000);
    let end = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    flush_range(start, end);
    0
}

pub fn test_flush_range_single_page() -> c_int {
    let start = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    let end = VirtAddr::new(0xFFFF_FFFF_8000_1000); // 4KB
    flush_range(start, end);
    0
}

pub fn test_flush_range_large() -> c_int {
    // Large range should trigger full TLB flush internally (>32 pages)
    let start = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    let end = VirtAddr::new(0xFFFF_FFFF_8010_0000); // 1MB = 256 pages
    flush_range(start, end);
    0
}

pub fn test_flush_range_threshold_boundary() -> c_int {
    // Exactly at INVLPG_THRESHOLD (32 pages)
    let start = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    let end = VirtAddr::new(0xFFFF_FFFF_8002_0000); // 32 * 4KB = 128KB
    flush_range(start, end);
    0
}

pub fn test_flush_all_basic() -> c_int {
    flush_all();
    0
}

pub fn test_flush_asid_kernel_cr3() -> c_int {
    // Flush with a fake CR3 value representing kernel address space
    let fake_asid = 0xFFFF_FFFF_0000_0000u64;
    flush_asid(fake_asid);
    0
}

pub fn test_flush_asid_zero() -> c_int {
    flush_asid(0);
    0
}

// =============================================================================
// TLB FLUSH BATCH TESTS
// =============================================================================

pub fn test_batch_empty_finish() -> c_int {
    let mut batch = TlbFlushBatch::new();
    batch.finish();
    0
}

pub fn test_batch_single_page() -> c_int {
    let mut batch = TlbFlushBatch::new();
    batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000));
    batch.finish();
    0
}

pub fn test_batch_multiple_pages() -> c_int {
    let mut batch = TlbFlushBatch::new();
    for i in 0..10u64 {
        batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000 + i * 0x1000));
    }
    batch.finish();
    0
}

pub fn test_batch_at_threshold() -> c_int {
    // Add exactly INVLPG_THRESHOLD pages (32)
    let mut batch = TlbFlushBatch::new();
    for i in 0..32u64 {
        batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000 + i * 0x1000));
    }
    batch.finish();
    0
}

pub fn test_batch_overflow() -> c_int {
    // Add more than INVLPG_THRESHOLD pages - should trigger full flush
    let mut batch = TlbFlushBatch::new();
    for i in 0..64u64 {
        batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000 + i * 0x1000));
    }
    batch.finish();
    0
}

pub fn test_batch_scattered_addresses() -> c_int {
    // Non-contiguous addresses should still work
    let mut batch = TlbFlushBatch::new();
    batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000));
    batch.add(VirtAddr::new(0xFFFF_FFFF_A000_0000));
    batch.add(VirtAddr::new(0xFFFF_8000_0000_0000));
    batch.finish();
    0
}

pub fn test_batch_drop_flushes() -> c_int {
    // Batch should flush on drop if not finished
    {
        let mut batch = TlbFlushBatch::new();
        batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000));
        // Intentionally not calling finish - drop should handle it
    }
    0
}

pub fn test_batch_double_finish() -> c_int {
    let mut batch = TlbFlushBatch::new();
    batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000));
    batch.finish();
    batch.finish();
    0
}

// =============================================================================
// SMP STATE TESTS
// =============================================================================

pub fn test_is_smp_active_initial() -> c_int {
    // Initially only BSP is active, so SMP should be inactive
    // But since tests run after kernel init, this may vary
    let _is_smp = is_smp_active();
    0
}

pub fn test_get_active_cpu_count() -> c_int {
    let count = get_active_cpu_count();

    if count == 0 {
        klog_info!("TLB_TEST: BUG - active_cpu_count is 0, should be at least 1");
        return -1;
    }

    if count > MAX_CPUS as u32 {
        klog_info!(
            "TLB_TEST: BUG - active_cpu_count {} exceeds MAX_CPUS {}",
            count,
            MAX_CPUS
        );
        return -1;
    }

    0
}

pub fn test_set_bsp_apic_id() -> c_int {
    set_bsp_apic_id(0);
    set_bsp_apic_id(0xFF);
    0
}

// =============================================================================
// HANDLE_SHOOTDOWN_IPI TESTS
// =============================================================================

pub fn test_handle_shootdown_ipi_cpu_zero() -> c_int {
    handle_shootdown_ipi(0);
    0
}

pub fn test_handle_shootdown_ipi_cpu_max_minus_one() -> c_int {
    handle_shootdown_ipi(MAX_CPUS - 1);
    0
}

pub fn test_handle_shootdown_ipi_cpu_overflow() -> c_int {
    // CPU index >= MAX_CPUS should be handled gracefully
    handle_shootdown_ipi(MAX_CPUS);
    handle_shootdown_ipi(MAX_CPUS + 100);
    handle_shootdown_ipi(usize::MAX);
    0
}

// =============================================================================
// CPU FEATURE DETECTION TESTS
// =============================================================================

pub fn test_has_invpcid_consistent() -> c_int {
    // Call twice - should return same result (cached)
    let first = has_invpcid();
    let second = has_invpcid();

    if first != second {
        klog_info!("TLB_TEST: BUG - has_invpcid returned different values");
        return -1;
    }

    0
}

pub fn test_has_pcid_consistent() -> c_int {
    let first = has_pcid();
    let second = has_pcid();

    if first != second {
        klog_info!("TLB_TEST: BUG - has_pcid returned different values");
        return -1;
    }

    0
}

// =============================================================================
// CONSTANTS VALIDATION TESTS
// =============================================================================

pub fn test_tlb_shootdown_vector_valid() -> c_int {
    // Vector should be in valid IPI range (0x20-0xFE)
    if TLB_SHOOTDOWN_VECTOR < 0x20 {
        klog_info!(
            "TLB_TEST: BUG - TLB_SHOOTDOWN_VECTOR 0x{:x} conflicts with exceptions",
            TLB_SHOOTDOWN_VECTOR
        );
        return -1;
    }

    if TLB_SHOOTDOWN_VECTOR > 0xFE {
        klog_info!(
            "TLB_TEST: BUG - TLB_SHOOTDOWN_VECTOR 0x{:x} is invalid (> 0xFE)",
            TLB_SHOOTDOWN_VECTOR
        );
        return -1;
    }

    0
}

pub fn test_max_cpus_reasonable() -> c_int {
    if MAX_CPUS < 1 {
        klog_info!("TLB_TEST: BUG - MAX_CPUS is 0");
        return -1;
    }

    if MAX_CPUS > 1024 {
        klog_info!(
            "TLB_TEST: WARNING - MAX_CPUS {} is unusually large",
            MAX_CPUS
        );
    }

    0
}

// =============================================================================
// FLUSH TYPE CONVERSION TESTS
// =============================================================================

pub fn test_flush_type_from_valid() -> c_int {
    if FlushType::from(0) != FlushType::None {
        klog_info!("TLB_TEST: FlushType::from(0) != None");
        return -1;
    }
    if FlushType::from(1) != FlushType::SinglePage {
        klog_info!("TLB_TEST: FlushType::from(1) != SinglePage");
        return -1;
    }
    if FlushType::from(2) != FlushType::Range {
        klog_info!("TLB_TEST: FlushType::from(2) != Range");
        return -1;
    }
    if FlushType::from(3) != FlushType::Full {
        klog_info!("TLB_TEST: FlushType::from(3) != Full");
        return -1;
    }

    0
}

pub fn test_flush_type_from_invalid() -> c_int {
    // Invalid values should map to None
    if FlushType::from(4) != FlushType::None {
        klog_info!("TLB_TEST: FlushType::from(4) != None");
        return -1;
    }
    if FlushType::from(255) != FlushType::None {
        klog_info!("TLB_TEST: FlushType::from(255) != None");
        return -1;
    }
    if FlushType::from(u32::MAX) != FlushType::None {
        klog_info!("TLB_TEST: FlushType::from(u32::MAX) != None");
        return -1;
    }

    0
}

// =============================================================================
// STRESS TESTS
// =============================================================================

pub fn test_rapid_flush_pages() -> c_int {
    // Rapidly flush many pages - potential race condition finder
    for i in 0..100u64 {
        flush_page(VirtAddr::new(0xFFFF_FFFF_8000_0000 + i * 0x1000));
    }
    0
}

pub fn test_rapid_flush_all() -> c_int {
    // Multiple full flushes in quick succession
    for _ in 0..10 {
        flush_all();
    }
    0
}

pub fn test_interleaved_flush_operations() -> c_int {
    // Mix different flush operations
    flush_page(VirtAddr::new(0xFFFF_FFFF_8000_0000));
    flush_all();
    flush_range(
        VirtAddr::new(0xFFFF_FFFF_8001_0000),
        VirtAddr::new(0xFFFF_FFFF_8002_0000),
    );
    flush_page(VirtAddr::new(0xFFFF_FFFF_8003_0000));
    flush_asid(0);
    0
}

// =============================================================================
// PUBLIC TEST RUNNER
// =============================================================================

pub fn run_tlb_tests() -> (u32, u32) {
    let mut passed = 0u32;
    let mut total = 0u32;

    macro_rules! run_test {
        ($name:expr, $test_fn:expr) => {{
            total += 1;
            let result = $test_fn();
            if result == 0 {
                passed += 1;
            } else {
                klog_info!("TLB_TEST FAILED: {}", $name);
            }
        }};
    }

    klog_info!("=== TLB Shootdown Tests ===");

    // Basic flush operations
    run_test!("flush_page_null_address", test_flush_page_null_address);
    run_test!("flush_page_kernel_address", test_flush_page_kernel_address);
    run_test!(
        "flush_page_user_max_address",
        test_flush_page_user_max_address
    );
    run_test!(
        "flush_page_high_kernel_address",
        test_flush_page_high_kernel_address
    );
    run_test!("flush_range_empty", test_flush_range_empty);
    run_test!("flush_range_inverted", test_flush_range_inverted);
    run_test!("flush_range_single_page", test_flush_range_single_page);
    run_test!("flush_range_large", test_flush_range_large);
    run_test!(
        "flush_range_threshold_boundary",
        test_flush_range_threshold_boundary
    );
    run_test!("flush_all_basic", test_flush_all_basic);
    run_test!("flush_asid_kernel_cr3", test_flush_asid_kernel_cr3);
    run_test!("flush_asid_zero", test_flush_asid_zero);

    // Batch tests
    run_test!("batch_empty_finish", test_batch_empty_finish);
    run_test!("batch_single_page", test_batch_single_page);
    run_test!("batch_multiple_pages", test_batch_multiple_pages);
    run_test!("batch_at_threshold", test_batch_at_threshold);
    run_test!("batch_overflow", test_batch_overflow);
    run_test!("batch_scattered_addresses", test_batch_scattered_addresses);
    run_test!("batch_drop_flushes", test_batch_drop_flushes);
    run_test!("batch_double_finish", test_batch_double_finish);

    // SMP state tests
    run_test!("is_smp_active_initial", test_is_smp_active_initial);
    run_test!("get_active_cpu_count", test_get_active_cpu_count);
    run_test!("set_bsp_apic_id", test_set_bsp_apic_id);

    // Handle shootdown IPI tests
    run_test!(
        "handle_shootdown_ipi_cpu_zero",
        test_handle_shootdown_ipi_cpu_zero
    );
    run_test!(
        "handle_shootdown_ipi_cpu_max_minus_one",
        test_handle_shootdown_ipi_cpu_max_minus_one
    );
    run_test!(
        "handle_shootdown_ipi_cpu_overflow",
        test_handle_shootdown_ipi_cpu_overflow
    );

    // Feature detection tests
    run_test!("has_invpcid_consistent", test_has_invpcid_consistent);
    run_test!("has_pcid_consistent", test_has_pcid_consistent);

    // Constants validation
    run_test!(
        "tlb_shootdown_vector_valid",
        test_tlb_shootdown_vector_valid
    );
    run_test!("max_cpus_reasonable", test_max_cpus_reasonable);

    // Flush type tests
    run_test!("flush_type_from_valid", test_flush_type_from_valid);
    run_test!("flush_type_from_invalid", test_flush_type_from_invalid);

    // Stress tests
    run_test!("rapid_flush_pages", test_rapid_flush_pages);
    run_test!("rapid_flush_all", test_rapid_flush_all);
    run_test!(
        "interleaved_flush_operations",
        test_interleaved_flush_operations
    );

    klog_info!("TLB tests: {}/{} passed", passed, total);
    (passed, total)
}
