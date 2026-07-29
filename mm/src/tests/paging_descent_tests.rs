//! Page-table descent tests for the kernel-half mapper.
//!
//! `map_page_4kb` is the only in-tree caller of the map descent and it
//! always passes `PAGE_SIZE_4KB`, and every `unmap_page` caller unmaps a
//! 4 KiB page the kernel VA allocator handed out. So the huge-leaf arms
//! of the unmap descent and both huge-page splits are unreachable from
//! any other test and from any boot — these drive them directly, over a
//! scratch page-table tree that is never installed in CR3.
//!
//! Every case brackets itself with the buddy's free-page count and
//! asserts the exact delta. That is what catches the two ways the
//! descent's frame bookkeeping can go wrong: a physical address the
//! prune forgets leaks a page, and one it tracks twice hands the same
//! frame to the buddy on two paths.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::util::ptr_buf;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::hhdm::PhysAddrHhdm;
use crate::mmu::MmContextId;
use crate::page_alloc::{alloc_kernel_page, free_page_frame, get_page_allocator_stats};
use crate::paging::page_table_defs::{PageTable, PageTableEntry, PageTableLevel};
use crate::paging::tables::{ProcessPageDir, map_page_in_directory, unmap_page_in_directory};
use crate::paging_defs::{PAGE_SIZE_2MB, PAGE_SIZE_4KB, PageFlags};

/// A scratch tree rooted at a PML4 the kernel never installs, plus the
/// frames it owns so a failing case can still hand them all back.
struct ScratchTree {
    dir: ProcessPageDir,
    /// Frames this tree allocated, in allocation order. The descent
    /// frees some of them; `release` skips whatever it already took.
    owned: [PhysAddr; 8],
    owned_len: usize,
}

fn free_pages() -> u32 {
    let mut free = 0u32;
    get_page_allocator_stats(core::ptr::null_mut(), &mut free, core::ptr::null_mut());
    free
}

fn new_table() -> Option<PhysAddr> {
    let phys = alloc_kernel_page();
    if phys.is_null() {
        return None;
    }
    ptr_buf::with_ref_mut::<PageTable, _>(phys.to_virt().as_mut_ptr(), PageTable::zero);
    Some(phys)
}

fn set_entry(table: PhysAddr, index: usize, entry: PageTableEntry) {
    ptr_buf::with_ref_mut::<PageTable, _>(table.to_virt().as_mut_ptr(), |t| {
        *t.entry_mut(index) = entry;
    });
}

fn entry(table: PhysAddr, index: usize) -> PageTableEntry {
    ptr_buf::with_ref::<PageTable, _>(table.to_virt().as_mut_ptr(), |t| *t.entry(index))
}

impl ScratchTree {
    /// A tree with just a PML4. `vaddr`'s levels are filled in by the
    /// `link_*` helpers.
    fn new() -> Option<Self> {
        let pml4 = new_table()?;
        let mut owned = [PhysAddr::NULL; 8];
        owned[0] = pml4;
        Some(Self {
            dir: ProcessPageDir::new(pml4.to_virt().as_mut_ptr(), pml4, 0, MmContextId::INVALID),
            owned,
            owned_len: 1,
        })
    }

    fn root(&self) -> PhysAddr {
        self.owned[0]
    }

    fn dir_ptr(&mut self) -> *mut ProcessPageDir {
        core::ptr::from_mut(&mut self.dir)
    }

    fn track(&mut self, phys: PhysAddr) {
        self.owned[self.owned_len] = phys;
        self.owned_len += 1;
    }

    /// Link a fresh table under `parent[index]` and return it.
    fn link_table(&mut self, parent: PhysAddr, index: usize) -> Option<PhysAddr> {
        let child = new_table()?;
        self.track(child);
        set_entry(
            parent,
            index,
            PageTableEntry::new(child, PageFlags::PRESENT | PageFlags::WRITABLE),
        );
        Some(child)
    }

    /// Link a huge leaf under `parent[index]`, pointing at `leaf`.
    fn link_huge_leaf(&self, parent: PhysAddr, index: usize, leaf: PhysAddr) {
        set_entry(
            parent,
            index,
            PageTableEntry::new(
                leaf,
                PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::HUGE,
            ),
        );
    }

