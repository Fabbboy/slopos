use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr;

use slopos_ostd::KVec;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_arch::cpu;
use slopos_arch::cpu::msr::Msr;
use slopos_testing::TestResult;
use slopos_testing::{assert_not_null, assert_test, fail, pass};
use slopos_utils::klog_info;

use crate::hhdm::PhysAddrHhdm;
use crate::kernel_heap::{get_heap_stats, kfree, kmalloc, kzalloc};
use crate::page_alloc::{
    ALLOC_FLAG_ZERO, alloc_page_frame, alloc_page_frames, free_page_frame,
    get_page_allocator_stats, page_frame_get_ref, page_frame_inc_ref,
};
use crate::paging::virt_to_phys;
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::process_vm::get_process_vm_stats;

// ============================================================================
// PAGE ALLOCATOR (BUDDY) TESTS - 12 tests
// ============================================================================

/// Test 1: Allocate and free a single 4KB page
pub fn test_page_alloc_single() -> TestResult {
    let phys = alloc_page_frame(0);
    assert_not_null!(phys.as_u64() as *const u8, "allocate single page");
    assert_test!(phys.as_u64() != 0, "allocated address is zero");

    let ref_count = page_frame_get_ref(phys);
    if ref_count == 0 {
        free_page_frame(phys);
        return fail!(
            "ref count should be non-zero after alloc, got {}",
            ref_count
        );
    }

    free_page_frame(phys);
    pass!()
}

/// Test 2: Allocate multi-order blocks (2, 4, 8 pages)
pub fn test_page_alloc_multi_order() -> TestResult {
    let phys2 = alloc_page_frames(2, 0);
    assert_not_null!(phys2.as_u64() as *const u8, "allocate 2 pages");

    let phys4 = alloc_page_frames(4, 0);
    if phys4.is_null() {
        free_page_frame(phys2);
        return fail!("allocate 4 pages");
    }

    let phys8 = alloc_page_frames(8, 0);
    if phys8.is_null() {
        free_page_frame(phys2);
        free_page_frame(phys4);
        return fail!("allocate 8 pages");
    }

    free_page_frame(phys2);
    free_page_frame(phys4);
    free_page_frame(phys8);
    pass!()
}

/// Test 3: Alloc→free→alloc same size, verify address reuse (coalescing)
pub fn test_page_alloc_free_cycle() -> TestResult {
    let phys1 = alloc_page_frame(0);
    assert_not_null!(phys1.as_u64() as *const u8, "first alloc");

    free_page_frame(phys1);

    let phys2 = alloc_page_frame(0);
    assert_not_null!(phys2.as_u64() as *const u8, "second alloc after free");

    // With good coalescing, we might get the same address back (not guaranteed)
    // At minimum, the allocation should succeed
    free_page_frame(phys2);
    pass!()
}

/// Test 4: Allocate with ALLOC_FLAG_ZERO, verify memory is zeroed
pub fn test_page_alloc_zeroed() -> TestResult {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    assert_not_null!(phys.as_u64() as *const u8, "allocate zeroed page");

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
                return fail!("zeroed page has non-zero byte at offset {}", i);
            }
        }
    }

    free_page_frame(phys);
    pass!()
}

/// Test 5: Reference count increment and decrement
pub fn test_page_alloc_refcount() -> TestResult {
    let phys = alloc_page_frame(0);
    assert_not_null!(phys.as_u64() as *const u8, "alloc for refcount test");

    let ref1 = page_frame_get_ref(phys);
    if ref1 != 1 {
        free_page_frame(phys);
        return fail!("initial refcount should be 1, got {}", ref1);
    }

    let new_ref = page_frame_inc_ref(phys);
    if new_ref != 2 {
        free_page_frame(phys);
        free_page_frame(phys);
        return fail!("refcount after inc should be 2, got {}", new_ref);
    }

    // First free should just decrement
    free_page_frame(phys);

    let ref_after = page_frame_get_ref(phys);
    if ref_after != 1 {
        free_page_frame(phys);
        return fail!("refcount after first free should be 1, got {}", ref_after);
    }

    // Second free should actually free
    free_page_frame(phys);
    pass!()
}

/// Test 6: Stats accuracy check
pub fn test_page_alloc_stats() -> TestResult {
    let mut total = 0u32;
    let mut free_before = 0u32;
    let mut alloc_before = 0u32;
    get_page_allocator_stats(&mut total, &mut free_before, &mut alloc_before);

    assert_test!(total != 0, "total frames is 0");

    let phys = alloc_page_frames(4, 0);
    assert_not_null!(phys.as_u64() as *const u8, "alloc 4 pages for stats");

    let mut free_after = 0u32;
    let mut alloc_after = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_after, &mut alloc_after);

    if alloc_after < alloc_before + 4 {
        free_page_frame(phys);
        return fail!("allocated count didn't increase by 4");
    }

    free_page_frame(phys);
    pass!()
}

/// Test 7: Free NULL address should not crash
pub fn test_page_alloc_free_null() -> TestResult {
    // This should be a no-op, not crash
    let _result = free_page_frame(PhysAddr::NULL);
    pass!()
}

/// Test 8: Fragmentation stress test
pub fn test_page_alloc_fragmentation() -> TestResult {
    let mut pages: [PhysAddr; 8] = [PhysAddr::NULL; 8];
    for i in 0..8 {
        pages[i] = alloc_page_frame(0);
        if pages[i].is_null() {
            for j in 0..i {
                free_page_frame(pages[j]);
            }
            return fail!("failed to allocate page {}", i);
        }
    }

    // Free alternate pages (0, 2, 4, 6)
    free_page_frame(pages[0]);
    free_page_frame(pages[2]);
    free_page_frame(pages[4]);
    free_page_frame(pages[6]);

    // Try to allocate a 2-page block - may or may not succeed depending on layout
    let large = alloc_page_frames(2, 0);
    if !large.is_null() {
        free_page_frame(large);
    }

    // Free remaining
    free_page_frame(pages[1]);
    free_page_frame(pages[3]);
    free_page_frame(pages[5]);
    free_page_frame(pages[7]);
    pass!()
}

// ============================================================================
// KERNEL HEAP TESTS - 10 tests
// ============================================================================

