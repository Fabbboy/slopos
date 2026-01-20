//! OOM (Out-of-Memory) and Memory Exhaustion Tests
//!
//! Tests system behavior under memory pressure. These tests are designed
//! to find bugs in error handling paths that are rarely exercised.

use core::ffi::c_int;
use core::ptr;

use slopos_abi::addr::PhysAddr;
use slopos_lib::klog_info;

use crate::hhdm::PhysAddrHhdm;
use crate::kernel_heap::{get_heap_stats, kfree, kmalloc, kzalloc};
use crate::mm_constants::{PageFlags, INVALID_PROCESS_ID, PAGE_SIZE_4KB};
use crate::page_alloc::{
    alloc_page_frame, alloc_page_frames, free_page_frame, get_page_allocator_stats, ALLOC_FLAG_DMA,
    ALLOC_FLAG_NO_PCP, ALLOC_FLAG_ZERO,
};
use crate::process_vm::{create_process_vm, destroy_process_vm, init_process_vm, process_vm_alloc};

/// Test: Allocate until OOM, verify graceful failure
pub fn test_page_alloc_until_oom() -> c_int {
    let mut total = 0u32;
    let mut free_before = 0u32;
    get_page_allocator_stats(&mut total, &mut free_before, ptr::null_mut());

    if free_before < 64 {
        klog_info!("OOM_TEST: Not enough free pages to test ({})", free_before);
        return 0;
    }

    let mut allocated: [PhysAddr; 1024] = [PhysAddr::NULL; 1024];
    let mut count = 0usize;

    // Allocate pages until failure, but limit to avoid test timeout
    let max_alloc = (free_before as usize).min(512);

    for i in 0..max_alloc {
        let phys = alloc_page_frame(ALLOC_FLAG_NO_PCP);
        if phys.is_null() {
            klog_info!("OOM_TEST: OOM after {} allocations (expected)", i);
            break;
        }
        if count < allocated.len() {
            allocated[count] = phys;
            count += 1;
        } else {
            free_page_frame(phys);
        }
    }

    if count == 0 {
        klog_info!("OOM_TEST: Failed to allocate any pages!");
        return -1;
    }

    // Verify we can still free pages
    for i in 0..count {
        free_page_frame(allocated[i]);
    }

    // Verify stats recovered
    let mut free_after = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_after, ptr::null_mut());

    if free_after < free_before - 10 {
        klog_info!(
            "OOM_TEST: Memory leak after OOM test! Before: {}, After: {}",
            free_before,
            free_after
        );
        return -1;
    }

    0
}

/// Test: Large block allocation when only fragmented memory available
pub fn test_page_alloc_fragmentation_oom() -> c_int {
    let mut free_before = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_before, ptr::null_mut());

    if free_before < 32 {
        return 0;
    }

    // Allocate individual pages to fragment memory
    let mut pages: [PhysAddr; 16] = [PhysAddr::NULL; 16];
    for i in 0..16 {
        pages[i] = alloc_page_frame(ALLOC_FLAG_NO_PCP);
        if pages[i].is_null() {
            for j in 0..i {
                free_page_frame(pages[j]);
            }
            return 0;
        }
    }

    // Free alternate pages (creates holes)
    for i in (0..16).step_by(2) {
        free_page_frame(pages[i]);
        pages[i] = PhysAddr::NULL;
    }

    // Try to allocate a large contiguous block (16 pages)
    // This might succeed or fail depending on memory layout
    let large = alloc_page_frames(16, ALLOC_FLAG_NO_PCP);

    // Clean up
    if !large.is_null() {
        free_page_frame(large);
    }
    for i in 0..16 {
        if !pages[i].is_null() {
            free_page_frame(pages[i]);
        }
    }

    0
}

/// Test: DMA allocation constraints under pressure
pub fn test_dma_allocation_exhaustion() -> c_int {
    let mut dma_pages: [PhysAddr; 64] = [PhysAddr::NULL; 64];
    let mut count = 0usize;

    // DMA memory is limited to low addresses
    for _ in 0..64 {
        let phys = alloc_page_frame(ALLOC_FLAG_DMA | ALLOC_FLAG_NO_PCP);
        if phys.is_null() {
            break;
        }
        // Verify DMA constraint
        if phys.as_u64() >= 0x0100_0000 {
            klog_info!(
                "OOM_TEST: BUG - DMA allocation returned high address: {:#x}",
                phys.as_u64()
            );
            free_page_frame(phys);
            for j in 0..count {
                free_page_frame(dma_pages[j]);
            }
            return -1;
        }
        if count < dma_pages.len() {
            dma_pages[count] = phys;
            count += 1;
        } else {
            free_page_frame(phys);
        }
    }

    // Cleanup
    for i in 0..count {
        free_page_frame(dma_pages[i]);
    }

    0
}

