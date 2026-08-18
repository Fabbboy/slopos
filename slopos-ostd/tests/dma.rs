//! Host-side integration tests for `DmaCoherent` / `DmaStream`.
//!
//! A leaked page-aligned scratch arena stands in for "physical memory", a
//! leaked `Vec<MetaSlot>` gives per-frame ref-count slots, a multi-page
//! `BumpAlloc` implements `FrameAlloc`, and a `RecordingMapper` impls
//! `IommuMapper` while recording every map/unmap call.
//!
//! `BumpAlloc` never re-hands a page, so each `dealloc` it records names a
//! paddr no other test can also have produced — which is what lets a test
//! assert on the *last* recorded release. It snapshots the head page's
//! `MetaSlot` at the instant of release, so a run handed back before the
//! per-frame lifecycle has reset the slots is caught by the recorded kind.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::dma::{
    self, DmaCoherent, DmaDirection, DmaError, DmaStream, IommuMapper, register_iommu_mapper,
};
use slopos_ostd::mm::frame::{
    FrameAlloc, FrameAllocOptions, MetaSlot, Paddr, SlotMetaKind, init_meta_slots, slot_snapshot,
};
use slopos_ostd::mm::frame_alloc::{self, register_frame_allocator};
use slopos_ostd::mm::phys::init_phys_virt_offset;

const N_PAGES: usize = 64;
const PAGE_SIZE: usize = 4096;

/// A page-aligned address one page past the end of the `MetaSlot` array, so a
/// caller's `Frame::from_unused` fails `OutOfRange` — the reachable shape of a
/// segment-build failure.
const OUT_OF_SLOT_RANGE_PADDR: u64 = (N_PAGES * PAGE_SIZE) as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeallocCall {
    paddr: u64,
    size_pages: usize,
    /// The head page's `MetaSlot` kind as it read when `dealloc` was
    /// entered. `Unused` means the segment had already been torn down.
    head_slot_kind: SlotMetaKind,
}

/// Bump allocator over the scratch arena, supporting multi-page requests.
struct BumpAlloc {
    next_page: AtomicU64,
    /// While set, `alloc` hands out [`OUT_OF_SLOT_RANGE_PADDR`] instead of a
    /// real page, without advancing the cursor.
    poisoned: AtomicBool,
}

impl FrameAlloc for BumpAlloc {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr> {
        let n = opts.size_pages as u64;
        if n == 0 {
            return None;
        }
        if self.poisoned.load(Ordering::Acquire) {
            return Some(PhysAddr::new(OUT_OF_SLOT_RANGE_PADDR));
        }
        let page = self.next_page.fetch_add(n, Ordering::Relaxed);
        if (page + n) as usize > N_PAGES {
            return None;
        }
        let paddr = PhysAddr::new(page * PAGE_SIZE as u64);
        if opts.zeroing {
            // SAFETY: backing buffer covers `[0, N_PAGES * PAGE_SIZE)`;
            // the just-allocated range is unique.
            unsafe {
                let base = BACKING_BASE.load(Ordering::Acquire) as usize;
                let virt: *mut u8 =
                    core::ptr::with_exposed_provenance_mut(base + paddr.as_u64() as usize);
                core::ptr::write_bytes(virt, 0, n as usize * PAGE_SIZE);
            }
        }
        Some(paddr)
    }

    fn dealloc(&self, paddr: Paddr, size_pages: usize) {
        CALL_LOG.deallocs.lock().unwrap().push(DeallocCall {
            paddr: paddr.as_u64(),
            size_pages,
            head_slot_kind: slot_snapshot(paddr).kind,
        });
    }
}

// BACKING_BASE holds the *exposed* address of the leaked scratch arena, so
// every later `with_exposed_provenance_mut` against an address inside the arena
// round-trips back to that provenance under strict provenance.
static BACKING_BASE: AtomicU64 = AtomicU64::new(0);
static BUMP_ALLOC: BumpAlloc = BumpAlloc {
    next_page: AtomicU64::new(0),
    poisoned: AtomicBool::new(false),
};
static BUMP_REF: &'static dyn FrameAlloc = &BUMP_ALLOC;

#[derive(Clone, Copy, Debug)]
struct MapCall {
    phys: u64,
    size: usize,
    direction: DmaDirection,
}

#[derive(Clone, Copy, Debug)]
struct UnmapCall {
    iova: u64,
    size: usize,
}

struct CallLog {
    maps: Mutex<Vec<MapCall>>,
    unmaps: Mutex<Vec<UnmapCall>>,
    deallocs: Mutex<Vec<DeallocCall>>,
    next_iova: AtomicU64,
}