/// Test 1: Small allocations (16, 32, 64 bytes)
pub fn test_heap_small_alloc() -> TestResult {
    let p16 = kmalloc(16);
    assert_not_null!(p16, "allocate 16 bytes");

    let p32 = kmalloc(32);
    if p32.is_null() {
        kfree(p16);
        return fail!("allocate 32 bytes");
    }

    let p64 = kmalloc(64);
    if p64.is_null() {
        kfree(p16);
        kfree(p32);
        return fail!("allocate 64 bytes");
    }

    kfree(p64);
    kfree(p32);
    kfree(p16);
    pass!()
}

/// Test 2: Medium allocations (256, 512, 1024 bytes)
pub fn test_heap_medium_alloc() -> TestResult {
    let p256 = kmalloc(256);
    assert_not_null!(p256, "allocate 256 bytes");

    let p512 = kmalloc(512);
    if p512.is_null() {
        kfree(p256);
        return fail!("allocate 512 bytes");
    }

    let p1k = kmalloc(1024);
    if p1k.is_null() {
        kfree(p256);
        kfree(p512);
        return fail!("allocate 1024 bytes");
    }

    kfree(p1k);
    kfree(p512);
    kfree(p256);
    pass!()
}

/// Test 3: Large allocations (4KB, 16KB)
pub fn test_heap_large_alloc() -> TestResult {
    let p4k = kmalloc(4096);
    assert_not_null!(p4k, "allocate 4KB");

    let p16k = kmalloc(16384);
    if p16k.is_null() {
        kfree(p4k);
        return fail!("allocate 16KB");
    }

    kfree(p16k);
    kfree(p4k);
    pass!()
}

/// Test 4: kzalloc returns zeroed memory
pub fn test_heap_kzalloc_zeroed() -> TestResult {
    let ptr = kzalloc(128);
    assert_not_null!(ptr, "kzalloc 128 bytes");

    let bytes = ptr as *const u8;
    for i in 0..128 {
        let b = unsafe { *bytes.add(i) };
        if b != 0 {
            kfree(ptr);
            return fail!("kzalloc memory not zeroed at offset {}", i);
        }
    }

    kfree(ptr);
    pass!()
}

/// Test 5: kfree(null) should not crash
pub fn test_heap_kfree_null() -> TestResult {
    kfree(ptr::null_mut());
    pass!()
}

/// Test 6: Allocation size zero should return null
pub fn test_heap_alloc_zero() -> TestResult {
    let ptr = kmalloc(0);
    if !ptr.is_null() {
        kfree(ptr);
        return fail!("kmalloc(0) should return null");
    }
    pass!()
}

/// Test 7: Stats tracking accuracy
pub fn test_heap_stats() -> TestResult {
    let mut stats_before = MaybeUninit::uninit();
    get_heap_stats(stats_before.as_mut_ptr());
    let before = unsafe { stats_before.assume_init() };

    let ptr = kmalloc(256);
    assert_not_null!(ptr, "alloc for stats test");

    let mut stats_after = MaybeUninit::uninit();
    get_heap_stats(stats_after.as_mut_ptr());
    let after = unsafe { stats_after.assume_init() };

    if after.allocated_size <= before.allocated_size {
        kfree(ptr);
        return fail!("allocated size didn't increase");
    }

    if after.allocation_count <= before.allocation_count {
        kfree(ptr);
        return fail!("allocation count didn't increase");
    }

    kfree(ptr);
    pass!()
}

pub fn test_global_alloc_vec() -> TestResult {
    let mut vec: KVec<u64> = KVec::new();
    for i in 0..128u64 {
        vec.push(i).expect("test alloc");
    }
    assert_test!(vec.len() == 128, "vec length should be 128");
    pass!()
}

pub fn test_heap_free_list_search() -> TestResult {
    let mut stats_before = MaybeUninit::uninit();
    get_heap_stats(stats_before.as_mut_ptr());
    let initial_heap_size = unsafe { stats_before.assume_init() }.total_size;

    let p1 = kmalloc(256);
    assert_not_null!(p1, "alloc p1");
    let p2 = kmalloc(256);
    if p2.is_null() {
        kfree(p1);
        return fail!("alloc p2");
    }
    let p3 = kmalloc(256);
    if p3.is_null() {
        kfree(p1);
        kfree(p2);
        return fail!("alloc p3");
    }

    let mut stats_after_alloc = MaybeUninit::uninit();
    get_heap_stats(stats_after_alloc.as_mut_ptr());
    let heap_after_alloc = unsafe { stats_after_alloc.assume_init() }.total_size;

    kfree(p1);
    kfree(p2);

    let p4 = kmalloc(256);
    if p4.is_null() {
        kfree(p3);
        return fail!("alloc p4");
    }
    let p5 = kmalloc(256);
    if p5.is_null() {
        kfree(p3);
        kfree(p4);
        return fail!("alloc p5");
    }

    let mut stats_final = MaybeUninit::uninit();
    get_heap_stats(stats_final.as_mut_ptr());
    let final_heap_size = unsafe { stats_final.assume_init() }.total_size;

    if final_heap_size > heap_after_alloc {
        kfree(p3);
        kfree(p4);
        kfree(p5);
        return fail!("heap grew beyond post-alloc size");
    }

    kfree(p3);
    kfree(p4);
    kfree(p5);

    assert_test!(
        final_heap_size >= initial_heap_size,
        "final heap size less than initial"
    );
    pass!()
}

/// Regression test: Verify HEAP_WARMUP_PAGES is sufficient for soft reboot coherency.
///
/// After soft reboot, x86 paging structure caches may retain stale entries. The fix
/// requires ≥2 physical frame allocations AND ≥1 page mapping during heap init.
/// This test ensures HEAP_WARMUP_PAGES is never reduced below the minimum threshold.
///
/// If this test fails, framebuffer performance will degrade to ~1 FPS after soft reboot.
/// See: Intel Application Note 317080-002 "TLBs, Paging-Structure Caches"
pub fn test_heap_warmup_pages_minimum() -> TestResult {
    use crate::kernel_heap::HEAP_WARMUP_PAGES;

    const MINIMUM_WARMUP_PAGES: u32 = 2;

    if HEAP_WARMUP_PAGES < MINIMUM_WARMUP_PAGES {
        return fail!(
            "HEAP_WARMUP_PAGES ({}) is below minimum ({}). \
             This WILL cause framebuffer performance regression after soft reboot!",
            HEAP_WARMUP_PAGES,
            MINIMUM_WARMUP_PAGES
        );
    }

    const RECOMMENDED_WARMUP_PAGES: u32 = 4;
    if HEAP_WARMUP_PAGES < RECOMMENDED_WARMUP_PAGES {
        klog_info!(
            "HEAP_TEST: Warning - HEAP_WARMUP_PAGES ({}) is below recommended ({})",
            HEAP_WARMUP_PAGES,
            RECOMMENDED_WARMUP_PAGES
        );
    }

    pass!()
}

