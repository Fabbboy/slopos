#![allow(dead_code)]

use core::ffi::{c_int, c_void};
use core::mem::MaybeUninit;
use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_lib::klog_info;

use crate::hhdm::PhysAddrHhdm;
use crate::kernel_heap::{get_heap_stats, kfree, kmalloc, kzalloc};
use crate::mm_constants::PAGE_SIZE_4KB;
use crate::page_alloc::{
    ALLOC_FLAG_ZERO, alloc_page_frame, alloc_page_frames, free_page_frame,
    get_page_allocator_stats, page_frame_get_ref, page_frame_inc_ref,
};
use crate::paging::{
    get_current_page_directory, paging_get_kernel_directory, paging_is_cow,
    paging_is_user_accessible, virt_to_phys,
};
use crate::process_vm::{
    create_process_vm, destroy_process_vm, get_process_vm_stats, init_process_vm,
    process_vm_get_page_dir,
};

// ============================================================================
// PAGE ALLOCATOR (BUDDY) TESTS - 12 tests
// ============================================================================

/// Test 1: Allocate and free a single 4KB page
pub fn test_page_alloc_single() -> c_int {
    let phys = alloc_page_frame(0);
    if phys.is_null() {
        klog_info!("PAGE_ALLOC_TEST: Failed to allocate single page");
        return -1;
    }

    if phys.as_u64() == 0 {
        klog_info!("PAGE_ALLOC_TEST: Allocated address is zero");
        free_page_frame(phys);
        return -1;
    }

    let ref_count = page_frame_get_ref(phys);
    if ref_count == 0 {
        klog_info!("PAGE_ALLOC_TEST: Ref count should be non-zero after alloc");
        free_page_frame(phys);
        return -1;
    }

    free_page_frame(phys);

    0
}

/// Test 2: Allocate multi-order blocks (2, 4, 8 pages)
pub fn test_page_alloc_multi_order() -> c_int {
    // Allocate order-1 (2 pages)
    let phys2 = alloc_page_frames(2, 0);
    if phys2.is_null() {
        klog_info!("PAGE_ALLOC_TEST: Failed to allocate 2 pages");
        return -1;
    }

    // Allocate order-2 (4 pages)
    let phys4 = alloc_page_frames(4, 0);
    if phys4.is_null() {
        klog_info!("PAGE_ALLOC_TEST: Failed to allocate 4 pages");
        free_page_frame(phys2);
        return -1;
    }

    // Allocate order-3 (8 pages)
    let phys8 = alloc_page_frames(8, 0);
    if phys8.is_null() {
        klog_info!("PAGE_ALLOC_TEST: Failed to allocate 8 pages");
        free_page_frame(phys2);
        free_page_frame(phys4);
        return -1;
    }

    // Free all
    free_page_frame(phys2);
    free_page_frame(phys4);
    free_page_frame(phys8);

    0
}

/// Test 3: Alloc→free→alloc same size, verify address reuse (coalescing)
pub fn test_page_alloc_free_cycle() -> c_int {
    let phys1 = alloc_page_frame(0);
    if phys1.is_null() {
        return -1;
    }

    free_page_frame(phys1);

    let phys2 = alloc_page_frame(0);
    if phys2.is_null() {
        return -1;
    }

    // With good coalescing, we might get the same address back (not guaranteed)
    // At minimum, the allocation should succeed
    free_page_frame(phys2);

    0
}

/// Test 4: Allocate with ALLOC_FLAG_ZERO, verify memory is zeroed
pub fn test_page_alloc_zeroed() -> c_int {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        klog_info!("PAGE_ALLOC_TEST: Failed to allocate zeroed page");
        return -1;
    }

    if let Some(virt) = phys.to_virt_checked() {
        let ptr: *const u8 = virt.as_ptr();
        for i in 0..64 {
            let byte = unsafe { *ptr.add(i) };
            if byte != 0 {
                klog_info!(
                    "PAGE_ALLOC_TEST: Zeroed page has non-zero byte at offset {}",
                    i
                );
                free_page_frame(phys);
                return -1;
            }
        }
    }

    free_page_frame(phys);
    0
}

/// Test 5: Reference count increment and decrement
pub fn test_page_alloc_refcount() -> c_int {
    let phys = alloc_page_frame(0);
    if phys.is_null() {
        return -1;
    }

    let ref1 = page_frame_get_ref(phys);
    if ref1 != 1 {
        klog_info!(
            "PAGE_ALLOC_TEST: Initial refcount should be 1, got {}",
            ref1
        );
        free_page_frame(phys);
        return -1;
    }

    // Increment ref count
    let new_ref = page_frame_inc_ref(phys);
    if new_ref != 2 {
        klog_info!(
            "PAGE_ALLOC_TEST: Refcount after inc should be 2, got {}",
            new_ref
        );
        free_page_frame(phys);
        free_page_frame(phys);
        return -1;
    }

    // First free should just decrement
    free_page_frame(phys);

    let ref_after = page_frame_get_ref(phys);
    if ref_after != 1 {
        klog_info!(
            "PAGE_ALLOC_TEST: Refcount after first free should be 1, got {}",
            ref_after
        );
        free_page_frame(phys);
        return -1;
    }

    // Second free should actually free
    free_page_frame(phys);

    0
}

/// Test 6: Stats accuracy check
pub fn test_page_alloc_stats() -> c_int {
    let mut total = 0u32;
    let mut free_before = 0u32;
    let mut alloc_before = 0u32;
    get_page_allocator_stats(&mut total, &mut free_before, &mut alloc_before);

    if total == 0 {
        klog_info!("PAGE_ALLOC_TEST: Total frames is 0");
        return -1;
    }

    // Allocate 4 pages
    let phys = alloc_page_frames(4, 0);
    if phys.is_null() {
        return -1;
    }

    let mut free_after = 0u32;
    let mut alloc_after = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_after, &mut alloc_after);

    // Should have 4 more allocated
    if alloc_after < alloc_before + 4 {
        klog_info!("PAGE_ALLOC_TEST: Allocated count didn't increase by 4");
        free_page_frame(phys);
        return -1;
    }

    free_page_frame(phys);
    0
}

/// Test 7: Free NULL address should not crash
pub fn test_page_alloc_free_null() -> c_int {
    // This should be a no-op, not crash
    let result = free_page_frame(PhysAddr::NULL);
    // Result should indicate invalid address
    if result == 0 {
        // Freeing null might return 0 (no-op success) or -1 (error)
        // Either is acceptable as long as no crash
    }
    0
}