static CALL_LOG: CallLog = CallLog {
    maps: Mutex::new(Vec::new()),
    unmaps: Mutex::new(Vec::new()),
    deallocs: Mutex::new(Vec::new()),
    next_iova: AtomicU64::new(0x1_0000_0000),
};

struct RecordingMapper;

impl IommuMapper for RecordingMapper {
    fn map(&self, phys: PhysAddr, size: usize, direction: DmaDirection) -> Result<u64, DmaError> {
        let iova = CALL_LOG.next_iova.fetch_add(size as u64, Ordering::Relaxed);
        CALL_LOG.maps.lock().unwrap().push(MapCall {
            phys: phys.as_u64(),
            size,
            direction,
        });
        Ok(iova)
    }

    fn unmap(&self, iova: u64, size: usize) {
        CALL_LOG
            .unmaps
            .lock()
            .unwrap()
            .push(UnmapCall { iova, size });
    }
}

static RECORDING_MAPPER: RecordingMapper = RecordingMapper;
static RECORDING_MAPPER_REF: &'static dyn IommuMapper = &RECORDING_MAPPER;

/// Stands in for an IOMMU policy that rejects a physical range: the error path
/// between "pages allocated" and "handle constructed".
struct RefusingMapper;

impl IommuMapper for RefusingMapper {
    fn map(
        &self,
        _phys: PhysAddr,
        _size: usize,
        _direction: DmaDirection,
    ) -> Result<u64, DmaError> {
        Err(DmaError::Forbidden)
    }

    fn unmap(&self, _iova: u64, _size: usize) {
        panic!("RefusingMapper::unmap called: nothing it mapped can exist");
    }
}

static REFUSING_MAPPER: RefusingMapper = RefusingMapper;
static REFUSING_MAPPER_REF: &'static dyn IommuMapper = &REFUSING_MAPPER;

static SETUP: OnceLock<Mutex<()>> = OnceLock::new();

fn setup() -> MutexGuard<'static, ()> {
    let m = SETUP.get_or_init(|| {
        let layout = std::alloc::Layout::from_size_align(N_PAGES * PAGE_SIZE, PAGE_SIZE)
            .expect("backing layout");
        // SAFETY: `layout.size() > 0`; standard allocator contract.
        let backing_ptr_real: *mut u8 = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!backing_ptr_real.is_null(), "backing alloc failed");
        let backing_ptr = backing_ptr_real.expose_provenance() as u64;
        BACKING_BASE.store(backing_ptr, Ordering::Release);

        let mut slots: Vec<MetaSlot> = (0..N_PAGES).map(|_| MetaSlot::new_unused()).collect();
        let slots_ptr: *mut MetaSlot = slots.as_mut_ptr();
        Box::leak(slots.into_boxed_slice());

        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            init_meta_slots(t, slots_ptr, N_PAGES);
            init_phys_virt_offset(t, backing_ptr);
            register_frame_allocator(t, &BUMP_REF);
            register_iommu_mapper(t, &RECORDING_MAPPER_REF);
        });
        Mutex::new(())
    });
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn snapshot_unmap_count() -> usize {
    CALL_LOG.unmaps.lock().unwrap().len()
}

fn snapshot_last_unmap() -> Option<UnmapCall> {
    CALL_LOG.unmaps.lock().unwrap().last().copied()
}

fn snapshot_dealloc_count() -> usize {
    CALL_LOG.deallocs.lock().unwrap().len()
}

fn snapshot_last_dealloc() -> Option<DeallocCall> {
    CALL_LOG.deallocs.lock().unwrap().last().copied()
}

/// Run `f` with the mapper temporarily swapped for `mapper`. The `setup()`
/// guard serialises tests, so the swap is never observed by another test.
fn with_mapper<R>(mapper: &'static &'static dyn IommuMapper, f: impl FnOnce() -> R) -> R {
    dma::reset_for_test();
    slopos_ostd::sync::run_bsp_init_for_test(|t| register_iommu_mapper(t, mapper));
    let out = f();
    dma::reset_for_test();
    slopos_ostd::sync::run_bsp_init_for_test(|t| register_iommu_mapper(t, &RECORDING_MAPPER_REF));
    out
}

/// Run `f` with the frame allocator handing out an address outside the
/// `MetaSlot` array, so segment construction fails.
fn with_poisoned_allocator<R>(f: impl FnOnce() -> R) -> R {
    BUMP_ALLOC.poisoned.store(true, Ordering::Release);
    let out = f();
    BUMP_ALLOC.poisoned.store(false, Ordering::Release);
    out
}

