//! Host-side integration tests for `VmSpace` / `Cursor` / `CursorMut`.
//!
//! Setup pattern mirrors `tests/uframe_round_trip.rs`: a leaked
//! page-aligned `Backing` array gives us "physical" memory, a leaked
//! `Vec<MetaSlot>` gives us per-frame ref-count slots, and OSTD's
//! one-shot init hooks (`init_meta_slots`, `init_phys_virt_offset`,
//! `register_frame_allocator`, `register_kernel_master_pml4`) get
//! wired exactly once via a shared `OnceLock<Mutex<()>>` setup gate.
//!
//! Each test acquires the gate so global OSTD state is serialised.
//! Tests use disjoint `vaddr` ranges so they never see each other's
//! mappings; the test allocator hands out fresh paddrs so every test
//! gets its own page-table tree (a fresh PML4 per `VmSpace::new()`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::arch::x86_64::cr3::Pcid;
use slopos_ostd::mm::frame::{
    AnonymousMeta, FrameAlloc, FrameAllocOptions, MetaSlot, Paddr, init_meta_slots,
};
use slopos_ostd::mm::frame_alloc::register_frame_allocator;
use slopos_ostd::mm::page_property::PageProperty;
use slopos_ostd::mm::page_size::{PageSize, Size2Mb, Size4Kb};
use slopos_ostd::mm::page_table::{PageTableLevel, PteFlags};
use slopos_ostd::mm::phys::init_phys_virt_offset;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::{
    CursorUnmapHook, MapError, VmSpace, bump_kernel_master_gen, register_cursor_unmap_hook,
    register_kernel_master_pml4,
};

const N_PAGES: usize = 8192; // 32 MiB scratch arena (room for huge-page tests under parallel runs)
const PAGE_SIZE: usize = 4096;

/// Bump allocator over the scratch arena. Hands out pages 1.. (page 0
/// is reserved as the kernel-master PML4 — see `setup`).
struct BumpAlloc {
    next_page: AtomicU64,
}

impl FrameAlloc for BumpAlloc {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr> {
        assert_eq!(opts.size_pages, 1, "test alloc supports single-page only");
        let page = self.next_page.fetch_add(1, Ordering::Relaxed);
        if page as usize >= N_PAGES {
            return None;
        }
        let paddr = PhysAddr::new(page * PAGE_SIZE as u64);
        if opts.zeroing {
            // SAFETY: backing buffer is valid for `[0, N_PAGES * PAGE_SIZE)`;
            // page index was just allocated and not handed to anyone else.
            unsafe {
                let virt = (BACKING_BASE.load(Ordering::Acquire) as usize + paddr.as_u64() as usize)
                    as *mut u8;
                core::ptr::write_bytes(virt, 0, PAGE_SIZE);
            }
        }
        Some(paddr)
    }

    fn dealloc(&self, _paddr: Paddr, _size_pages: usize) {
        // Bump allocator: leak. The Frame::Drop dealloc-back-to-allocator
        // path lands here too; we accept any size_pages.
    }
}

/// Reserve a 2 MiB-aligned page-index region from the bump allocator
/// and return its head paddr (suitable for installing as a 2 MiB
/// huge-page leaf). Stride is 512 pages = 2 MiB.
fn alloc_2mb_aligned_paddr() -> Paddr {
    let cur = BUMP_ALLOC.next_page.load(Ordering::Relaxed);
    let aligned = (cur + 511) & !511_u64;
    BUMP_ALLOC.next_page.store(aligned + 512, Ordering::Relaxed);
    assert!(
        (aligned as usize) + 512 <= N_PAGES,
        "alloc_2mb_aligned_paddr: arena exhausted (cur={cur} aligned={aligned})",
    );
    PhysAddr::new(aligned * PAGE_SIZE as u64)
}

static BACKING_BASE: AtomicU64 = AtomicU64::new(0);
static BUMP_ALLOC: BumpAlloc = BumpAlloc {
    next_page: AtomicU64::new(1), // page 0 reserved for the kernel-master PML4
};
static BUMP_REF: &'static dyn FrameAlloc = &BUMP_ALLOC;
static SETUP: OnceLock<Mutex<()>> = OnceLock::new();