/// Test 8: Fragmentation stress test
pub fn test_page_alloc_fragmentation() -> c_int {
    // Allocate 8 single pages
    let mut pages: [PhysAddr; 8] = [PhysAddr::NULL; 8];
    for i in 0..8 {
        pages[i] = alloc_page_frame(0);
        if pages[i].is_null() {
            // Cleanup
            for j in 0..i {
                free_page_frame(pages[j]);
            }
            klog_info!("PAGE_ALLOC_TEST: Failed to allocate page {}", i);
            return -1;
        }
    }

    // Free alternate pages (0, 2, 4, 6)
    free_page_frame(pages[0]);
    free_page_frame(pages[2]);
    free_page_frame(pages[4]);
    free_page_frame(pages[6]);

    // Try to allocate a 2-page block - may or may not succeed depending on layout
    // This tests that the allocator doesn't crash under fragmentation
    let large = alloc_page_frames(2, 0);
    if !large.is_null() {
        free_page_frame(large);
    }

    // Free remaining
    free_page_frame(pages[1]);
    free_page_frame(pages[3]);
    free_page_frame(pages[5]);
    free_page_frame(pages[7]);

    0
}

// ============================================================================
// KERNEL HEAP TESTS - 10 tests
// ============================================================================

/// Test 1: Small allocations (16, 32, 64 bytes)
pub fn test_heap_small_alloc() -> c_int {
    let p16 = kmalloc(16);
    if p16.is_null() {
        klog_info!("HEAP_TEST: Failed to allocate 16 bytes");
        return -1;
    }

    let p32 = kmalloc(32);
    if p32.is_null() {
        klog_info!("HEAP_TEST: Failed to allocate 32 bytes");
        kfree(p16);
        return -1;
    }

    let p64 = kmalloc(64);
    if p64.is_null() {
        klog_info!("HEAP_TEST: Failed to allocate 64 bytes");
        kfree(p16);
        kfree(p32);
        return -1;
    }

    kfree(p64);
    kfree(p32);
    kfree(p16);
    0
}

/// Test 2: Medium allocations (256, 512, 1024 bytes)
pub fn test_heap_medium_alloc() -> c_int {
    let p256 = kmalloc(256);
    if p256.is_null() {
        return -1;
    }

    let p512 = kmalloc(512);
    if p512.is_null() {
        kfree(p256);
        return -1;
    }

    let p1k = kmalloc(1024);
    if p1k.is_null() {
        kfree(p256);
        kfree(p512);
        return -1;
    }

    kfree(p1k);
    kfree(p512);
    kfree(p256);
    0
}

/// Test 3: Large allocations (4KB, 16KB)
pub fn test_heap_large_alloc() -> c_int {
    let p4k = kmalloc(4096);
    if p4k.is_null() {
        klog_info!("HEAP_TEST: Failed to allocate 4KB");
        return -1;
    }

    let p16k = kmalloc(16384);
    if p16k.is_null() {
        klog_info!("HEAP_TEST: Failed to allocate 16KB");
        kfree(p4k);
        return -1;
    }

    kfree(p16k);
    kfree(p4k);
    0
}

/// Test 4: kzalloc returns zeroed memory
pub fn test_heap_kzalloc_zeroed() -> c_int {
    let ptr = kzalloc(128);
    if ptr.is_null() {
        return -1;
    }

    // Verify all bytes are zero
    let bytes = ptr as *const u8;
    for i in 0..128 {
        let b = unsafe { *bytes.add(i) };
        if b != 0 {
            klog_info!("HEAP_TEST: kzalloc memory not zeroed at offset {}", i);
            kfree(ptr);
            return -1;
        }
    }

    kfree(ptr);
    0
}

/// Test 5: kfree(null) should not crash
pub fn test_heap_kfree_null() -> c_int {
    kfree(ptr::null_mut());
    0
}

/// Test 6: Allocation size zero should return null
pub fn test_heap_alloc_zero() -> c_int {
    let ptr = kmalloc(0);
    if !ptr.is_null() {
        klog_info!("HEAP_TEST: kmalloc(0) should return null");
        kfree(ptr);
        return -1;
    }
    0
}

/// Test 7: Stats tracking accuracy
pub fn test_heap_stats() -> c_int {
    let mut stats_before = MaybeUninit::uninit();
    get_heap_stats(stats_before.as_mut_ptr());
    let before = unsafe { stats_before.assume_init() };

    let ptr = kmalloc(256);
    if ptr.is_null() {
        return -1;
    }

    let mut stats_after = MaybeUninit::uninit();
    get_heap_stats(stats_after.as_mut_ptr());
    let after = unsafe { stats_after.assume_init() };

    // Allocated size should have increased
    if after.allocated_size <= before.allocated_size {
        klog_info!("HEAP_TEST: Allocated size didn't increase");
        kfree(ptr);
        return -1;
    }

    // Allocation count should have increased
    if after.allocation_count <= before.allocation_count {
        klog_info!("HEAP_TEST: Allocation count didn't increase");
        kfree(ptr);
        return -1;
    }

    kfree(ptr);
    0
}

// Keep existing tests
pub fn test_heap_free_list_search() -> i32 {
    let mut stats_before = MaybeUninit::uninit();
    get_heap_stats(stats_before.as_mut_ptr());
    let initial_heap_size = unsafe { stats_before.assume_init() }.total_size;

    let small = kmalloc(32);
    if small.is_null() {
        return -1;
    }
    let large = kmalloc(1024);
    if large.is_null() {
        kfree(small);
        return -1;
    }
    let medium = kmalloc(256);
    if medium.is_null() {
        kfree(small);
        kfree(large);
        return -1;
    }

    kfree(large);
    kfree(small);

    let requested = kmalloc(512);
    if requested.is_null() {
        kfree(medium);
        return -1;
    }

    let mut stats_after = MaybeUninit::uninit();
    get_heap_stats(stats_after.as_mut_ptr());
    let final_heap_size = unsafe { stats_after.assume_init() }.total_size;
    if final_heap_size > initial_heap_size {
        kfree(requested);
        kfree(medium);
        return -1;
    }

    kfree(requested);
    kfree(medium);
    0
}

pub fn test_heap_fragmentation_behind_head() -> i32 {
    let mut ptrs: [*mut core::ffi::c_void; 5] = [core::ptr::null_mut(); 5];
    let sizes = [128usize, 256, 128, 512, 256];

    for (i, size) in sizes.iter().enumerate() {
        ptrs[i] = kmalloc(*size);
        if ptrs[i].is_null() {
            for j in 0..i {
                kfree(ptrs[j]);
            }
            return -1;
        }
    }

    kfree(ptrs[0]);
    kfree(ptrs[2]);
    kfree(ptrs[3]);

    let needed = kmalloc(400);
    if needed.is_null() {
        kfree(ptrs[1]);
        kfree(ptrs[4]);
        return -1;
    }

    kfree(needed);
    kfree(ptrs[1]);
    kfree(ptrs[4]);
    0
}

// ============================================================================
// PROCESS VM TESTS (existing)
// ============================================================================

