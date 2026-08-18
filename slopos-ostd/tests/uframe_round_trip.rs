//! Round-trip integration tests for `UFrame` / `USegment`.
//!
//! These run host-side under `cargo test` against a scratch `META_SLOTS` array
//! and a phys-virt offset over a leaked, page-aligned heap buffer.

use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_ostd::mm::frame::{AnonymousMeta, MetaSlot, Paddr, init_meta_slots};
use slopos_ostd::mm::phys::init_phys_virt_offset;
use slopos_ostd::mm::uframe::{UFrame, UFrameError, USegment};

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
        // OSTD keeps a `'static` view of the slots.
        Box::leak(slots.into_boxed_slice());
        let backing_ptr = backing.0.as_mut_ptr();
        // Expose provenance so `phys_to_virt`'s `with_exposed_provenance_mut`
        // can soundly reconstruct pointers into this arena.
        let backing_addr = backing_ptr.expose_provenance() as u64;
        // The offset places paddr `0` at the start of the backing buffer, so
        // paddrs in `[0, N_PAGES * 4096)` map into it.
        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            init_meta_slots(t, slots_ptr, N_PAGES);
            init_phys_virt_offset(t, backing_addr);
        });
        Mutex::new(())
    });
    m.lock().unwrap()
}

#[test]
fn round_trip_u64_pod() {
    let _g = setup();
    let f = UFrame::<AnonymousMeta>::from_unused(Paddr::new(0), AnonymousMeta::default()).unwrap();
    f.write_pod::<u64>(8, 0xdead_beef_cafe_babe).unwrap();
    let v = f.read_pod::<u64>(8).unwrap();
    assert_eq!(v, 0xdead_beef_cafe_babe);
    drop(f);
    // Re-arm the slot for tests reusing this paddr: `from_unused` requires the
    // UNUSED → TYPED transition.
    let _ = UFrame::<AnonymousMeta>::from_unused(Paddr::new(0), AnonymousMeta::default()).unwrap();
}

#[test]
fn round_trip_array_pod() {
    let _g = setup();
    let f =
        UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x1000), AnonymousMeta::default()).unwrap();
    let val: [u8; 16] = *b"abcdefghijklmnop";
    f.write_pod(0, val).unwrap();
    assert_eq!(f.read_pod::<[u8; 16]>(0).unwrap(), val);
}

#[test]
fn round_trip_bytes() {
    let _g = setup();
    let f =
        UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x2000), AnonymousMeta::default()).unwrap();
    let src: [u8; 64] = core::array::from_fn(|i| i as u8);
    f.write_bytes(100, &src).unwrap();
    let mut dst = [0u8; 64];
    f.read_bytes(100, &mut dst).unwrap();
    assert_eq!(src, dst);
}

#[test]
fn out_of_bounds_returns_err() {
    let _g = setup();
    let f =
        UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x3000), AnonymousMeta::default()).unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(f.read_bytes(4090, &mut buf), Err(UFrameError::OutOfBounds));
    assert_eq!(
        f.write_bytes(4096, &[0u8; 1]),
        Err(UFrameError::OutOfBounds)
    );
}

#[test]
fn misaligned_pod_returns_err() {
    let _g = setup();
    let f =
        UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x4000), AnonymousMeta::default()).unwrap();
    assert_eq!(f.read_pod::<u64>(1).unwrap_err(), UFrameError::Misaligned);
    assert_eq!(
        f.write_pod::<u32>(2, 0).unwrap_err(),
        UFrameError::Misaligned
    );
}

#[test]
fn round_trip_derive_pod() {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq, slopos_ostd::Pod)]
    struct Header {
        magic: u32,
        version: u32,
        len: u64,
    }

    let _g = setup();
    let f =
        UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x5000), AnonymousMeta::default()).unwrap();
    let h = Header {
        magic: 0xfeed_face,
        version: 7,
        len: 1024,
    };
    f.write_pod(64, h).unwrap();
    assert_eq!(f.read_pod::<Header>(64).unwrap(), h);
}

#[test]
fn volatile_u32_index_round_trip() {
    let _g = setup();
    let f =
        UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x5000), AnonymousMeta::default()).unwrap();
    f.store_u32_release(0, 0xabad_1dea).unwrap();
    assert_eq!(f.load_u32_acquire(0).unwrap(), 0xabad_1dea);
    f.store_u32_release(64, 7).unwrap();
    assert_eq!(f.load_u32_acquire(64).unwrap(), 7);
}

#[test]
fn volatile_u32_rejects_misaligned_and_oob() {
    let _g = setup();
    let f =
        UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x4000), AnonymousMeta::default()).unwrap();
    assert_eq!(f.load_u32_acquire(1).unwrap_err(), UFrameError::Misaligned);
    assert_eq!(
        f.store_u32_release(2, 0).unwrap_err(),
        UFrameError::Misaligned
    );
    assert_eq!(
        f.load_u32_acquire(4096).unwrap_err(),
        UFrameError::OutOfBounds
    );
}

#[test]
fn volatile_byte_copy_round_trip() {
    let _g = setup();
    let f =
        UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x3000), AnonymousMeta::default()).unwrap();
    let src: [u8; 64] = core::array::from_fn(|i| (i as u8).wrapping_mul(3));
    f.copy_in_volatile(128, &src).unwrap();
    let mut dst = [0u8; 64];
    f.copy_out_volatile(128, &mut dst).unwrap();
    assert_eq!(src, dst);
    let mut overflow = [0u8; 8];
    assert_eq!(
        f.copy_out_volatile(4090, &mut overflow),
        Err(UFrameError::OutOfBounds)
    );
    assert_eq!(
        f.copy_in_volatile(4096, &[0u8; 1]),
        Err(UFrameError::OutOfBounds)
    );
}

#[test]
fn usegment_round_trip_crosses_page_boundary() {
    let _g = setup();
    let seg = USegment::<AnonymousMeta>::from_unused_run(Paddr::new(0x6000), 2).unwrap();
    assert_eq!(seg.len_pages(), 2);
    assert_eq!(seg.len_bytes(), 8192);

    let src: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
    seg.write_bytes(4000, &src).unwrap();
    let mut dst = vec![0u8; 256];
    seg.read_bytes(4000, &mut dst).unwrap();
    assert_eq!(src, dst);

    let mut overflow = [0u8; 8];
    assert_eq!(
        seg.read_bytes(8189, &mut overflow),
        Err(UFrameError::OutOfBounds)
    );

    let slices = seg.io_slices();
    assert_eq!(slices.len(), 1);
    assert_eq!(slices[0].paddr.as_u64(), 0x6000);
    assert_eq!(slices[0].len, 8192);
}