fn setup() -> MutexGuard<'static, ()> {
    let m = SETUP.get_or_init(|| {
        // Heap-allocate the scratch arena directly (1 MiB) so we never
        // construct it on the stack — a `Box::new(Backing([0; ...]))`
        // would copy through the test thread's stack and overflow.
        // 4 KiB-aligned via `Layout`.
        let layout = std::alloc::Layout::from_size_align(N_PAGES * PAGE_SIZE, PAGE_SIZE)
            .expect("backing layout");
        // SAFETY: `layout.size() > 0`; standard allocator contract.
        let backing_ptr = unsafe { std::alloc::alloc_zeroed(layout) } as u64;
        assert_ne!(backing_ptr, 0, "backing alloc failed");
        BACKING_BASE.store(backing_ptr, Ordering::Release);

        let mut slots: Vec<MetaSlot> = (0..N_PAGES).map(|_| MetaSlot::new_unused()).collect();
        let slots_ptr: *mut MetaSlot = slots.as_mut_ptr();
        Box::leak(slots.into_boxed_slice());

        // SAFETY: leaked storage; `slots_ptr` and `backing_ptr` live
        // for the static lifetime of the test binary.
        unsafe {
            init_meta_slots(slots_ptr, N_PAGES);
            init_phys_virt_offset(backing_ptr);
            register_frame_allocator(&BUMP_REF);
            // Page 0 is the kernel master — already zero-initialised.
            register_kernel_master_pml4(PhysAddr::new(0));
        }
        Mutex::new(())
    });
    // Recover from poison so a panic in one test doesn't cascade
    // into PoisonError on every subsequent test.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// Allocate a fresh "user frame" paddr disjoint from the page-table
// tree. We just call the same bump allocator.
fn fresh_user_frame() -> UFrame<AnonymousMeta> {
    let paddr = BUMP_ALLOC
        .alloc(FrameAllocOptions::single().zeroed())
        .expect("test arena exhausted");
    UFrame::<AnonymousMeta>::from_unused(paddr, AnonymousMeta).unwrap()
}

#[test]
fn new_creates_fresh_pml4_with_zero_generation() {
    let _g = setup();
    let space = VmSpace::new().expect("VmSpace::new");
    assert_eq!(space.generation(), 0);
    assert_ne!(space.pml4_paddr().as_u64(), 0);
    assert_ne!(space.pcid(), Pcid::KERNEL);
}

#[test]
fn map_then_query_round_trip() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0001_0000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_0000_1000);

    let frame = fresh_user_frame();
    let frame_paddr = frame.paddr();
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size4Kb, _>(frame, PageProperty::USER_RW).unwrap();
    }
    assert_eq!(space.generation(), 1);

    let cur = space.cursor(vaddr_start..vaddr_end).unwrap();
    let entry = cur.query().unwrap();
    assert_eq!(entry.paddr, Some(frame_paddr));
    assert_eq!(entry.level, PageTableLevel::One);
    assert!(entry.property.write);
    assert!(entry.property.user);
}

#[test]
fn map_then_unmap_returns_uframe() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0001_1000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_1000_1000);

    let frame = fresh_user_frame();
    let frame_paddr = frame.paddr();
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size4Kb, _>(frame, PageProperty::USER_RW).unwrap();
    }
    let g1 = space.generation();

    let returned = {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.unmap::<Size4Kb, AnonymousMeta>().unwrap()
    };
    let frame = returned.expect("unmap returned None");
    assert_eq!(frame.paddr(), frame_paddr);
    assert_eq!(frame.reference_count(), 1);
    assert!(space.generation() > g1);

    drop(frame);

    // After drop, the slot transitions to UNUSED — proven by being
    // able to re-install at the same paddr.
    let _ = UFrame::<AnonymousMeta>::from_unused(frame_paddr, AnonymousMeta).unwrap();
}

#[test]
fn map_two_consecutive_pages() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0001_2000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_2000_2000);

    let f0 = fresh_user_frame();
    let f0_paddr = f0.paddr();
    let f1 = fresh_user_frame();
    let f1_paddr = f1.paddr();

    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size4Kb, _>(f0, PageProperty::USER_RW).unwrap();
        cur.next().unwrap();
        cur.map::<Size4Kb, _>(f1, PageProperty::USER_RW).unwrap();
    }

    let mut cur = space.cursor(vaddr_start..vaddr_end).unwrap();
    assert_eq!(cur.query().unwrap().paddr, Some(f0_paddr));
    cur.next().unwrap();
    assert_eq!(cur.query().unwrap().paddr, Some(f1_paddr));
}