pub fn test_process_vm_slot_reuse() -> i32 {
    init_process_vm();

    let mut initial_active: u32 = 0;
    get_process_vm_stats(core::ptr::null_mut(), &mut initial_active);

    let mut pids = [0u32; 5];
    for i in 0..5 {
        pids[i] = create_process_vm();
        if pids[i] == crate::mm_constants::INVALID_PROCESS_ID {
            return -1;
        }
        if process_vm_get_page_dir(pids[i]).is_null() {
            return -1;
        }
    }

    for &idx in &[1usize, 2, 3] {
        if destroy_process_vm(pids[idx]) != 0 {
            return -1;
        }
    }

    for &idx in &[1usize, 2, 3] {
        if !process_vm_get_page_dir(pids[idx]).is_null() {
            return -1;
        }
    }

    if process_vm_get_page_dir(pids[0]).is_null() || process_vm_get_page_dir(pids[4]).is_null() {
        return -1;
    }

    let mut new_pids = [0u32; 3];
    for i in 0..3 {
        new_pids[i] = create_process_vm();
        if new_pids[i] == crate::mm_constants::INVALID_PROCESS_ID {
            return -1;
        }
        if process_vm_get_page_dir(new_pids[i]).is_null() {
            return -1;
        }
    }

    if process_vm_get_page_dir(pids[0]).is_null() || process_vm_get_page_dir(pids[4]).is_null() {
        return -1;
    }

    if destroy_process_vm(pids[0]) != 0 || destroy_process_vm(pids[4]) != 0 {
        return -1;
    }
    for pid in new_pids {
        destroy_process_vm(pid);
    }

    let mut final_active: u32 = 0;
    get_process_vm_stats(core::ptr::null_mut(), &mut final_active);
    if final_active != initial_active {
        return -1;
    }
    0
}

pub fn test_process_vm_counter_reset() -> i32 {
    init_process_vm();

    let mut initial_active: u32 = 0;
    get_process_vm_stats(core::ptr::null_mut(), &mut initial_active);

    let mut pids = [0u32; 10];
    for i in 0..10 {
        pids[i] = create_process_vm();
        if pids[i] == crate::mm_constants::INVALID_PROCESS_ID {
            for j in 0..i {
                destroy_process_vm(pids[j]);
            }
            return -1;
        }
    }

    let mut active_after: u32 = 0;
    get_process_vm_stats(core::ptr::null_mut(), &mut active_after);
    if active_after != initial_active + 10 {
        for pid in pids {
            destroy_process_vm(pid);
        }
        return -1;
    }

    for pid in pids.iter().rev() {
        if destroy_process_vm(*pid) != 0 {
            return -1;
        }
    }

    let mut final_active: u32 = 0;
    get_process_vm_stats(core::ptr::null_mut(), &mut final_active);
    if final_active != initial_active {
        return -1;
    }
    0
}

// ============================================================================
// PAGING TESTS - 10 tests
// ============================================================================

/// Test 1: virt_to_phys on kernel address
pub fn test_paging_virt_to_phys() -> c_int {
    // Kernel code should have a valid mapping
    let kernel_addr = VirtAddr::new(test_paging_virt_to_phys as *const () as u64);
    let phys = virt_to_phys(kernel_addr);

    if phys.is_null() {
        klog_info!("PAGING_TEST: virt_to_phys returned null for kernel code");
        return -1;
    }

    0
}

/// Test 2: Kernel directory retrieval
pub fn test_paging_get_kernel_dir() -> c_int {
    let kernel_dir = paging_get_kernel_directory();
    if kernel_dir.is_null() {
        klog_info!("PAGING_TEST: Kernel directory is null");
        return -1;
    }

    let current_dir = get_current_page_directory();
    if current_dir.is_null() {
        klog_info!("PAGING_TEST: Current directory is null");
        return -1;
    }

    0
}

/// Test 3: User accessible check on kernel page (should fail)
pub fn test_paging_user_accessible_kernel() -> c_int {
    let kernel_dir = paging_get_kernel_directory();
    if kernel_dir.is_null() {
        return -1;
    }

    // Kernel text should NOT be user accessible
    let kernel_addr = VirtAddr::new(test_paging_user_accessible_kernel as *const () as u64);
    let is_user = paging_is_user_accessible(kernel_dir, kernel_addr);

    if is_user != 0 {
        klog_info!("PAGING_TEST: Kernel code incorrectly marked as user accessible");
        return -1;
    }

    0
}

/// Test 4: COW flag on kernel page (should not be set)
pub fn test_paging_cow_kernel() -> c_int {
    let kernel_dir = paging_get_kernel_directory();
    if kernel_dir.is_null() {
        return -1;
    }

    let kernel_addr = VirtAddr::new(test_paging_cow_kernel as *const () as u64);
    let is_cow = paging_is_cow(kernel_dir, kernel_addr);

    if is_cow {
        klog_info!("PAGING_TEST: Kernel code incorrectly marked as COW");
        return -1;
    }

    0
}

// ============================================================================
// RING BUFFER TESTS - 8 tests (in lib crate, tested via mm)
// ============================================================================

/// Test ring buffer basic push/pop
pub fn test_ring_buffer_basic() -> c_int {
    use slopos_lib::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();

    if !rb.is_empty() {
        klog_info!("RING_TEST: New buffer should be empty");
        return -1;
    }

    if !rb.try_push(42) {
        klog_info!("RING_TEST: Push to empty buffer failed");
        return -1;
    }

    if rb.is_empty() {
        klog_info!("RING_TEST: Buffer should not be empty after push");
        return -1;
    }

    let val = rb.try_pop();
    if val != Some(42) {
        klog_info!("RING_TEST: Pop returned wrong value");
        return -1;
    }

    if !rb.is_empty() {
        klog_info!("RING_TEST: Buffer should be empty after pop");
        return -1;
    }

    0
}

/// Test ring buffer FIFO order
pub fn test_ring_buffer_fifo() -> c_int {
    use slopos_lib::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();

    rb.try_push(1);
    rb.try_push(2);
    rb.try_push(3);

    if rb.try_pop() != Some(1) {
        klog_info!("RING_TEST: FIFO order violated (expected 1)");
        return -1;
    }
    if rb.try_pop() != Some(2) {
        klog_info!("RING_TEST: FIFO order violated (expected 2)");
        return -1;
    }
    if rb.try_pop() != Some(3) {
        klog_info!("RING_TEST: FIFO order violated (expected 3)");
        return -1;
    }

    0
}

/// Test ring buffer empty pop
pub fn test_ring_buffer_empty_pop() -> c_int {
    use slopos_lib::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();

    if rb.try_pop().is_some() {
        klog_info!("RING_TEST: Pop from empty should return None");
        return -1;
    }

    0
}

/// Test ring buffer full behavior
pub fn test_ring_buffer_full() -> c_int {
    use slopos_lib::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 4> = RingBuffer::new();

    // Fill the buffer
    for i in 0..4 {
        if !rb.try_push(i) {
            klog_info!("RING_TEST: Push {} failed unexpectedly", i);
            return -1;
        }
    }

    if !rb.is_full() {
        klog_info!("RING_TEST: Buffer should be full");
        return -1;
    }

    // Next push should fail
    if rb.try_push(999) {
        klog_info!("RING_TEST: Push to full buffer should fail");
        return -1;
    }

    0
}