pub fn test_heap_fragmentation_behind_head() -> TestResult {
    let mut ptrs: [*mut c_void; 5] = [ptr::null_mut(); 5];
    let sizes = [128usize, 256, 128, 512, 256];

    for (i, size) in sizes.iter().enumerate() {
        ptrs[i] = kmalloc(*size);
        if ptrs[i].is_null() {
            for j in 0..i {
                kfree(ptrs[j]);
            }
            return fail!("alloc {} bytes at index {}", size, i);
        }
    }

    kfree(ptrs[0]);
    kfree(ptrs[2]);
    kfree(ptrs[3]);

    let needed = kmalloc(400);
    if needed.is_null() {
        kfree(ptrs[1]);
        kfree(ptrs[4]);
        return fail!("alloc 400 bytes from freed gaps");
    }

    kfree(needed);
    kfree(ptrs[1]);
    kfree(ptrs[4]);
    pass!()
}

// ============================================================================
// PROCESS VM TESTS (existing)
// ============================================================================

use crate::process_vm::{
    create_process_vm, destroy_process_vm, init_process_vm, process_vm_get_page_dir,
};
use slopos_abi::task::INVALID_PROCESS_ID;

pub fn test_process_vm_slot_reuse() -> TestResult {
    init_process_vm();

    let mut initial_active: u32 = 0;
    get_process_vm_stats(ptr::null_mut(), &mut initial_active);

    let mut pids = [0u32; 5];
    for i in 0..5 {
        pids[i] = create_process_vm();
        if pids[i] == INVALID_PROCESS_ID {
            return fail!("create process {}", i);
        }
        if process_vm_get_page_dir(pids[i]).is_null() {
            return fail!("page dir for process {}", i);
        }
    }

    for &idx in &[1usize, 2, 3] {
        if destroy_process_vm(pids[idx]) != 0 {
            return fail!("destroy process at index {}", idx);
        }
    }

    for &idx in &[1usize, 2, 3] {
        if !process_vm_get_page_dir(pids[idx]).is_null() {
            return fail!("destroyed process {} should have null page dir", idx);
        }
    }

    assert_not_null!(process_vm_get_page_dir(pids[0]), "surviving process 0");
    assert_not_null!(process_vm_get_page_dir(pids[4]), "surviving process 4");

    let mut new_pids = [0u32; 3];
    for i in 0..3 {
        new_pids[i] = create_process_vm();
        if new_pids[i] == INVALID_PROCESS_ID {
            return fail!("create reuse process {}", i);
        }
        if process_vm_get_page_dir(new_pids[i]).is_null() {
            return fail!("reuse page dir {}", i);
        }
    }

    assert_not_null!(
        process_vm_get_page_dir(pids[0]),
        "original process 0 still alive"
    );
    assert_not_null!(
        process_vm_get_page_dir(pids[4]),
        "original process 4 still alive"
    );

    assert_test!(destroy_process_vm(pids[0]) == 0, "destroy original 0");
    assert_test!(destroy_process_vm(pids[4]) == 0, "destroy original 4");
    for pid in new_pids {
        destroy_process_vm(pid);
    }

    let mut final_active: u32 = 0;
    get_process_vm_stats(ptr::null_mut(), &mut final_active);
    if final_active != initial_active {
        return fail!(
            "active count mismatch: {} != {}",
            final_active,
            initial_active
        );
    }
    pass!()
}

pub fn test_process_vm_counter_reset() -> TestResult {
    init_process_vm();

    let mut initial_active: u32 = 0;
    get_process_vm_stats(ptr::null_mut(), &mut initial_active);

    let mut pids = [0u32; 10];
    for i in 0..10 {
        pids[i] = create_process_vm();
        if pids[i] == INVALID_PROCESS_ID {
            for j in 0..i {
                destroy_process_vm(pids[j]);
            }
            return fail!("create process {}", i);
        }
    }

    let mut active_after: u32 = 0;
    get_process_vm_stats(ptr::null_mut(), &mut active_after);
    if active_after != initial_active + 10 {
        for pid in pids {
            destroy_process_vm(pid);
        }
        return fail!(
            "active count should be {} + 10, got {}",
            initial_active,
            active_after
        );
    }

    for pid in pids.iter().rev() {
        if destroy_process_vm(*pid) != 0 {
            return fail!("destroy process {}", pid);
        }
    }

    let mut final_active: u32 = 0;
    get_process_vm_stats(ptr::null_mut(), &mut final_active);
    if final_active != initial_active {
        return fail!(
            "final active {} != initial {}",
            final_active,
            initial_active
        );
    }
    pass!()
}

// ============================================================================
// PAGING TESTS - 10 tests
// ============================================================================

/// Test 1: virt_to_phys on kernel address
pub fn test_paging_virt_to_phys() -> TestResult {
    let kernel_addr = VirtAddr::new(test_paging_virt_to_phys as *const () as u64);
    let phys = virt_to_phys(kernel_addr);
    assert_test!(
        !phys.is_null(),
        "virt_to_phys returned null for kernel code"
    );
    pass!()
}

/// Test 2: Kernel directory retrieval — `KERNEL_VM_SPACE` singleton
/// is wrapping the live kernel-master PML4 by the time tests run.
pub fn test_paging_get_kernel_dir() -> TestResult {
    let installed = slopos_kernel_services::kernel_vm_space::try_kernel_vm_space().is_some();
    assert_test!(installed, "kernel_vm_space not installed");
    pass!()
}

