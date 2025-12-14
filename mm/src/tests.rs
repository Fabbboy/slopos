
use core::mem::MaybeUninit;

use crate::kernel_heap::{get_heap_stats, kfree, kmalloc};
use crate::process_vm::{
    create_process_vm, destroy_process_vm, get_process_vm_stats, process_vm_get_page_dir, init_process_vm,
};

#[unsafe(no_mangle)]
pub extern "C" fn test_heap_free_list_search() -> i32 {
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

#[unsafe(no_mangle)]
pub extern "C" fn test_heap_fragmentation_behind_head() -> i32 {
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

#[unsafe(no_mangle)]
pub extern "C" fn test_process_vm_slot_reuse() -> i32 {
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

#[unsafe(no_mangle)]
pub extern "C" fn test_process_vm_counter_reset() -> i32 {
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
