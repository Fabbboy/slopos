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
use slopos_ostd::klog_info;
use slopos_testing::TestResult;

use crate::process_vm::{create_process_vm, destroy_process_vm, process_vm_handle};
use crate::tlb::TlbProcessKey;
use crate::tlb::{
    CpuMask, FlushType, TLB_SHOOTDOWN_VECTOR, TlbFlushBatch, enter_lazy_tlb, exit_lazy_tlb,
    flush_all, flush_asid, flush_page, flush_range, get_active_cpu_count, handle_shootdown_ipi,
    has_invpcid, has_pcid, is_smp_active, notify_mm_switch, process_tlb_cpumask_count,
    should_flush_tlb,
};
use slopos_abi::task::INVALID_PROCESS_ID;

/// Two CPU indices past anything this machine will bring online, so a test
/// can drive the per-process shootdown mask without disturbing a live
/// CPU's address-space bookkeeping or provoking a real IPI.
const OFFLINE_CPU_A: usize = MAX_CPUS - 1;
const OFFLINE_CPU_B: usize = MAX_CPUS - 2;

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

pub fn test_cpumask_set_clear() -> TestResult {
    let mask = CpuMask::new();
    mask.set(3);
    mask.set(129);
    if !mask.contains(3) || !mask.contains(129) {
        return TestResult::Fail;
    }
    mask.clear(3);
    if mask.contains(3) || !mask.contains(129) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_cpumask_iter_set() -> TestResult {
    let mask = CpuMask::new();
    mask.set(1);
    mask.set(65);
    mask.set(130);
    let mut found = [false; 3];
    let mut count = 0usize;

    for cpu in mask.iter_set() {
        match cpu {
            1 => found[0] = true,
            65 => found[1] = true,
            130 => found[2] = true,
            _ => return TestResult::Fail,
        }
        count += 1;
    }

    if count != 3 || !found[0] || !found[1] || !found[2] {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_cpumask_boundary_cpus() -> TestResult {
    let boundary = [0usize, 63, 64, 127, 128, 191, 192, 255];
    let mask = CpuMask::new();
    for cpu in boundary {
        mask.set(cpu);
    }
    for cpu in boundary {
        if !mask.contains(cpu) {
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_cpumask_clear_all() -> TestResult {
    let mask = CpuMask::new();
    for cpu in [0usize, 4, 67, 129, 255] {
        mask.set(cpu);
    }
    mask.clear_all();
    if mask.count() != 0 {
        return TestResult::Fail;
    }
    for cpu in [0usize, 4, 67, 129, 255] {
        if mask.contains(cpu) {
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_lazy_tlb_flag() -> TestResult {
    let cpu = 0usize;
    exit_lazy_tlb(cpu);
    if !should_flush_tlb(cpu) {
        return TestResult::Fail;
    }
    enter_lazy_tlb(cpu);
    if should_flush_tlb(cpu) {
        return TestResult::Fail;
    }
    exit_lazy_tlb(cpu);
    if !should_flush_tlb(cpu) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_should_flush_tlb_lazy_skips() -> TestResult {
    let cpu = 0usize;
    enter_lazy_tlb(cpu);
    let result = should_flush_tlb(cpu);
    exit_lazy_tlb(cpu);
    if result {
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

// =============================================================================
// PER-PROCESS SHOOTDOWN MASK TESTS
// =============================================================================

/// Destroying a process leaves no CPU in its shootdown mask.
///
/// Process ids are recycled, so a mask that outlives its process is
/// inherited by the id's next holder: it would shoot down CPUs that never
/// mapped the new address space, and — because the mask is the complete
/// target list — say nothing about the CPUs that did.
pub fn test_destroy_clears_the_process_shootdown_mask() -> TestResult {
    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        klog_info!("TLB_TEST: could not create a process VM");
        return TestResult::Fail;
    }
    let Some(key) = process_vm_handle(pid).and_then(|h| TlbProcessKey::from_slot(h.slot())) else {
        klog_info!("TLB_TEST: a live process has no shootdown key");
        destroy_process_vm(pid);
        return TestResult::Fail;
    };

    notify_mm_switch(Some(key), pid, OFFLINE_CPU_A);
    notify_mm_switch(Some(key), pid, OFFLINE_CPU_B);
    let masked = process_tlb_cpumask_count(key);
    if masked != 2 {
        klog_info!(
            "TLB_TEST: notify_mm_switch left {} CPUs in pid {}'s mask, expected 2",
            masked,
            pid
        );
        destroy_process_vm(pid);
        return TestResult::Fail;
    }

    destroy_process_vm(pid);

    let after = process_tlb_cpumask_count(key);
    if after != 0 {
        klog_info!(
            "TLB_TEST: destroyed pid {} left {} CPUs in its shootdown mask",
            pid,
            after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A targeted flush reaches every CPU the mask names.
///
/// `targeted_flush_request` skips offline CPUs, so an offline-only mask
/// must still complete rather than hang waiting for an ack that cannot
/// come — and the mask must survive the flush, because a live process
/// keeps running on those CPUs afterwards.
pub fn test_targeted_flush_covers_every_masked_cpu() -> TestResult {
    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        klog_info!("TLB_TEST: could not create a process VM");
        return TestResult::Fail;
    }
    let Some(key) = process_vm_handle(pid).and_then(|h| TlbProcessKey::from_slot(h.slot())) else {
        klog_info!("TLB_TEST: a live process has no shootdown key");
        destroy_process_vm(pid);
        return TestResult::Fail;
    };

    let live_cpu = slopos_arch::pcr::get_current_cpu();
    notify_mm_switch(Some(key), pid, live_cpu);
    notify_mm_switch(Some(key), pid, OFFLINE_CPU_A);
    notify_mm_switch(Some(key), pid, OFFLINE_CPU_B);

    let masked = process_tlb_cpumask_count(key);
    if masked != 3 {
        klog_info!(
            "TLB_TEST: pid {} mask holds {} CPUs, expected 3",
            pid,
            masked
        );
        destroy_process_vm(pid);
        return TestResult::Fail;
    }

    crate::tlb::flush_all_for_process(key);

    let after = process_tlb_cpumask_count(key);
    // Switch this CPU back off the address space, the way a real context
    // switch would, before the process goes away.
    notify_mm_switch(None, INVALID_PROCESS_ID, live_cpu);
    destroy_process_vm(pid);
    if after != 3 {
        klog_info!(
            "TLB_TEST: flushing pid {} dropped its mask to {} CPUs",
            pid,
            after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_flush_page_null_address, suite = tlb);
slopos_testing::stest!(name = test_flush_page_kernel_address, suite = tlb);
slopos_testing::stest!(name = test_flush_page_user_max_address, suite = tlb);
slopos_testing::stest!(name = test_flush_page_high_kernel_address, suite = tlb);
slopos_testing::stest!(name = test_flush_range_empty, suite = tlb);
slopos_testing::stest!(name = test_flush_range_inverted, suite = tlb);
slopos_testing::stest!(name = test_flush_range_single_page, suite = tlb);
slopos_testing::stest!(name = test_flush_range_large, suite = tlb);
slopos_testing::stest!(name = test_flush_range_threshold_boundary, suite = tlb);
slopos_testing::stest!(name = test_flush_all_basic, suite = tlb);
slopos_testing::stest!(name = test_flush_asid_kernel_cr3, suite = tlb);
slopos_testing::stest!(name = test_flush_asid_zero, suite = tlb);
slopos_testing::stest!(name = test_batch_empty_finish, suite = tlb);
slopos_testing::stest!(name = test_batch_single_page, suite = tlb);
slopos_testing::stest!(name = test_batch_multiple_pages, suite = tlb);
slopos_testing::stest!(name = test_batch_at_threshold, suite = tlb);
slopos_testing::stest!(name = test_batch_overflow, suite = tlb);
slopos_testing::stest!(name = test_batch_scattered_addresses, suite = tlb);
slopos_testing::stest!(name = test_batch_drop_flushes, suite = tlb);
slopos_testing::stest!(name = test_batch_double_finish, suite = tlb);
slopos_testing::stest!(name = test_is_smp_active_initial, suite = tlb);
slopos_testing::stest!(name = test_get_active_cpu_count, suite = tlb);
slopos_testing::stest!(name = test_bsp_apic_id_from_pcr, suite = tlb);
slopos_testing::stest!(name = test_handle_shootdown_ipi_cpu_zero, suite = tlb);
slopos_testing::stest!(
    name = test_handle_shootdown_ipi_cpu_max_minus_one,
    suite = tlb
);
slopos_testing::stest!(name = test_handle_shootdown_ipi_cpu_overflow, suite = tlb);
slopos_testing::stest!(name = test_has_invpcid_consistent, suite = tlb);
slopos_testing::stest!(name = test_has_pcid_consistent, suite = tlb);
slopos_testing::stest!(name = test_tlb_shootdown_vector_valid, suite = tlb);
slopos_testing::stest!(name = test_max_cpus_reasonable, suite = tlb);
slopos_testing::stest!(name = test_flush_type_from_valid, suite = tlb);
slopos_testing::stest!(name = test_flush_type_from_invalid, suite = tlb);
slopos_testing::stest!(name = test_cpumask_set_clear, suite = tlb);
slopos_testing::stest!(name = test_cpumask_iter_set, suite = tlb);
slopos_testing::stest!(name = test_cpumask_boundary_cpus, suite = tlb);
slopos_testing::stest!(name = test_cpumask_clear_all, suite = tlb);
slopos_testing::stest!(name = test_lazy_tlb_flag, suite = tlb);
slopos_testing::stest!(name = test_should_flush_tlb_lazy_skips, suite = tlb);
slopos_testing::stest!(name = test_rapid_flush_pages, suite = tlb);
slopos_testing::stest!(name = test_rapid_flush_all, suite = tlb);
slopos_testing::stest!(name = test_interleaved_flush_operations, suite = tlb);
slopos_testing::stest!(
    name = test_destroy_clears_the_process_shootdown_mask,
    suite = tlb
);
slopos_testing::stest!(
    name = test_targeted_flush_covers_every_masked_cpu,
    suite = tlb
);