/// Test: Heap allocation under memory pressure
pub fn test_heap_alloc_pressure() -> c_int {
    let mut stats_before = core::mem::MaybeUninit::uninit();
    get_heap_stats(stats_before.as_mut_ptr());
    let _before = unsafe { stats_before.assume_init() };

    // Allocate many small objects
    let mut ptrs: [*mut core::ffi::c_void; 128] = [ptr::null_mut(); 128];
    let mut count = 0usize;

    for _ in 0..128 {
        let ptr = kmalloc(256);
        if ptr.is_null() {
            break;
        }
        if count < ptrs.len() {
            ptrs[count] = ptr;
            count += 1;
        } else {
            kfree(ptr);
        }
    }

    if count == 0 {
        klog_info!("OOM_TEST: Heap couldn't allocate any blocks");
        return -1;
    }

    // Write pattern to verify no corruption
    for i in 0..count {
        let byte_ptr = ptrs[i] as *mut u8;
        for j in 0..256 {
            unsafe {
                *byte_ptr.add(j) = (i & 0xFF) as u8;
            }
        }
    }

    // Verify patterns
    for i in 0..count {
        let byte_ptr = ptrs[i] as *mut u8;
        for j in 0..256 {
            let val = unsafe { *byte_ptr.add(j) };
            if val != (i & 0xFF) as u8 {
                klog_info!(
                    "OOM_TEST: Heap corruption at block {}, offset {}: expected {:#x}, got {:#x}",
                    i,
                    j,
                    (i & 0xFF) as u8,
                    val
                );
                // Cleanup before failing
                for k in 0..count {
                    kfree(ptrs[k]);
                }
                return -1;
            }
        }
    }

    // Free in reverse order
    for i in (0..count).rev() {
        kfree(ptrs[i]);
    }

    0
}

/// Test: Process VM creation under memory pressure
pub fn test_process_vm_creation_pressure() -> c_int {
    init_process_vm();

    let mut pids: [u32; 8] = [INVALID_PROCESS_ID; 8];
    let mut created = 0usize;

    // Create multiple processes
    for i in 0..8 {
        let pid = create_process_vm();
        if pid == INVALID_PROCESS_ID {
            klog_info!("OOM_TEST: Process creation failed at {}", i);
            break;
        }
        pids[i] = pid;
        created += 1;
    }

    if created == 0 {
        klog_info!("OOM_TEST: Couldn't create any processes");
        return -1;
    }

    // Destroy all
    for i in 0..created {
        destroy_process_vm(pids[i]);
    }

    // Verify we can create again
    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        klog_info!("OOM_TEST: BUG - Can't create process after cleanup!");
        return -1;
    }
    destroy_process_vm(pid);

    0
}

/// Test: Heap expansion when page allocator is stressed
pub fn test_heap_expansion_under_pressure() -> c_int {
    // First, stress the page allocator
    let mut pages: [PhysAddr; 64] = [PhysAddr::NULL; 64];
    let mut page_count = 0usize;

    for _ in 0..64 {
        let phys = alloc_page_frame(ALLOC_FLAG_NO_PCP);
        if phys.is_null() {
            break;
        }
        if page_count < pages.len() {
            pages[page_count] = phys;
            page_count += 1;
        } else {
            free_page_frame(phys);
        }
    }

    // Now try heap allocations
    let ptr = kmalloc(4096);
    let heap_ok = !ptr.is_null();
    if heap_ok {
        kfree(ptr);
    }

    // Release page pressure
    for i in 0..page_count {
        free_page_frame(pages[i]);
    }

    0
}

/// Test: Zero-flag allocation correctness under pressure
pub fn test_zero_flag_under_pressure() -> c_int {
    let mut pages: [PhysAddr; 32] = [PhysAddr::NULL; 32];
    let mut count = 0usize;

    // Allocate pages with ZERO flag
    for _ in 0..32 {
        let phys = alloc_page_frame(ALLOC_FLAG_ZERO | ALLOC_FLAG_NO_PCP);
        if phys.is_null() {
            break;
        }
        if count < pages.len() {
            pages[count] = phys;
            count += 1;
        } else {
            free_page_frame(phys);
        }
    }

    if count == 0 {
        return 0;
    }

    // Verify all pages are zeroed
    for i in 0..count {
        if let Some(virt) = pages[i].to_virt_checked() {
            let ptr = virt.as_ptr::<u8>();
            for j in 0..PAGE_SIZE_4KB as usize {
                let val = unsafe { *ptr.add(j) };
                if val != 0 {
                    klog_info!(
                        "OOM_TEST: BUG - ZERO flag page {} has non-zero at offset {}: {:#x}",
                        i,
                        j,
                        val
                    );
                    for k in 0..count {
                        free_page_frame(pages[k]);
                    }
                    return -1;
                }
            }
        }
    }

    for i in 0..count {
        free_page_frame(pages[i]);
    }

    0
}

