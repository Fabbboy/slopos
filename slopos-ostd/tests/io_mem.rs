//! Host-side integration tests for `IoMem` / `IoMemRegistry`.
//!
//! Setup pattern matches `tests/uframe_round_trip.rs` and
//! `tests/vm_space.rs`: a leaked page-aligned `Backing` array stands
//! in for "MMIO" storage, a fake [`IoMemMapper`] implementation maps
//! every requested phys address to the same offset within the
//! backing buffer, and a leaked `&'static [PhysRange]` covers the
//! address space we test against.
//!
//! Tests share a `OnceLock<Mutex<()>>` setup gate so the OSTD
//! globals are wired exactly once and tests serialise.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::io_mem::{
    self, IoMemCachePolicy, IoMemError, IoMemMapper, IoMemRegistry, PhysRange,
    register_io_mem_mapper, register_io_mem_range, register_io_mem_registry,
};

const N_PAGES: usize = 16;
const PAGE_SIZE: usize = 4096;
const REGION_BASE: u64 = 0xfee0_0000;
const REGION_SIZE: usize = N_PAGES * PAGE_SIZE;

static BACKING_BASE: AtomicU64 = AtomicU64::new(0);

struct FakeMapper;

impl IoMemMapper for FakeMapper {
    fn map(
        &self,
        phys: PhysAddr,
        _size: usize,
        _policy: IoMemCachePolicy,
    ) -> Result<u64, IoMemError> {
        let offset = phys.as_u64() - REGION_BASE;
        let backing = BACKING_BASE.load(Ordering::Acquire);
        Ok(backing + offset)
    }

    fn unmap(&self, _virt: u64, _size: usize) {}
}

static FAKE_MAPPER: FakeMapper = FakeMapper;
static FAKE_MAPPER_REF: &'static dyn IoMemMapper = &FAKE_MAPPER;

static SETUP: OnceLock<Mutex<()>> = OnceLock::new();

fn ranges_static() -> &'static [PhysRange] {
    Box::leak(Box::new([PhysRange {
        base: PhysAddr::new(REGION_BASE),
        len: REGION_SIZE,
    }]))
}

fn setup() -> MutexGuard<'static, ()> {
    let m = SETUP.get_or_init(|| {
        let layout =
            std::alloc::Layout::from_size_align(REGION_SIZE, PAGE_SIZE).expect("backing layout");
        // SAFETY: layout has nonzero size; standard allocator contract.
        let backing_ptr = unsafe { std::alloc::alloc_zeroed(layout) } as u64;
        assert_ne!(backing_ptr, 0, "backing alloc failed");
        BACKING_BASE.store(backing_ptr, Ordering::Release);
        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            register_io_mem_registry(t, ranges_static());
            register_io_mem_mapper(t, &FAKE_MAPPER_REF);
        });
        Mutex::new(())
    });
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone, Copy, slopos_ostd::Pod)]
#[repr(C)]
struct DeviceRegs {
    status: u32,
    control: u32,
    data: u64,
}

#[test]
fn reserve_succeeds_for_registered_range() {
    let _g = setup();
    let m = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve");
    assert_eq!(m.size(), PAGE_SIZE);
    assert_eq!(m.phys_base().as_u64(), REGION_BASE);
}

#[test]
fn reserve_rejects_unregistered_range() {
    let _g = setup();
    let r = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE - 0x10000),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    );
    assert_eq!(r.unwrap_err(), IoMemError::NotReserved);
}

#[test]
fn reserve_rejects_when_uninitialised() {
    let _g = setup();
    io_mem::reset_for_test();
    let r = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    );
    assert_eq!(r.unwrap_err(), IoMemError::Uninitialised);
    // Re-install for subsequent tests in this binary.
    slopos_ostd::sync::run_bsp_init_for_test(|t| {
        register_io_mem_registry(t, ranges_static());
        register_io_mem_mapper(t, &FAKE_MAPPER_REF);
    });
}

#[test]
fn read_write_round_trip_u32() {
    let _g = setup();
    let m = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE + (1 * PAGE_SIZE) as u64),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve");
    m.write::<u32>(0, 0xdead_beef);
    assert_eq!(m.read::<u32>(0), 0xdead_beef);
    m.write::<u32>(8, 0xfeed_face);
    assert_eq!(m.read::<u32>(8), 0xfeed_face);
}

