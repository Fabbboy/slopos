//! Host-side integration tests for `VmSpace` / `Cursor` / `CursorMut`.
//!
//! OSTD's one-shot init hooks are wired exactly once behind a shared setup
//! gate, which every test acquires so global OSTD state is serialised. Tests
//! use disjoint `vaddr` ranges so they never see each other's mappings.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::arch::x86_64::cr3::Pcid;
use slopos_ostd::mm::frame::{
    AnonymousMeta, Frame, FrameAlloc, FrameAllocOptions, KernelMeta, MetaSlot, Paddr,
    init_meta_slots,
};
use slopos_ostd::mm::frame_alloc::register_frame_allocator;
use slopos_ostd::mm::page_property::PageProperty;
use slopos_ostd::mm::page_size::{PageSize, Size2Mb, Size4Kb};
use slopos_ostd::mm::page_table::{PageTableLevel, PteFlags};
use slopos_ostd::mm::phys::init_phys_virt_offset;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::{
    CursorUnmapHook, MapError, VmSpace, prepopulate_kernel_half, register_cursor_unmap_hook,
    register_kernel_master_pml4,
};

const N_PAGES: usize = 8192; // 32 MiB scratch arena (room for huge-page tests under parallel runs)
const PAGE_SIZE: usize = 4096;

/// Bump allocator over the scratch arena; page 0 is reserved as the
/// kernel-master PML4.
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
                let base = BACKING_BASE.load(Ordering::Acquire) as usize;
                let virt: *mut u8 =
                    core::ptr::with_exposed_provenance_mut(base + paddr.as_u64() as usize);
                core::ptr::write_bytes(virt, 0, PAGE_SIZE);
            }
        }
        Some(paddr)
    }

    fn dealloc(&self, _paddr: Paddr, _size_pages: usize) {
        // Bump allocator: leak.
    }
}

/// Reserve a 2 MiB-aligned page-index region and return its head paddr.
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
        // Heap-allocate the arena directly: `Box::new(Backing([0; ...]))` would
        // copy through the test thread's stack and overflow it.
        let layout = std::alloc::Layout::from_size_align(N_PAGES * PAGE_SIZE, PAGE_SIZE)
            .expect("backing layout");
        // SAFETY: `layout.size() > 0`; standard allocator contract.
        let backing_ptr_real: *mut u8 = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!backing_ptr_real.is_null(), "backing alloc failed");
        // Expose the allocation's provenance once so every later
        // `with_exposed_provenance_mut` into the arena is sound under
        // `-Zmiri-strict-provenance`.
        let backing_ptr = backing_ptr_real.expose_provenance() as u64;
        BACKING_BASE.store(backing_ptr, Ordering::Release);

        let mut slots: Vec<MetaSlot> = (0..N_PAGES).map(|_| MetaSlot::new_unused()).collect();
        let slots_ptr: *mut MetaSlot = slots.as_mut_ptr();
        Box::leak(slots.into_boxed_slice());

        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            init_meta_slots(t, slots_ptr, N_PAGES);
            init_phys_virt_offset(t, backing_ptr);
            register_frame_allocator(t, &BUMP_REF);
            register_kernel_master_pml4(t, PhysAddr::new(0));
        });
        Mutex::new(())
    });
    // Recover from poison so one test's panic doesn't cascade.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn fresh_user_frame() -> UFrame<AnonymousMeta> {
    let paddr = BUMP_ALLOC
        .alloc(FrameAllocOptions::single().zeroed())
        .expect("test arena exhausted");
    UFrame::<AnonymousMeta>::from_unused(paddr, AnonymousMeta::default()).unwrap()
}

