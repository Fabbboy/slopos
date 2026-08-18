//! Kernel-half mapping tests, over the one surface that writes the
//! kernel master's page tables.
//!
//! Every case brackets itself with the buddy's free-page count and asserts
//! the exact delta: a page the unmap path forgets leaks, and one it accounts
//! for twice is reissued while a live mapping still names it.
//!
//! Each bracket is preceded by a warm-up map/unmap at the same address,
//! because the first mapping at a fresh VA also allocates the PD and the PT
//! that reach it and a kernel-half intermediate is never released.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_kernel_services::kernel_vm_space::kernel_vm_space;
use slopos_ostd::mm::frame::{Frame, KernelMeta, meta_slots_coverage};
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::kernel_mappings::{
    kernel_is_mapped, kernel_map_4kb, kernel_map_4kb_frame, kernel_map_io_4kb, kernel_unmap_4kb,
};
use crate::page_alloc::{alloc_kernel_page, get_page_allocator_stats};
use crate::paging::{PageTableLevel, kernel_pml4_phys, walk_phys};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};

/// A kernel-half window nothing else claims: above the HHDM's PML4 entry and
/// below the MMIO window. Distinct indices at every level, so a transposed
/// path slot cannot alias a correct one.
const SCRATCH_VA_BASE: u64 = 0xFFFF_9137_9ABC_D000;

#[inline]
fn scratch_va(slot: u64) -> VirtAddr {
    VirtAddr::new(SCRATCH_VA_BASE + slot * PAGE_SIZE_4KB)
}

fn free_pages() -> u32 {
    let free = get_page_allocator_stats().free;
    free
}

/// Map and unmap `va` once, so the PD and PT that reach it exist before a
/// bracketed case measures anything.
fn warm_up_path(va: VirtAddr) -> bool {
    let pa = alloc_kernel_page();
    if pa.is_null() {
        return false;
    }
    if kernel_map_4kb(va, pa, PageFlags::KERNEL_RW.bits()) != 0 {
        return false;
    }
    !kernel_unmap_4kb(va).is_null()
}

/// A physical address `META_SLOTS` does not cover — the class of paddr
/// [`kernel_map_io_4kb`] exists for. The array is sized by the highest RAM
/// frame, so nothing past its end can ever be wrapped in a `Frame`.
fn slotless_paddr() -> Option<PhysAddr> {
    let (_slots, max_pa, inited) = meta_slots_coverage();
    if !inited {
        return None;
    }
    Some(PhysAddr::new(max_pa + 0x10_0000))
}

pub fn test_kernel_mapping_round_trip_returns_frame() -> TestResult {
    let va = scratch_va(0);
    assert_test!(!kernel_is_mapped(va), "scratch VA starts unmapped");
    if !warm_up_path(va) {
        return fail!("warm-up map/unmap");
    }

    let before = free_pages();
    let pa = alloc_kernel_page();
    if pa.is_null() {
        return fail!("frame allocation");
    }
    assert_test!(
        kernel_map_4kb(va, pa, PageFlags::KERNEL_RW.bits()) == 0,
        "kernel_map_4kb succeeded"
    );
    assert_test!(kernel_is_mapped(va), "mapped VA resolves");

    let unmapped = kernel_unmap_4kb(va);
    assert_test!(unmapped == pa, "unmap returned the mapped physical page");
    assert_test!(!kernel_is_mapped(va), "unmapped VA no longer resolves");
    assert_test!(
        free_pages() == before,
        "free-page count returned to baseline"
    );
    pass!()
}

/// A kernel leaf without GLOBAL still translates, so a regression here breaks
/// no other test — the machine merely discards its kernel translations on
/// every CR3 reload.
pub fn test_kernel_mapping_leaf_is_global_supervisor() -> TestResult {
    let va = scratch_va(1);
    assert_test!(!kernel_is_mapped(va), "scratch VA starts unmapped");

    let pa = alloc_kernel_page();
    if pa.is_null() {
        return fail!("frame allocation");
    }
    assert_test!(
        kernel_map_4kb(va, pa, PageFlags::KERNEL_RW.bits()) == 0,
        "kernel_map_4kb succeeded"
    );

    let result = match walk_phys(kernel_pml4_phys(), va) {
        Ok(r) => r,
        Err(_) => {
            let _ = kernel_unmap_4kb(va);
            return fail!("walk did not reach the installed leaf");
        }
    };
    let flags = result.entry.flags();
    let level = result.level;
    let _ = kernel_unmap_4kb(va);

    assert_test!(level == PageTableLevel::One, "leaf is a 4 KiB entry");
    assert_test!(
        flags.contains(PageFlags::GLOBAL),
        "kernel-half leaf carries GLOBAL"
    );
    assert_test!(
        !flags.contains(PageFlags::USER),
        "kernel-half leaf does not carry USER"
    );
    pass!()
}

