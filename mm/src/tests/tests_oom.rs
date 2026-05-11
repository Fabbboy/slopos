use core::ptr;

use slopos_abi::addr::PhysAddr;
use slopos_ostd::KBox;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};
use slopos_utils::klog_info;

use crate::hhdm::PhysAddrHhdm;
use crate::kernel_heap::{get_heap_stats, kfree, kmalloc, kzalloc};
use crate::memory_init::get_memory_statistics;
use crate::page_alloc::{
    alloc_kernel_page_with, alloc_kernel_pages_with, free_page_frame, get_page_allocator_stats,
};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::{create_process_vm, destroy_process_vm, init_process_vm, process_vm_alloc};
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_ostd::mm::frame::FrameAllocOptions;

pub fn test_page_alloc_until_oom() -> TestResult {
    let mut total = 0u32;
    let mut free_before = 0u32;
    get_page_allocator_stats(&mut total, &mut free_before, ptr::null_mut());

    if free_before < 64 {
        klog_info!("OOM_TEST: Not enough free pages to test ({})", free_before);
        return pass!();
    }

    // PhysAddr is repr(transparent) over u64; all-zero == PhysAddr::NULL.
    let mut allocated: KBox<[PhysAddr; 1024]> = unsafe {
        let raw = KBox::<[u64; 1024]>::zeroed().expect("alloc");
        KBox::from_raw(KBox::into_raw(raw) as *mut [PhysAddr; 1024])
    };
    let mut count = 0usize;

    let max_alloc = (free_before as usize).min(512);
    for i in 0..max_alloc {
        let phys = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp());
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

    assert_test!(count > 0, "failed to allocate any pages");

    for i in 0..count {
        free_page_frame(allocated[i]);
    }

    let mut free_after = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_after, ptr::null_mut());

    assert_test!(free_after >= free_before - 10, "memory leak after OOM test");

    pass!()
}

pub fn test_page_alloc_fragmentation_oom() -> TestResult {
    let mut free_before = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_before, ptr::null_mut());

    if free_before < 32 {
        return pass!();
    }

    let mut pages: [PhysAddr; 16] = [PhysAddr::NULL; 16];
    for i in 0..16 {
        pages[i] = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp());
        if pages[i].is_null() {
            for j in 0..i {
                free_page_frame(pages[j]);
            }
            return pass!();
        }
    }

    for i in (0..16).step_by(2) {
        free_page_frame(pages[i]);
        pages[i] = PhysAddr::NULL;
    }

    let large = alloc_kernel_pages_with(16, FrameAllocOptions::single().with_no_pcp());

    if !large.is_null() {
        free_page_frame(large);
    }
    for i in 0..16 {
        if !pages[i].is_null() {
            free_page_frame(pages[i]);
        }
    }

    pass!()
}

pub fn test_dma_allocation_exhaustion() -> TestResult {
    let mut dma_pages: [PhysAddr; 64] = [PhysAddr::NULL; 64];
    let mut count = 0usize;

    for _ in 0..64 {
        let phys = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp().with_dma());
        if phys.is_null() {
            break;
        }
        if phys.as_u64() >= 0x0100_0000 {
            free_page_frame(phys);
            for j in 0..count {
                free_page_frame(dma_pages[j]);
            }
            return fail!("DMA allocation returned high address: {:#x}", phys.as_u64());
        }
        if count < dma_pages.len() {
            dma_pages[count] = phys;
            count += 1;
        } else {
            free_page_frame(phys);
        }
    }

    for i in 0..count {
        free_page_frame(dma_pages[i]);
    }

    pass!()
}