#[test]
fn coherent_alloc_succeeds_when_registered() {
    let _g = setup();
    let h = DmaCoherent::alloc(2).expect("alloc");
    assert_eq!(h.len_pages(), 2);
    assert_eq!(h.len_bytes(), 2 * PAGE_SIZE);
    assert!(h.iova() >= 0x1_0000_0000);
    let last_map = CALL_LOG
        .maps
        .lock()
        .unwrap()
        .last()
        .copied()
        .expect("map recorded");
    assert_eq!(last_map.size, 2 * PAGE_SIZE);
    assert_eq!(last_map.phys, h.phys_base().as_u64());
}

#[test]
fn coherent_alloc_returns_not_initialised_without_mapper() {
    let _g = setup();
    dma::reset_for_test();
    let r = DmaCoherent::alloc(1);
    assert_eq!(r.unwrap_err(), DmaError::NotInitialised);
    // Re-install so subsequent tests in this binary still work.
    slopos_ostd::sync::run_bsp_init_for_test(|t| {
        register_iommu_mapper(t, &RECORDING_MAPPER_REF);
    });
}

#[test]
fn coherent_alloc_returns_not_initialised_without_frame_alloc() {
    let _g = setup();
    frame_alloc::reset_for_test();
    let r = DmaCoherent::alloc(1);
    assert_eq!(r.unwrap_err(), DmaError::NotInitialised);
    slopos_ostd::sync::run_bsp_init_for_test(|t| {
        register_frame_allocator(t, &BUMP_REF);
    });
}

#[test]
fn coherent_drop_calls_unmap() {
    let _g = setup();
    let before = snapshot_unmap_count();
    let h = DmaCoherent::alloc(1).expect("alloc");
    let iova = h.iova();
    let len = h.len_bytes();
    drop(h);
    let after = snapshot_unmap_count();
    assert_eq!(after, before + 1);
    let last = snapshot_last_unmap().expect("unmap recorded");
    assert_eq!(last.iova, iova);
    assert_eq!(last.size, len);
}

#[test]
fn coherent_read_write_pod_round_trip() {
    let _g = setup();
    let h = DmaCoherent::alloc(1).expect("alloc");
    h.write_pod::<u64>(0, 0xdead_beef_cafe_babe).unwrap();
    let v = h.read_pod::<u64>(0).unwrap();
    assert_eq!(v, 0xdead_beef_cafe_babe);
}

#[test]
fn stream_alloc_records_direction() {
    let _g = setup();
    let s = DmaStream::alloc(1, DmaDirection::ToDevice).expect("alloc");
    assert_eq!(s.direction(), DmaDirection::ToDevice);
    let last_map = CALL_LOG
        .maps
        .lock()
        .unwrap()
        .last()
        .copied()
        .expect("map recorded");
    assert_eq!(last_map.direction, DmaDirection::ToDevice);
}

#[test]
fn stream_drop_calls_unmap() {
    let _g = setup();
    let before = snapshot_unmap_count();
    let s = DmaStream::alloc(1, DmaDirection::FromDevice).expect("alloc");
    let iova = s.iova();
    let len = s.len_bytes();
    drop(s);
    let after = snapshot_unmap_count();
    assert_eq!(after, before + 1);
    let last = snapshot_last_unmap().expect("unmap recorded");
    assert_eq!(last.iova, iova);
    assert_eq!(last.size, len);
}

#[test]
fn stream_sync_methods_are_noops() {
    let _g = setup();
    let s = DmaStream::alloc(1, DmaDirection::Bidirectional).expect("alloc");
    s.sync_for_device();
    s.sync_for_cpu();
}

#[test]
fn coherent_alloc_zero_pages_returns_exhausted() {
    let _g = setup();
    let before = snapshot_dealloc_count();
    let r = DmaCoherent::alloc(0);
    assert_eq!(r.unwrap_err(), DmaError::Exhausted);
    // The zero-page rejection precedes the allocation, so there is nothing
    // to hand back.
    assert_eq!(snapshot_dealloc_count(), before);
}

// `DmaCoherentMeta` / `DmaStreamMeta` declare `returns_frame_on_last_drop() ==
// false`, so nothing in the per-frame lifecycle returns these pages — the run's
// own release does, and the head slot must read `Unused` at that moment for the
// next claimant's `from_unused`.

/// The paddr `BumpAlloc` will hand out next, so a test can name the run an
/// about-to-fail `alloc` is going to take and give back.
fn next_bump_paddr() -> u64 {
    BUMP_ALLOC.next_page.load(Ordering::Relaxed) * PAGE_SIZE as u64
}

