//! Host-side integration tests for `DmaCoherent` / `DmaStream`.
//!
//! Setup mirrors `tests/vm_space.rs`: a leaked page-aligned scratch
//! arena stands in for "physical memory", a leaked `Vec<MetaSlot>`
//! gives per-frame ref-count slots, a multi-page `BumpAlloc`
//! implements `FrameAlloc`, and a `RecordingMapper` impls
//! `IommuMapper` while recording every map/unmap call.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::dma::{
    self, DmaCoherent, DmaDirection, DmaError, DmaStream, IommuMapper, register_iommu_mapper,
};
use slopos_ostd::mm::frame::{FrameAlloc, FrameAllocOptions, MetaSlot, Paddr, init_meta_slots};
use slopos_ostd::mm::frame_alloc::{self, register_frame_allocator};
use slopos_ostd::mm::phys::init_phys_virt_offset;

const N_PAGES: usize = 64;
const PAGE_SIZE: usize = 4096;

/// Bump allocator over the scratch arena. Supports multi-page
/// requests so `DmaCoherent::alloc(npages > 1)` works in tests.
struct BumpAlloc {
    next_page: AtomicU64,
}

impl FrameAlloc for BumpAlloc {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr> {
        let n = opts.size_pages as u64;
        if n == 0 {
            return None;
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
                let virt = (BACKING_BASE.load(Ordering::Acquire) as usize + paddr.as_u64() as usize)
                    as *mut u8;
                core::ptr::write_bytes(virt, 0, n as usize * PAGE_SIZE);
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
    next_page: AtomicU64::new(0),
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
    next_iova: AtomicU64,
}

static CALL_LOG: CallLog = CallLog {
    maps: Mutex::new(Vec::new()),
    unmaps: Mutex::new(Vec::new()),
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

static SETUP: OnceLock<Mutex<()>> = OnceLock::new();

fn setup() -> MutexGuard<'static, ()> {
    let m = SETUP.get_or_init(|| {
        let layout = std::alloc::Layout::from_size_align(N_PAGES * PAGE_SIZE, PAGE_SIZE)
            .expect("backing layout");
        // SAFETY: nonzero size; standard allocator contract.
        let backing_ptr = unsafe { std::alloc::alloc_zeroed(layout) } as u64;
        assert_ne!(backing_ptr, 0, "backing alloc failed");
        BACKING_BASE.store(backing_ptr, Ordering::Release);

        let mut slots: Vec<MetaSlot> = (0..N_PAGES).map(|_| MetaSlot::new_unused()).collect();
        let slots_ptr: *mut MetaSlot = slots.as_mut_ptr();
        Box::leak(slots.into_boxed_slice());

        // SAFETY: leaked storage; pointers live `'static`.
        unsafe {
            init_meta_slots(slots_ptr, N_PAGES);
            init_phys_virt_offset(backing_ptr);
            register_frame_allocator(&BUMP_REF);
            register_iommu_mapper(&RECORDING_MAPPER_REF);
        }
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
    // SAFETY: leaked statics; one-shot already cleared.
    unsafe {
        register_iommu_mapper(&RECORDING_MAPPER_REF);
    }
}

#[test]
fn coherent_alloc_returns_not_initialised_without_frame_alloc() {
    let _g = setup();
    frame_alloc::reset_for_test();
    let r = DmaCoherent::alloc(1);
    assert_eq!(r.unwrap_err(), DmaError::NotInitialised);
    // SAFETY: leaked static; one-shot already cleared.
    unsafe {
        register_frame_allocator(&BUMP_REF);
    }
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
    let r = DmaCoherent::alloc(0);
    assert_eq!(r.unwrap_err(), DmaError::Exhausted);
}