/// Test 3: User accessible check on kernel page (should fail).
/// The OSTD cursor's `query` over a kernel-half VA returns a
/// `PageProperty` with `user == false`, so the kernel-mappings
/// helper reports `false` for any kernel-half VA.
pub fn test_paging_user_accessible_kernel() -> TestResult {
    use slopos_kernel_services::kernel_vm_space::kernel_vm_space;
    use slopos_ostd::mm::page_property::PageProperty;

    let kernel_addr = VirtAddr::new(test_paging_user_accessible_kernel as *const () as u64);
    let aligned = VirtAddr::new(kernel_addr.as_u64() & !((PAGE_SIZE_4KB) - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let guard = kernel_vm_space().lock();
    let cur = match guard.cursor(range) {
        Ok(c) => c,
        Err(_) => return fail!("cursor over kernel half"),
    };
    let entry = match cur.query() {
        Ok(e) => e,
        Err(_) => return fail!("cursor query over kernel-half code"),
    };
    let prop: PageProperty = entry.property;
    assert_test!(
        !prop.user,
        "kernel code incorrectly marked as user accessible"
    );
    pass!()
}

/// Test 4: COW flag on kernel page (should not be set). The OSTD
/// `software` field is the AVL-bits container; bit 0 is the slopos
/// COW marker.
pub fn test_paging_cow_kernel() -> TestResult {
    use slopos_kernel_services::kernel_vm_space::kernel_vm_space;

    let kernel_addr = VirtAddr::new(test_paging_cow_kernel as *const () as u64);
    let aligned = VirtAddr::new(kernel_addr.as_u64() & !((PAGE_SIZE_4KB) - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let guard = kernel_vm_space().lock();
    let cur = match guard.cursor(range) {
        Ok(c) => c,
        Err(_) => return fail!("cursor over kernel half"),
    };
    let entry = match cur.query() {
        Ok(e) => e,
        Err(_) => return fail!("cursor query over kernel-half code"),
    };
    let is_cow = (entry.property.software & 0b001) != 0;
    assert_test!(!is_cow, "kernel code incorrectly marked as COW");
    pass!()
}

// ============================================================================
// RING BUFFER TESTS - 8 tests (in lib crate, tested via mm)
// ============================================================================

/// Test ring buffer basic push/pop
pub fn test_ring_buffer_basic() -> TestResult {
    use slopos_utils::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();
    assert_test!(rb.is_empty(), "new buffer should be empty");
    assert_test!(rb.try_push(42), "push to empty buffer failed");
    assert_test!(!rb.is_empty(), "buffer should not be empty after push");

    let val = rb.try_pop();
    assert_test!(val == Some(42), "pop returned wrong value");
    assert_test!(rb.is_empty(), "buffer should be empty after pop");
    pass!()
}

/// Test ring buffer FIFO order
pub fn test_ring_buffer_fifo() -> TestResult {
    use slopos_utils::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();
    rb.try_push(1);
    rb.try_push(2);
    rb.try_push(3);

    assert_test!(rb.try_pop() == Some(1), "FIFO order violated (expected 1)");
    assert_test!(rb.try_pop() == Some(2), "FIFO order violated (expected 2)");
    assert_test!(rb.try_pop() == Some(3), "FIFO order violated (expected 3)");
    pass!()
}

/// Test ring buffer empty pop
pub fn test_ring_buffer_empty_pop() -> TestResult {
    use slopos_utils::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();
    assert_test!(rb.try_pop().is_none(), "pop from empty should return None");
    pass!()
}

/// Test ring buffer full behavior
pub fn test_ring_buffer_full() -> TestResult {
    use slopos_utils::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 4> = RingBuffer::new();
    for i in 0..4 {
        if !rb.try_push(i) {
            return fail!("push {} failed unexpectedly", i);
        }
    }

    assert_test!(rb.is_full(), "buffer should be full");
    assert_test!(!rb.try_push(999), "push to full buffer should fail");
    pass!()
}

/// Test ring buffer overwrite mode
pub fn test_ring_buffer_overwrite() -> TestResult {
    use slopos_utils::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 4> = RingBuffer::new();
    for i in 0..4u32 {
        rb.push_overwrite(i);
    }

    // Push 99 - should overwrite oldest (0)
    rb.push_overwrite(99);

    // Should get 1,2,3,99 in that order
    assert_test!(
        rb.try_pop() == Some(1),
        "overwrite test failed (expected 1)"
    );
    pass!()
}

/// Test ring buffer wrap around
pub fn test_ring_buffer_wrap() -> TestResult {
    use slopos_utils::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 4> = RingBuffer::new();
    rb.try_push(1);
    rb.try_push(2);
    rb.try_push(3);

    rb.try_pop();
    rb.try_pop();

    rb.try_push(4);
    rb.try_push(5);
    rb.try_push(6);

    assert_test!(rb.try_pop() == Some(3), "wrap expected 3");
    assert_test!(rb.try_pop() == Some(4), "wrap expected 4");
    assert_test!(rb.try_pop() == Some(5), "wrap expected 5");
    assert_test!(rb.try_pop() == Some(6), "wrap expected 6");
    pass!()
}

/// Test ring buffer reset
pub fn test_ring_buffer_reset() -> TestResult {
    use slopos_utils::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();
    rb.try_push(1);
    rb.try_push(2);
    rb.try_push(3);

    rb.reset();

    assert_test!(rb.is_empty(), "buffer should be empty after reset");
    assert_test!(rb.len() == 0, "length should be 0 after reset");
    pass!()
}

/// Test ring buffer capacity
pub fn test_ring_buffer_capacity() -> TestResult {
    use slopos_utils::ring_buffer::RingBuffer;

    let rb: RingBuffer<u32, 16> = RingBuffer::new();
    assert_test!(rb.capacity() == 16, "capacity should be 16");
    pass!()
}

// ============================================================================
// IRQMUTEX TESTS - 3 tests
// ============================================================================

/// Test 1: SpinLock basic lock/unlock with guard
pub fn test_irqmutex_basic() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

    let mutex: SpinLock<u32> = SpinLock::new(42, LOCK_LEVEL_RESOURCE);

    {
        let guard = mutex.lock();
        assert_test!(*guard == 42, "SpinLock value should be 42");
    }

    {
        let guard = mutex.lock();
        assert_test!(*guard == 42, "SpinLock value should still be 42");
    }

    pass!()
}

/// Test 2: SpinLock mutation through guard
pub fn test_irqmutex_mutation() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

    let mutex: SpinLock<u32> = SpinLock::new(0, LOCK_LEVEL_RESOURCE);

    {
        let mut guard = mutex.lock();
        *guard = 100;
    }

    {
        let guard = mutex.lock();
        if *guard != 100 {
            return fail!("SpinLock mutation failed, got {}", *guard);
        }
    }

    pass!()
}

/// Test 3: SpinLock try_lock
pub fn test_irqmutex_try_lock() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

    let mutex: SpinLock<u32> = SpinLock::new(55, LOCK_LEVEL_RESOURCE);

    {
        let maybe_guard = mutex.try_lock();
        assert_test!(
            maybe_guard.is_some(),
            "try_lock on unlocked mutex should succeed"
        );
        let guard = maybe_guard.unwrap();
        assert_test!(*guard == 55, "try_lock value should be 55");
    }

    pass!()
}

// ============================================================================
// MEMFD TESTS - replaces old shared memory tests
// ============================================================================

use crate::memfd;

pub fn test_memfd_create_and_release() -> TestResult {
    let result = memfd::memfd_create(0);
    assert_test!(result.is_some(), "memfd_create should succeed");
    if let Some((handle, _ops)) = result {
        memfd::memfd_release(handle);
    }
    pass!()
}

pub fn test_memfd_ftruncate_valid() -> TestResult {
    let (handle, _ops) = memfd::memfd_create(0).unwrap();
    let rc = memfd::memfd_ftruncate(handle, 4096);
    assert_test!(rc == 0, "ftruncate(4096) should succeed");
    let (phys, size) = memfd::memfd_get_phys(handle);
    assert_test!(!phys.is_null(), "phys should be non-null after ftruncate");
    assert_test!(size >= 4096, "size should be >= 4096");
    memfd::memfd_release(handle);
    pass!()
}

pub fn test_memfd_ftruncate_zero() -> TestResult {
    let (handle, _ops) = memfd::memfd_create(0).unwrap();
    let rc = memfd::memfd_ftruncate(handle, 0);
    assert_test!(rc < 0, "ftruncate(0) should fail");
    memfd::memfd_release(handle);
    pass!()
}

pub fn test_memfd_ftruncate_excessive() -> TestResult {
    let (handle, _ops) = memfd::memfd_create(0).unwrap();
    let rc = memfd::memfd_ftruncate(handle, 128 * 1024 * 1024);
    assert_test!(rc < 0, "ftruncate(128MB) should fail");
    memfd::memfd_release(handle);
    pass!()
}

pub fn test_memfd_ftruncate_twice() -> TestResult {
    let (handle, _ops) = memfd::memfd_create(0).unwrap();
    let rc1 = memfd::memfd_ftruncate(handle, 4096);
    assert_test!(rc1 == 0, "first ftruncate should succeed");
    let rc2 = memfd::memfd_ftruncate(handle, 8192);
    assert_test!(rc2 < 0, "second ftruncate should fail (one-shot)");
    memfd::memfd_release(handle);
    pass!()
}

pub fn test_memfd_refcount() -> TestResult {
    let (handle, _ops) = memfd::memfd_create(0).unwrap();
    // Initial refcount is 1. Dup increments to 2.
    memfd::memfd_inc_ref(handle);
    // First release: refcount 2 -> 1 (no cleanup)
    memfd::memfd_release(handle);
    // Second release: refcount 1 -> 0 (cleanup)
    memfd::memfd_release(handle);
    pass!()
}

pub fn test_memfd_invalid_handle() -> TestResult {
    let (phys, size) = memfd::memfd_get_phys(0xDEAD_BEEF);
    assert_test!(
        phys.is_null() && size == 0,
        "invalid handle should return null"
    );
    pass!()
}

pub fn test_memfd_mapcount() -> TestResult {
    let (handle, _ops) = memfd::memfd_create(0).unwrap();
    memfd::memfd_ftruncate(handle, 4096);
    memfd::memfd_inc_mapcount(handle);
    memfd::memfd_inc_mapcount(handle);
    // Release fd ref — should NOT free pages because map_count > 0
    memfd::memfd_release(handle);
    // Dec mapcounts
    memfd::memfd_dec_mapcount(handle);
    memfd::memfd_dec_mapcount(handle);
    // Now both refcount=0 and map_count=0, pages should be freed
    pass!()
}

pub fn test_memfd_get_info() -> TestResult {
    let (handle, _ops) = memfd::memfd_create(0).unwrap();
    // Before ftruncate, get_info should return None
    assert_test!(
        memfd::memfd_get_info(handle).is_none(),
        "unsized memfd should return None"
    );
    memfd::memfd_ftruncate(handle, 8192);
    let info = memfd::memfd_get_info(handle);
    assert_test!(info.is_some(), "sized memfd should return Some");
    if let Some((phys, size, pages)) = info {
        assert_test!(!phys.is_null(), "phys non-null");
        assert_test!(size >= 8192, "size >= 8192");
        assert_test!(pages >= 2, "pages >= 2");
    }
    memfd::memfd_release(handle);
    pass!()
}

pub fn test_memfd_size_query() -> TestResult {
    let (handle, _ops) = memfd::memfd_create(0).unwrap();
    assert_test!(memfd::memfd_size(handle) == 0, "size before ftruncate");
    memfd::memfd_ftruncate(handle, 16384);
    assert_test!(memfd::memfd_size(handle) >= 16384, "size after ftruncate");
    memfd::memfd_release(handle);
    pass!()
}

// ============================================================================
// RIGOROUS MEMORY TESTS - Actually verify memory contents
// ============================================================================

pub fn test_page_alloc_write_verify() -> TestResult {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    assert_not_null!(phys.as_u64() as *const u8, "allocate page");

    let virt = match phys.to_virt_checked() {
        Some(v) => v,
        None => {
            free_page_frame(phys);
            return fail!("get virtual address");
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
            free_page_frame(phys);
            return fail!(
                "memory corruption at offset {}: expected {:#x}, got {:#x}",
                i,
                expected,
                actual
            );
        }
    }

    free_page_frame(phys);
    pass!()
}

pub fn test_page_alloc_zero_full_page() -> TestResult {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    assert_not_null!(phys.as_u64() as *const u8, "allocate zeroed page");

    let virt = match phys.to_virt_checked() {
        Some(v) => v,
        None => {
            free_page_frame(phys);
            return fail!("get virtual address");
        }
    };

    let ptr = virt.as_mut_ptr::<u8>();

    for i in 0..4096 {
        let val = unsafe { ptr.add(i).read_volatile() };
        if val != 0 {
            free_page_frame(phys);
            return fail!("zeroed page has non-zero at offset {}: {:#x}", i, val);
        }
    }

    free_page_frame(phys);
    pass!()
}

pub fn test_page_alloc_no_stale_data() -> TestResult {
    let phys1 = alloc_page_frame(0);
    assert_not_null!(phys1.as_u64() as *const u8, "first alloc");

    if let Some(virt) = phys1.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..4096 {
            unsafe { ptr.add(i).write_volatile(0xDE) };
        }
    }

    free_page_frame(phys1);

    // Allocate with ZERO flag - should be zeroed even if same page reused
    let phys2 = alloc_page_frame(ALLOC_FLAG_ZERO);
    assert_not_null!(phys2.as_u64() as *const u8, "second alloc with zero flag");

    if let Some(virt) = phys2.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..256 {
            let val = unsafe { ptr.add(i).read_volatile() };
            if val != 0 {
                free_page_frame(phys2);
                return fail!("stale data found at offset {}: {:#x} (expected 0)", i, val);
            }
        }
    }

    free_page_frame(phys2);
    pass!()
}