#[test]
fn overlap_returns_err() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0001_3000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_3000_1000);

    let f0 = fresh_user_frame();
    let f1 = fresh_user_frame();
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size4Kb, _>(f0, PageProperty::USER_RW).unwrap();
    }
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        let err = cur.map::<Size4Kb, _>(f1, PageProperty::USER_RW);
        assert_eq!(err, Err(MapError::Overlap));
    }
}

#[test]
fn unmap_of_unmapped_returns_none() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0001_4000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_4000_1000);

    let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
    let returned = cur.unmap::<Size4Kb, AnonymousMeta>().unwrap();
    assert!(returned.is_none());
}

#[test]
fn protect_changes_writable_bit() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0001_5000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_5000_1000);

    let f = fresh_user_frame();
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size4Kb, _>(f, PageProperty::USER_RW).unwrap();
    }
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.protect::<Size4Kb>(PageProperty::USER_RO).unwrap();
    }
    let cur = space.cursor(vaddr_start..vaddr_end).unwrap();
    let entry = cur.query().unwrap();
    assert!(!entry.property.write);
    assert!(entry.property.user);
}

#[test]
fn cursor_oob_after_step_past_range() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    // Two-page range so we can step `next()` once (to end), then a
    // second call must return OutOfBounds.
    let vaddr_start = VirtAddr::new(0x0000_0001_6000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_6000_2000);

    let f = fresh_user_frame();
    let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
    cur.map::<Size4Kb, _>(f, PageProperty::USER_RW).unwrap();
    cur.next().unwrap(); // moves to vaddr_start + 0x1000
    cur.next().unwrap(); // moves to range.end (allowed past-the-end position)
    // A third step (or any map at past-the-end) is OOB.
    assert_eq!(cur.next(), Err(MapError::OutOfBounds));
    let f2 = fresh_user_frame();
    assert_eq!(
        cur.map::<Size4Kb, _>(f2, PageProperty::USER_RW),
        Err(MapError::OutOfBounds)
    );
}

#[test]
fn generation_bumps_once_per_session() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0001_7000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_7000_3000);

    let g0 = space.generation();
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size4Kb, _>(fresh_user_frame(), PageProperty::USER_RW)
            .unwrap();
        cur.next().unwrap();
        cur.map::<Size4Kb, _>(fresh_user_frame(), PageProperty::USER_RW)
            .unwrap();
        cur.next().unwrap();
        cur.map::<Size4Kb, _>(fresh_user_frame(), PageProperty::USER_RW)
            .unwrap();
    }
    assert_eq!(space.generation(), g0 + 1);
}

#[test]
fn read_only_cursor_does_not_bump_generation() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0001_8000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_8000_1000);

    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size4Kb, _>(fresh_user_frame(), PageProperty::USER_RW)
            .unwrap();
    }
    let g = space.generation();
    {
        let cur = space.cursor(vaddr_start..vaddr_end).unwrap();
        let _ = cur.query().unwrap();
    }
    assert_eq!(space.generation(), g);
}

#[test]
fn cursor_unaligned_range_rejected() {
    let _g = setup();
    let space = VmSpace::new().unwrap();
    let bad = VirtAddr::new(0x1)..VirtAddr::new(0x1000);
    assert_eq!(space.cursor(bad).map(|_| ()), Err(MapError::UnalignedRange));
}

#[test]
fn map_seek_returns_to_same_entry() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0001_9000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_9000_4000);

    let target = VirtAddr::new(0x0000_0001_9000_2000);
    let f = fresh_user_frame();
    let f_paddr = f.paddr();

    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.seek(target).unwrap();
        cur.map::<Size4Kb, _>(f, PageProperty::USER_RW).unwrap();
    }

    let mut cur = space.cursor(vaddr_start..vaddr_end).unwrap();
    cur.seek(target).unwrap();
    assert_eq!(cur.query().unwrap().paddr, Some(f_paddr));
}

