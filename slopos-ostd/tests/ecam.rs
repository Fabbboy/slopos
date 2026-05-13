//! Host-side tests for `slopos_ostd::pci::EcamConfigSpace`.
//!
//! Sets up a fake `IoMem` over a leaked backing buffer (pattern
//! mirrors `tests/io_mem.rs`) and exercises the BDF arithmetic,
//! bounds-checks, and the read/write round-trip.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::io_mem::{
    IoMemCachePolicy, IoMemError, IoMemMapper, IoMemRegistry, PhysRange, register_io_mem_mapper,
    register_io_mem_registry,
};
use slopos_ostd::pci::{Bdf, EcamConfigSpace, EcamError};

const PAGE_SIZE: usize = 4096;
// Cover bus_start..bus_end inclusive, so 4 buses == 4 MiB.
const BUS_START: u8 = 0;
const BUS_END: u8 = 3;
const BUSES: usize = (BUS_END as usize - BUS_START as usize) + 1;
const ECAM_SIZE: usize = BUSES * (1 << 20);
const ECAM_BASE: u64 = 0xE000_0000;

static BACKING_BASE: AtomicU64 = AtomicU64::new(0);

struct FakeMapper;

impl IoMemMapper for FakeMapper {
    fn map(
        &self,
        phys: PhysAddr,
        _size: usize,
        _policy: IoMemCachePolicy,
    ) -> Result<u64, IoMemError> {
        let offset = phys.as_u64() - ECAM_BASE;
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
        base: PhysAddr::new(ECAM_BASE),
        len: ECAM_SIZE,
    }]))
}

fn setup() -> MutexGuard<'static, ()> {
    let m = SETUP.get_or_init(|| {
        let layout =
            std::alloc::Layout::from_size_align(ECAM_SIZE, PAGE_SIZE).expect("backing layout");
        // SAFETY: nonzero size; standard allocator contract.
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

fn ecam() -> EcamConfigSpace {
    let region = IoMemRegistry::reserve(
        PhysAddr::new(ECAM_BASE),
        ECAM_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .expect("reserve ECAM region");
    EcamConfigSpace::new(region, BUS_START, BUS_END).expect("ecam ctor")
}

#[test]
fn bdf_constructor_rejects_oversized_device() {
    assert!(Bdf::new(0, 32, 0).is_none());
    assert!(Bdf::new(0, 31, 0).is_some());
}

#[test]
fn bdf_constructor_rejects_oversized_function() {
    assert!(Bdf::new(0, 0, 8).is_none());
    assert!(Bdf::new(0, 0, 7).is_some());
}

#[test]
fn ecam_new_rejects_inverted_bus_range() {
    let _g = setup();
    let region = IoMemRegistry::reserve(
        PhysAddr::new(ECAM_BASE),
        ECAM_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .unwrap();
    assert!(EcamConfigSpace::new(region, 5, 1).is_none());
}

#[test]
fn ecam_new_rejects_undersized_region() {
    let _g = setup();
    // Reserve only one page — much smaller than the 4 MiB the bus
    // range demands.
    let region = IoMemRegistry::reserve(
        PhysAddr::new(ECAM_BASE),
        PAGE_SIZE,
        IoMemCachePolicy::Uncacheable,
    )
    .unwrap();
    assert!(EcamConfigSpace::new(region, 0, 3).is_none());
}

#[test]
fn contains_reflects_bus_range() {
    let _g = setup();
    let e = ecam();
    let (lo, hi) = e.bus_range();
    assert_eq!((lo, hi), (BUS_START, BUS_END));
    assert!(e.contains(Bdf::new(0, 0, 0).unwrap()));
    assert!(e.contains(Bdf::new(BUS_END, 0, 0).unwrap()));
    assert!(!e.contains(Bdf::new(BUS_END + 1, 0, 0).unwrap()));
}

#[test]
fn read_write_u32_round_trip() {
    let _g = setup();
    let e = ecam();
    let bdf = Bdf::new(2, 5, 1).unwrap();
    e.write::<u32>(bdf, 0x10, 0xDEAD_BEEF).expect("write");
    let v: u32 = e.read::<u32>(bdf, 0x10).expect("read");
    assert_eq!(v, 0xDEAD_BEEF);
}

#[test]
fn read_write_u16_round_trip() {
    let _g = setup();
    let e = ecam();
    let bdf = Bdf::new(0, 0, 0).unwrap();
    e.write::<u16>(bdf, 0x4, 0xBABE).expect("write");
    assert_eq!(e.read::<u16>(bdf, 0x4).unwrap(), 0xBABE);
}

#[test]
fn read_returns_err_for_out_of_range_bus() {
    let _g = setup();
    let e = ecam();
    let bdf = Bdf {
        bus: BUS_END + 1,
        device: 0,
        function: 0,
    };
    assert_eq!(e.try_read::<u32>(bdf, 0), Err(EcamError::BusOutOfRange));
}

#[test]
fn distinct_bdf_addresses_are_independent() {
    let _g = setup();
    let e = ecam();
    let a = Bdf::new(0, 0, 0).unwrap();
    let b = Bdf::new(0, 0, 1).unwrap();
    e.write::<u32>(a, 0, 0xAAAA_AAAA).unwrap();
    e.write::<u32>(b, 0, 0xBBBB_BBBB).unwrap();
    assert_eq!(e.read::<u32>(a, 0).unwrap(), 0xAAAA_AAAA);
    assert_eq!(e.read::<u32>(b, 0).unwrap(), 0xBBBB_BBBB);
}