pub fn test_heap_alloc_pressure() -> TestResult {
    let mut stats_before = core::mem::MaybeUninit::uninit();
    get_heap_stats(stats_before.as_mut_ptr());
    let _before = unsafe { stats_before.assume_init() };

    let mut ptrs: [*mut core::ffi::c_void; 128] = [ptr::null_mut(); 128];
    let mut count = 0usize;

    for _ in 0..128 {
        let p = kmalloc(256);
        if p.is_null() {
            break;
        }
        if count < ptrs.len() {
            ptrs[count] = p;
            count += 1;
        } else {
            kfree(p);
        }
    }

    assert_test!(count > 0, "heap couldn't allocate any blocks");

    for i in 0..count {
        let byte_ptr = ptrs[i] as *mut u8;
        for j in 0..256 {
            unsafe { *byte_ptr.add(j) = (i & 0xFF) as u8 };
        }
    }

    for i in 0..count {
        let byte_ptr = ptrs[i] as *mut u8;
        for j in 0..256 {
            let val = unsafe { *byte_ptr.add(j) };
            if val != (i & 0xFF) as u8 {
                for k in 0..count {
                    kfree(ptrs[k]);
                }
                return fail!(
                    "heap corruption at block {}, offset {}: expected {:#x}, got {:#x}",
                    i,
                    j,
                    (i & 0xFF) as u8,
                    val
                );
            }
        }
    }

    for i in (0..count).rev() {
        kfree(ptrs[i]);
    }

    pass!()
}

pub fn test_heap_alloc_one_gib() -> TestResult {
    const ONE_MIB: usize = 1024 * 1024;
    const TARGET_BLOCKS: usize = 1024;
    const TARGET_BYTES: u64 = ONE_MIB as u64 * TARGET_BLOCKS as u64;

    let mut total = 0u64;
    let mut available = 0u64;
    let mut regions = 0u32;
    get_memory_statistics(&mut total, &mut available, &mut regions);

    if available < TARGET_BYTES {
        klog_info!(
            "OOM_TEST: Skipping 1GiB heap test (available {} MB)",
            available / (1024 * 1024)
        );
        return pass!();
    }

    let mut ptrs: KBox<[*mut core::ffi::c_void; TARGET_BLOCKS]> = KBox::zeroed().expect("alloc");

    for i in 0..TARGET_BLOCKS {
        let p = kmalloc(ONE_MIB);
        if p.is_null() {
            for j in 0..i {
                kfree(ptrs[j]);
            }
            return fail!("failed to allocate 1MiB block {}", i);
        }
        unsafe { *(p as *mut u8) = (i & 0xFF) as u8 };
        ptrs[i] = p;
    }

    for i in 0..TARGET_BLOCKS {
        kfree(ptrs[i]);
    }

    pass!()
}

pub fn test_process_vm_creation_pressure() -> TestResult {
    init_process_vm();

    let mut pids: [u32; 8] = [INVALID_PROCESS_ID; 8];
    let mut created = 0usize;

    for i in 0..8 {
        let pid = create_process_vm();
        if pid == INVALID_PROCESS_ID {
            klog_info!("OOM_TEST: Process creation failed at {}", i);
            break;
        }
        pids[i] = pid;
        created += 1;
    }

    assert_test!(created > 0, "couldn't create any processes");

    for i in 0..created {
        destroy_process_vm(pids[i]);
    }

    let pid = create_process_vm();
    assert_test!(
        pid != INVALID_PROCESS_ID,
        "can't create process after cleanup"
    );
    destroy_process_vm(pid);

    pass!()
}

pub fn test_heap_expansion_under_pressure() -> TestResult {
    let mut pages: [PhysAddr; 64] = [PhysAddr::NULL; 64];
    let mut page_count = 0usize;

    for _ in 0..64 {
        let phys = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp());
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

    let p = kmalloc(4096);
    if !p.is_null() {
        kfree(p);
    }

    for i in 0..page_count {
        free_page_frame(pages[i]);
    }

    pass!()
}

pub fn test_zero_flag_under_pressure() -> TestResult {
    let mut pages: [PhysAddr; 32] = [PhysAddr::NULL; 32];
    let mut count = 0usize;

    for _ in 0..32 {
        let phys = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp());
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
        return pass!();
    }

    for i in 0..count {
        if let Some(virt) = pages[i].to_virt_checked() {
            let ptr = virt.as_ptr::<u8>();
            for j in 0..PAGE_SIZE_4KB as usize {
                let val = unsafe { *ptr.add(j) };
                if val != 0 {
                    for k in 0..count {
                        free_page_frame(pages[k]);
                    }
                    return fail!(
                        "ZERO flag page {} has non-zero at offset {}: {:#x}",
                        i,
                        j,
                        val
                    );
                }
            }
        }
    }

    for i in 0..count {
        free_page_frame(pages[i]);
    }

    pass!()
}