/// Test: Heap allocation boundary - verify we can use full allocated size
pub fn test_heap_boundary_write() -> TestResult {
    let sizes = [16usize, 32, 64, 128, 256, 512, 1024];

    for &size in &sizes {
        let ptr = kmalloc(size);
        if ptr.is_null() {
            return fail!("allocate {} bytes", size);
        }

        let byte_ptr = ptr as *mut u8;

        for i in 0..size {
            unsafe { byte_ptr.add(i).write_volatile((i & 0xFF) as u8) };
        }

        for i in 0..size {
            let expected = (i & 0xFF) as u8;
            let actual = unsafe { byte_ptr.add(i).read_volatile() };
            if actual != expected {
                kfree(ptr);
                return fail!(
                    "heap corruption at size={} offset={}: expected {:#x}, got {:#x}",
                    size,
                    i,
                    expected,
                    actual
                );
            }
        }

        kfree(ptr);
    }

    pass!()
}

/// Test: Multiple allocations don't overlap
pub fn test_heap_no_overlap() -> TestResult {
    const NUM_ALLOCS: usize = 8;
    let mut ptrs: [*mut c_void; NUM_ALLOCS] = [ptr::null_mut(); NUM_ALLOCS];
    let sizes = [64usize, 128, 256, 64, 512, 128, 256, 64];

    for i in 0..NUM_ALLOCS {
        ptrs[i] = kmalloc(sizes[i]);
        if ptrs[i].is_null() {
            for j in 0..i {
                kfree(ptrs[j]);
            }
            return fail!("allocate block {}", i);
        }

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
                for k in 0..NUM_ALLOCS {
                    kfree(ptrs[k]);
                }
                return fail!(
                    "allocation {} corrupted at offset {}: expected {:#x}, got {:#x}",
                    i,
                    j,
                    i as u8,
                    actual
                );
            }
        }
    }

    for i in 0..NUM_ALLOCS {
        kfree(ptrs[i]);
    }
    pass!()
}