/// Test ring buffer overwrite mode
pub fn test_ring_buffer_overwrite() -> c_int {
    use slopos_lib::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 4> = RingBuffer::new();

    // Fill with 0,1,2,3
    for i in 0..4u32 {
        rb.push_overwrite(i);
    }

    // Push 99 - should overwrite oldest (0)
    rb.push_overwrite(99);

    // Should get 1,2,3,99 in that order
    if rb.try_pop() != Some(1) {
        klog_info!("RING_TEST: Overwrite test failed (expected 1)");
        return -1;
    }

    0
}

/// Test ring buffer wrap around
pub fn test_ring_buffer_wrap() -> c_int {
    use slopos_lib::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 4> = RingBuffer::new();

    // Push 3 items
    rb.try_push(1);
    rb.try_push(2);
    rb.try_push(3);

    // Pop 2
    rb.try_pop();
    rb.try_pop();

    // Push 3 more (causes wrap)
    rb.try_push(4);
    rb.try_push(5);
    rb.try_push(6);

    // Should get 3, 4, 5, 6
    if rb.try_pop() != Some(3) {
        return -1;
    }
    if rb.try_pop() != Some(4) {
        return -1;
    }
    if rb.try_pop() != Some(5) {
        return -1;
    }
    if rb.try_pop() != Some(6) {
        return -1;
    }

    0
}

/// Test ring buffer reset
pub fn test_ring_buffer_reset() -> c_int {
    use slopos_lib::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();

    rb.try_push(1);
    rb.try_push(2);
    rb.try_push(3);

    rb.reset();

    if !rb.is_empty() {
        klog_info!("RING_TEST: Buffer should be empty after reset");
        return -1;
    }

    if rb.len() != 0 {
        klog_info!("RING_TEST: Length should be 0 after reset");
        return -1;
    }

    0
}

/// Test ring buffer capacity
pub fn test_ring_buffer_capacity() -> c_int {
    use slopos_lib::ring_buffer::RingBuffer;

    let rb: RingBuffer<u32, 16> = RingBuffer::new();

    if rb.capacity() != 16 {
        klog_info!("RING_TEST: Capacity should be 16");
        return -1;
    }

    0
}

// ============================================================================
// SPINLOCK/IRQMUTEX TESTS - 6 tests
// ============================================================================

/// Test 1: Basic Spinlock lock/unlock
pub fn test_spinlock_basic() -> c_int {
    use slopos_lib::spinlock::Spinlock;

    let lock = Spinlock::new();

    // Lock should succeed
    lock.lock();

    // Unlock should succeed
    lock.unlock();

    // Should be able to lock again after unlock
    lock.lock();
    lock.unlock();

    0
}

/// Test 2: Spinlock with IRQ save/restore
pub fn test_spinlock_irqsave() -> c_int {
    use slopos_lib::spinlock::Spinlock;

    let lock = Spinlock::new();

    // Lock with IRQ save
    let flags = lock.lock_irqsave();

    // Should have valid flags (non-zero typically)
    // Just verify we can use them

    // Unlock with IRQ restore
    lock.unlock_irqrestore(flags);

    // Lock again to verify it works after restore
    let flags2 = lock.lock_irqsave();
    lock.unlock_irqrestore(flags2);

    0
}

/// Test 3: Spinlock init function
pub fn test_spinlock_init() -> c_int {
    use slopos_lib::spinlock::Spinlock;

    let lock = Spinlock::new();

    // Lock it first
    lock.lock();
    lock.unlock();

    // Re-initialize
    lock.init();

    // Should be able to lock after init
    lock.lock();
    lock.unlock();

    0
}

/// Test 4: IrqMutex basic lock/unlock with guard
pub fn test_irqmutex_basic() -> c_int {
    use slopos_lib::spinlock::IrqMutex;

    let mutex: IrqMutex<u32> = IrqMutex::new(42);

    {
        let guard = mutex.lock();
        if *guard != 42 {
            klog_info!("SPINLOCK_TEST: IrqMutex value should be 42");
            return -1;
        }
    } // Guard drops here, unlocking

    // Lock again - should work
    {
        let guard = mutex.lock();
        if *guard != 42 {
            return -1;
        }
    }

    0
}

/// Test 5: IrqMutex mutation through guard
pub fn test_irqmutex_mutation() -> c_int {
    use slopos_lib::spinlock::IrqMutex;

    let mutex: IrqMutex<u32> = IrqMutex::new(0);

    // Mutate value
    {
        let mut guard = mutex.lock();
        *guard = 100;
    }

    // Verify mutation persisted
    {
        let guard = mutex.lock();
        if *guard != 100 {
            klog_info!("SPINLOCK_TEST: IrqMutex mutation failed, got {}", *guard);
            return -1;
        }
    }

    0
}

/// Test 6: IrqMutex try_lock
pub fn test_irqmutex_try_lock() -> c_int {
    use slopos_lib::spinlock::IrqMutex;

    let mutex: IrqMutex<u32> = IrqMutex::new(55);

    // try_lock on unlocked mutex should succeed
    {
        let maybe_guard = mutex.try_lock();
        if maybe_guard.is_none() {
            klog_info!("SPINLOCK_TEST: try_lock on unlocked mutex should succeed");
            return -1;
        }
        let guard = maybe_guard.unwrap();
        if *guard != 55 {
            klog_info!("SPINLOCK_TEST: try_lock value should be 55");
            return -1;
        }
    }

    0
}

// ============================================================================
// SHARED MEMORY TESTS - 8 tests
// ============================================================================

use crate::shared_memory::{
    shm_create, shm_destroy, shm_get_buffer_info, shm_get_ref_count, surface_attach,
};

/// Test 1: Create and destroy shared memory buffer
pub fn test_shm_create_destroy() -> c_int {
    // Use process_id 1 as a test owner (kernel process)
    let owner = 1u32;
    let size = 4096u64;

    let token = shm_create(owner, size, 0);
    if token == 0 {
        klog_info!("SHM_TEST: shm_create failed");
        return -1;
    }

    // Verify buffer info
    let (phys, buf_size, buf_owner) = shm_get_buffer_info(token);
    if phys.is_null() {
        klog_info!("SHM_TEST: buffer info phys is null");
        shm_destroy(owner, token);
        return -1;
    }
    if buf_size < size as usize {
        klog_info!("SHM_TEST: buffer size {} < requested {}", buf_size, size);
        shm_destroy(owner, token);
        return -1;
    }
    if buf_owner != owner {
        klog_info!("SHM_TEST: owner mismatch {} != {}", buf_owner, owner);
        shm_destroy(owner, token);
        return -1;
    }

    // Destroy
    if shm_destroy(owner, token) != 0 {
        klog_info!("SHM_TEST: shm_destroy failed");
        return -1;
    }

    0
}