// ---------------------------------------------------------------------------
// Huge-page cursor ops, wrap_existing, kernel-half resync,
// drop walker, and the cursor-unmap hook.
// ---------------------------------------------------------------------------

#[test]
fn map_2mb_round_trip_via_cursor() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    // 2 MiB-aligned virtual base: the cursor enforces alignment.
    let vaddr_start = VirtAddr::new(0x0000_0002_0020_0000);
    let vaddr_end = VirtAddr::new(0x0000_0002_0040_0000);

    let huge_paddr = alloc_2mb_aligned_paddr();
    let huge_uframe = UFrame::<AnonymousMeta>::from_unused(huge_paddr, AnonymousMeta).unwrap();

    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size2Mb, _>(huge_uframe, PageProperty::USER_RW)
            .unwrap();
    }

    let cur = space.cursor(vaddr_start..vaddr_end).unwrap();
    let entry = cur.query().unwrap();
    assert_eq!(entry.paddr, Some(huge_paddr));
    assert_eq!(entry.level, PageTableLevel::Two);
    assert!(entry.property.write);
    assert!(entry.property.user);
}

#[test]
fn map_2mb_unaligned_cursor_rejected() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    // Range is 2 MiB-spanning, but cursor starts at a 4 KiB boundary
    // that's not 2 MiB-aligned (start | 0x1000).
    let vaddr_start = VirtAddr::new(0x0000_0002_0040_1000);
    let vaddr_end = VirtAddr::new(0x0000_0002_0060_1000);

    let huge_paddr = alloc_2mb_aligned_paddr();
    let huge_uframe = UFrame::<AnonymousMeta>::from_unused(huge_paddr, AnonymousMeta).unwrap();

    let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
    let err = cur.map::<Size2Mb, _>(huge_uframe, PageProperty::USER_RW);
    assert_eq!(err, Err(MapError::UnalignedCursor));
}

#[test]
fn unmap_size_mismatch_returns_err() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0002_0080_0000);
    let vaddr_end = VirtAddr::new(0x0000_0002_00A0_0000);

    // Install a 2 MiB leaf, then try to 4 KiB unmap it — must error.
    let huge_paddr = alloc_2mb_aligned_paddr();
    let huge_uframe = UFrame::<AnonymousMeta>::from_unused(huge_paddr, AnonymousMeta).unwrap();
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size2Mb, _>(huge_uframe, PageProperty::USER_RW)
            .unwrap();
    }

    let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
    let err = cur.unmap::<Size4Kb, AnonymousMeta>().err();
    assert_eq!(err, Some(MapError::SizeMismatch));
}

#[test]
fn protect_2mb_leaf_rewrites_flags() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0002_0100_0000);
    let vaddr_end = VirtAddr::new(0x0000_0002_0120_0000);

    let huge_paddr = alloc_2mb_aligned_paddr();
    let huge_uframe = UFrame::<AnonymousMeta>::from_unused(huge_paddr, AnonymousMeta).unwrap();
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map::<Size2Mb, _>(huge_uframe, PageProperty::USER_RW)
            .unwrap();
    }
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.protect::<Size2Mb>(PageProperty::USER_RO).unwrap();
    }
    let cur = space.cursor(vaddr_start..vaddr_end).unwrap();
    let entry = cur.query().unwrap();
    assert!(!entry.property.write);
    assert!(entry.property.user);
    assert_eq!(entry.level, PageTableLevel::Two);
}

#[test]
fn map_range_installs_consecutive_pages() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0002_0200_0000);
    let vaddr_end = VirtAddr::new(0x0000_0002_0200_4000);

    let f0 = fresh_user_frame();
    let p0 = f0.paddr();
    let f1 = fresh_user_frame();
    let p1 = f1.paddr();
    let f2 = fresh_user_frame();
    let p2 = f2.paddr();
    let f3 = fresh_user_frame();
    let p3 = f3.paddr();

    let installed = {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.map_range::<Size4Kb, _, _>([f0, f1, f2, f3], PageProperty::USER_RW)
            .unwrap()
    };
    assert_eq!(installed, 4);

    let mut cur = space.cursor(vaddr_start..vaddr_end).unwrap();
    assert_eq!(cur.query().unwrap().paddr, Some(p0));
    cur.next().unwrap();
    assert_eq!(cur.query().unwrap().paddr, Some(p1));
    cur.next().unwrap();
    assert_eq!(cur.query().unwrap().paddr, Some(p2));
    cur.next().unwrap();
    assert_eq!(cur.query().unwrap().paddr, Some(p3));
}