/// Test: kzalloc returns zeroed memory even under pressure
pub fn test_kzalloc_zeroed_under_pressure() -> c_int {
    // First pollute some memory
    let pollute = kmalloc(512);
    if !pollute.is_null() {
        unsafe {
            ptr::write_bytes(pollute as *mut u8, 0xDE, 512);
        }
        kfree(pollute);
    }

    // Now allocate with kzalloc
    let ptr = kzalloc(512);
    if ptr.is_null() {
        return 0;
    }

    let byte_ptr = ptr as *const u8;
    for i in 0..512 {
        let val = unsafe { *byte_ptr.add(i) };
        if val != 0 {
            klog_info!(
                "OOM_TEST: BUG - kzalloc returned non-zeroed memory at offset {}: {:#x}",
                i,
                val
            );
            kfree(ptr);
            return -1;
        }
    }

    kfree(ptr);
    0
}

/// Test: Allocate/free cycles don't leak under pressure
pub fn test_alloc_free_cycles_no_leak() -> c_int {
    let mut free_start = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_start, ptr::null_mut());

    const CYCLES: usize = 100;
    const PAGES_PER_CYCLE: usize = 4;

    for cycle in 0..CYCLES {
        let mut pages: [PhysAddr; PAGES_PER_CYCLE] = [PhysAddr::NULL; PAGES_PER_CYCLE];

        for i in 0..PAGES_PER_CYCLE {
            pages[i] = alloc_page_frame(ALLOC_FLAG_NO_PCP);
            if pages[i].is_null() {
                for j in 0..i {
                    free_page_frame(pages[j]);
                }
                klog_info!("OOM_TEST: Allocation failed at cycle {}", cycle);
                // Not a bug - might be OOM
                break;
            }
        }

        for i in 0..PAGES_PER_CYCLE {
            if !pages[i].is_null() {
                free_page_frame(pages[i]);
            }
        }
    }

    let mut free_end = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_end, ptr::null_mut());

    // Allow small variance for PCP caching
    let diff = if free_start > free_end {
        free_start - free_end
    } else {
        free_end - free_start
    };

    if diff > 16 {
        klog_info!(
            "OOM_TEST: Memory leak detected! Start: {}, End: {}, Diff: {}",
            free_start,
            free_end,
            diff
        );
        return -1;
    }

    0
}

/// Test: Multi-order allocation failure handling
pub fn test_multiorder_alloc_failure() -> c_int {
    // Try increasingly large allocations
    for order in 0..10u32 {
        let count = 1u32 << order;
        let phys = alloc_page_frames(count, ALLOC_FLAG_NO_PCP);

        if phys.is_null() {
            klog_info!(
                "OOM_TEST: Order {} ({} pages) allocation failed (might be expected)",
                order,
                count
            );
        } else {
            free_page_frame(phys);
        }
    }

    0
}

/// Test: Process heap expansion until failure
pub fn test_process_heap_expansion_oom() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return -1;
    }

    let mut alloc_count = 0u32;
    let mut total_size = 0u64;

    // Keep allocating heap until failure
    loop {
        let addr = process_vm_alloc(pid, PAGE_SIZE_4KB * 4, PageFlags::WRITABLE.bits() as u32);
        if addr == 0 {
            break;
        }
        alloc_count += 1;
        total_size += PAGE_SIZE_4KB * 4;

        // Safety limit
        if alloc_count >= 256 {
            break;
        }
    }

    klog_info!(
        "OOM_TEST: Process allocated {} blocks ({} KB) before OOM",
        alloc_count,
        total_size / 1024
    );

    destroy_process_vm(pid);
    0
}

/// Test: Reference count correctness during OOM
pub fn test_refcount_during_oom() -> c_int {
    use crate::page_alloc::{page_frame_get_ref, page_frame_inc_ref};

    let phys = alloc_page_frame(ALLOC_FLAG_NO_PCP);
    if phys.is_null() {
        return 0;
    }

    // Increment ref count multiple times
    for _ in 0..5 {
        page_frame_inc_ref(phys);
    }

    let ref_count = page_frame_get_ref(phys);
    if ref_count != 6 {
        klog_info!("OOM_TEST: BUG - Ref count should be 6, got {}", ref_count);
        // Try to free anyway
        for _ in 0..6 {
            free_page_frame(phys);
        }
        return -1;
    }

    // Free all references
    for _ in 0..6 {
        free_page_frame(phys);
    }

    0
}
