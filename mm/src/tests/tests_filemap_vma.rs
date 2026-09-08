//! A `SharedFile` VMA's accounting.
//!
//! The pages belong to the filesystem's page set, so every teardown path must
//! tell the set how many page references it dropped and must not free a page
//! the set still owns. Neither is visible from a mapping's return value, so
//! these assert against the registry's own counters and the `MetaSlot`
//! refcount.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::syscall::{MAP_SHARED, PROT_READ, PROT_WRITE};
use slopos_ostd::mm::frame::{claim_owned_anon_page, reference_count_at, release_owned_anon_page};
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::filemap_hook::{FileMapOps, filemap_swap_ops};
use crate::page_alloc::alloc_kernel_page;
use crate::process_vm::{process_vm_get_region, process_vm_mmap_file_shared, process_vm_munmap};
use crate::tests::test_fixtures::ProcessVmGuard;
use crate::vma_region::FileMapRef;
use slopos_abi::addr::PhysAddr;

const PAGES: u32 = 2;
const LENGTH: u64 = PAGES as u64 * 4096;

/// Stands in for the filesystem's page set: it counts what `mm` tells it.
struct CountingOps;

static RETAINED: AtomicU32 = AtomicU32::new(0);
static RELEASED: AtomicU32 = AtomicU32::new(0);
/// Retains that said the mapping can store into the pages.
static RETAINED_WRITABLE: AtomicU32 = AtomicU32::new(0);
static DRAINED: AtomicU32 = AtomicU32::new(0);

static COUNTING_OPS: CountingOps = CountingOps;

impl FileMapOps for CountingOps {
    fn retain(
        &self,
        _map: FileMapRef,
        pages: u32,
        writable: bool,
        _holder: slopos_ostd::process::AccountId,
    ) -> bool {
        RETAINED.fetch_add(pages, Ordering::Relaxed);
        if writable {
            RETAINED_WRITABLE.fetch_add(pages, Ordering::Relaxed);
        }
        true
    }

    fn release(&self, _map: FileMapRef, pages: u32) {
        RELEASED.fetch_add(pages, Ordering::Relaxed);
    }

