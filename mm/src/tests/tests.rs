use core::ffi::c_void;
use core::ptr;
use slopos_ostd::lock_class;

use slopos_ostd::KVec;
use slopos_ostd::test_support::page_io;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_arch::cpu;
use slopos_arch::cpu::msr::Msr;
use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_not_null, assert_test, fail, pass};

use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{
    FrameAccounting, alloc_kernel_page, alloc_kernel_pages, frame_accounting, free_page_frame,
    get_page_allocator_stats,
};
use crate::paging::virt_to_phys;
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::process_vm::get_process_vm_stats;
use crate::slab::{get_heap_stats_owned, kfree, kmalloc, kzalloc};

use slopos_ostd::process::ProcessId;

/// Resolves a raw pid a test just created into the designator `mm` takes;
/// panics on a pid that names no live process.
pub(super) fn resolve_pid(pid: u32) -> slopos_ostd::process::ProcessId {
    slopos_ostd::process::ProcessId::resolve(pid).expect("a pid this test just created")
}

pub fn test_page_alloc_single() -> TestResult {
    let phys = alloc_kernel_page();
    assert_not_null!(phys.as_u64() as *const u8, "allocate single page");
    assert_test!(phys.as_u64() != 0, "allocated address is zero");
    free_page_frame(phys);
    pass!()
}