    /// Hand back every frame the descent did not already free. A frame
    /// the descent took is no longer ours, so `frame_is_tracked` gates
    /// the release: a double free here would mask the very corruption
    /// these tests exist to catch.
    fn release(&self) {
        for phys in self.owned.iter().take(self.owned_len) {
            if crate::page_alloc::page_frame_is_tracked(*phys) != 0 {
                free_page_frame(*phys);
            }
        }
    }
}

/// Run `body` with a fresh scratch tree, asserting the buddy's
/// free-page count returns to where it started once everything is
/// released. `body` reports its own failure; the accounting check runs
/// either way.
fn with_scratch_tree(body: impl FnOnce(&mut ScratchTree) -> TestResult) -> TestResult {
    let before = free_pages();
    let Some(mut tree) = ScratchTree::new() else {
        return fail!("scratch PML4 allocation");
    };
    let result = body(&mut tree);
    tree.release();
    let after = free_pages();
    if result.is_pass() && after != before {
        return fail!("free-page count did not return to baseline");
    }
    result
}

/// A VA whose four indices are all distinct and non-zero, so a
/// transposed path slot cannot alias a correct one.
const PROBE_VA_RAW: u64 = 0x0000_5137_9ABC_D000;

#[inline]
fn probe_va() -> VirtAddr {
    VirtAddr::new(PROBE_VA_RAW)
}

/// 1 GiB leaf: the PDPT entry is the leaf. Unmapping it must return the
/// leaf phys, clear the PML4 entry, and release the PDPT frame — one
/// prune level.
pub fn test_paging_unmap_huge_1gib() -> TestResult {
    with_scratch_tree(|tree| {
        let root = tree.root();
        let l4 = PageTableLevel::Four.index_of(probe_va());
        let l3 = PageTableLevel::Three.index_of(probe_va());

        let Some(pdpt) = tree.link_table(root, l4) else {
            return fail!("PDPT allocation");
        };
        let Some(leaf) = new_table() else {
            return fail!("leaf frame allocation");
        };
        tree.track(leaf);
        tree.link_huge_leaf(pdpt, l3, leaf);

        let before = free_pages();
        let got = unmap_page_in_directory(tree.dir_ptr(), probe_va());
        let after = free_pages();

        assert_test!(got == leaf, "1 GiB unmap returned the huge leaf phys");
        assert_test!(
            !entry(root, l4).is_present(),
            "1 GiB unmap cleared the PML4 entry"
        );
        assert_test!(
            after == before + 1,
            "1 GiB unmap released exactly the PDPT frame"
        );
        pass!()
    })
}

/// 2 MiB leaf: the PD entry is the leaf, and the prune walks up two
/// levels, releasing the PD and then the PDPT.
pub fn test_paging_unmap_huge_2mib() -> TestResult {
    with_scratch_tree(|tree| {
        let root = tree.root();
        let l4 = PageTableLevel::Four.index_of(probe_va());
        let l3 = PageTableLevel::Three.index_of(probe_va());
        let l2 = PageTableLevel::Two.index_of(probe_va());

        let Some(pdpt) = tree.link_table(root, l4) else {
            return fail!("PDPT allocation");
        };
        let Some(pd) = tree.link_table(pdpt, l3) else {
            return fail!("PD allocation");
        };
        let Some(leaf) = new_table() else {
            return fail!("leaf frame allocation");
        };
        tree.track(leaf);
        tree.link_huge_leaf(pd, l2, leaf);

        let before = free_pages();
        let got = unmap_page_in_directory(tree.dir_ptr(), probe_va());
        let after = free_pages();

        assert_test!(got == leaf, "2 MiB unmap returned the huge leaf phys");
        assert_test!(
            !entry(root, l4).is_present(),
            "2 MiB unmap cleared the PML4 entry"
        );
        assert_test!(
            after == before + 2,
            "2 MiB unmap released the PD and the PDPT"
        );
        pass!()
    })
}

