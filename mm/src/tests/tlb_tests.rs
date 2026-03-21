//! TLB Shootdown Tests - Finding Real Bugs in SMP TLB Invalidation
//!
//! These tests target dangerous edge cases in TLB management:
//! - flush_page/flush_range/flush_all with invalid addresses
//! - TlbFlushBatch overflow behavior
//! - SMP state consistency (active_cpu_count, notify_cpu_online edge cases)
//! - handle_shootdown_ipi with invalid cpu_idx
//! - Race conditions in broadcast_flush_request

use slopos_abi::addr::VirtAddr;
use slopos_arch::MAX_CPUS;
use slopos_testing::TestResult;
use slopos_utils::klog_info;

use crate::tlb::{
    FlushType, TLB_SHOOTDOWN_VECTOR, TlbFlushBatch, flush_all, flush_asid, flush_page, flush_range,
    get_active_cpu_count, handle_shootdown_ipi, has_invpcid, has_pcid, is_smp_active,
};

// =============================================================================
// BASIC FLUSH OPERATION TESTS
// =============================================================================

pub fn test_flush_page_null_address() -> TestResult {
    flush_page(VirtAddr::NULL);
    TestResult::Pass
}

pub fn test_flush_page_kernel_address() -> TestResult {
    let kernel_addr = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    flush_page(kernel_addr);
    TestResult::Pass
}

pub fn test_flush_page_user_max_address() -> TestResult {
    let user_max = VirtAddr::new(0x0000_7FFF_FFFF_F000);
    flush_page(user_max);
    TestResult::Pass
}

pub fn test_flush_page_high_kernel_address() -> TestResult {
    // High canonical kernel address (valid but unusual)
    let high_kernel = VirtAddr::new(0xFFFF_FFFF_FFFF_0000);
    flush_page(high_kernel);
    TestResult::Pass
}

pub fn test_flush_range_empty() -> TestResult {
    let addr = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    // Start == end means empty range
    flush_range(addr, addr);
    TestResult::Pass
}

pub fn test_flush_range_inverted() -> TestResult {
    // End < start - should be handled gracefully (probably no-op)
    let start = VirtAddr::new(0xFFFF_FFFF_8001_0000);
    let end = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    flush_range(start, end);
    TestResult::Pass
}

pub fn test_flush_range_single_page() -> TestResult {
    let start = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    let end = VirtAddr::new(0xFFFF_FFFF_8000_1000); // 4KB
    flush_range(start, end);
    TestResult::Pass
}

pub fn test_flush_range_large() -> TestResult {
    // Large range should trigger full TLB flush internally (>32 pages)
    let start = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    let end = VirtAddr::new(0xFFFF_FFFF_8010_0000); // 1MB = 256 pages
    flush_range(start, end);
    TestResult::Pass
}

pub fn test_flush_range_threshold_boundary() -> TestResult {
    // Exactly at INVLPG_THRESHOLD (32 pages)
    let start = VirtAddr::new(0xFFFF_FFFF_8000_0000);
    let end = VirtAddr::new(0xFFFF_FFFF_8002_0000); // 32 * 4KB = 128KB
    flush_range(start, end);
    TestResult::Pass
}

pub fn test_flush_all_basic() -> TestResult {
    flush_all();
    TestResult::Pass
}

pub fn test_flush_asid_kernel_cr3() -> TestResult {
    // Flush with a fake CR3 value representing kernel address space
    let fake_asid = 0xFFFF_FFFF_0000_0000u64;
    flush_asid(fake_asid);
    TestResult::Pass
}

pub fn test_flush_asid_zero() -> TestResult {
    flush_asid(0);
    TestResult::Pass
}

// =============================================================================
// TLB FLUSH BATCH TESTS
// =============================================================================

pub fn test_batch_empty_finish() -> TestResult {
    let mut batch = TlbFlushBatch::new();
    batch.finish();
    TestResult::Pass
}

pub fn test_batch_single_page() -> TestResult {
    let mut batch = TlbFlushBatch::new();
    batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000));
    batch.finish();
    TestResult::Pass
}

pub fn test_batch_multiple_pages() -> TestResult {
    let mut batch = TlbFlushBatch::new();
    for i in 0..10u64 {
        batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000 + i * 0x1000));
    }
    batch.finish();
    TestResult::Pass
}