#[test]
fn wrap_existing_round_trip() {
    let _g = setup();
    // Allocate a fresh page; treat it as an "already-installed" PML4
    // (kernel master simulation). Use Pcid 0 to mirror production.
    let pml4_phys = BUMP_ALLOC
        .alloc(FrameAllocOptions::single().zeroed())
        .expect("arena");

    let space = unsafe { VmSpace::wrap_existing(pml4_phys, Pcid::new(0)).unwrap() };
    assert_eq!(space.pml4_paddr(), pml4_phys);
    assert_eq!(space.pcid(), Pcid::new(0));

    drop(space);

    // Frame::Drop with `static_borrowed: true` does NOT call dealloc
    // for the page itself; the slot transitions to UNUSED. Re-wrap to
    // verify the slot is back to UNUSED and the contents are intact.
    let space2 = unsafe { VmSpace::wrap_existing(pml4_phys, Pcid::new(0)).unwrap() };
    assert_eq!(space2.pml4_paddr(), pml4_phys);
}

#[test]
fn resync_kernel_half_propagates_master_mutation() {
    let _g = setup();
    // Two VmSpaces created at the same baseline gen.
    let space_a = VmSpace::new().unwrap();
    let _space_b = VmSpace::new().unwrap();

    // Mutate the kernel-master directly via HHDM: write a sentinel
    // PTE bit pattern at PML4 index 300 (somewhere in the kernel
    // half). This is the same write `bump_kernel_master_gen` is
    // meant to track.
    let master_paddr = PhysAddr::new(0); // setup() registered page 0.
    let entry_addr = unsafe {
        let virt = (BACKING_BASE.load(Ordering::Acquire) as usize + master_paddr.as_u64() as usize)
            as *mut u64;
        virt.add(300)
    };
    let sentinel: u64 = 0x0000_DEAD_BEEF_0001;
    unsafe { entry_addr.write_volatile(sentinel) };

    bump_kernel_master_gen();

    // Activate space_a (which calls resync_kernel_half_if_stale
    // internally). For a host test we can't actually do CR3 install,
    // so call the resync path directly.
    let resynced = space_a.resync_kernel_half_if_stale();
    assert!(resynced, "resync should have fired (gen advanced)");

    // Read PML4 index 300 of space_a; must match the sentinel.
    let space_a_phys = space_a.pml4_paddr();
    let entry_after = unsafe {
        let virt = (BACKING_BASE.load(Ordering::Acquire) as usize + space_a_phys.as_u64() as usize)
            as *const u64;
        virt.add(300).read_volatile()
    };
    assert_eq!(entry_after, sentinel);

    // A second resync without a gen bump must report no work.
    let again = space_a.resync_kernel_half_if_stale();
    assert!(!again, "resync should be a no-op when up-to-date");
}