/// 4 KiB leaf: the full four-level prune releases PT, PD and PDPT.
pub fn test_paging_unmap_4kib_prunes_three_levels() -> TestResult {
    with_scratch_tree(|tree| {
        let root = tree.root();
        let l4 = PageTableLevel::Four.index_of(probe_va());
        let l3 = PageTableLevel::Three.index_of(probe_va());
        let l2 = PageTableLevel::Two.index_of(probe_va());
        let l1 = PageTableLevel::One.index_of(probe_va());

        let Some(pdpt) = tree.link_table(root, l4) else {
            return fail!("PDPT allocation");
        };
        let Some(pd) = tree.link_table(pdpt, l3) else {
            return fail!("PD allocation");
        };
        let Some(pt) = tree.link_table(pd, l2) else {
            return fail!("PT allocation");
        };
        let Some(leaf) = new_table() else {
            return fail!("leaf frame allocation");
        };
        tree.track(leaf);
        set_entry(
            pt,
            l1,
            PageTableEntry::new(leaf, PageFlags::PRESENT | PageFlags::WRITABLE),
        );

        let before = free_pages();
        let got = unmap_page_in_directory(tree.dir_ptr(), probe_va());
        let after = free_pages();

        assert_test!(got == leaf, "4 KiB unmap returned the leaf phys");
        assert_test!(
            !entry(root, l4).is_present(),
            "4 KiB unmap cleared the PML4 entry"
        );
        assert_test!(
            after == before + 3,
            "4 KiB unmap released the PT, the PD and the PDPT"
        );
        pass!()
    })
}

/// A present path with an absent leaf: nothing to return, but the three
/// now-empty intermediates are still released.
pub fn test_paging_unmap_absent_leaf_still_prunes() -> TestResult {
    with_scratch_tree(|tree| {
        let root = tree.root();
        let l4 = PageTableLevel::Four.index_of(probe_va());
        let l3 = PageTableLevel::Three.index_of(probe_va());
        let l2 = PageTableLevel::Two.index_of(probe_va());

        let Some(pdpt) = tree.link_table(root, l4) else {
            return fail!("PDPT allocation");
        };
        let Some(pd) = tree.link_table(pdpt, l3) else {
            return fail!("PD allocation");
        };
        if tree.link_table(pd, l2).is_none() {
            return fail!("PT allocation");
        }

        let before = free_pages();
        let got = unmap_page_in_directory(tree.dir_ptr(), probe_va());
        let after = free_pages();

        assert_test!(got.is_null(), "absent leaf unmaps to NULL");
        assert_test!(
            !entry(root, l4).is_present(),
            "absent-leaf unmap cleared the PML4 entry"
        );
        assert_test!(
            after == before + 3,
            "absent-leaf unmap still released all three intermediates"
        );
        pass!()
    })
}

/// A 4 KiB map into a tree whose PDPT entry is a 1 GiB leaf must demote
/// it: a PD and a PT appear, the requested VA resolves to the new frame,
/// and a neighbouring 2 MiB offset inside the demoted range still
/// resolves to where the huge leaf had it.
pub fn test_paging_map_splits_1gib_leaf() -> TestResult {
    with_scratch_tree(|tree| {
        let root = tree.root();
        let l4 = PageTableLevel::Four.index_of(probe_va());
        let l3 = PageTableLevel::Three.index_of(probe_va());

        let Some(pdpt) = tree.link_table(root, l4) else {
            return fail!("PDPT allocation");
        };
        // A 1 GiB-aligned base the demoted children will subdivide.
        let huge_base = PhysAddr::new(0x4000_0000);
        tree.link_huge_leaf(pdpt, l3, huge_base);

        let Some(target) = new_table() else {
            return fail!("target frame allocation");
        };
        tree.track(target);

        let before = free_pages();
        let rc = map_page_in_directory(
            tree.dir_ptr(),
            probe_va(),
            target,
            (PageFlags::PRESENT | PageFlags::WRITABLE).bits(),
            PAGE_SIZE_4KB,
        );
        let after = free_pages();

        assert_test!(rc == 0, "map over a 1 GiB leaf succeeded");
        assert_test!(
            before == after + 2,
            "splitting a 1 GiB leaf allocated a PD and a PT"
        );

        // Track what the split allocated so `release` hands it back.
        let pd = entry(pdpt, l3).address();
        tree.track(pd);
        let l2 = PageTableLevel::Two.index_of(probe_va());
        tree.track(entry(pd, l2).address());

        assert_test!(
            walk_leaf(root, probe_va()) == Some(target),
            "the mapped VA resolves to the new frame"
        );

        // A 2 MiB-aligned neighbour inside the demoted 1 GiB range keeps
        // the translation the huge leaf gave it.
        let neighbour = VirtAddr::new(probe_va().as_u64() ^ PAGE_SIZE_2MB);
        let expected = huge_base.offset(neighbour.as_u64() & (0x4000_0000 - 1));
        assert_test!(
            walk_leaf(root, neighbour) == Some(expected),
            "a neighbour inside the demoted range keeps its translation"
        );
        pass!()
    })
}