#[test]
fn new_creates_fresh_pml4_with_zero_generation() {
    let _g = setup();
    let space = VmSpace::new().expect("VmSpace::new");
    assert_eq!(space.generation(), 0);
    assert_ne!(space.pml4_paddr().as_u64(), 0);
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

    // Re-installing at the same paddr succeeds only if the slot went UNUSED.
    let _ = UFrame::<AnonymousMeta>::from_unused(frame_paddr, AnonymousMeta::default()).unwrap();
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
        let f1_paddr = f1.paddr();
        let (returned, err) = cur
            .map::<Size4Kb, _>(f1, PageProperty::USER_RW)
            .expect_err("map over a present leaf must be refused");
        assert_eq!(err, MapError::Overlap);
        assert_eq!(returned.paddr(), f1_paddr);
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
    let vaddr_start = VirtAddr::new(0x0000_0001_6000_0000);
    let vaddr_end = VirtAddr::new(0x0000_0001_6000_2000);

    let f = fresh_user_frame();
    let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
    cur.map::<Size4Kb, _>(f, PageProperty::USER_RW).unwrap();
    cur.next().unwrap();
    cur.next().unwrap(); // range.end is an allowed past-the-end position
    assert_eq!(cur.next(), Err(MapError::OutOfBounds));
    let f2 = fresh_user_frame();
    let f2_paddr = f2.paddr();
    let (returned, err) = cur
        .map::<Size4Kb, _>(f2, PageProperty::USER_RW)
        .expect_err("map past range.end must be refused");
    assert_eq!(err, MapError::OutOfBounds);
    assert_eq!(returned.paddr(), f2_paddr);
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

#[test]
fn map_2mb_round_trip_via_cursor() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0002_0020_0000);
    let vaddr_end = VirtAddr::new(0x0000_0002_0040_0000);

    let huge_paddr = alloc_2mb_aligned_paddr();
    let huge_uframe =
        UFrame::<AnonymousMeta>::from_unused(huge_paddr, AnonymousMeta::default()).unwrap();

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
    // 2 MiB-spanning range starting at a 4 KiB boundary that is not 2 MiB-aligned.
    let vaddr_start = VirtAddr::new(0x0000_0002_0040_1000);
    let vaddr_end = VirtAddr::new(0x0000_0002_0060_1000);

    let huge_paddr = alloc_2mb_aligned_paddr();
    let huge_uframe =
        UFrame::<AnonymousMeta>::from_unused(huge_paddr, AnonymousMeta::default()).unwrap();

    let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
    let (returned, err) = cur
        .map::<Size2Mb, _>(huge_uframe, PageProperty::USER_RW)
        .expect_err("a 2 MiB map at a 4 KiB-aligned cursor must be refused");
    assert_eq!(err, MapError::UnalignedCursor);
    assert_eq!(returned.paddr(), huge_paddr);
}

#[test]
fn unmap_size_mismatch_returns_err() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr_start = VirtAddr::new(0x0000_0002_0080_0000);
    let vaddr_end = VirtAddr::new(0x0000_0002_00A0_0000);

    let huge_paddr = alloc_2mb_aligned_paddr();
    let huge_uframe =
        UFrame::<AnonymousMeta>::from_unused(huge_paddr, AnonymousMeta::default()).unwrap();
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
    let huge_uframe =
        UFrame::<AnonymousMeta>::from_unused(huge_paddr, AnonymousMeta::default()).unwrap();
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
    // Treat a fresh page as an already-installed PML4; Pcid 0 mirrors production.
    let pml4_phys = BUMP_ALLOC
        .alloc(FrameAllocOptions::single().zeroed())
        .expect("arena");

    let space = unsafe { VmSpace::wrap_existing(pml4_phys, Pcid::new(0)).unwrap() };
    assert_eq!(space.pml4_paddr(), pml4_phys);

    drop(space);

    // Re-wrapping succeeds only if Drop returned the slot to UNUSED without
    // deallocating the borrowed page.
    let space2 = unsafe { VmSpace::wrap_existing(pml4_phys, Pcid::new(0)).unwrap() };
    assert_eq!(space2.pml4_paddr(), pml4_phys);
}

#[test]
fn drop_user_half_returns_intermediate_tables_to_allocator() {
    let _g = setup();

    let mut space = VmSpace::new().unwrap();
    let pages_before = BUMP_ALLOC.next_page.load(Ordering::Relaxed);

    // Spread across distinct PT/PD subtrees so Drop has several
    // intermediates to reclaim.
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
    // Not an exact count: the PD/PDPT sharing pattern varies. This only
    // confirms the map calls allocated at all.
    let allocated = pages_after_map - pages_before;
    assert!(
        allocated >= 4 + 4,
        "expected at least 4 leaves + 4 PTs, got {allocated} pages allocated",
    );

    drop(space);

    // The bump allocator hands out fresh paddrs, so a leaked intermediate is
    // never re-encountered; the real check is that the recursive teardown
    // walker completes without panicking.
    let _space2 = VmSpace::new().unwrap();
}

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
    fn select_cr3(&self, _mm_ctx_handle: u64, _tlb_gen: u64) -> Option<(u16, bool)> {
        // No pool in the host harness: OSTD must fall back to a flushing
        // kernel-PCID load.
        None
    }
}

// Installed once and never removed; the setup mutex serialises tests, so a
// later test observes the same hook.
static HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

