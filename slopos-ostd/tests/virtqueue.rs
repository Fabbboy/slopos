//! Host-side tests for `slopos_ostd::dma::VirtqueueRegion`.

use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_ostd::Pod as PodDerive;
use slopos_ostd::dma::VirtqueueRegion;
use slopos_ostd::mm::frame::{Frame, KernelMeta, MetaSlot, Paddr, init_meta_slots};
use slopos_ostd::mm::phys::init_phys_virt_offset;

const N_PAGES: usize = 8;
const PAGE_SIZE: usize = 4096;

#[repr(C, align(4096))]
struct Backing([u8; PAGE_SIZE * N_PAGES]);

static SETUP: OnceLock<Mutex<()>> = OnceLock::new();

fn setup() -> MutexGuard<'static, ()> {
    let m = SETUP.get_or_init(|| {
        let backing: &'static mut Backing =
            Box::leak(Box::new(Backing([0u8; PAGE_SIZE * N_PAGES])));
        let mut slots: Vec<MetaSlot> = (0..N_PAGES).map(|_| MetaSlot::new_unused()).collect();
        let slots_ptr: *mut MetaSlot = slots.as_mut_ptr();
        Box::leak(slots.into_boxed_slice());
        let backing_ptr = backing.0.as_mut_ptr();
        // Expose provenance once so `phys_to_virt` can soundly
        // reconstruct pointers into this arena under
        // `-Zmiri-strict-provenance`.
        let backing_addr = backing_ptr.expose_provenance() as u64;
        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            init_meta_slots(t, slots_ptr, N_PAGES);
            init_phys_virt_offset(t, backing_addr);
        });
        Mutex::new(())
    });
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PodDerive)]
#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

fn frame_at(paddr: u64) -> Frame<KernelMeta> {
    Frame::<KernelMeta>::from_unused(Paddr::new(paddr), KernelMeta).expect("frame setup")
}

#[test]
fn new_succeeds_with_valid_count() {
    let _g = setup();
    let f = frame_at(0);
    let region = VirtqueueRegion::<Desc>::new(f, 16).expect("fits in 4 KiB");
    assert_eq!(region.desc_count(), 16);
    assert_eq!(region.payload_offset(), 16 * core::mem::size_of::<Desc>());
}

#[test]
fn new_rejects_overflow() {
    let _g = setup();
    let f = frame_at(0x1000);
    // 4 KiB / size_of::<Desc>() = 4096 / 16 = 256 max descriptors.
    assert!(VirtqueueRegion::<Desc>::new(f, 257).is_none());
}

#[test]
fn desc_round_trip() {
    let _g = setup();
    let f = frame_at(0x2000);
    let mut region = VirtqueueRegion::<Desc>::new(f, 8).unwrap();
    let d = Desc {
        addr: 0xDEAD_BEEF_CAFE_BABE,
        len: 1024,
        flags: 0x1,
        next: 3,
    };
    assert!(region.write_desc(2, &d));
    let r = region.desc(2).unwrap();
    assert_eq!(r, d);
}

#[test]
fn write_desc_volatile_round_trip() {
    let _g = setup();
    let f = frame_at(0x3000);
    let mut region = VirtqueueRegion::<Desc>::new(f, 8).unwrap();
    let d = Desc {
        addr: 0x1111_2222_3333_4444,
        len: 64,
        flags: 0,
        next: 0,
    };
    assert!(region.write_desc_volatile(0, d));
    let r = region.read_desc_volatile(0).unwrap();
    assert_eq!(r, d);
}

#[test]
fn desc_out_of_range_returns_none() {
    let _g = setup();
    let f = frame_at(0x4000);
    let region = VirtqueueRegion::<Desc>::new(f, 4).unwrap();
    assert!(region.desc(4).is_none());
    assert!(region.desc(usize::MAX).is_none());
    assert!(region.read_desc_volatile(4).is_none());
}

#[test]
fn write_desc_out_of_range_returns_false() {
    let _g = setup();
    let f = frame_at(0x5000);
    let mut region = VirtqueueRegion::<Desc>::new(f, 4).unwrap();
    let d = Desc {
        addr: 0,
        len: 0,
        flags: 0,
        next: 0,
    };
    assert!(!region.write_desc(4, &d));
    assert!(!region.write_desc_volatile(4, d));
}

#[test]
fn slice_payload_bounds_check() {
    let _g = setup();
    let f = frame_at(0x6000);
    let region = VirtqueueRegion::<Desc>::new(f, 16).unwrap();
    let payload_start = 16 * core::mem::size_of::<Desc>();
    let max = PAGE_SIZE - payload_start;
    assert!(region.slice_payload(0, max).is_some());
    assert!(region.slice_payload(0, max + 1).is_none());
}

#[test]
fn payload_round_trip() {
    let _g = setup();
    let f = frame_at(0x7000);
    let mut region = VirtqueueRegion::<Desc>::new(f, 4).unwrap();
    let src: [u8; 16] = *b"abcdefghijklmnop";
    assert!(region.write_payload(0, &src));
    let mut dst = [0u8; 16];
    assert!(region.read_payload(0, &mut dst));
    assert_eq!(dst, src);
}