/// The same one level down: a 2 MiB leaf demoted into a PT.
pub fn test_paging_map_splits_2mib_leaf() -> TestResult {
    with_scratch_tree(|tree| {
        let root = tree.root();
        let l4 = PageTableLevel::Four.index_of(probe_va());
        let l3 = PageTableLevel::Three.index_of(probe_va());
        let l2 = PageTableLevel::Two.index_of(probe_va());

        let Some(pdpt) = tree.link_table(root, l4) else {
            return fail!("PDPT allocation");
        };
        let Some(pd) = tree.link_table(pdpt, l3) else {
            return fail!("PD allocation");
        };
        let huge_base = PhysAddr::new(0x4000_0000);
        tree.link_huge_leaf(pd, l2, huge_base);

        let Some(target) = new_table() else {
            return fail!("target frame allocation");
        };
        tree.track(target);

        let before = free_pages();
        let rc = map_page_in_directory(
            tree.dir_ptr(),
            probe_va(),
            target,
            (PageFlags::PRESENT | PageFlags::WRITABLE).bits(),
            PAGE_SIZE_4KB,
        );
        let after = free_pages();

        assert_test!(rc == 0, "map over a 2 MiB leaf succeeded");
        assert_test!(
            before == after + 1,
            "splitting a 2 MiB leaf allocated exactly a PT"
        );
        tree.track(entry(pd, l2).address());

        assert_test!(
            walk_leaf(root, probe_va()) == Some(target),
            "the mapped VA resolves to the new frame"
        );

        let neighbour = VirtAddr::new(probe_va().as_u64() ^ PAGE_SIZE_4KB);
        let expected = huge_base.offset(neighbour.as_u64() & (PAGE_SIZE_2MB - 1));
        assert_test!(
            walk_leaf(root, neighbour) == Some(expected),
            "a neighbour inside the demoted range keeps its translation"
        );
        pass!()
    })
}

/// Resolve `vaddr` through a scratch tree by hand, stopping at the
/// first huge leaf. Independent of the production walker so a bug there
/// cannot make these tests agree with it.
fn walk_leaf(root: PhysAddr, vaddr: VirtAddr) -> Option<PhysAddr> {
    let mut table = root;
    let mut level = PageTableLevel::Four;
    loop {
        let e = entry(table, level.index_of(vaddr));
        if !e.is_present() {
            return None;
        }
        if e.is_huge() && level.supports_huge_pages() {
            let size = level.page_size()?;
            return Some(e.address().offset(vaddr.as_u64() & (size - 1)));
        }
        match level.next_lower() {
            Some(next) => {
                table = e.address();
                level = next;
            }
            None => return Some(e.address().offset(vaddr.as_u64() & (PAGE_SIZE_4KB - 1))),
        }
    }
}

slopos_testing::stest!(name = test_paging_unmap_huge_1gib, suite = paging_descent);
slopos_testing::stest!(name = test_paging_unmap_huge_2mib, suite = paging_descent);
slopos_testing::stest!(
    name = test_paging_unmap_4kib_prunes_three_levels,
    suite = paging_descent
);
slopos_testing::stest!(
    name = test_paging_unmap_absent_leaf_still_prunes,
    suite = paging_descent
);
slopos_testing::stest!(
    name = test_paging_map_splits_1gib_leaf,
    suite = paging_descent
);
slopos_testing::stest!(
    name = test_paging_map_splits_2mib_leaf,
    suite = paging_descent
);