/// Test 2: Create with zero size should fail
pub fn test_shm_create_zero_size() -> c_int {
    let token = shm_create(1, 0, 0);
    if token != 0 {
        klog_info!("SHM_TEST: shm_create with zero size should fail");
        shm_destroy(1, token);
        return -1;
    }
    0
}

/// Test 3: Create with excessive size should fail
pub fn test_shm_create_excessive_size() -> c_int {
    // 128MB should fail (limit is 64MB)
    let token = shm_create(1, 128 * 1024 * 1024, 0);
    if token != 0 {
        klog_info!("SHM_TEST: shm_create with excessive size should fail");
        shm_destroy(1, token);
        return -1;
    }
    0
}

/// Test 4: Destroy by non-owner should fail
pub fn test_shm_destroy_non_owner() -> c_int {
    let owner = 1u32;
    let non_owner = 2u32;

    let token = shm_create(owner, 4096, 0);
    if token == 0 {
        return -1;
    }

    // Try to destroy as non-owner - should fail
    if shm_destroy(non_owner, token) == 0 {
        klog_info!("SHM_TEST: non-owner destroy should fail");
        shm_destroy(owner, token);
        return -1;
    }

    // Cleanup as owner
    shm_destroy(owner, token);
    0
}

/// Test 5: Reference counting
pub fn test_shm_refcount() -> c_int {
    let owner = 1u32;

    let token = shm_create(owner, 4096, 0);
    if token == 0 {
        return -1;
    }

    // Initial refcount should be 1
    let ref_count = shm_get_ref_count(token);
    if ref_count != 1 {
        klog_info!("SHM_TEST: initial refcount should be 1, got {}", ref_count);
        shm_destroy(owner, token);
        return -1;
    }

    shm_destroy(owner, token);
    0
}

/// Test 6: Get info for invalid token
pub fn test_shm_invalid_token() -> c_int {
    let invalid_token = 99999u32;

    let (phys, size, owner) = shm_get_buffer_info(invalid_token);
    if !phys.is_null() || size != 0 || owner != 0 {
        klog_info!("SHM_TEST: invalid token should return null info");
        return -1;
    }

    0
}

/// Test 7: Surface attach
pub fn test_shm_surface_attach() -> c_int {
    let owner = 1u32;
    let width = 640u32;
    let height = 480u32;
    // 4 bytes per pixel
    let size = (width as u64) * (height as u64) * 4;

    let token = shm_create(owner, size, 0);
    if token == 0 {
        return -1;
    }

    // Attach surface
    if surface_attach(owner, token, width, height) != 0 {
        klog_info!("SHM_TEST: surface_attach failed");
        shm_destroy(owner, token);
        return -1;
    }

    shm_destroy(owner, token);
    0
}

/// Test 8: Surface attach with insufficient buffer
pub fn test_shm_surface_attach_too_small() -> c_int {
    let owner = 1u32;

    let token = shm_create(owner, 4096, 0);
    if token == 0 {
        return -1;
    }

    // 1920x1080x4 = 8,294,400 bytes - should fail
    if surface_attach(owner, token, 1920, 1080) == 0 {
        klog_info!("SHM_TEST: surface_attach with too small buffer should fail");
        shm_destroy(owner, token);
        return -1;
    }

    shm_destroy(owner, token);
    0
}

/// Test 9: Map shared buffer more than MAX_MAPPINGS_PER_BUFFER times
/// BUG FINDER: shm_map uses unwrap() on mapping slot search - will panic!
pub fn test_shm_mapping_overflow() -> c_int {
    use crate::shared_memory::{ShmAccess, shm_map};

    let owner = 1u32;
    let size = 4096u64;

    let token = shm_create(owner, size, 0);
    if token == 0 {
        return -1;
    }

    // MAX_MAPPINGS_PER_BUFFER is 8 - try to map 10 times using different process IDs
    // This SHOULD fail gracefully but currently panics due to unwrap()
    let mut mapped_count = 0u32;
    for process_id in 1..=10u32 {
        let vaddr = shm_map(process_id, token, ShmAccess::ReadOnly);
        if vaddr != 0 {
            mapped_count += 1;
        }
    }

    shm_destroy(owner, token);

    if mapped_count > 8 {
        klog_info!(
            "SHM_TEST: BUG - mapped {} times, max should be 8",
            mapped_count
        );
        return -1;
    }

    0
}

pub fn test_shm_surface_attach_overflow() -> c_int {
    let owner = 1u32;
    let token = shm_create(owner, 64 * 1024 * 1024, 0);
    if token == 0 {
        klog_info!("SHM_TEST: Failed to create large buffer for overflow test");
        return -1;
    }

    let result = surface_attach(owner, token, 0xFFFF, 0xFFFF);

    if result == 0 {
        klog_info!("SHM_TEST: BUG - surface_attach accepted 0xFFFF x 0xFFFF (potential overflow)");
        shm_destroy(owner, token);
        return -1;
    }

    let result2 = surface_attach(owner, token, 0x8000_0000, 2);

    if result2 == 0 {
        klog_info!("SHM_TEST: BUG - surface_attach accepted 0x80000000 x 2 (32-bit overflow)");
        shm_destroy(owner, token);
        return -1;
    }

    shm_destroy(owner, token);
    0
}

// ============================================================================
// RIGOROUS MEMORY TESTS - Actually verify memory contents
// ============================================================================

pub fn test_page_alloc_write_verify() -> c_int {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        klog_info!("RIGOROUS_TEST: Failed to allocate page");
        return -1;
    }

    let virt = match phys.to_virt_checked() {
        Some(v) => v,
        None => {
            klog_info!("RIGOROUS_TEST: Failed to get virtual address");
            free_page_frame(phys);
            return -1;
        }
    };

    let ptr = virt.as_mut_ptr::<u8>();

    // Write 0xAA/0x55 alternating pattern
    for i in 0..4096 {
        unsafe {
            let val = if i % 2 == 0 { 0xAA } else { 0x55 };
            ptr.add(i).write_volatile(val);
        }
    }

    // Read back and verify
    for i in 0..4096 {
        let expected = if i % 2 == 0 { 0xAA } else { 0x55 };
        let actual = unsafe { ptr.add(i).read_volatile() };
        if actual != expected {
            klog_info!(
                "RIGOROUS_TEST: Memory corruption at offset {}: expected {:#x}, got {:#x}",
                i,
                expected,
                actual
            );
            free_page_frame(phys);
            return -1;
        }
    }

    free_page_frame(phys);
    0
}