fn install_hook_once() {
    HOOK_INSTALLED.get_or_init(|| {
        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            register_cursor_unmap_hook(t, &COUNTING_HOOK_REF);
        });
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
    let prop_back = entry.property;
    let bits = prop_back.to_leaf_flags().bits();
    assert_eq!(bits & PteFlags::AVL_9.bits(), PteFlags::AVL_9.bits());
    assert_eq!(bits & PteFlags::AVL_10.bits(), 0);
    assert_eq!(bits & PteFlags::AVL_11.bits(), PteFlags::AVL_11.bits());
}

/// Raw pointer to entry `index` of the page-table frame at `table_phys`,
/// through the test arena's HHDM. Used to install a 1 GiB leaf by hand, since
/// no 1 GiB-aligned page fits the arena; that leaf's range is never dereferenced.
fn arena_entry_ptr(table_phys: PhysAddr, index: usize) -> *mut u64 {
    let base = BACKING_BASE.load(Ordering::Acquire) as usize;
    // SAFETY: `table_phys` is a page inside the arena, whose provenance
    // was exposed once in `setup`; `index < 512` keeps the offset inside
    // the 4 KiB frame.
    unsafe {
        let virt: *mut u64 =
            core::ptr::with_exposed_provenance_mut(base + table_phys.as_u64() as usize);
        virt.add(index)
    }
}

fn resolve(space: &VmSpace, vaddr: VirtAddr) -> Option<Paddr> {
    let cur = space
        .cursor(vaddr..VirtAddr::new(vaddr.as_u64() + 0x1000))
        .unwrap();
    let entry = cur.query().unwrap();
    let paddr = entry.paddr?;
    let leaf_size = match entry.level {
        PageTableLevel::One => 0x1000u64,
        PageTableLevel::Two => 0x20_0000,
        PageTableLevel::Three => 0x4000_0000,
        PageTableLevel::Four => return None,
    };
    Some(PhysAddr::new(
        paddr.as_u64() | (vaddr.as_u64() & (leaf_size - 1)),
    ))
}

#[test]
fn map_4kb_inside_2mb_leaf_demotes_and_keeps_translations() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();

    let huge_base = alloc_2mb_aligned_paddr();
    let huge_va = VirtAddr::new(0x0000_0005_0000_0000);
    {
        let mut cur = space
            .cursor_mut(huge_va..VirtAddr::new(huge_va.as_u64() + 0x20_0000))
            .unwrap();
        let frame = UFrame::<AnonymousMeta>::from_unused(huge_base, AnonymousMeta::default())
            .expect("huge frame");
        cur.map::<Size2Mb, _>(frame, PageProperty::USER_RW).unwrap();
    }
    assert_eq!(
        space
            .cursor(huge_va..VirtAddr::new(huge_va.as_u64() + 0x1000))
            .unwrap()
            .query()
            .unwrap()
            .level,
        PageTableLevel::Two,
    );

    let target = VirtAddr::new(huge_va.as_u64() + 0x2000);
    let neighbour = VirtAddr::new(huge_va.as_u64() + 0x9000);

    // The create-mode walk demotes the leaf on the way down and then still
    // refuses: overwriting a present leaf would strand the reference it holds.
    {
        let mut cur = space
            .cursor_mut(target..VirtAddr::new(target.as_u64() + 0x1000))
            .unwrap();
        let offered = fresh_user_frame();
        let offered_paddr = offered.paddr();
        let (returned, err) = cur
            .map::<Size4Kb, _>(offered, PageProperty::USER_RW)
            .expect_err("a demoted-but-present leaf must refuse");
        assert_eq!(err, MapError::Overlap);
        assert_eq!(returned.paddr(), offered_paddr);
    }

    assert_eq!(
        space
            .cursor(target..VirtAddr::new(target.as_u64() + 0x1000))
            .unwrap()
            .query()
            .unwrap()
            .level,
        PageTableLevel::One,
        "the blocking huge leaf was demoted to 4 KiB children",
    );
    assert_eq!(
        resolve(&space, target),
        Some(PhysAddr::new(huge_base.as_u64() + 0x2000)),
        "the demoted child keeps the translation the huge leaf gave it",
    );
    assert_eq!(
        resolve(&space, neighbour),
        Some(PhysAddr::new(huge_base.as_u64() + 0x9000)),
        "a neighbour inside the demoted range keeps its translation",
    );
}