pub fn test_page_alloc_multi_order() -> TestResult {
    let phys2 = alloc_kernel_pages(2);
    assert_not_null!(phys2.as_u64() as *const u8, "allocate 2 pages");

    let phys4 = alloc_kernel_pages(4);
    if phys4.is_null() {
        free_page_frame(phys2);
        return fail!("allocate 4 pages");
    }

    let phys8 = alloc_kernel_pages(8);
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

pub fn test_page_alloc_free_cycle() -> TestResult {
    let phys1 = alloc_kernel_page();
    assert_not_null!(phys1.as_u64() as *const u8, "first alloc");

    free_page_frame(phys1);

    let phys2 = alloc_kernel_page();
    assert_not_null!(phys2.as_u64() as *const u8, "second alloc after free");

    free_page_frame(phys2);
    pass!()
}

pub fn test_page_alloc_zeroed() -> TestResult {
    let phys = alloc_kernel_page();
    assert_not_null!(phys.as_u64() as *const u8, "allocate zeroed page");

    if let Some(virt) = phys.to_virt_checked() {
        let ptr: *const u8 = virt.as_ptr();
        if let Some(i) = page_io::verify_pattern(ptr, 0, 64) {
            klog_info!(
                "PAGE_ALLOC_TEST: Zeroed page has non-zero byte at offset {}",
                i
            );
            free_page_frame(phys);
            return fail!("zeroed page has non-zero byte at offset {}", i);
        }
    }

    free_page_frame(phys);
    pass!()
}

/// `io_slices_len` truncates to the send length, not the whole pin, so a short
/// `OP_SEND_ZC` never DMAs stale tail bytes; `keepalive_frames` yields one
/// independent owning ref per page so a teardown cannot free pages mid-DMA.
pub fn test_pinned_io_slices_len_and_keepalive() -> TestResult {
    use crate::pinned_user_buffer::PinnedUserBuffer;
    let Some(pin) = PinnedUserBuffer::alloc_for_test(8192) else {
        return fail!("pin alloc_for_test failed");
    };
    let full: u32 = pin.io_slices().iter().map(|s| s.len).sum();
    assert_test!(full as usize == 8192, "full io_slices must cover the pin");
    let part: u32 = pin.io_slices_len(100).iter().map(|s| s.len).sum();
    assert_test!(part == 100, "io_slices_len(100) summed {}", part);
    let cross: u32 = pin.io_slices_len(5000).iter().map(|s| s.len).sum();
    assert_test!(cross == 5000, "io_slices_len(5000) summed {}", cross);
    let capped: u32 = pin.io_slices_len(99999).iter().map(|s| s.len).sum();
    assert_test!(capped as usize == 8192, "io_slices_len caps at pin length");
    let Some(keepalive) = pin.keepalive_frames(slopos_ostd::process::quota::root()) else {
        return fail!("keepalive_frames returned None");
    };
    assert_test!(
        keepalive.len() == 2,
        "keepalive page count {}",
        keepalive.len()
    );
    pass!()
}

/// Covers TCP `MSG_ZEROCOPY` retransmit, which reads a segment from the middle
/// of a pin rather than from its start.
pub fn test_pinned_io_runs_at_offset() -> TestResult {
    use crate::pinned_user_buffer::PinnedUserBuffer;
    let Some(pin) = PinnedUserBuffer::alloc_for_test(8192) else {
        return fail!("pin alloc_for_test failed");
    };
    let runs = pin.io_runs_at(100, 200);
    let total: u32 = runs.iter().map(|(_, l)| *l).sum();
    assert_test!(
        total as usize == 200,
        "io_runs_at(100,200) summed {}",
        total
    );
    assert_test!(!runs.is_empty(), "io_runs_at must yield a run");
    let first_pa = runs[0].0;

    // `base_off` is 0 for a test pin, so the offset lands directly in the start paddr.
    let runs0 = pin.io_runs_at(0, 200);
    assert_test!(
        first_pa == runs0[0].0 + 100,
        "offset must advance the start paddr by 100 ({} vs {})",
        first_pa,
        runs0[0].0
    );

    let cross: u32 = pin.io_runs_at(4000, 200).iter().map(|(_, l)| *l).sum();
    assert_test!(cross == 200, "cross-page io_runs_at summed {}", cross);

    assert_test!(
        pin.io_runs_at(8000, 1000).is_empty(),
        "io_runs_at past the pin must be empty"
    );
    pass!()
}

pub fn test_page_alloc_stats() -> TestResult {
    let stats = get_page_allocator_stats();
    assert_test!(stats.total != 0, "total frames is 0");
    assert_test!(
        stats.free <= stats.total && stats.allocated <= stats.total,
        "{} free and {} allocated against a total of {}",
        stats.free,
        stats.allocated,
        stats.total
    );

    let phys = alloc_kernel_pages(4);
    assert_not_null!(phys.as_u64() as *const u8, "alloc 4 pages for stats");

    // The block this test holds, not the counters every CPU writes.
    let accounting = frame_accounting(phys);
    let block_base = phys.as_u64() % (4 * PAGE_SIZE_4KB);
    free_page_frame(phys);

    assert_test!(
        accounting == FrameAccounting::HandedOut,
        "the buddy accounts a block it just handed out as {:?}",
        accounting
    );
    assert_test!(
        block_base == 0,
        "a 4-page block starts at {:#x}, off its own order's boundary",
        phys.as_u64()
    );
    pass!()
}

pub fn test_page_alloc_free_null() -> TestResult {
    let _result = free_page_frame(PhysAddr::NULL);
    pass!()
}

pub fn test_page_alloc_fragmentation() -> TestResult {
    let mut pages: [PhysAddr; 8] = [PhysAddr::NULL; 8];
    for i in 0..8 {
        pages[i] = alloc_kernel_page();
        if pages[i].is_null() {
            for j in 0..i {
                free_page_frame(pages[j]);
            }
            return fail!("failed to allocate page {}", i);
        }
    }

    free_page_frame(pages[0]);
    free_page_frame(pages[2]);
    free_page_frame(pages[4]);
    free_page_frame(pages[6]);

    let large = alloc_kernel_pages(2);
    if !large.is_null() {
        free_page_frame(large);
    }

    free_page_frame(pages[1]);
    free_page_frame(pages[3]);
    free_page_frame(pages[5]);
    free_page_frame(pages[7]);
    pass!()
}

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

pub fn test_heap_kzalloc_zeroed() -> TestResult {
    let ptr = kzalloc(128);
    assert_not_null!(ptr, "kzalloc 128 bytes");

    let bytes = ptr as *const u8;
    if let Some(i) = page_io::verify_pattern(bytes, 0, 128) {
        kfree(ptr);
        return fail!("kzalloc memory not zeroed at offset {}", i);
    }

    kfree(ptr);
    pass!()
}

pub fn test_heap_kfree_null() -> TestResult {
    kfree(ptr::null_mut());
    pass!()
}

pub fn test_heap_alloc_zero() -> TestResult {
    let ptr = kmalloc(0);
    if !ptr.is_null() {
        kfree(ptr);
        return fail!("kmalloc(0) should return null");
    }
    pass!()
}

pub fn test_heap_stats() -> TestResult {
    let before = get_heap_stats_owned();

    let ptr = kmalloc(256);
    assert_not_null!(ptr, "alloc for stats test");
    let allocated = get_heap_stats_owned();

    kfree(ptr);
    let freed = get_heap_stats_owned();

    // Byte totals move under a peer's `kfree`; the two counts only ever climb.
    if allocated.allocation_count <= before.allocation_count {
        return fail!(
            "allocation count did not advance ({} -> {})",
            before.allocation_count,
            allocated.allocation_count
        );
    }

    if freed.free_count <= allocated.free_count {
        return fail!(
            "free count did not advance ({} -> {})",
            allocated.free_count,
            freed.free_count
        );
    }

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

enum HeapReuse {
    Reused,
    Missed {
        freed: (usize, usize),
        handed_back: (usize, usize),
    },
    AllocFailed,
}

fn heap_reuse_round() -> HeapReuse {
    let p1 = kmalloc(256);
    if p1.is_null() {
        return HeapReuse::AllocFailed;
    }
    let p2 = kmalloc(256);
    if p2.is_null() {
        kfree(p1);
        return HeapReuse::AllocFailed;
    }
    let p3 = kmalloc(256);
    if p3.is_null() {
        kfree(p1);
        kfree(p2);
        return HeapReuse::AllocFailed;
    }

    let (p4, p5) = cpu::IrqDisabled::with(|_irq| {
        kfree(p1);
        kfree(p2);
        (kmalloc(256), kmalloc(256))
    });

    let from_the_freed_pair = |p: *mut c_void| p == p1 || p == p2;
    let reused = !p4.is_null()
        && !p5.is_null()
        && p4 != p5
        && from_the_freed_pair(p4)
        && from_the_freed_pair(p5);

    kfree(p3);
    if !p4.is_null() {
        kfree(p4);
    }
    if !p5.is_null() {
        kfree(p5);
    }

    if p4.is_null() || p5.is_null() {
        return HeapReuse::AllocFailed;
    }
    if reused {
        HeapReuse::Reused
    } else {
        HeapReuse::Missed {
            freed: (p1 as usize, p2 as usize),
            handed_back: (p4 as usize, p5 as usize),
        }
    }
}

/// A round may legitimately miss: a peer holding the class lock diverts the
/// free past the magazine onto a shared slab list. A broken class misses all.
pub fn test_heap_free_list_search() -> TestResult {
    const ROUNDS: u32 = 8;

    let mut last = ((0usize, 0usize), (0usize, 0usize));
    for _ in 0..ROUNDS {
        match heap_reuse_round() {
            HeapReuse::Reused => return pass!(),
            HeapReuse::Missed { freed, handed_back } => last = (freed, handed_back),
            HeapReuse::AllocFailed => return fail!("256-byte allocation for a reuse round"),
        }
    }

    let ((f1, f2), (h1, h2)) = last;
    fail!(
        "{} rounds freed {:#x} and {:#x} and were handed back {:#x} and {:#x}",
        ROUNDS,
        f1,
        f2,
        h1,
        h2
    )
}

/// After a soft reboot, x86 paging-structure caches may retain stale entries;
/// evicting them needs ≥2 physical frame allocations and ≥1 page mapping during
/// heap init (Intel Application Note 317080-002, "TLBs, Paging-Structure Caches").
pub fn test_heap_warmup_pages_minimum() -> TestResult {
    use crate::slab::HEAP_WARMUP_PAGES;

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

use crate::process_vm::{
    create_process_vm, destroy_process_vm, init_process_vm, process_vm_get_ostd_pml4_paddr,
    process_vm_handle, process_vm_with_handle,
};
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_ostd::handle::HandleError;

pub fn test_process_vm_slot_reuse() -> TestResult {
    init_process_vm();

    let initial_active = get_process_vm_stats().active_processes;

    // Designators are captured at creation and held: re-resolving a destroyed
    // pid would name whichever process later occupies the slot.
    let none = ProcessId::resolve(1).filter(|_| false);
    let mut procs = [none; 5];
    for i in 0..5 {
        let pid = create_process_vm();
        if pid == INVALID_PROCESS_ID {
            return fail!("create process {}", i);
        }
        procs[i] = ProcessId::resolve(pid);
        let Some(p) = procs[i] else {
            return fail!("resolve process {}", i);
        };
        if process_vm_get_ostd_pml4_paddr(p) == 0 {
            return fail!("address space for process {}", i);
        }
    }

    for &idx in &[1usize, 2, 3] {
        let Some(p) = procs[idx] else {
            return fail!("process {} designator", idx);
        };
        if destroy_process_vm(p) != 0 {
            return fail!("destroy process at index {}", idx);
        }
    }

    for &idx in &[1usize, 2, 3] {
        let Some(p) = procs[idx] else {
            return fail!("process {} designator", idx);
        };
        if process_vm_get_ostd_pml4_paddr(p) != 0 {
            return fail!("destroyed process {} should have no address space", idx);
        }
    }

    let (Some(p0), Some(p4)) = (procs[0], procs[4]) else {
        return fail!("surviving designators");
    };
    assert_test!(
        process_vm_get_ostd_pml4_paddr(p0) != 0,
        "surviving process 0"
    );
    assert_test!(
        process_vm_get_ostd_pml4_paddr(p4) != 0,
        "surviving process 4"
    );

    let mut reused = [none; 3];
    for i in 0..3 {
        let pid = create_process_vm();
        if pid == INVALID_PROCESS_ID {
            return fail!("create reuse process {}", i);
        }
        reused[i] = ProcessId::resolve(pid);
        let Some(p) = reused[i] else {
            return fail!("resolve reuse process {}", i);
        };
        if process_vm_get_ostd_pml4_paddr(p) == 0 {
            return fail!("reuse address space {}", i);
        }
    }

    assert_test!(
        process_vm_get_ostd_pml4_paddr(p0) != 0,
        "original process 0 still alive"
    );
    assert_test!(
        process_vm_get_ostd_pml4_paddr(p4) != 0,
        "original process 4 still alive"
    );

    assert_test!(destroy_process_vm(p0) == 0, "destroy original 0");
    assert_test!(destroy_process_vm(p4) == 0, "destroy original 4");
    for p in reused.into_iter().flatten() {
        destroy_process_vm(p);
    }

    let final_active = get_process_vm_stats().active_processes;
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

    let initial_active = get_process_vm_stats().active_processes;

    let mut pids = [0u32; 10];
    for i in 0..10 {
        pids[i] = create_process_vm();
        if pids[i] == INVALID_PROCESS_ID {
            for j in 0..i {
                destroy_process_vm(resolve_pid(pids[j]));
            }
            return fail!("create process {}", i);
        }
    }

    let active_after = get_process_vm_stats().active_processes;
    if active_after != initial_active + 10 {
        for pid in pids {
            destroy_process_vm(resolve_pid(pid));
        }
        return fail!(
            "active count should be {} + 10, got {}",
            initial_active,
            active_after
        );
    }

    for pid in pids.iter().rev() {
        if destroy_process_vm(resolve_pid(*pid)) != 0 {
            return fail!("destroy process {}", pid);
        }
    }

    let final_active = get_process_vm_stats().active_processes;
    if final_active != initial_active {
        return fail!(
            "final active {} != initial {}",
            final_active,
            initial_active
        );
    }
    pass!()
}

/// The id allocator draws lowest-free, so a freed id is redrawn immediately;
/// safe because the designator carries a generation (see
/// [`test_process_vm_handle_stale_after_reuse`]). A FIFO allocator fails here.
pub fn test_a_freed_process_id_is_reissued_promptly() -> TestResult {
    init_process_vm();

    let first = create_process_vm();
    if first == INVALID_PROCESS_ID {
        return fail!("create");
    }
    destroy_process_vm(resolve_pid(first));

    let second = create_process_vm();
    if second == INVALID_PROCESS_ID {
        return fail!("create after free");
    }
    destroy_process_vm(resolve_pid(second));

    if second != first {
        return fail!(
            "id {} was freed but the next draw was {} — the allocator is \
             delaying reuse, which the generation check made unnecessary",
            first,
            second
        );
    }
    pass!()
}

/// A `Handle<ProcessVm>` minted for one process never resolves to the process
/// that later reuses its id: the generation stamped on the slot separates them,
/// so the old handle reports `Stale` rather than a stranger's address space.
pub fn test_process_vm_handle_stale_after_reuse() -> TestResult {
    init_process_vm();

    let p1 = create_process_vm();
    if p1 == INVALID_PROCESS_ID {
        return fail!("create p1");
    }
    let Some(h1) = process_vm_handle(resolve_pid(p1)) else {
        destroy_process_vm(resolve_pid(p1));
        return fail!("handle for live p1");
    };
    if process_vm_with_handle(h1, |_| ()).is_err() {
        destroy_process_vm(resolve_pid(p1));
        return fail!("live handle should resolve");
    }

    destroy_process_vm(resolve_pid(p1));
    if process_vm_with_handle(h1, |_| ()) != Err(HandleError::NoEntry) {
        return fail!("destroyed-slot handle should be NoEntry");
    }

    // The allocator is lowest-free, so `p1`'s id comes straight back.
    let p2 = create_process_vm();
    if p2 == INVALID_PROCESS_ID {
        return fail!("create p2");
    }
    if p2 != p1 {
        return fail!("id {} was not reissued (got {})", p1, p2);
    }

    let Some(h2) = process_vm_handle(resolve_pid(p2)) else {
        destroy_process_vm(resolve_pid(p2));
        return fail!("handle for live p2");
    };

    let stale = process_vm_with_handle(h1, |_| ());
    let live = process_vm_with_handle(h2, |_| ());
    destroy_process_vm(resolve_pid(p2));

    if h1.slot() != h2.slot() {
        return fail!(
            "the reissued id bound slot {}, not slot {} — the two handles no \
             longer name the same slot and this test is not exercising reuse",
            h2.slot(),
            h1.slot()
        );
    }
    if stale != Err(HandleError::Stale) {
        return fail!(
            "a handle for the previous holder of pid {} resolved as {:?}",
            p1,
            stale
        );
    }
    if live.is_err() {
        return fail!("p2 handle should resolve");
    }
    pass!()
}

pub fn test_paging_virt_to_phys() -> TestResult {
    let kernel_addr = VirtAddr::new(test_paging_virt_to_phys as *const () as u64);
    let phys = virt_to_phys(kernel_addr);
    assert_test!(
        !phys.is_null(),
        "virt_to_phys returned null for kernel code"
    );
    pass!()
}

/// `KERNEL_VM_SPACE` wraps the live kernel-master PML4 by the time tests run.
pub fn test_paging_get_kernel_dir() -> TestResult {
    let installed = slopos_kernel_services::kernel_vm_space::try_kernel_vm_space().is_some();
    assert_test!(installed, "kernel_vm_space not installed");
    pass!()
}

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

/// The OSTD `software` field holds the AVL bits; bit 0 is the slopos COW marker.
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

pub fn test_ring_buffer_basic() -> TestResult {
    use slopos_ostd::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();
    assert_test!(rb.is_empty(), "new buffer should be empty");
    assert_test!(rb.try_push(42), "push to empty buffer failed");
    assert_test!(!rb.is_empty(), "buffer should not be empty after push");

    let val = rb.try_pop();
    assert_test!(val == Some(42), "pop returned wrong value");
    assert_test!(rb.is_empty(), "buffer should be empty after pop");
    pass!()
}

pub fn test_ring_buffer_fifo() -> TestResult {
    use slopos_ostd::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();
    rb.try_push(1);
    rb.try_push(2);
    rb.try_push(3);

    assert_test!(rb.try_pop() == Some(1), "FIFO order violated (expected 1)");
    assert_test!(rb.try_pop() == Some(2), "FIFO order violated (expected 2)");
    assert_test!(rb.try_pop() == Some(3), "FIFO order violated (expected 3)");
    pass!()
}

pub fn test_ring_buffer_empty_pop() -> TestResult {
    use slopos_ostd::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();
    assert_test!(rb.try_pop().is_none(), "pop from empty should return None");
    pass!()
}

pub fn test_ring_buffer_full() -> TestResult {
    use slopos_ostd::ring_buffer::RingBuffer;

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

pub fn test_ring_buffer_overwrite() -> TestResult {
    use slopos_ostd::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 4> = RingBuffer::new();
    for i in 0..4u32 {
        rb.push_overwrite(i);
    }

    rb.push_overwrite(99);

    assert_test!(
        rb.try_pop() == Some(1),
        "overwrite test failed (expected 1)"
    );
    pass!()
}

pub fn test_ring_buffer_wrap() -> TestResult {
    use slopos_ostd::ring_buffer::RingBuffer;

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

pub fn test_ring_buffer_reset() -> TestResult {
    use slopos_ostd::ring_buffer::RingBuffer;

    let mut rb: RingBuffer<u32, 8> = RingBuffer::new();
    rb.try_push(1);
    rb.try_push(2);
    rb.try_push(3);

    rb.reset();

    assert_test!(rb.is_empty(), "buffer should be empty after reset");
    assert_test!(rb.len() == 0, "length should be 0 after reset");
    pass!()
}

pub fn test_ring_buffer_capacity() -> TestResult {
    use slopos_ostd::ring_buffer::RingBuffer;

    let rb: RingBuffer<u32, 16> = RingBuffer::new();
    assert_test!(rb.capacity() == 16, "capacity should be 16");
    pass!()
}

pub fn test_irqmutex_basic() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

    let mutex: SpinLock<u32> =
        SpinLock::new(42, lock_class!("test.irqmutex_basic", LOCK_LEVEL_RESOURCE));

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

pub fn test_irqmutex_mutation() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

    let mutex: SpinLock<u32> = SpinLock::new(
        0,
        lock_class!("test.irqmutex_mutation", LOCK_LEVEL_RESOURCE),
    );

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

pub fn test_irqmutex_try_lock() -> TestResult {
    use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

    let mutex: SpinLock<u32> = SpinLock::new(
        55,
        lock_class!("test.irqmutex_try_lock", LOCK_LEVEL_RESOURCE),
    );

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

use crate::memfd;

pub fn test_memfd_create_and_release() -> TestResult {
    let result = memfd::memfd_create(0, slopos_ostd::process::quota::root());
    assert_test!(result.is_some(), "memfd_create should succeed");
    if let Some((_handle, _ops, backing)) = result {
        drop(backing);
    }
    pass!()
}

pub fn test_memfd_ftruncate_valid() -> TestResult {
    let (handle, _ops, backing) =
        memfd::memfd_create(0, slopos_ostd::process::quota::root()).unwrap();
    let rc = memfd::memfd_ftruncate(handle, 4096);
    assert_test!(rc == 0, "ftruncate(4096) should succeed");
    let (phys, size) = memfd::memfd_get_phys(handle);
    assert_test!(!phys.is_null(), "phys should be non-null after ftruncate");
    assert_test!(size >= 4096, "size should be >= 4096");
    drop(backing);
    pass!()
}

pub fn test_memfd_ftruncate_zero() -> TestResult {
    let (handle, _ops, backing) =
        memfd::memfd_create(0, slopos_ostd::process::quota::root()).unwrap();
    let rc = memfd::memfd_ftruncate(handle, 0);
    assert_test!(rc < 0, "ftruncate(0) should fail");
    drop(backing);
    pass!()
}

pub fn test_memfd_ftruncate_excessive() -> TestResult {
    let (handle, _ops, backing) =
        memfd::memfd_create(0, slopos_ostd::process::quota::root()).unwrap();
    let rc = memfd::memfd_ftruncate(handle, 128 * 1024 * 1024);
    assert_test!(rc < 0, "ftruncate(128MB) should fail");
    drop(backing);
    pass!()
}

pub fn test_memfd_ftruncate_twice() -> TestResult {
    let (handle, _ops, backing) =
        memfd::memfd_create(0, slopos_ostd::process::quota::root()).unwrap();
    let rc1 = memfd::memfd_ftruncate(handle, 4096);
    assert_test!(rc1 == 0, "first ftruncate should succeed");
    let rc2 = memfd::memfd_ftruncate(handle, 8192);
    assert_test!(rc2 < 0, "second ftruncate should fail (one-shot)");
    drop(backing);
    pass!()
}

pub fn test_memfd_refcount() -> TestResult {
    let (handle, _ops, backing) =
        memfd::memfd_create(0, slopos_ostd::process::quota::root()).unwrap();
    let alias = backing.clone();
    drop(backing);
    assert_test!(
        memfd::memfd_ftruncate(handle, 4096) == 0,
        "memfd must stay alive while an alias holds it"
    );
    drop(alias);
    assert_test!(
        memfd::memfd_size(handle) == 0,
        "memfd must be gone after the last alias drops"
    );
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
    let (handle, _ops, backing) =
        memfd::memfd_create(0, slopos_ostd::process::quota::root()).unwrap();
    let h = memfd::handle_from_raw(handle);
    memfd::memfd_ftruncate(handle, 4096);
    memfd::memfd_inc_mapcount_by(h, 1);
    memfd::memfd_inc_mapcount_by(h, 1);
    // Closing the fd side must not free pages while map_count > 0; the pages go
    // on the second dec.
    drop(backing);
    memfd::memfd_dec_mapcount_by(h, 1);
    memfd::memfd_dec_mapcount_by(h, 1);
    pass!()
}

pub fn test_memfd_get_info() -> TestResult {
    let (handle, _ops, backing) =
        memfd::memfd_create(0, slopos_ostd::process::quota::root()).unwrap();
    let h = memfd::handle_from_raw(handle);
    assert_test!(
        memfd::memfd_get_info(h).is_none(),
        "unsized memfd should return None"
    );
    memfd::memfd_ftruncate(handle, 8192);
    let info = memfd::memfd_get_info(h);
    assert_test!(info.is_some(), "sized memfd should return Some");
    if let Some((phys, size, pages)) = info {
        assert_test!(!phys.is_null(), "phys non-null");
        assert_test!(size >= 8192, "size >= 8192");
        assert_test!(pages >= 2, "pages >= 2");
    }
    drop(backing);
    pass!()
}

pub fn test_memfd_size_query() -> TestResult {
    let (handle, _ops, backing) =
        memfd::memfd_create(0, slopos_ostd::process::quota::root()).unwrap();
    assert_test!(memfd::memfd_size(handle) == 0, "size before ftruncate");
    memfd::memfd_ftruncate(handle, 16384);
    assert_test!(memfd::memfd_size(handle) >= 16384, "size after ftruncate");
    drop(backing);
    pass!()
}

pub fn test_page_alloc_write_verify() -> TestResult {
    let phys = alloc_kernel_page();
    assert_not_null!(phys.as_u64() as *const u8, "allocate page");

    let virt = match phys.to_virt_checked() {
        Some(v) => v,
        None => {
            free_page_frame(phys);
            return fail!("get virtual address");
        }
    };

    let ptr = virt.as_mut_ptr::<u8>();

    for i in 0..4096 {
        let val = if i % 2 == 0 { 0xAA } else { 0x55 };
        page_io::write_volatile_byte(ptr, i, val);
    }

    for i in 0..4096 {
        let expected = if i % 2 == 0 { 0xAA } else { 0x55 };
        let actual = page_io::read_volatile_byte(ptr, i);
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
    let phys = alloc_kernel_page();
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
        let val = page_io::read_volatile_byte(ptr, i);
        if val != 0 {
            free_page_frame(phys);
            return fail!("zeroed page has non-zero at offset {}: {:#x}", i, val);
        }
    }

    free_page_frame(phys);
    pass!()
}

pub fn test_page_alloc_no_stale_data() -> TestResult {
    let phys1 = alloc_kernel_page();
    assert_not_null!(phys1.as_u64() as *const u8, "first alloc");

    if let Some(virt) = phys1.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        page_io::fill_volatile(ptr, 0xDE, 4096);
    }

    free_page_frame(phys1);

    let phys2 = alloc_kernel_page();
    assert_not_null!(phys2.as_u64() as *const u8, "second alloc with zero flag");

    if let Some(virt) = phys2.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        for i in 0..256 {
            let val = page_io::read_volatile_byte(ptr, i);
            if val != 0 {
                free_page_frame(phys2);
                return fail!("stale data found at offset {}: {:#x} (expected 0)", i, val);
            }
        }
    }

    free_page_frame(phys2);
    pass!()
}

pub fn test_heap_boundary_write() -> TestResult {
    let sizes = [16usize, 32, 64, 128, 256, 512, 1024];

    for &size in &sizes {
        let ptr = kmalloc(size);
        if ptr.is_null() {
            return fail!("allocate {} bytes", size);
        }

        let byte_ptr = ptr as *mut u8;

        for i in 0..size {
            page_io::write_volatile_byte(byte_ptr, i, (i & 0xFF) as u8);
        }

        for i in 0..size {
            let expected = (i & 0xFF) as u8;
            let actual = page_io::read_volatile_byte(byte_ptr, i);
            if actual != expected {
                scrub_chunk(ptr, size);
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

        scrub_chunk(ptr, size);
        kfree(ptr);
    }

    pass!()
}

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
            page_io::write_volatile_byte(byte_ptr, j, i as u8);
        }
    }

    for i in 0..NUM_ALLOCS {
        let byte_ptr = ptrs[i] as *mut u8;
        for j in 0..sizes[i] {
            let actual = page_io::read_volatile_byte(byte_ptr, j);
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

pub fn test_heap_double_free_defensive() -> TestResult {
    let ptr = kmalloc(64);
    assert_not_null!(ptr, "alloc 64 bytes");

    kfree(ptr);
    kfree(ptr);
    pass!()
}

pub fn test_heap_large_block_integrity() -> TestResult {
    let size = 8192usize;
    let ptr = kmalloc(size);
    assert_not_null!(ptr, "allocate 8KB");

    let byte_ptr = ptr as *mut u8;

    for i in 0..size {
        let pattern = ((i * 17) & 0xFF) as u8;
        page_io::write_volatile_byte(byte_ptr, i, pattern);
    }

    for i in 0..size {
        let expected = ((i * 17) & 0xFF) as u8;
        let actual = page_io::read_volatile_byte(byte_ptr, i);
        if actual != expected {
            scrub_chunk(ptr, size);
            kfree(ptr);
            return fail!(
                "large block corruption at offset {}: expected {:#x}, got {:#x}",
                i,
                expected,
                actual
            );
        }
    }

    scrub_chunk(ptr, size);
    kfree(ptr);
    pass!()
}

pub fn test_heap_stress_cycles() -> TestResult {
    for cycle in 0..100 {
        let ptr = kmalloc(128);
        if ptr.is_null() {
            return fail!("stress test failed at cycle {}", cycle);
        }

        let byte_ptr = ptr as *mut u8;
        page_io::write_volatile_byte(byte_ptr, 0, 0xAB);
        page_io::write_volatile_byte(byte_ptr, 127, 0xCD);

        let first = page_io::read_volatile_byte(byte_ptr, 0);
        let last = page_io::read_volatile_byte(byte_ptr, 127);

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
    let phys = alloc_kernel_pages(4);
    assert_not_null!(phys.as_u64() as *const u8, "allocate 4 pages");

    for page in 0..4u64 {
        let page_phys = PhysAddr::new(phys.as_u64() + page * 4096);
        if let Some(virt) = page_phys.to_virt_checked() {
            let ptr = virt.as_mut_ptr::<u8>();
            for i in 0..4096 {
                let pattern = ((page as u8).wrapping_mul(17)).wrapping_add((i & 0xFF) as u8);
                page_io::write_volatile_byte(ptr, i, pattern);
            }
        }
    }

    for page in 0..4u64 {
        let page_phys = PhysAddr::new(phys.as_u64() + page * 4096);
        if let Some(virt) = page_phys.to_virt_checked() {
            let ptr = virt.as_mut_ptr::<u8>();
            for i in 0..4096 {
                let expected = ((page as u8).wrapping_mul(17)).wrapping_add((i & 0xFF) as u8);
                let actual = page_io::read_volatile_byte(ptr, i);
                if actual != expected {
                    scrub_pages(phys, 4);
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

    scrub_pages(phys, 4);
    free_page_frame(phys);
    pass!()
}

/// The buddy allocator does not zero on free, so a test's fill pattern would
/// otherwise leak into unrelated kernel allocations later in the run.
fn scrub_pages(phys: PhysAddr, npages: u64) {
    for page in 0..npages {
        let page_phys = PhysAddr::new(phys.as_u64() + page * 4096);
        if let Some(virt) = page_phys.to_virt_checked() {
            let ptr = virt.as_mut_ptr::<u8>();
            page_io::write_bytes(ptr, 0, 4096);
        }
    }
}

/// Same rationale as [`scrub_pages`]: the slab does not zero on free either.
fn scrub_chunk(ptr: *mut core::ffi::c_void, len: usize) {
    if ptr.is_null() {
        return;
    }
    page_io::write_bytes(ptr as *mut u8, 0, len);
}

use crate::cow::is_cow_fault;
use crate::paging_defs::PageFlags;
use crate::process_vm::process_vm_with_vm_space;
use crate::tests::test_fixtures::ProcessVmGuard;
use crate::user_mappings::ostd_map_4kb_user;

pub fn test_process_vm_create_destroy_memory() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

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
    let user_addr = process_vm_alloc(vm.process, 4096, PageFlags::WRITABLE.bits() as u32);
    assert_test!(user_addr != 0, "process_vm_alloc returned 0");

    // The allocation is lazy: pages are not mapped until accessed.
    let phys = vm.virt_to_phys(user_addr);
    if !phys.is_null() {
        if let Some(virt) = phys.to_virt_checked() {
            let ptr = virt.as_mut_ptr::<u8>();
            page_io::write_volatile_byte(ptr, 0, 0x42);
            let val = page_io::read_volatile_byte(ptr, 0);
            assert_test!(val == 0x42, "memory write/read mismatch");
        }
    }

    pass!()
}

pub fn test_process_vm_brk_expansion() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    use crate::process_vm::process_vm_brk;

    let initial_brk = process_vm_brk(vm.process, 0);
    assert_test!(initial_brk != 0, "initial brk is 0");

    let new_brk = process_vm_brk(vm.process, initial_brk + 8192);
    if new_brk <= initial_brk {
        return fail!("brk expansion failed: {} -> {}", initial_brk, new_brk);
    }

    let shrunk_brk = process_vm_brk(vm.process, initial_brk + 4096);
    if shrunk_brk != initial_brk + 4096 {
        return fail!(
            "brk shrink failed: expected {}, got {}",
            initial_brk + 4096,
            shrunk_brk
        );
    }

    pass!()
}

pub fn test_process_vm_brk_byte_granular() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    use crate::process_vm::process_vm_brk;

    let base = process_vm_brk(vm.process, 0);
    assert_test!(base != 0, "initial brk is 0");
    assert_test!(
        base & (PAGE_SIZE_4KB - 1) == 0,
        "initial brk not page-aligned"
    );

    let big = base + 64 * PAGE_SIZE_4KB;
    assert_test!(
        process_vm_brk(vm.process, big) == big,
        "aligned grow not exact"
    );

    // The userland allocator's top-trim shape: an unaligned break a few bytes
    // shy of a page boundary, which its success check compares for exact equality.
    let trimmed = base + 16 * PAGE_SIZE_4KB - 8;
    assert_test!(
        process_vm_brk(vm.process, trimmed) == trimmed,
        "unaligned shrink did not return the requested break"
    );
    assert_test!(
        process_vm_brk(vm.process, 0) == trimmed,
        "break not persisted byte-granular"
    );

    let tail_page = trimmed & !(PAGE_SIZE_4KB - 1);
    assert_test!(
        !vm.virt_to_phys(tail_page).is_null(),
        "partial tail page below the break got unmapped"
    );
    assert_test!(
        vm.virt_to_phys(tail_page + PAGE_SIZE_4KB).is_null(),
        "page above the rounded break still mapped after shrink"
    );

    let regrown = base + 32 * PAGE_SIZE_4KB + 24;
    assert_test!(
        process_vm_brk(vm.process, regrown) == regrown,
        "unaligned regrow did not return the requested break"
    );
    assert_test!(
        !vm.virt_to_phys(regrown & !(PAGE_SIZE_4KB - 1)).is_null(),
        "page under the regrown break not mapped"
    );

    pass!()
}

pub fn test_cow_page_isolation() -> TestResult {
    let Some(parent) = ProcessVmGuard::new() else {
        return fail!("create parent VM");
    };

    // COW clone iterates VMAs, not raw mappings, so the page needs a real VMA.
    use crate::process_vm::process_vm_alloc;
    let test_addr = process_vm_alloc(
        parent.process,
        PAGE_SIZE_4KB,
        PageFlags::WRITABLE.bits() as u32,
    );
    assert_test!(test_addr != 0, "process_vm_alloc failed");

    let phys = alloc_kernel_page();
    assert_not_null!(phys.as_u64() as *const u8, "alloc page frame");

    let map_result = process_vm_with_vm_space(parent.process, |vs| {
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

    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        page_io::fill_volatile(ptr, 0xAA, 4096);
    }

    let Some(child) = parent.clone_cow() else {
        return fail!("COW clone");
    };

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

    if let Some(virt) = child_phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        let val = page_io::read_volatile_byte(ptr, 0);
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
    let phys = alloc_kernel_page();
    assert_not_null!(phys.as_u64() as *const u8, "alloc page frame");

    let map_result = process_vm_with_vm_space(vm.process, |vs| {
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

    vm.mark_cow(test_addr);

    // 0x03: write to a present page.
    let error_code = 0x03u64;
    let is_cow = process_vm_with_vm_space(vm.process, |vs| is_cow_fault(error_code, vs, test_addr))
        .unwrap_or(false);
    assert_test!(is_cow, "is_cow_fault returned false for COW page");

    match vm.handle_cow_fault(test_addr) {
        Ok(()) => {}
        Err(e) => {
            return fail!("handle_cow_fault failed: {:?}", e);
        }
    }

    let new_phys = vm.virt_to_phys(test_addr);
    assert_test!(!new_phys.is_null(), "page unmapped after COW resolution");

    if let Some(virt) = new_phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        page_io::write_volatile_byte(ptr, 0, 0xBB);
        let val = page_io::read_volatile_byte(ptr, 0);
        assert_test!(val == 0xBB, "post-COW write verification failed");
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
                destroy_process_vm(resolve_pid(pids[j]));
            }
            return fail!("create process {}", i);
        }
    }

    let mut roots = [0u64; NUM_PROCESSES];
    for i in 0..NUM_PROCESSES {
        roots[i] = process_vm_get_ostd_pml4_paddr(resolve_pid(pids[i]));
        if roots[i] == 0 {
            for j in 0..NUM_PROCESSES {
                destroy_process_vm(resolve_pid(pids[j]));
            }
            return fail!("process {} has no address space", i);
        }
    }

    for i in 0..NUM_PROCESSES {
        for j in (i + 1)..NUM_PROCESSES {
            if roots[i] == roots[j] {
                for k in 0..NUM_PROCESSES {
                    destroy_process_vm(resolve_pid(pids[k]));
                }
                return fail!("processes {} and {} share an address space!", i, j);
            }
        }
    }

    for i in 0..NUM_PROCESSES {
        destroy_process_vm(resolve_pid(pids[i]));
    }
    pass!()
}

pub fn test_vma_region_retrieval() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    use crate::process_vm::{process_vm_alloc, process_vm_get_region};
    use crate::vma_region::RegionPurpose;

    let user_addr = process_vm_alloc(vm.process, 8192, PageFlags::WRITABLE.bits() as u32);
    assert_test!(user_addr != 0, "process_vm_alloc returned 0");

    let region = process_vm_get_region(vm.process, user_addr);
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

use slopos_testing::stest;

stest!(name = test_process_vm_slot_reuse, suite = vm);
stest!(name = test_process_vm_counter_reset, suite = vm);
stest!(name = test_process_vm_handle_stale_after_reuse, suite = vm);
stest!(
    name = test_a_freed_process_id_is_reissued_promptly,
    suite = vm
);

stest!(name = test_heap_free_list_search, suite = heap);
stest!(name = test_heap_fragmentation_behind_head, suite = heap);

stest!(name = test_page_alloc_single, suite = page_alloc);
stest!(name = test_page_alloc_multi_order, suite = page_alloc);
stest!(name = test_page_alloc_free_cycle, suite = page_alloc);
stest!(name = test_page_alloc_zeroed, suite = page_alloc);
stest!(
    name = test_pinned_io_slices_len_and_keepalive,
    suite = page_alloc
);
stest!(name = test_pinned_io_runs_at_offset, suite = page_alloc);
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
stest!(name = test_process_vm_brk_byte_granular, suite = process_vm);
stest!(name = test_cow_page_isolation, suite = process_vm);
stest!(name = test_cow_fault_handling, suite = process_vm);
stest!(name = test_multiple_process_vms, suite = process_vm);
stest!(name = test_vma_region_retrieval, suite = process_vm);