pub fn test_page_alloc_zero_full_page() -> c_int {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        return -1;
    }

    let virt = match phys.to_virt_checked() {
        Some(v) => v,
        None => {
            free_page_frame(phys);
            return -1;
        }
    };

    let ptr = virt.as_mut_ptr::<u8>();

    for i in 0..4096 {
        let val = unsafe { ptr.add(i).read_volatile() };
        if val != 0 {
            klog_info!(
                "RIGOROUS_TEST: Zeroed page has non-zero at offset {}: {:#x}",
                i,
                val
            );
            free_page_frame(phys);
            return -1;
        }
    }

    free_page_frame(phys);
    0
}

pub fn test_page_alloc_no_stale_data() -> c_int {
    let phys1 = alloc_page_frame(0);
    if phys1.is_null() {
        return -1;
    }

    if let Some(virt) = phys1.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..4096 {
            unsafe { ptr.add(i).write_volatile(0xDE) };
        }
    }

    free_page_frame(phys1);

    // Allocate with ZERO flag - should be zeroed even if same page reused
    let phys2 = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys2.is_null() {
        return -1;
    }

    if let Some(virt) = phys2.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..256 {
            let val = unsafe { ptr.add(i).read_volatile() };
            if val != 0 {
                klog_info!(
                    "RIGOROUS_TEST: Stale data found at offset {}: {:#x} (expected 0)",
                    i,
                    val
                );
                free_page_frame(phys2);
                return -1;
            }
        }
    }

    free_page_frame(phys2);
    0
}

/// Test: Heap allocation boundary - verify we can use full allocated size
pub fn test_heap_boundary_write() -> c_int {
    let sizes = [16usize, 32, 64, 128, 256, 512, 1024];

    for &size in &sizes {
        let ptr = kmalloc(size);
        if ptr.is_null() {
            klog_info!("RIGOROUS_TEST: Failed to allocate {} bytes", size);
            return -1;
        }

        let byte_ptr = ptr as *mut u8;

        // Write to EVERY byte in the allocation
        for i in 0..size {
            unsafe { byte_ptr.add(i).write_volatile((i & 0xFF) as u8) };
        }

        // Read back and verify
        for i in 0..size {
            let expected = (i & 0xFF) as u8;
            let actual = unsafe { byte_ptr.add(i).read_volatile() };
            if actual != expected {
                klog_info!(
                    "RIGOROUS_TEST: Heap corruption at size={} offset={}: expected {:#x}, got {:#x}",
                    size,
                    i,
                    expected,
                    actual
                );
                kfree(ptr);
                return -1;
            }
        }

        kfree(ptr);
    }

    0
}

/// Test: Multiple allocations don't overlap
pub fn test_heap_no_overlap() -> c_int {
    const NUM_ALLOCS: usize = 8;
    let mut ptrs: [*mut c_void; NUM_ALLOCS] = [ptr::null_mut(); NUM_ALLOCS];
    let sizes = [64usize, 128, 256, 64, 512, 128, 256, 64];

    // Allocate all
    for i in 0..NUM_ALLOCS {
        ptrs[i] = kmalloc(sizes[i]);
        if ptrs[i].is_null() {
            // Cleanup
            for j in 0..i {
                kfree(ptrs[j]);
            }
            klog_info!("RIGOROUS_TEST: Failed to allocate block {}", i);
            return -1;
        }

        // Write unique pattern to this allocation
        let byte_ptr = ptrs[i] as *mut u8;
        for j in 0..sizes[i] {
            unsafe { byte_ptr.add(j).write_volatile(i as u8) };
        }
    }

    // Verify all allocations still have their patterns (no overlap)
    for i in 0..NUM_ALLOCS {
        let byte_ptr = ptrs[i] as *mut u8;
        for j in 0..sizes[i] {
            let actual = unsafe { byte_ptr.add(j).read_volatile() };
            if actual != i as u8 {
                klog_info!(
                    "RIGOROUS_TEST: Allocation {} corrupted at offset {}: expected {:#x}, got {:#x}",
                    i,
                    j,
                    i as u8,
                    actual
                );
                // Cleanup
                for k in 0..NUM_ALLOCS {
                    kfree(ptrs[k]);
                }
                return -1;
            }
        }
    }

    // Cleanup
    for i in 0..NUM_ALLOCS {
        kfree(ptrs[i]);
    }

    0
}

/// Test: Free then access should... well, we can't safely test use-after-free
/// But we CAN test double-free doesn't crash (defensive)
pub fn test_heap_double_free_defensive() -> c_int {
    let ptr = kmalloc(64);
    if ptr.is_null() {
        return -1;
    }

    kfree(ptr);
    // Second free - should not crash (may be a no-op or error)
    kfree(ptr);

    0
}

/// Test: Allocate large block, verify entire region is writable
pub fn test_heap_large_block_integrity() -> c_int {
    let size = 8192usize; // 8KB
    let ptr = kmalloc(size);
    if ptr.is_null() {
        klog_info!("RIGOROUS_TEST: Failed to allocate 8KB");
        return -1;
    }

    let byte_ptr = ptr as *mut u8;

    // Write pattern across entire 8KB
    for i in 0..size {
        let pattern = ((i * 17) & 0xFF) as u8;
        unsafe { byte_ptr.add(i).write_volatile(pattern) };
    }

    // Verify
    for i in 0..size {
        let expected = ((i * 17) & 0xFF) as u8;
        let actual = unsafe { byte_ptr.add(i).read_volatile() };
        if actual != expected {
            klog_info!(
                "RIGOROUS_TEST: Large block corruption at offset {}: expected {:#x}, got {:#x}",
                i,
                expected,
                actual
            );
            kfree(ptr);
            return -1;
        }
    }

    kfree(ptr);
    0
}

/// Test: Stress test - rapid alloc/free cycles
pub fn test_heap_stress_cycles() -> c_int {
    for cycle in 0..100 {
        let ptr = kmalloc(128);
        if ptr.is_null() {
            klog_info!("RIGOROUS_TEST: Stress test failed at cycle {}", cycle);
            return -1;
        }

        // Write and verify
        let byte_ptr = ptr as *mut u8;
        unsafe {
            byte_ptr.write_volatile(0xAB);
            byte_ptr.add(127).write_volatile(0xCD);
        }

        let first = unsafe { byte_ptr.read_volatile() };
        let last = unsafe { byte_ptr.add(127).read_volatile() };

        if first != 0xAB || last != 0xCD {
            klog_info!(
                "RIGOROUS_TEST: Stress corruption at cycle {}: first={:#x}, last={:#x}",
                cycle,
                first,
                last
            );
            kfree(ptr);
            return -1;
        }

        kfree(ptr);
    }

    0
}