#[test]
fn map_4kb_inside_1gb_leaf_demotes_and_keeps_translations() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();

    // Force the PDPT into existence, then overwrite its entry with a huge leaf.
    let region = VirtAddr::new(0x0000_0006_0000_0000);
    {
        let mut cur = space
            .cursor_mut(region..VirtAddr::new(region.as_u64() + 0x1000))
            .unwrap();
        cur.map::<Size4Kb, _>(fresh_user_frame(), PageProperty::USER_RW)
            .unwrap();
    }
    let pml4_idx = PageTableLevel::Four.index_of(region);
    let pdpt_idx = PageTableLevel::Three.index_of(region);
    // SAFETY: both pointers address one entry inside a live page-table
    // frame in the arena; this thread holds the setup gate.
    let pdpt_phys = unsafe {
        PhysAddr::new(
            arena_entry_ptr(space.pml4_paddr(), pml4_idx).read_volatile() & PteFlags::ADDRESS_MASK,
        )
    };
    // 1 GiB-aligned base outside the arena; only PTE values name it.
    let huge_base = PhysAddr::new(0x4000_0000);
    let huge_entry = huge_base.as_u64()
        | (PteFlags::PRESENT | PteFlags::WRITABLE | PteFlags::USER | PteFlags::HUGE).bits();
    // SAFETY: as above.
    unsafe { arena_entry_ptr(pdpt_phys, pdpt_idx).write_volatile(huge_entry) };

    let target = VirtAddr::new(region.as_u64() + 0x20_3000);
    let neighbour = VirtAddr::new(region.as_u64() + 0x40_5000);

    // Two demotions in one descent: 1 GiB leaf -> 2 MiB children -> 4 KiB
    // children, then the same refusal at the leaf.
    {
        let mut cur = space
            .cursor_mut(target..VirtAddr::new(target.as_u64() + 0x1000))
            .unwrap();
        let offered = fresh_user_frame();
        let offered_paddr = offered.paddr();
        let (returned, err) = cur
            .map::<Size4Kb, _>(offered, PageProperty::USER_RW)
            .expect_err("a twice-demoted but present leaf must refuse");
        assert_eq!(err, MapError::Overlap);
        assert_eq!(returned.paddr(), offered_paddr);
    }

    assert_eq!(
        space
            .cursor(target..VirtAddr::new(target.as_u64() + 0x1000))
            .unwrap()
            .query()
            .unwrap()
            .level,
        PageTableLevel::One,
        "the 1 GiB leaf was demoted all the way to 4 KiB children",
    );
    assert_eq!(
        resolve(&space, target),
        Some(PhysAddr::new(huge_base.as_u64() + 0x20_3000)),
        "the demoted child keeps the translation the huge leaf gave it",
    );
    assert_eq!(
        resolve(&space, neighbour),
        Some(PhysAddr::new(huge_base.as_u64() + 0x40_5000)),
        "a neighbour still covered by a 2 MiB child keeps its translation",
    );
}

/// The kernel entry point installs a supervisor leaf from a sensitive
/// `Frame<KernelMeta>` and hands the same frame back on unmap.
#[test]
fn map_kernel_round_trips_a_sensitive_frame() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr = VirtAddr::new(0xFFFF_9000_0000_0000);
    let end = VirtAddr::new(vaddr.as_u64() + 0x1000);

    let paddr = BUMP_ALLOC
        .alloc(FrameAllocOptions::single().zeroed())
        .unwrap();
    let frame = Frame::<KernelMeta>::from_unused(paddr, KernelMeta).unwrap();
    {
        let mut cur = space.cursor_mut(vaddr..end).unwrap();
        cur.map_kernel::<Size4Kb, KernelMeta>(frame, PageProperty::KERNEL_RW)
            .unwrap();
    }

    let entry = space.cursor(vaddr..end).unwrap().query().unwrap();
    assert_eq!(entry.paddr, Some(paddr));
    assert!(!entry.property.user);
    assert!(entry.property.global);

    let mut cur = space.cursor_mut(vaddr..end).unwrap();
    let back = cur
        .unmap_kernel::<Size4Kb, KernelMeta>()
        .unwrap()
        .expect("unmap_kernel yields the frame");
    assert_eq!(back.paddr(), paddr);
}