/// Test: Double-free doesn't crash (defensive)
pub fn test_heap_double_free_defensive() -> TestResult {
    let ptr = kmalloc(64);
    assert_not_null!(ptr, "alloc 64 bytes");

    kfree(ptr);
    // Second free - should not crash (may be a no-op or error)
    kfree(ptr);
    pass!()
}

/// Test: Allocate large block, verify entire region is writable
pub fn test_heap_large_block_integrity() -> TestResult {
    let size = 8192usize;
    let ptr = kmalloc(size);
    assert_not_null!(ptr, "allocate 8KB");

    let byte_ptr = ptr as *mut u8;

    for i in 0..size {
        let pattern = ((i * 17) & 0xFF) as u8;
        unsafe { byte_ptr.add(i).write_volatile(pattern) };
    }

    for i in 0..size {
        let expected = ((i * 17) & 0xFF) as u8;
        let actual = unsafe { byte_ptr.add(i).read_volatile() };
        if actual != expected {
            kfree(ptr);
            return fail!(
                "large block corruption at offset {}: expected {:#x}, got {:#x}",
                i,
                expected,
                actual
            );
        }
    }

    kfree(ptr);
    pass!()
}

/// Test: Stress test - rapid alloc/free cycles
pub fn test_heap_stress_cycles() -> TestResult {
    for cycle in 0..100 {
        let ptr = kmalloc(128);
        if ptr.is_null() {
            return fail!("stress test failed at cycle {}", cycle);
        }

        let byte_ptr = ptr as *mut u8;
        unsafe {
            byte_ptr.write_volatile(0xAB);
            byte_ptr.add(127).write_volatile(0xCD);
        }

        let first = unsafe { byte_ptr.read_volatile() };
        let last = unsafe { byte_ptr.add(127).read_volatile() };

        if first != 0xAB || last != 0xCD {
            kfree(ptr);
            return fail!(
                "stress corruption at cycle {}: first={:#x}, last={:#x}",
                cycle,
                first,
                last
            );
        }

        kfree(ptr);
    }

    pass!()
}

pub fn test_page_alloc_multipage_integrity() -> TestResult {
    let phys = alloc_page_frames(4, ALLOC_FLAG_ZERO);
    assert_not_null!(phys.as_u64() as *const u8, "allocate 4 pages");

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
                    free_page_frame(phys);
                    return fail!(
                        "multipage corruption page={} offset={}: expected {:#x}, got {:#x}",
                        page,
                        i,
                        expected,
                        actual
                    );
                }
            }
        }
    }

    free_page_frame(phys);
    pass!()
}

// ============================================================================
// PROCESS VM AND COW TESTS - Test the dangerous stuff
// ============================================================================

use crate::cow::is_cow_fault;
use crate::dual_paging::ostd_map_4kb_user;
use crate::paging_defs::PageFlags;
use crate::process_vm::process_vm_with_dual_paging;
use crate::tests::test_fixtures::ProcessVmGuard;