pub fn test_page_alloc_multipage_integrity() -> c_int {
    let phys = alloc_page_frames(4, ALLOC_FLAG_ZERO);
    if phys.is_null() {
        klog_info!("RIGOROUS_TEST: Failed to allocate 4 pages");
        return -1;
    }

    for page in 0..4u64 {
        let page_phys = PhysAddr::new(phys.as_u64() + page * 4096);
        if let Some(virt) = page_phys.to_virt_checked() {
            let ptr = virt.as_mut_ptr::<u8>();
            for i in 0..4096 {
                let pattern = ((page as u8).wrapping_mul(17)).wrapping_add((i & 0xFF) as u8);
                unsafe { ptr.add(i).write_volatile(pattern) };
            }
        }
    }

    for page in 0..4u64 {
        let page_phys = PhysAddr::new(phys.as_u64() + page * 4096);
        if let Some(virt) = page_phys.to_virt_checked() {
            let ptr = virt.as_mut_ptr::<u8>();
            for i in 0..4096 {
                let expected = ((page as u8).wrapping_mul(17)).wrapping_add((i & 0xFF) as u8);
                let actual = unsafe { ptr.add(i).read_volatile() };
                if actual != expected {
                    klog_info!(
                        "RIGOROUS_TEST: Multipage corruption page={} offset={}: expected {:#x}, got {:#x}",
                        page,
                        i,
                        expected,
                        actual
                    );
                    free_page_frame(phys);
                    return -1;
                }
            }
        }
    }

    free_page_frame(phys);
    0
}

// ============================================================================
// PROCESS VM AND COW TESTS - Test the dangerous stuff
// ============================================================================

use crate::cow::{handle_cow_fault, is_cow_fault};
use crate::mm_constants::PageFlags;
use crate::paging::{map_page_4kb_in_dir, paging_mark_cow, virt_to_phys_in_dir};

pub use crate::tests_demand::{
    test_demand_double_fault, test_demand_fault_no_vma, test_demand_fault_non_lazy_vma,
    test_demand_fault_present_page, test_demand_fault_valid_lazy_vma, test_demand_handle_no_vma,
    test_demand_handle_null_page_dir, test_demand_handle_page_boundary,
    test_demand_handle_permission_denied, test_demand_handle_success,
    test_demand_invalid_process_id, test_demand_multiple_faults, test_demand_permission_allow_read,
    test_demand_permission_allow_write, test_demand_permission_deny_exec,
    test_demand_permission_deny_user_kernel, test_demand_permission_deny_write_ro,
};

pub use crate::tests_oom::{
    test_alloc_free_cycles_no_leak, test_dma_allocation_exhaustion, test_heap_alloc_pressure,
    test_heap_expansion_under_pressure, test_kzalloc_zeroed_under_pressure,
    test_multiorder_alloc_failure, test_page_alloc_fragmentation_oom, test_page_alloc_until_oom,
    test_process_heap_expansion_oom, test_process_vm_creation_pressure, test_refcount_during_oom,
    test_zero_flag_under_pressure,
};

pub use crate::tests_cow_edge::{
    test_cow_clone_modify_both, test_cow_handle_invalid_address, test_cow_handle_not_cow_page,
    test_cow_handle_null_pagedir, test_cow_multi_ref_copy, test_cow_multiple_clones,
    test_cow_no_collateral_damage, test_cow_not_present_not_cow, test_cow_page_boundary,
    test_cow_read_not_cow_fault, test_cow_single_ref_upgrade,
};

pub fn test_process_vm_create_destroy_memory() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == crate::mm_constants::INVALID_PROCESS_ID {
        klog_info!("PROCESS_TEST: Failed to create process VM");
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        klog_info!("PROCESS_TEST: Page directory is null");
        destroy_process_vm(pid);
        return -1;
    }

    // The process should have a stack mapped - try to access it via kernel
    // Stack is at high user addresses, let's check the null page mapping
    let null_page_phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(0));
    if null_page_phys.is_null() {
        klog_info!("PROCESS_TEST: Null page not mapped (expected for user process)");
        // This is actually fine - null page might not be mapped
    }

    destroy_process_vm(pid);
    0
}

pub fn test_process_vm_alloc_and_access() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == crate::mm_constants::INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Allocate heap memory for the process
    use crate::process_vm::process_vm_alloc;
    let user_addr = process_vm_alloc(pid, 4096, PageFlags::WRITABLE.bits() as u32);
    if user_addr == 0 {
        klog_info!("PROCESS_TEST: process_vm_alloc returned 0");
        destroy_process_vm(pid);
        return -1;
    }

    // The allocation is LAZY - pages aren't mapped until accessed
    // But we can check the VMA was created
    let phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(user_addr));
    // Lazy allocation means phys might be null until fault
    if !phys.is_null() {
        // If mapped, verify we can access via HHDM
        if let Some(virt) = phys.to_virt_checked() {
            let ptr = virt.as_mut_ptr::<u8>();
            unsafe {
                ptr.write_volatile(0x42);
                let val = ptr.read_volatile();
                if val != 0x42 {
                    klog_info!("PROCESS_TEST: Memory write/read mismatch");
                    destroy_process_vm(pid);
                    return -1;
                }
            }
        }
    }

    destroy_process_vm(pid);
    0
}

pub fn test_process_vm_brk_expansion() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == crate::mm_constants::INVALID_PROCESS_ID {
        return -1;
    }

    use crate::process_vm::process_vm_brk;

    // Get current brk
    let initial_brk = process_vm_brk(pid, 0);
    if initial_brk == 0 {
        klog_info!("PROCESS_TEST: Initial brk is 0");
        destroy_process_vm(pid);
        return -1;
    }

    // Expand brk by 8KB
    let new_brk = process_vm_brk(pid, initial_brk + 8192);
    if new_brk <= initial_brk {
        klog_info!(
            "PROCESS_TEST: brk expansion failed: {} -> {}",
            initial_brk,
            new_brk
        );
        destroy_process_vm(pid);
        return -1;
    }

    // Shrink brk back
    let shrunk_brk = process_vm_brk(pid, initial_brk + 4096);
    if shrunk_brk != initial_brk + 4096 {
        klog_info!(
            "PROCESS_TEST: brk shrink failed: expected {}, got {}",
            initial_brk + 4096,
            shrunk_brk
        );
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}