/// Both `map_kernel` guards are refusals, not warnings — a user-visible
/// leaf and a low-half address each fail before anything is written.
#[test]
fn map_kernel_refuses_user_property_and_low_half() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();

    let kernel_va = VirtAddr::new(0xFFFF_9000_0001_0000);
    let paddr = BUMP_ALLOC
        .alloc(FrameAllocOptions::single().zeroed())
        .unwrap();
    {
        let frame = Frame::<KernelMeta>::from_unused(paddr, KernelMeta).unwrap();
        let mut cur = space
            .cursor_mut(kernel_va..VirtAddr::new(kernel_va.as_u64() + 0x1000))
            .unwrap();
        let (returned, err) = cur
            .map_kernel::<Size4Kb, KernelMeta>(frame, PageProperty::USER_RW)
            .expect_err("map_kernel must refuse a user leaf");
        assert_eq!(err, MapError::NotKernelMapping);
        assert_eq!(returned.paddr(), paddr);
    }
    assert_eq!(
        space
            .cursor(kernel_va..VirtAddr::new(kernel_va.as_u64() + 0x1000))
            .unwrap()
            .query()
            .unwrap()
            .paddr,
        None
    );

    let low_va = VirtAddr::new(0x0000_0007_0000_0000);
    let paddr2 = BUMP_ALLOC
        .alloc(FrameAllocOptions::single().zeroed())
        .unwrap();
    let frame2 = Frame::<KernelMeta>::from_unused(paddr2, KernelMeta).unwrap();
    let mut cur = space
        .cursor_mut(low_va..VirtAddr::new(low_va.as_u64() + 0x1000))
        .unwrap();
    let (returned, err) = cur
        .map_kernel::<Size4Kb, KernelMeta>(frame2, PageProperty::KERNEL_RW)
        .expect_err("map_kernel must refuse a leaf below the higher half");
    assert_eq!(err, MapError::NotKernelMapping);
    assert_eq!(returned.paddr(), paddr2);
}

/// A `map_io` leaf owns nothing: unmapping it yields no frame, and the
/// physical range it named never had a `MetaSlot` to reclaim from.
#[test]
fn map_io_owns_no_reference() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr = VirtAddr::new(0xFFFF_9000_0002_0000);
    let end = VirtAddr::new(vaddr.as_u64() + 0x1000);
    // Well past the arena: no MetaSlot exists for it, which is the case
    // `map_io` is for.
    let device_pa = PhysAddr::new(0xFEE0_0000);

    {
        let mut cur = space.cursor_mut(vaddr..end).unwrap();
        cur.map_io::<Size4Kb>(device_pa, PageProperty::KERNEL_RW)
            .unwrap();
    }

    let entry = space.cursor(vaddr..end).unwrap().query().unwrap();
    assert_eq!(entry.paddr, Some(device_pa));
    assert_eq!(
        entry.property.software & PageProperty::SOFTWARE_NO_FRAME_REF,
        PageProperty::SOFTWARE_NO_FRAME_REF,
        "the leaf records that it owns no reference"
    );

    let mut cur = space.cursor_mut(vaddr..end).unwrap();
    assert!(
        cur.unmap_kernel::<Size4Kb, KernelMeta>().unwrap().is_none(),
        "unmapping a map_io leaf reclaims nothing"
    );
    drop(cur);
    assert_eq!(
        space.cursor(vaddr..end).unwrap().query().unwrap().paddr,
        None
    );
}

#[test]
fn map_io_refuses_user_property() {
    let _g = setup();
    let mut space = VmSpace::new().unwrap();
    let vaddr = VirtAddr::new(0xFFFF_9000_0003_0000);
    let mut cur = space
        .cursor_mut(vaddr..VirtAddr::new(vaddr.as_u64() + 0x1000))
        .unwrap();
    assert_eq!(
        cur.map_io::<Size4Kb>(PhysAddr::new(0xFEE0_0000), PageProperty::USER_RW),
        Err(MapError::NotKernelMapping)
    );
}

/// Every kernel-half PML4 entry of the registered master is linked, so
/// a fresh address space's one-shot copy of the top level can never go
/// stale. Idempotent: a second call links nothing.
#[test]
fn prepopulate_kernel_half_links_every_top_level_entry() {
    let _g = setup();
    slopos_ostd::sync::run_bsp_init_for_test(|t| {
        prepopulate_kernel_half(t).expect("prepopulate");
    });

    let master = PhysAddr::new(0);
    for i in 256..512 {
        // SAFETY: page 0 is the registered master, inside the arena.
        let raw = unsafe { arena_entry_ptr(master, i).read_volatile() };
        assert!(
            raw & PteFlags::PRESENT.bits() != 0,
            "kernel-half PML4 entry {i} is not linked"
        );
    }

    let again = slopos_ostd::sync::run_bsp_init_for_test(|t| prepopulate_kernel_half(t).unwrap());
    assert_eq!(again, 0, "prepopulation is idempotent");
}