pub fn test_batch_at_threshold() -> TestResult {
    // Add exactly INVLPG_THRESHOLD pages (32)
    let mut batch = TlbFlushBatch::new();
    for i in 0..32u64 {
        batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000 + i * 0x1000));
    }
    batch.finish();
    TestResult::Pass
}

pub fn test_batch_overflow() -> TestResult {
    // Add more than INVLPG_THRESHOLD pages - should trigger full flush
    let mut batch = TlbFlushBatch::new();
    for i in 0..64u64 {
        batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000 + i * 0x1000));
    }
    batch.finish();
    TestResult::Pass
}

pub fn test_batch_scattered_addresses() -> TestResult {
    // Non-contiguous addresses should still work
    let mut batch = TlbFlushBatch::new();
    batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000));
    batch.add(VirtAddr::new(0xFFFF_FFFF_A000_0000));
    batch.add(VirtAddr::new(0xFFFF_8000_0000_0000));
    batch.finish();
    TestResult::Pass
}

pub fn test_batch_drop_flushes() -> TestResult {
    // Batch should flush on drop if not finished
    {
        let mut batch = TlbFlushBatch::new();
        batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000));
        // Intentionally not calling finish - drop should handle it
    }
    TestResult::Pass
}

pub fn test_batch_double_finish() -> TestResult {
    let mut batch = TlbFlushBatch::new();
    batch.add(VirtAddr::new(0xFFFF_FFFF_8000_0000));
    batch.finish();
    batch.finish();
    TestResult::Pass
}

// =============================================================================
// SMP STATE TESTS
// =============================================================================

pub fn test_is_smp_active_initial() -> TestResult {
    // Initially only BSP is active, so SMP should be inactive
    // But since tests run after kernel init, this may vary
    let _is_smp = is_smp_active();
    TestResult::Pass
}