pub fn test_kernel_mapping_overlap_refused() -> TestResult {
    let va = scratch_va(2);
    assert_test!(!kernel_is_mapped(va), "scratch VA starts unmapped");
    if !warm_up_path(va) {
        return fail!("warm-up map/unmap");
    }

    let before = free_pages();
    let first = alloc_kernel_page();
    if first.is_null() {
        return fail!("first frame allocation");
    }
    assert_test!(
        kernel_map_4kb(va, first, PageFlags::KERNEL_RW.bits()) == 0,
        "first map succeeded"
    );

    let second = alloc_kernel_page();
    if second.is_null() {
        let _ = kernel_unmap_4kb(va);
        return fail!("second frame allocation");
    }
    assert_test!(
        kernel_map_4kb(va, second, PageFlags::KERNEL_RW.bits()) != 0,
        "second map over a present leaf was refused"
    );

    let resolved = walk_phys(kernel_pml4_phys(), va).map(|r| r.phys_addr);
    let unmapped = kernel_unmap_4kb(va);

    assert_test!(
        resolved == Ok(first),
        "the refused map left the first translation intact"
    );
    assert_test!(unmapped == first, "unmap returned the first page");
    assert_test!(
        free_pages() == before,
        "the refused page was not leaked — count back to baseline"
    );
    pass!()
}

/// `kernel_map_4kb_frame` takes a `Frame<KernelMeta>` directly, so a caller
/// need not destructure a typed frame into a raw address the page table
/// cannot account for.
pub fn test_kernel_map_frame_accepts_kernel_meta() -> TestResult {
    let va = scratch_va(3);
    assert_test!(!kernel_is_mapped(va), "scratch VA starts unmapped");
    if !warm_up_path(va) {
        return fail!("warm-up map/unmap");
    }

    let before = free_pages();
    let Some(frame) = Frame::<KernelMeta>::alloc_zeroed() else {
        return fail!("typed frame allocation");
    };
    let expected = PhysAddr::new(frame.paddr().as_u64());
    assert_test!(
        kernel_map_4kb_frame(va, frame, PageFlags::KERNEL_RW.bits()) == 0,
        "kernel_map_4kb_frame succeeded"
    );
    assert_test!(kernel_is_mapped(va), "mapped VA resolves");

    let unmapped = kernel_unmap_4kb(va);
    assert_test!(unmapped == expected, "unmap returned the frame's page");
    assert_test!(
        free_pages() == before,
        "free-page count returned to baseline"
    );
    pass!()
}

/// The root the read-only walker descends and the root the cursor writes must
/// be the same 4 KiB-aligned frame: a CR3 read that kept its PCID or PWT/PCD
/// bits would differ, and both are dereferenced as a table base.
pub fn test_kernel_pml4_roots_agree() -> TestResult {
    let walker_root = kernel_pml4_phys();
    let cursor_root = kernel_vm_space().lock().pml4_paddr();

    assert_test!(!walker_root.is_null(), "walker root is recorded");
    assert_test!(
        walker_root == cursor_root,
        "walker root and cursor root name the same frame"
    );
    assert_test!(
        walker_root.as_u64() & (PAGE_SIZE_4KB - 1) == 0,
        "kernel master PML4 is 4 KiB-aligned"
    );
    pass!()
}

/// A device-physical mapping owns no page: the alternative is a leaf that
/// believes it owns a device aperture and eventually hands it to the
/// allocator.
pub fn test_kernel_map_io_owns_no_frame() -> TestResult {
    let va = scratch_va(4);
    assert_test!(!kernel_is_mapped(va), "scratch VA starts unmapped");
    let Some(device_pa) = slotless_paddr() else {
        return fail!("META_SLOTS not installed");
    };
    // Warm the intermediates with a device mapping too — this path
    // allocates the same PD/PT the RAM path would.
    if kernel_map_io_4kb(va, device_pa, PageFlags::MMIO.bits()) != 0 {
        return fail!("warm-up map_io");
    }
    let _ = kernel_unmap_4kb(va);

    let before = free_pages();
    assert_test!(
        kernel_map_io_4kb(va, device_pa, PageFlags::MMIO.bits()) == 0,
        "kernel_map_io_4kb succeeded"
    );
    assert_test!(kernel_is_mapped(va), "mapped device VA resolves");
    let resolved = walk_phys(kernel_pml4_phys(), va).map(|r| r.phys_addr);
    assert_test!(
        resolved == Ok(device_pa),
        "device VA resolves to the requested aperture"
    );

    let unmapped = kernel_unmap_4kb(va);
    assert_test!(
        unmapped.is_null(),
        "unmapping a device leaf yields no page to free"
    );
    assert_test!(
        !kernel_is_mapped(va),
        "unmapped device VA no longer resolves"
    );
    assert_test!(
        free_pages() == before,
        "device map/unmap left the allocator untouched"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_kernel_mapping_round_trip_returns_frame,
    suite = kernel_mapping
);
slopos_testing::stest!(
    name = test_kernel_mapping_leaf_is_global_supervisor,
    suite = kernel_mapping
);
slopos_testing::stest!(
    name = test_kernel_mapping_overlap_refused,
    suite = kernel_mapping
);
slopos_testing::stest!(
    name = test_kernel_map_frame_accepts_kernel_meta,
    suite = kernel_mapping
);
slopos_testing::stest!(name = test_kernel_pml4_roots_agree, suite = kernel_mapping);
slopos_testing::stest!(
    name = test_kernel_map_io_owns_no_frame,
    suite = kernel_mapping
);