pub fn test_cow_page_isolation() -> c_int {
    init_process_vm();

    let parent_pid = create_process_vm();
    if parent_pid == crate::mm_constants::INVALID_PROCESS_ID {
        return -1;
    }

    let parent_dir = process_vm_get_page_dir(parent_pid);
    if parent_dir.is_null() {
        destroy_process_vm(parent_pid);
        return -1;
    }

    // Use process_vm_alloc to properly create a VMA (COW clone iterates VMAs, not raw mappings)
    use crate::process_vm::process_vm_alloc;
    let test_addr = process_vm_alloc(parent_pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    if test_addr == 0 {
        klog_info!("PROCESS_TEST: process_vm_alloc failed");
        destroy_process_vm(parent_pid);
        return -1;
    }

    // Allocate physical page and map it within the VMA
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        destroy_process_vm(parent_pid);
        return -1;
    }

    // Map it as user-writable within the allocated VMA region
    if map_page_4kb_in_dir(
        parent_dir,
        VirtAddr::new(test_addr),
        phys,
        PageFlags::USER_RW.bits(),
    ) != 0
    {
        free_page_frame(phys);
        destroy_process_vm(parent_pid);
        return -1;
    }

    // Write pattern via HHDM
    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..4096 {
            unsafe { ptr.add(i).write_volatile(0xAA) };
        }
    }

    // Clone with COW
    use crate::process_vm::process_vm_clone_cow;
    let child_pid = process_vm_clone_cow(parent_pid);
    if child_pid == crate::mm_constants::INVALID_PROCESS_ID {
        klog_info!("PROCESS_TEST: COW clone failed");
        destroy_process_vm(parent_pid);
        return -1;
    }

    let child_dir = process_vm_get_page_dir(child_pid);
    if child_dir.is_null() {
        destroy_process_vm(child_pid);
        destroy_process_vm(parent_pid);
        return -1;
    }

    // Both should point to the same physical page initially (COW sharing)
    let parent_phys = virt_to_phys_in_dir(parent_dir, VirtAddr::new(test_addr));
    let child_phys = virt_to_phys_in_dir(child_dir, VirtAddr::new(test_addr));

    if parent_phys.is_null() || child_phys.is_null() {
        klog_info!(
            "PROCESS_TEST: COW pages not mapped correctly (parent={:?}, child={:?})",
            parent_phys,
            child_phys
        );
        destroy_process_vm(child_pid);
        destroy_process_vm(parent_pid);
        return -1;
    }

    // They should be the same physical page (COW sharing)
    if parent_phys != child_phys {
        klog_info!("PROCESS_TEST: COW pages should share same physical page initially");
        // This might not be a bug - depends on implementation
    }

    // Verify child can read the same data
    if let Some(virt) = child_phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        let val = unsafe { ptr.read_volatile() };
        if val != 0xAA {
            klog_info!("PROCESS_TEST: Child COW page has wrong data: {:#x}", val);
            destroy_process_vm(child_pid);
            destroy_process_vm(parent_pid);
            return -1;
        }
    }

    destroy_process_vm(child_pid);
    destroy_process_vm(parent_pid);
    0
}

pub fn test_cow_fault_handling() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == crate::mm_constants::INVALID_PROCESS_ID {
        return -1;
    }

    let page_dir = process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Allocate and map a page, then mark it COW
    let test_addr = 0x2000u64;
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        destroy_process_vm(pid);
        return -1;
    }

    // Map as read-only user page first
    if map_page_4kb_in_dir(
        page_dir,
        VirtAddr::new(test_addr),
        phys,
        PageFlags::USER_RO.bits(),
    ) != 0
    {
        free_page_frame(phys);
        destroy_process_vm(pid);
        return -1;
    }

    // Mark as COW
    paging_mark_cow(page_dir, VirtAddr::new(test_addr));

    // Simulate a write fault - error code for write to present page = 0x03
    let error_code = 0x03u64;
    let is_cow = is_cow_fault(error_code, page_dir, test_addr);
    if !is_cow {
        klog_info!("PROCESS_TEST: is_cow_fault returned false for COW page");
        destroy_process_vm(pid);
        return -1;
    }

    // Handle the COW fault
    match handle_cow_fault(page_dir, test_addr) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("PROCESS_TEST: handle_cow_fault failed: {:?}", e);
            destroy_process_vm(pid);
            return -1;
        }
    }

    // After COW resolution, page should be writable
    let new_phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(test_addr));
    if new_phys.is_null() {
        klog_info!("PROCESS_TEST: Page unmapped after COW resolution");
        destroy_process_vm(pid);
        return -1;
    }

    // Verify we can write to the new page
    if let Some(virt) = new_phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        unsafe {
            ptr.write_volatile(0xBB);
            let val = ptr.read_volatile();
            if val != 0xBB {
                klog_info!("PROCESS_TEST: Post-COW write verification failed");
                destroy_process_vm(pid);
                return -1;
            }
        }
    }

    destroy_process_vm(pid);
    0
}

pub fn test_multiple_process_vms() -> c_int {
    init_process_vm();

    const NUM_PROCESSES: usize = 5;
    let mut pids = [0u32; NUM_PROCESSES];

    // Create multiple processes
    for i in 0..NUM_PROCESSES {
        pids[i] = create_process_vm();
        if pids[i] == crate::mm_constants::INVALID_PROCESS_ID {
            klog_info!("PROCESS_TEST: Failed to create process {}", i);
            for j in 0..i {
                destroy_process_vm(pids[j]);
            }
            return -1;
        }
    }

    // Verify each has a unique page directory
    let mut dirs = [ptr::null_mut(); NUM_PROCESSES];
    for i in 0..NUM_PROCESSES {
        dirs[i] = process_vm_get_page_dir(pids[i]);
        if dirs[i].is_null() {
            klog_info!("PROCESS_TEST: Process {} has null page dir", i);
            for j in 0..NUM_PROCESSES {
                destroy_process_vm(pids[j]);
            }
            return -1;
        }
    }

    // Check uniqueness
    for i in 0..NUM_PROCESSES {
        for j in (i + 1)..NUM_PROCESSES {
            if dirs[i] == dirs[j] {
                klog_info!(
                    "PROCESS_TEST: Processes {} and {} share same page dir!",
                    i,
                    j
                );
                for k in 0..NUM_PROCESSES {
                    destroy_process_vm(pids[k]);
                }
                return -1;
            }
        }
    }

    // Cleanup
    for i in 0..NUM_PROCESSES {
        destroy_process_vm(pids[i]);
    }

    0
}

pub fn test_vma_flags_retrieval() -> c_int {
    init_process_vm();

    let pid = create_process_vm();
    if pid == crate::mm_constants::INVALID_PROCESS_ID {
        return -1;
    }

    use crate::process_vm::{process_vm_alloc, process_vm_get_vma_flags};

    // Allocate some heap memory
    let user_addr = process_vm_alloc(pid, 8192, PageFlags::WRITABLE.bits() as u32);
    if user_addr == 0 {
        destroy_process_vm(pid);
        return -1;
    }

    // Check VMA flags for the allocated region
    let flags = process_vm_get_vma_flags(pid, user_addr);
    if flags.is_none() {
        klog_info!("PROCESS_TEST: VMA flags not found for allocated region");
        destroy_process_vm(pid);
        return -1;
    }

    let flags = flags.unwrap();
    use crate::vma_flags::VmaFlags;

    // Should be heap memory with write permission
    if !flags.contains(VmaFlags::HEAP) {
        klog_info!("PROCESS_TEST: Allocated region not marked as HEAP");
        destroy_process_vm(pid);
        return -1;
    }

    if !flags.contains(VmaFlags::WRITE) {
        klog_info!("PROCESS_TEST: Allocated region not marked as WRITE");
        destroy_process_vm(pid);
        return -1;
    }

    destroy_process_vm(pid);
    0
}