    fn drain(&self) {
        DRAINED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Installs the counting registry and puts the real one back on drop, so a
/// failing assertion cannot leave the kernel's own page sets unreachable.
struct OpsSwap {
    previous: Option<&'static dyn FileMapOps>,
}

impl OpsSwap {
    fn install() -> Self {
        RETAINED.store(0, Ordering::Relaxed);
        RELEASED.store(0, Ordering::Relaxed);
        RETAINED_WRITABLE.store(0, Ordering::Relaxed);
        DRAINED.store(0, Ordering::Relaxed);
        Self {
            previous: filemap_swap_ops(Some(&COUNTING_OPS)),
        }
    }
}

impl Drop for OpsSwap {
    fn drop(&mut self) {
        filemap_swap_ops(self.previous);
    }
}

/// Two owned frames, claimed the way the page set claims its own.
fn claim_pages() -> Option<(PhysAddr, PhysAddr)> {
    let first = alloc_kernel_page();
    let second = alloc_kernel_page();
    if first.is_null() || second.is_null() {
        return None;
    }
    if !claim_owned_anon_page(first) || !claim_owned_anon_page(second) {
        return None;
    }
    Some((first, second))
}

fn drop_pages(pages: (PhysAddr, PhysAddr)) {
    release_owned_anon_page(pages.0);
    release_owned_anon_page(pages.1);
}

const MAP: FileMapRef = FileMapRef {
    slot: 3,
    generation: 9,
};

/// `munmap` drops exactly the mapping's page references and leaves the pages
/// with the set that owns them.
pub fn test_shared_file_vma_unmap_releases_without_freeing() -> TestResult {
    let _swap = OpsSwap::install();
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(pages) = claim_pages() else {
        return fail!("claim the backing pages");
    };
    let paddrs = [pages.0.as_u64(), pages.1.as_u64()];

    let va = process_vm_mmap_file_shared(
        vm.process,
        0,
        LENGTH,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        MAP,
        &paddrs,
    );
    if va == 0 {
        drop_pages(pages);
        return fail!("the shared file mapping was refused");
    }

    let retained = RETAINED.load(Ordering::Relaxed);
    let mapped = vm.virt_to_phys(va);
    let region = process_vm_get_region(vm.process, va);
    let refs_mapped = reference_count_at(pages.0);

    let rc = process_vm_munmap(vm.process, va, LENGTH);
    let released = RELEASED.load(Ordering::Relaxed);
    let refs_unmapped = reference_count_at(pages.0);
    let still_mapped = vm.virt_to_phys(va);
    drop_pages(pages);

    assert_test!(
        retained == PAGES,
        "the mapping retained {} page refs, expected {}",
        retained,
        PAGES
    );
    assert_test!(
        mapped.as_u64() == paddrs[0],
        "the first page maps {:#x}, expected {:#x}",
        mapped.as_u64(),
        paddrs[0]
    );
    assert_test!(
        region.is_some_and(|r| r.filemap_ref() == Some(MAP)),
        "the VMA does not name the page set it was mapped from"
    );
    assert_test!(
        refs_mapped == 2,
        "a mapped page should hold the set's ref and the PTE's, holds {}",
        refs_mapped
    );
    assert_test!(rc == 0, "munmap of the file mapping failed: {}", rc);
    assert_test!(
        released == PAGES,
        "munmap released {} page refs, expected {}",
        released,
        PAGES
    );
    assert_test!(
        refs_unmapped == 1,
        "munmap dropped {} refs; the page set must keep holding the page",
        2 - refs_unmapped.min(2)
    );
    assert_test!(
        still_mapped.is_null(),
        "the range is still mapped after munmap"
    );
    pass!()
}

/// Address-space teardown is the path a process exit takes, and it runs under
/// a preempt guard: it must still tell the page set, and still not free.
pub fn test_shared_file_vma_teardown_releases() -> TestResult {
    let _swap = OpsSwap::install();
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(pages) = claim_pages() else {
        return fail!("claim the backing pages");
    };
    let paddrs = [pages.0.as_u64(), pages.1.as_u64()];

    let va = process_vm_mmap_file_shared(
        vm.process,
        0,
        LENGTH,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        MAP,
        &paddrs,
    );
    if va == 0 {
        drop_pages(pages);
        return fail!("the shared file mapping was refused");
    }

    drop(vm);

    let released = RELEASED.load(Ordering::Relaxed);
    let refs_after = reference_count_at(pages.0);
    drop_pages(pages);

    assert_test!(
        released == PAGES,
        "teardown released {} page refs, expected {}",
        released,
        PAGES
    );
    assert_test!(
        refs_after == 1,
        "teardown left {} refs on a page the set still owns, expected 1",
        refs_after
    );
    pass!()
}

/// A read-only shared file mapping must not arm the writeback, and an ordinary
/// `munmap` must complete what the release queued rather than leave it to a
/// flusher that may not exist.
pub fn test_shared_file_vma_readonly_is_not_armed_and_munmap_drains() -> TestResult {
    let _swap = OpsSwap::install();
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(pages) = claim_pages() else {
        return fail!("claim the backing pages");
    };
    let paddrs = [pages.0.as_u64(), pages.1.as_u64()];

    let va =
        process_vm_mmap_file_shared(vm.process, 0, LENGTH, PROT_READ, MAP_SHARED, MAP, &paddrs);
    if va == 0 {
        drop_pages(pages);
        return fail!("the read-only file mapping was refused");
    }
    let armed = RETAINED_WRITABLE.load(Ordering::Relaxed);
    let retained = RETAINED.load(Ordering::Relaxed);

    let rc = process_vm_munmap(vm.process, va, LENGTH);
    let drained = DRAINED.load(Ordering::Relaxed);
    drop_pages(pages);

    assert_test!(
        rc == 0,
        "munmap of the read-only file mapping failed: {}",
        rc
    );
    assert_test!(
        retained == PAGES,
        "the mapping retained {} page refs, expected {}",
        retained,
        PAGES
    );
    assert_test!(
        armed == 0,
        "a PROT_READ mapping armed the writeback for {} page(s)",
        armed
    );
    assert_test!(
        drained >= 1,
        "munmap left the queued writeback for someone else ({} drains)",
        drained
    );
    pass!()
}

slopos_testing::stest!(
    name = test_shared_file_vma_unmap_releases_without_freeing,
    suite = filemap_vma
);
slopos_testing::stest!(
    name = test_shared_file_vma_teardown_releases,
    suite = filemap_vma
);
slopos_testing::stest!(
    name = test_shared_file_vma_readonly_is_not_armed_and_munmap_drains,
    suite = filemap_vma
);
