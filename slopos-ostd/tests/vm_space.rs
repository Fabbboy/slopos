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
use slopos_ostd::mm::page_table::PageTableLevel;
use slopos_ostd::mm::phys::init_phys_virt_offset;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::{MapError, VmSpace, register_kernel_master_pml4};

const N_PAGES: usize = 256; // 1 MiB scratch arena
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
        // Bump allocator: leak.
    }
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
        cur.map(frame, PageProperty::USER_RW).unwrap();
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
        cur.map(frame, PageProperty::USER_RW).unwrap();
    }
    let g1 = space.generation();

    let returned = {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.unmap::<AnonymousMeta>().unwrap()
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
        cur.map(f0, PageProperty::USER_RW).unwrap();
        cur.next().unwrap();
        cur.map(f1, PageProperty::USER_RW).unwrap();
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
        cur.map(f0, PageProperty::USER_RW).unwrap();
    }
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        let err = cur.map(f1, PageProperty::USER_RW);
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
    let returned = cur.unmap::<AnonymousMeta>().unwrap();
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
        cur.map(f, PageProperty::USER_RW).unwrap();
    }
    {
        let mut cur = space.cursor_mut(vaddr_start..vaddr_end).unwrap();
        cur.protect(PageProperty::USER_RO).unwrap();
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
    cur.map(f, PageProperty::USER_RW).unwrap();
    cur.next().unwrap(); // moves to vaddr_start + 0x1000
    cur.next().unwrap(); // moves to range.end (allowed past-the-end position)
    // A third step (or any map at past-the-end) is OOB.
    assert_eq!(cur.next(), Err(MapError::OutOfBounds));
    let f2 = fresh_user_frame();
    assert_eq!(
        cur.map(f2, PageProperty::USER_RW),
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
        cur.map(fresh_user_frame(), PageProperty::USER_RW).unwrap();
        cur.next().unwrap();
        cur.map(fresh_user_frame(), PageProperty::USER_RW).unwrap();
        cur.next().unwrap();
        cur.map(fresh_user_frame(), PageProperty::USER_RW).unwrap();
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
        cur.map(fresh_user_frame(), PageProperty::USER_RW).unwrap();
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
        cur.map(f, PageProperty::USER_RW).unwrap();
    }

    let mut cur = space.cursor(vaddr_start..vaddr_end).unwrap();
    cur.seek(target).unwrap();
    assert_eq!(cur.query().unwrap().paddr, Some(f_paddr));
}