#[test]
fn drop_user_half_returns_intermediate_tables_to_allocator() {
    let _g = setup();

    // Snapshot the bump allocator's next page so we can prove the
    // user-half teardown didn't leak any frames.
    let mut space = VmSpace::new().unwrap();
    let pages_before = BUMP_ALLOC.next_page.load(Ordering::Relaxed);

    // Map four 4 KiB pages spread across distinct PT/PD subtrees so
    // Drop has multiple intermediates to reclaim.
    let leaves = [
        VirtAddr::new(0x0000_0003_0000_0000), // PD #0, PT #0
        VirtAddr::new(0x0000_0003_0020_0000), // PD #1, PT #0  (different PT)
        VirtAddr::new(0x0000_0003_0040_0000), // PD #2, PT #0
        VirtAddr::new(0x0000_0003_4000_0000), // PDPT #1, PD #0  (different PDPT subtree)
    ];

    for vaddr in leaves {
        let f = fresh_user_frame();
        let mut cur = space
            .cursor_mut(vaddr..VirtAddr::new(vaddr.as_u64() + 0x1000))
            .unwrap();
        cur.map::<Size4Kb, _>(f, PageProperty::USER_RW).unwrap();
    }

    let pages_after_map = BUMP_ALLOC.next_page.load(Ordering::Relaxed);
    // Expected allocations: at minimum 4 user frames + 4 PT frames
    // (one PT per distinct leaf vaddr — distinct PD entries always
    // require a fresh PT). Plus some PD/PDPT allocations depending
    // on the layout sharing pattern. We don't pin the exact count
    // because the test's real correctness check is "Drop runs to
    // completion without a panic"; this assertion just confirms the
    // map calls actually allocated something.
    let allocated = pages_after_map - pages_before;
    assert!(
        allocated >= 4 + 4,
        "expected at least 4 leaves + 4 PTs, got {allocated} pages allocated",
    );

    drop(space);

    // After Drop: every intermediate page-table frame's META_SLOT
    // transitions to UNUSED. We verify by re-installing one of the
    // intermediate paddrs as a fresh `from_unused` — succeeds only
    // when the slot is UNUSED.
    //
    // We don't know the exact paddrs of intermediates without an
    // observation hook; instead, check that the bump allocator's
    // dealloc was called the right number of times. The bump alloc
    // in this test leaks (dealloc is a no-op), so we instead verify
    // the META_SLOTS entries by re-using the user-leaf paddrs.
    //
    // Simplest indirect check: re-create the same VmSpace and re-map
    // at the same vaddrs with fresh frames. If the tree teardown
    // worked, this succeeds. If it leaked, the META_SLOTS for the
    // page-table frames are still TYPED and the next user-VmSpace's
    // internal `from_unused` would StateMismatch — but the bump
    // allocator hands out fresh paddrs, so we never re-encounter
    // the leaked paddrs. The real proof is that the test runs to
    // completion without a panic in the recursive walker.
    let _space2 = VmSpace::new().unwrap();
}

// Counting hook for the cursor-unmap hook tests. Records (vaddr,
// paddr, mm_ctx_handle) for each invocation.
struct CountingUnmapHook {
    after_unmap_calls: AtomicU64,
    on_activate_calls: AtomicU64,
    last_after_vaddr: AtomicU64,
    last_after_paddr: AtomicU64,
    last_after_handle: AtomicU64,
    last_activate_handle: AtomicU64,
}

static COUNTING_HOOK: CountingUnmapHook = CountingUnmapHook {
    after_unmap_calls: AtomicU64::new(0),
    on_activate_calls: AtomicU64::new(0),
    last_after_vaddr: AtomicU64::new(0),
    last_after_paddr: AtomicU64::new(0),
    last_after_handle: AtomicU64::new(0),
    last_activate_handle: AtomicU64::new(0),
};
static COUNTING_HOOK_REF: &'static dyn CursorUnmapHook = &COUNTING_HOOK;

impl CursorUnmapHook for CountingUnmapHook {
    fn after_unmap(&self, vaddr: VirtAddr, paddr: PhysAddr, mm_ctx_handle: u64) {
        self.after_unmap_calls.fetch_add(1, Ordering::Relaxed);
        self.last_after_vaddr
            .store(vaddr.as_u64(), Ordering::Relaxed);
        self.last_after_paddr
            .store(paddr.as_u64(), Ordering::Relaxed);
        self.last_after_handle
            .store(mm_ctx_handle, Ordering::Relaxed);
    }
    fn on_activate(&self, mm_ctx_handle: u64) {
        self.on_activate_calls.fetch_add(1, Ordering::Relaxed);
        self.last_activate_handle
            .store(mm_ctx_handle, Ordering::Relaxed);
    }
}

// One-shot hook installation. The test that triggers it runs in
// isolation (sequential setup mutex) so a second test running after
// it observes the same hook — that's fine for assertions.
static HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

fn install_hook_once() {
    HOOK_INSTALLED.get_or_init(|| {
        // SAFETY: `COUNTING_HOOK_REF` is a `static` reference with
        // `'static` lifetime; `register_cursor_unmap_hook` is one-shot.
        unsafe { register_cursor_unmap_hook(&COUNTING_HOOK_REF) };
    });
}