pub fn test_get_active_cpu_count() -> TestResult {
    let count = get_active_cpu_count();

    if count == 0 {
        klog_info!("TLB_TEST: BUG - active_cpu_count is 0, should be at least 1");
        return TestResult::Fail;
    }

    if count > MAX_CPUS as u32 {
        klog_info!(
            "TLB_TEST: BUG - active_cpu_count {} exceeds MAX_CPUS {}",
            count,
            MAX_CPUS
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_bsp_apic_id_from_pcr() -> TestResult {
    let bsp_id = slopos_arch::pcr::get_bsp_apic_id();
    if bsp_id == u32::MAX {
        klog_info!("TLB_TEST: BUG - BSP APIC ID not set in PCR");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// =============================================================================
// HANDLE_SHOOTDOWN_IPI TESTS
// =============================================================================

pub fn test_handle_shootdown_ipi_cpu_zero() -> TestResult {
    handle_shootdown_ipi(0);
    TestResult::Pass
}

pub fn test_handle_shootdown_ipi_cpu_max_minus_one() -> TestResult {
    handle_shootdown_ipi(MAX_CPUS - 1);
    TestResult::Pass
}

pub fn test_handle_shootdown_ipi_cpu_overflow() -> TestResult {
    // CPU index >= MAX_CPUS should be handled gracefully
    handle_shootdown_ipi(MAX_CPUS);
    handle_shootdown_ipi(MAX_CPUS + 100);
    handle_shootdown_ipi(usize::MAX);
    TestResult::Pass
}

// =============================================================================
// CPU FEATURE DETECTION TESTS
// =============================================================================

pub fn test_has_invpcid_consistent() -> TestResult {
    // Call twice - should return same result (cached)
    let first = has_invpcid();
    let second = has_invpcid();

    if first != second {
        klog_info!("TLB_TEST: BUG - has_invpcid returned different values");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_has_pcid_consistent() -> TestResult {
    let first = has_pcid();
    let second = has_pcid();

    if first != second {
        klog_info!("TLB_TEST: BUG - has_pcid returned different values");
        return TestResult::Fail;
    }

    TestResult::Pass
}

// =============================================================================
// CONSTANTS VALIDATION TESTS
// =============================================================================

pub fn test_tlb_shootdown_vector_valid() -> TestResult {
    // Vector should be in valid IPI range (0x20-0xFE)
    if TLB_SHOOTDOWN_VECTOR < 0x20 {
        klog_info!(
            "TLB_TEST: BUG - TLB_SHOOTDOWN_VECTOR 0x{:x} conflicts with exceptions",
            TLB_SHOOTDOWN_VECTOR
        );
        return TestResult::Fail;
    }

    if TLB_SHOOTDOWN_VECTOR > 0xFE {
        klog_info!(
            "TLB_TEST: BUG - TLB_SHOOTDOWN_VECTOR 0x{:x} is invalid (> 0xFE)",
            TLB_SHOOTDOWN_VECTOR
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_max_cpus_reasonable() -> TestResult {
    if MAX_CPUS < 1 {
        klog_info!("TLB_TEST: BUG - MAX_CPUS is 0");
        return TestResult::Fail;
    }

    if MAX_CPUS > 1024 {
        klog_info!(
            "TLB_TEST: WARNING - MAX_CPUS {} is unusually large",
            MAX_CPUS
        );
    }

    TestResult::Pass
}

// =============================================================================
// FLUSH TYPE CONVERSION TESTS
// =============================================================================

pub fn test_flush_type_from_valid() -> TestResult {
    if FlushType::from(0) != FlushType::None {
        klog_info!("TLB_TEST: FlushType::from(0) != None");
        return TestResult::Fail;
    }
    if FlushType::from(1) != FlushType::SinglePage {
        klog_info!("TLB_TEST: FlushType::from(1) != SinglePage");
        return TestResult::Fail;
    }
    if FlushType::from(2) != FlushType::Range {
        klog_info!("TLB_TEST: FlushType::from(2) != Range");
        return TestResult::Fail;
    }
    if FlushType::from(3) != FlushType::Full {
        klog_info!("TLB_TEST: FlushType::from(3) != Full");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_flush_type_from_invalid() -> TestResult {
    // Invalid values should map to None
    if FlushType::from(4) != FlushType::None {
        klog_info!("TLB_TEST: FlushType::from(4) != None");
        return TestResult::Fail;
    }
    if FlushType::from(255) != FlushType::None {
        klog_info!("TLB_TEST: FlushType::from(255) != None");
        return TestResult::Fail;
    }
    if FlushType::from(u32::MAX) != FlushType::None {
        klog_info!("TLB_TEST: FlushType::from(u32::MAX) != None");
        return TestResult::Fail;
    }

    TestResult::Pass
}

// =============================================================================
// STRESS TESTS
// =============================================================================

pub fn test_rapid_flush_pages() -> TestResult {
    // Rapidly flush many pages - potential race condition finder
    for i in 0..100u64 {
        flush_page(VirtAddr::new(0xFFFF_FFFF_8000_0000 + i * 0x1000));
    }
    TestResult::Pass
}

pub fn test_rapid_flush_all() -> TestResult {
    // Multiple full flushes in quick succession
    for _ in 0..10 {
        flush_all();
    }
    TestResult::Pass
}

pub fn test_interleaved_flush_operations() -> TestResult {
    // Mix different flush operations
    flush_page(VirtAddr::new(0xFFFF_FFFF_8000_0000));
    flush_all();
    flush_range(
        VirtAddr::new(0xFFFF_FFFF_8001_0000),
        VirtAddr::new(0xFFFF_FFFF_8002_0000),
    );
    flush_page(VirtAddr::new(0xFFFF_FFFF_8003_0000));
    flush_asid(0);
    TestResult::Pass
}

slopos_testing::define_test_suite!(
    tlb,
    [
        test_flush_page_null_address,
        test_flush_page_kernel_address,
        test_flush_page_user_max_address,
        test_flush_page_high_kernel_address,
        test_flush_range_empty,
        test_flush_range_inverted,
        test_flush_range_single_page,
        test_flush_range_large,
        test_flush_range_threshold_boundary,
        test_flush_all_basic,
        test_flush_asid_kernel_cr3,
        test_flush_asid_zero,
        test_batch_empty_finish,
        test_batch_single_page,
        test_batch_multiple_pages,
        test_batch_at_threshold,
        test_batch_overflow,
        test_batch_scattered_addresses,
        test_batch_drop_flushes,
        test_batch_double_finish,
        test_is_smp_active_initial,
        test_get_active_cpu_count,
        test_bsp_apic_id_from_pcr,
        test_handle_shootdown_ipi_cpu_zero,
        test_handle_shootdown_ipi_cpu_max_minus_one,
        test_handle_shootdown_ipi_cpu_overflow,
        test_has_invpcid_consistent,
        test_has_pcid_consistent,
        test_tlb_shootdown_vector_valid,
        test_max_cpus_reasonable,
        test_flush_type_from_valid,
        test_flush_type_from_invalid,
        test_rapid_flush_pages,
        test_rapid_flush_all,
        test_interleaved_flush_operations,
    ]
);