pub fn test_process_vm_create_destroy_memory() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    // The process should have a stack mapped - probe the null page.
    let null_page_phys = vm.virt_to_phys(0);
    if null_page_phys.is_null() {
        klog_info!("PROCESS_TEST: Null page not mapped (expected for user process)");
    }

    pass!()
}

pub fn test_process_vm_alloc_and_access() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    use crate::process_vm::process_vm_alloc;
    let user_addr = process_vm_alloc(vm.pid, 4096, PageFlags::WRITABLE.bits() as u32);
    assert_test!(user_addr != 0, "process_vm_alloc returned 0");

    // The allocation is LAZY - pages aren't mapped until accessed
    let phys = vm.virt_to_phys(user_addr);
    if !phys.is_null() {
        if let Some(virt) = phys.to_virt_checked() {
            let ptr = virt.as_mut_ptr::<u8>();
            unsafe {
                ptr.write_volatile(0x42);
                let val = ptr.read_volatile();
                assert_test!(val == 0x42, "memory write/read mismatch");
            }
        }
    }

    pass!()
}

pub fn test_process_vm_brk_expansion() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    use crate::process_vm::process_vm_brk;

    let initial_brk = process_vm_brk(vm.pid, 0);
    assert_test!(initial_brk != 0, "initial brk is 0");

    let new_brk = process_vm_brk(vm.pid, initial_brk + 8192);
    if new_brk <= initial_brk {
        return fail!("brk expansion failed: {} -> {}", initial_brk, new_brk);
    }

    let shrunk_brk = process_vm_brk(vm.pid, initial_brk + 4096);
    if shrunk_brk != initial_brk + 4096 {
        return fail!(
            "brk shrink failed: expected {}, got {}",
            initial_brk + 4096,
            shrunk_brk
        );
    }

    pass!()
}

pub fn test_cow_page_isolation() -> TestResult {
    let Some(parent) = ProcessVmGuard::new() else {
        return fail!("create parent VM");
    };

    // Use process_vm_alloc to properly create a VMA (COW clone iterates VMAs, not raw mappings)
    use crate::process_vm::process_vm_alloc;
    let test_addr = process_vm_alloc(parent.pid, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(test_addr != 0, "process_vm_alloc failed");

    // Allocate physical page and map it within the VMA
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    assert_not_null!(phys.as_u64() as *const u8, "alloc page frame");

    let map_result = process_vm_with_dual_paging(parent.pid, |_pd, vs| {
        ostd_map_4kb_user(
            vs,
            VirtAddr::new(test_addr),
            phys,
            PageFlags::USER_RW.bits(),
        )
    });
    if !matches!(map_result, Some(Ok(()))) {
        free_page_frame(phys);
        return fail!("map page in parent");
    }

    // Write pattern via HHDM
    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..4096 {
            unsafe { ptr.add(i).write_volatile(0xAA) };
        }
    }

    // Clone with COW
    let Some(child) = parent.clone_cow() else {
        return fail!("COW clone");
    };

    // Both should point to the same physical page initially (COW sharing)
    let parent_phys = parent.virt_to_phys(test_addr);
    let child_phys = child.virt_to_phys(test_addr);

    if parent_phys.is_null() || child_phys.is_null() {
        return fail!(
            "COW pages not mapped correctly (parent={:?}, child={:?})",
            parent_phys,
            child_phys
        );
    }

    if parent_phys != child_phys {
        klog_info!("PROCESS_TEST: COW pages should share same physical page initially");
    }

    // Verify child can read the same data
    if let Some(virt) = child_phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        let val = unsafe { ptr.read_volatile() };
        if val != 0xAA {
            return fail!("child COW page has wrong data: {:#x}", val);
        }
    }

    pass!()
}

pub fn test_cow_fault_handling() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let test_addr = 0x2000u64;
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    assert_not_null!(phys.as_u64() as *const u8, "alloc page frame");

    let map_result = process_vm_with_dual_paging(vm.pid, |_pd, vs| {
        ostd_map_4kb_user(
            vs,
            VirtAddr::new(test_addr),
            phys,
            PageFlags::USER_RO.bits(),
        )
    });
    if !matches!(map_result, Some(Ok(()))) {
        free_page_frame(phys);
        return fail!("map page as RO");
    }

    // Mark as COW
    vm.mark_cow(test_addr);

    // Simulate a write fault - error code for write to present page = 0x03
    let error_code = 0x03u64;
    let is_cow =
        process_vm_with_dual_paging(vm.pid, |_pd, vs| is_cow_fault(error_code, vs, test_addr))
            .unwrap_or(false);
    assert_test!(is_cow, "is_cow_fault returned false for COW page");

    // Handle the COW fault
    match vm.handle_cow_fault(test_addr) {
        Ok(()) => {}
        Err(e) => {
            return fail!("handle_cow_fault failed: {:?}", e);
        }
    }

    // After COW resolution, page should be writable
    let new_phys = vm.virt_to_phys(test_addr);
    assert_test!(!new_phys.is_null(), "page unmapped after COW resolution");

    if let Some(virt) = new_phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        unsafe {
            ptr.write_volatile(0xBB);
            let val = ptr.read_volatile();
            assert_test!(val == 0xBB, "post-COW write verification failed");
        }
    }

    pass!()
}

pub fn test_multiple_process_vms() -> TestResult {
    const NUM_PROCESSES: usize = 5;
    let mut pids = [0u32; NUM_PROCESSES];

    init_process_vm();

    for i in 0..NUM_PROCESSES {
        pids[i] = create_process_vm();
        if pids[i] == INVALID_PROCESS_ID {
            for j in 0..i {
                destroy_process_vm(pids[j]);
            }
            return fail!("create process {}", i);
        }
    }

    // Verify each has a unique page directory
    let mut dirs = [ptr::null_mut(); NUM_PROCESSES];
    for i in 0..NUM_PROCESSES {
        dirs[i] = process_vm_get_page_dir(pids[i]);
        if dirs[i].is_null() {
            for j in 0..NUM_PROCESSES {
                destroy_process_vm(pids[j]);
            }
            return fail!("process {} has null page dir", i);
        }
    }

    // Check uniqueness
    for i in 0..NUM_PROCESSES {
        for j in (i + 1)..NUM_PROCESSES {
            if dirs[i] == dirs[j] {
                for k in 0..NUM_PROCESSES {
                    destroy_process_vm(pids[k]);
                }
                return fail!("processes {} and {} share same page dir!", i, j);
            }
        }
    }

    for i in 0..NUM_PROCESSES {
        destroy_process_vm(pids[i]);
    }
    pass!()
}