pub fn test_kzalloc_zeroed_under_pressure() -> TestResult {
    let pollute = kmalloc(512);
    if !pollute.is_null() {
        unsafe { ptr::write_bytes(pollute as *mut u8, 0xDE, 512) };
        kfree(pollute);
    }

    let p = kzalloc(512);
    if p.is_null() {
        return pass!();
    }

    let byte_ptr = p as *const u8;
    for i in 0..512 {
        let val = unsafe { *byte_ptr.add(i) };
        if val != 0 {
            kfree(p);
            return fail!(
                "kzalloc returned non-zeroed memory at offset {}: {:#x}",
                i,
                val
            );
        }
    }

    kfree(p);
    pass!()
}

pub fn test_alloc_free_cycles_no_leak() -> TestResult {
    let mut free_start = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_start, ptr::null_mut());

    const CYCLES: usize = 100;
    const PAGES_PER_CYCLE: usize = 4;

    for _cycle in 0..CYCLES {
        let mut pages: [PhysAddr; PAGES_PER_CYCLE] = [PhysAddr::NULL; PAGES_PER_CYCLE];
        let mut allocated = 0usize;

        for i in 0..PAGES_PER_CYCLE {
            pages[i] = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp());
            if pages[i].is_null() {
                for j in 0..i {
                    free_page_frame(pages[j]);
                }
                break;
            }
            allocated += 1;
        }

        for i in 0..allocated {
            free_page_frame(pages[i]);
        }
    }

    let mut free_end = 0u32;
    get_page_allocator_stats(ptr::null_mut(), &mut free_end, ptr::null_mut());

    assert_test!(
        !(free_start > free_end && (free_start - free_end) > 16),
        "memory leak detected"
    );

    pass!()
}

pub fn test_multiorder_alloc_failure() -> TestResult {
    for order in 0..10u32 {
        let count = 1u32 << order;
        let phys = alloc_kernel_pages_with(count, FrameAllocOptions::single().with_no_pcp());

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

    pass!()
}

pub fn test_process_heap_expansion_oom() -> TestResult {
    init_process_vm();

    let pid = create_process_vm();
    assert_test!(pid != INVALID_PROCESS_ID, "create process VM");

    let mut alloc_count = 0u32;
    let mut total_size = 0u64;

    loop {
        let addr = process_vm_alloc(pid, PAGE_SIZE_4KB * 4, PageFlags::WRITABLE.bits() as u32);
        if addr == 0 {
            break;
        }
        alloc_count += 1;
        total_size += PAGE_SIZE_4KB * 4;
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
    pass!()
}

pub fn test_refcount_during_oom() -> TestResult {
    use crate::page_alloc::{page_frame_get_ref, page_frame_inc_ref};

    let phys = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp());
    if phys.is_null() {
        return pass!();
    }

    for _ in 0..5 {
        page_frame_inc_ref(phys);
    }

    let ref_count = page_frame_get_ref(phys);
    if ref_count != 6 {
        for _ in 0..6 {
            free_page_frame(phys);
        }
        return fail!("ref count should be 6, got {}", ref_count);
    }

    for _ in 0..6 {
        free_page_frame(phys);
    }

    pass!()
}

slopos_testing::stest!(name = test_page_alloc_until_oom, suite = oom);
slopos_testing::stest!(name = test_page_alloc_fragmentation_oom, suite = oom);
slopos_testing::stest!(name = test_dma_allocation_exhaustion, suite = oom);
slopos_testing::stest!(name = test_heap_alloc_pressure, suite = oom);
slopos_testing::stest!(name = test_heap_alloc_one_gib, suite = oom);
slopos_testing::stest!(name = test_process_vm_creation_pressure, suite = oom);
slopos_testing::stest!(name = test_heap_expansion_under_pressure, suite = oom);
slopos_testing::stest!(name = test_zero_flag_under_pressure, suite = oom);
slopos_testing::stest!(name = test_kzalloc_zeroed_under_pressure, suite = oom);
slopos_testing::stest!(name = test_alloc_free_cycles_no_leak, suite = oom);
slopos_testing::stest!(name = test_multiorder_alloc_failure, suite = oom);
slopos_testing::stest!(name = test_process_heap_expansion_oom, suite = oom);
slopos_testing::stest!(name = test_refcount_during_oom, suite = oom);