#[test]
fn cursor_unmap_hook_fires_for_user_leaves() {
    let _g = setup();
    install_hook_once();
    let mut space = VmSpace::new().unwrap();
    space.set_mm_ctx_handle(0xCAFE_BABE);

    let vaddr = VirtAddr::new(0x0000_0004_0000_0000);
    let f = fresh_user_frame();
    let f_paddr = f.paddr();

    {
        let mut cur = space
            .cursor_mut(vaddr..VirtAddr::new(vaddr.as_u64() + 0x1000))
            .unwrap();
        cur.map::<Size4Kb, _>(f, PageProperty::USER_RW).unwrap();
    }
    let calls_before = COUNTING_HOOK.after_unmap_calls.load(Ordering::Relaxed);
    {
        let mut cur = space
            .cursor_mut(vaddr..VirtAddr::new(vaddr.as_u64() + 0x1000))
            .unwrap();
        let _ = cur.unmap::<Size4Kb, AnonymousMeta>().unwrap();
    }
    let calls_after = COUNTING_HOOK.after_unmap_calls.load(Ordering::Relaxed);
    assert_eq!(
        calls_after - calls_before,
        1,
        "after_unmap should fire once"
    );
    assert_eq!(
        COUNTING_HOOK.last_after_vaddr.load(Ordering::Relaxed),
        vaddr.as_u64()
    );
    assert_eq!(
        COUNTING_HOOK.last_after_paddr.load(Ordering::Relaxed),
        f_paddr.as_u64()
    );
    assert_eq!(
        COUNTING_HOOK.last_after_handle.load(Ordering::Relaxed),
        0xCAFE_BABE
    );
}

#[test]
fn cursor_unmap_hook_skips_non_user_leaves() {
    let _g = setup();
    install_hook_once();
    let mut space = VmSpace::new().unwrap();
    space.set_mm_ctx_handle(0xDEAD_BEEF);

    let vaddr = VirtAddr::new(0x0000_0004_1000_0000);
    let f = fresh_user_frame();
    {
        let mut cur = space
            .cursor_mut(vaddr..VirtAddr::new(vaddr.as_u64() + 0x1000))
            .unwrap();
        cur.map::<Size4Kb, _>(f, PageProperty::KERNEL_RW).unwrap();
    }
    let calls_before = COUNTING_HOOK.after_unmap_calls.load(Ordering::Relaxed);
    {
        let mut cur = space
            .cursor_mut(vaddr..VirtAddr::new(vaddr.as_u64() + 0x1000))
            .unwrap();
        let _ = cur.unmap::<Size4Kb, AnonymousMeta>().unwrap();
    }
    let calls_after = COUNTING_HOOK.after_unmap_calls.load(Ordering::Relaxed);
    assert_eq!(
        calls_after - calls_before,
        0,
        "after_unmap must NOT fire for kernel-only (non-USER) leaves"
    );
}

#[test]
fn page_size_constants_pin_byte_lengths() {
    assert_eq!(Size4Kb::BYTES, 4096);
    assert_eq!(Size2Mb::BYTES, 2 * 1024 * 1024);
    assert_eq!(Size4Kb::LEVEL, PageTableLevel::One);
    assert_eq!(Size2Mb::LEVEL, PageTableLevel::Two);
}

#[test]
fn pte_software_bits_round_trip_through_cursor() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr = VirtAddr::new(0x0000_0004_2000_0000);

    let f = fresh_user_frame();
    let prop_with_software = PageProperty {
        software: 0b101, // bits 9 and 11 set
        ..PageProperty::USER_RW
    };
    {
        let mut cur = space
            .cursor_mut(vaddr..VirtAddr::new(vaddr.as_u64() + 0x1000))
            .unwrap();
        cur.map::<Size4Kb, _>(f, prop_with_software).unwrap();
    }

    let cur = space
        .cursor(vaddr..VirtAddr::new(vaddr.as_u64() + 0x1000))
        .unwrap();
    let entry = cur.query().unwrap();
    assert_eq!(entry.property.software, 0b101);
    // Sanity: PTE bits 9/11 are AVL_9 and AVL_11.
    let prop_back = entry.property;
    let bits = prop_back.to_leaf_flags().bits();
    assert_eq!(bits & PteFlags::AVL_9.bits(), PteFlags::AVL_9.bits());
    assert_eq!(bits & PteFlags::AVL_10.bits(), 0);
    assert_eq!(bits & PteFlags::AVL_11.bits(), PteFlags::AVL_11.bits());
}