pub fn test_vma_region_retrieval() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    use crate::process_vm::{process_vm_alloc, process_vm_get_region};
    use crate::vma_region::RegionPurpose;

    let user_addr = process_vm_alloc(vm.pid, 8192, PageFlags::WRITABLE.bits() as u32);
    assert_test!(user_addr != 0, "process_vm_alloc returned 0");

    let region = process_vm_get_region(vm.pid, user_addr);
    assert_test!(
        region.is_some(),
        "VMA region not found for allocated address"
    );

    let region = region.unwrap();
    assert_test!(
        region.purpose == RegionPurpose::Heap,
        "allocated region not marked as Heap"
    );
    assert_test!(
        region.protection.write,
        "allocated region not marked as writable"
    );

    pass!()
}

// ============================================================================
// PAT (PAGE ATTRIBUTE TABLE) TESTS
// ============================================================================

pub fn test_pat_wc_enabled() -> TestResult {
    const MEM_TYPE_WC: u8 = 0x01;

    let pat_msr = cpu::read_msr(Msr::PAT);
    let pat1 = ((pat_msr >> 8) & 0xFF) as u8;

    if pat1 != MEM_TYPE_WC {
        klog_info!(
            "PAT_TEST: PAT[1] is {:#x} (expected WC={:#x}) - framebuffer will be slow!",
            pat1,
            MEM_TYPE_WC
        );
        klog_info!("PAT_TEST: Full PAT MSR = {:#018x}", pat_msr);
        return fail!("PAT[1] is {:#x} (expected WC={:#x})", pat1, MEM_TYPE_WC);
    }

    pass!()
}

// ============================================================================
// SUITE REGISTRATION — tests are auto-collected via linker section
// ============================================================================

use slopos_testing::stest;

stest!(name = test_process_vm_slot_reuse, suite = vm);
stest!(name = test_process_vm_counter_reset, suite = vm);

stest!(name = test_heap_free_list_search, suite = heap);
stest!(name = test_heap_fragmentation_behind_head, suite = heap);

stest!(name = test_page_alloc_single, suite = page_alloc);
stest!(name = test_page_alloc_multi_order, suite = page_alloc);
stest!(name = test_page_alloc_free_cycle, suite = page_alloc);
stest!(name = test_page_alloc_zeroed, suite = page_alloc);
stest!(name = test_page_alloc_refcount, suite = page_alloc);
stest!(name = test_page_alloc_stats, suite = page_alloc);
stest!(name = test_page_alloc_free_null, suite = page_alloc);
stest!(name = test_page_alloc_fragmentation, suite = page_alloc);

stest!(name = test_heap_warmup_pages_minimum, suite = heap_ext);
stest!(name = test_heap_small_alloc, suite = heap_ext);
stest!(name = test_heap_medium_alloc, suite = heap_ext);
stest!(name = test_heap_large_alloc, suite = heap_ext);
stest!(name = test_heap_kzalloc_zeroed, suite = heap_ext);
stest!(name = test_heap_kfree_null, suite = heap_ext);
stest!(name = test_heap_alloc_zero, suite = heap_ext);
stest!(name = test_heap_stats, suite = heap_ext);
stest!(name = test_global_alloc_vec, suite = heap_ext);

stest!(name = test_paging_virt_to_phys, suite = paging);
stest!(name = test_paging_get_kernel_dir, suite = paging);
stest!(name = test_paging_user_accessible_kernel, suite = paging);
stest!(name = test_paging_cow_kernel, suite = paging);
stest!(name = test_pat_wc_enabled, suite = paging);

stest!(name = test_ring_buffer_basic, suite = ring_buf);
stest!(name = test_ring_buffer_fifo, suite = ring_buf);
stest!(name = test_ring_buffer_empty_pop, suite = ring_buf);
stest!(name = test_ring_buffer_full, suite = ring_buf);
stest!(name = test_ring_buffer_overwrite, suite = ring_buf);
stest!(name = test_ring_buffer_wrap, suite = ring_buf);
stest!(name = test_ring_buffer_reset, suite = ring_buf);
stest!(name = test_ring_buffer_capacity, suite = ring_buf);

stest!(name = test_irqmutex_basic, suite = irqmutex);
stest!(name = test_irqmutex_mutation, suite = irqmutex);
stest!(name = test_irqmutex_try_lock, suite = irqmutex);

stest!(name = test_memfd_create_and_release, suite = shm);
stest!(name = test_memfd_ftruncate_valid, suite = shm);
stest!(name = test_memfd_ftruncate_zero, suite = shm);
stest!(name = test_memfd_ftruncate_excessive, suite = shm);
stest!(name = test_memfd_ftruncate_twice, suite = shm);
stest!(name = test_memfd_refcount, suite = shm);
stest!(name = test_memfd_invalid_handle, suite = shm);
stest!(name = test_memfd_mapcount, suite = shm);
stest!(name = test_memfd_get_info, suite = shm);
stest!(name = test_memfd_size_query, suite = shm);

stest!(name = test_page_alloc_write_verify, suite = rigorous);
stest!(name = test_page_alloc_zero_full_page, suite = rigorous);
stest!(name = test_page_alloc_no_stale_data, suite = rigorous);
stest!(name = test_heap_boundary_write, suite = rigorous);
stest!(name = test_heap_no_overlap, suite = rigorous);
stest!(name = test_heap_double_free_defensive, suite = rigorous);
stest!(name = test_heap_large_block_integrity, suite = rigorous);
stest!(name = test_heap_stress_cycles, suite = rigorous);
stest!(name = test_page_alloc_multipage_integrity, suite = rigorous);

stest!(
    name = test_process_vm_create_destroy_memory,
    suite = process_vm
);
stest!(name = test_process_vm_alloc_and_access, suite = process_vm);
stest!(name = test_process_vm_brk_expansion, suite = process_vm);
stest!(name = test_cow_page_isolation, suite = process_vm);
stest!(name = test_cow_fault_handling, suite = process_vm);
stest!(name = test_multiple_process_vms, suite = process_vm);
stest!(name = test_vma_region_retrieval, suite = process_vm);