#[test]
fn read_write_round_trip_pod_struct() {
    let _g = setup();
    let m = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE + (2 * PAGE_SIZE) as u64),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve");
    let regs = DeviceRegs {
        status: 0x1234_5678,
        control: 0x9abc_def0,
        data: 0xcafe_babe_dead_beef,
    };
    m.write::<DeviceRegs>(0, regs);
    let read_back = m.read::<DeviceRegs>(0);
    assert_eq!(read_back.status, regs.status);
    assert_eq!(read_back.control, regs.control);
    assert_eq!(read_back.data, regs.data);
}

#[test]
fn try_read_returns_out_of_bounds() {
    let _g = setup();
    let m = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE + (3 * PAGE_SIZE) as u64),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve");
    let r: Result<u64, IoMemError> = m.try_read(PAGE_SIZE);
    assert_eq!(r.unwrap_err(), IoMemError::OutOfBounds);
    let r: Result<u64, IoMemError> = m.try_read(PAGE_SIZE - 4);
    assert_eq!(r.unwrap_err(), IoMemError::OutOfBounds);
}

#[test]
fn try_read_returns_misaligned() {
    let _g = setup();
    let m = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE + (4 * PAGE_SIZE) as u64),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve");
    let r: Result<u64, IoMemError> = m.try_read(1);
    assert_eq!(r.unwrap_err(), IoMemError::Misaligned);
}

#[test]
fn try_write_returns_out_of_bounds() {
    let _g = setup();
    let m = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE + (5 * PAGE_SIZE) as u64),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve");
    let r = m.try_write::<u32>(PAGE_SIZE, 0);
    assert_eq!(r.unwrap_err(), IoMemError::OutOfBounds);
}

#[test]
fn sub_region_inherits_phys_offset() {
    let _g = setup();
    let m = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE + (6 * PAGE_SIZE) as u64),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve");
    let s = m.sub_region(0x100, 0x200).expect("sub_region");
    assert_eq!(s.size(), 0x200);
    assert_eq!(
        s.phys_base().as_u64(),
        REGION_BASE + (6 * PAGE_SIZE) as u64 + 0x100
    );
}

#[test]
fn sub_region_rejects_overrun() {
    let _g = setup();
    let m = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE + (7 * PAGE_SIZE) as u64),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve");
    assert!(m.sub_region(0x800, 0x900).is_none());
    assert!(m.sub_region(0, PAGE_SIZE + 1).is_none());
}

#[test]
fn dynamic_range_register_then_reserve() {
    let _g = setup();
    // Pick a phys range outside the static slice (the static covers
    // REGION_BASE..REGION_BASE + REGION_SIZE, i.e. 0xfee0_0000..
    // 0xfee1_0000). 0xfee2_0000 is comfortably outside.
    let dyn_base = PhysAddr::new(REGION_BASE + (REGION_SIZE as u64) * 2);
    register_io_mem_range(PhysRange {
        base: dyn_base,
        len: PAGE_SIZE,
    })
    .expect("register_io_mem_range");
    // Reserve a sub-range — fake mapper happily produces a virt
    // address (we don't dereference; just check reserve succeeds and
    // returns the right metadata).
    let m = IoMemRegistry::reserve(dyn_base, PAGE_SIZE, IoMemCachePolicy::Uncacheable)
        .expect("reserve");
    assert_eq!(m.size(), PAGE_SIZE);
    assert_eq!(m.phys_base().as_u64(), dyn_base.as_u64());
}

#[test]
fn dynamic_range_outside_static_and_dynamic_rejected() {
    let _g = setup();
    // 0xff00_0000 is outside both the static slice and any range the
    // companion `dynamic_range_register_then_reserve` test registers,
    // so the order in which the two tests run doesn't matter.
    let r = IoMemRegistry::reserve(
        PhysAddr::new(0xff00_0000),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    );
    assert_eq!(r.unwrap_err(), IoMemError::NotReserved);
}

#[test]
fn iomem_is_clone() {
    let _g = setup();
    let m = IoMemRegistry::reserve(
        PhysAddr::new(REGION_BASE + (8 * PAGE_SIZE) as u64),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve");
    let n = m.clone();
    assert_eq!(m.phys_base().as_u64(), n.phys_base().as_u64());
    assert_eq!(m.size(), n.size());
}