#[test]
fn coherent_drop_returns_pages_to_allocator() {
    let _g = setup();
    let before = snapshot_dealloc_count();
    let h = DmaCoherent::alloc(2).expect("alloc");
    let head = h.phys_base().as_u64();
    assert_eq!(
        snapshot_dealloc_count(),
        before,
        "released while still held"
    );
    drop(h);
    assert_eq!(snapshot_dealloc_count(), before + 1);
    let last = snapshot_last_dealloc().expect("dealloc recorded");
    assert_eq!(last.paddr, head);
    assert_eq!(last.size_pages, 2);
    assert_eq!(last.head_slot_kind, SlotMetaKind::Unused);
}

#[test]
fn stream_drop_returns_pages_to_allocator() {
    let _g = setup();
    let before = snapshot_dealloc_count();
    let s = DmaStream::alloc(2, DmaDirection::ToDevice).expect("alloc");
    let head = s.phys_base().as_u64();
    assert_eq!(
        snapshot_dealloc_count(),
        before,
        "released while still held"
    );
    drop(s);
    assert_eq!(snapshot_dealloc_count(), before + 1);
    let last = snapshot_last_dealloc().expect("dealloc recorded");
    assert_eq!(last.paddr, head);
    assert_eq!(last.size_pages, 2);
    assert_eq!(last.head_slot_kind, SlotMetaKind::Unused);
}

#[test]
fn coherent_alloc_returns_pages_when_mapper_refuses() {
    let _g = setup();
    let before = snapshot_dealloc_count();
    let head = next_bump_paddr();
    let r = with_mapper(&REFUSING_MAPPER_REF, || DmaCoherent::alloc(2));
    assert_eq!(r.unwrap_err(), DmaError::Forbidden);
    assert_eq!(snapshot_dealloc_count(), before + 1);
    let last = snapshot_last_dealloc().expect("dealloc recorded");
    assert_eq!(last.paddr, head);
    assert_eq!(last.size_pages, 2);
    assert_eq!(last.head_slot_kind, SlotMetaKind::Unused);
}

#[test]
fn stream_alloc_returns_pages_when_mapper_refuses() {
    let _g = setup();
    let before = snapshot_dealloc_count();
    let head = next_bump_paddr();
    let r = with_mapper(&REFUSING_MAPPER_REF, || {
        DmaStream::alloc(2, DmaDirection::FromDevice)
    });
    assert_eq!(r.unwrap_err(), DmaError::Forbidden);
    assert_eq!(snapshot_dealloc_count(), before + 1);
    let last = snapshot_last_dealloc().expect("dealloc recorded");
    assert_eq!(last.paddr, head);
    assert_eq!(last.size_pages, 2);
    assert_eq!(last.head_slot_kind, SlotMetaKind::Unused);
}

#[test]
fn coherent_alloc_returns_pages_when_segment_build_fails() {
    let _g = setup();
    let before = snapshot_dealloc_count();
    let r = with_poisoned_allocator(|| DmaCoherent::alloc(2));
    assert_eq!(r.unwrap_err(), DmaError::Exhausted);
    assert_eq!(snapshot_dealloc_count(), before + 1);
    let last = snapshot_last_dealloc().expect("dealloc recorded");
    assert_eq!(last.paddr, OUT_OF_SLOT_RANGE_PADDR);
    assert_eq!(last.size_pages, 2);
    // No slot was ever claimed for this run, so there is none to classify.
    assert_eq!(last.head_slot_kind, SlotMetaKind::Unknown);
}

#[test]
fn stream_alloc_returns_pages_when_segment_build_fails() {
    let _g = setup();
    let before = snapshot_dealloc_count();
    let r = with_poisoned_allocator(|| DmaStream::alloc(2, DmaDirection::Bidirectional));
    assert_eq!(r.unwrap_err(), DmaError::Exhausted);
    assert_eq!(snapshot_dealloc_count(), before + 1);
    let last = snapshot_last_dealloc().expect("dealloc recorded");
    assert_eq!(last.paddr, OUT_OF_SLOT_RANGE_PADDR);
    assert_eq!(last.size_pages, 2);
    assert_eq!(last.head_slot_kind, SlotMetaKind::Unknown);
}

#[test]
fn repeated_alloc_and_drop_returns_every_run() {
    let _g = setup();
    for _ in 0..8 {
        let before = snapshot_dealloc_count();
        let head = next_bump_paddr();
        let h = DmaCoherent::alloc(1).expect("alloc");
        assert_eq!(h.phys_base().as_u64(), head);
        drop(h);
        assert_eq!(snapshot_dealloc_count(), before + 1);
        assert_eq!(snapshot_last_dealloc().expect("recorded").paddr, head);
    }
}
