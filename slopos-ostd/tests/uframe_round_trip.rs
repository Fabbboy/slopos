//! Round-trip integration tests for `UFrame` / `USegment`.
//!
//! These run host-side under `cargo test`. They install a scratch
//! `META_SLOTS` array and a phys-virt offset that maps physical
//! address `0` (within the test arena) to the start of a leaked
//! page-aligned heap buffer, then exercise the byte-copy interface.
//!
//! Test isolation: `cargo test` runs tests in this binary on
//! multiple threads in one process. We share the static OSTD state
//! across tests via a `OnceLock<Mutex<()>>` setup gate, so
//! `init_meta_slots` and `init_phys_virt_offset` fire exactly once
//! and each test serialises against the others while it owns the
//! arena.

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
        // Leak the slot array so the `'static` view OSTD takes
        // remains valid for the lifetime of the test binary.
        Box::leak(slots.into_boxed_slice());
        let backing_ptr = backing.0.as_mut_ptr();
        // SAFETY: slots_ptr / backing live for `'static` (both
        // leaked above); the offset places paddr `0` at the start
        // of the backing buffer, so paddrs in `[0, N_PAGES * 4096)`
        // map into the buffer.
        unsafe {
            init_meta_slots(slots_ptr, N_PAGES);
            init_phys_virt_offset(backing_ptr as u64);
        }
        Mutex::new(())
    });
    m.lock().unwrap()
}

#[test]
fn round_trip_u64_pod() {
    let _g = setup();
    let f = UFrame::<AnonymousMeta>::from_unused(Paddr::new(0), AnonymousMeta).unwrap();
    f.write_pod::<u64>(8, 0xdead_beef_cafe_babe).unwrap();
    let v = f.read_pod::<u64>(8).unwrap();
    assert_eq!(v, 0xdead_beef_cafe_babe);
    drop(f);
    // Reset the slot for re-use by other tests addressing the same
    // paddr (each from_unused requires UNUSED → TYPED transition).
    let _ = UFrame::<AnonymousMeta>::from_unused(Paddr::new(0), AnonymousMeta).unwrap();
}

#[test]
fn round_trip_array_pod() {
    let _g = setup();
    let f = UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x1000), AnonymousMeta).unwrap();
    let val: [u8; 16] = *b"abcdefghijklmnop";
    f.write_pod(0, val).unwrap();
    assert_eq!(f.read_pod::<[u8; 16]>(0).unwrap(), val);
}

#[test]
fn round_trip_bytes() {
    let _g = setup();
    let f = UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x2000), AnonymousMeta).unwrap();
    let src: [u8; 64] = core::array::from_fn(|i| i as u8);
    f.write_bytes(100, &src).unwrap();
    let mut dst = [0u8; 64];
    f.read_bytes(100, &mut dst).unwrap();
    assert_eq!(src, dst);
}

#[test]
fn out_of_bounds_returns_err() {
    let _g = setup();
    let f = UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x3000), AnonymousMeta).unwrap();
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
    let f = UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x4000), AnonymousMeta).unwrap();
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
    let f = UFrame::<AnonymousMeta>::from_unused(Paddr::new(0x5000), AnonymousMeta).unwrap();
    let h = Header {
        magic: 0xfeed_face,
        version: 7,
        len: 1024,
    };
    f.write_pod(64, h).unwrap();
    assert_eq!(f.read_pod::<Header>(64).unwrap(), h);
}

#[test]
fn usegment_round_trip_crosses_page_boundary() {
    let _g = setup();
    let seg = USegment::<AnonymousMeta>::from_unused_run(Paddr::new(0x6000), 2).unwrap();
    assert_eq!(seg.len_pages(), 2);
    assert_eq!(seg.len_bytes(), 8192);

    // Write a buffer that straddles the two physical pages.
    let src: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
    seg.write_bytes(4000, &src).unwrap();
    let mut dst = vec![0u8; 256];
    seg.read_bytes(4000, &mut dst).unwrap();
    assert_eq!(src, dst);

    // Bounds check at the end of the run.
    let mut overflow = [0u8; 8];
    assert_eq!(
        seg.read_bytes(8189, &mut overflow),
        Err(UFrameError::OutOfBounds)
    );

    // Vectored-I/O descriptor is single-element + correct.
    let slices = seg.io_slices();
    assert_eq!(slices.len(), 1);
    assert_eq!(slices[0].paddr.as_u64(), 0x6000);
    assert_eq!(slices[0].len, 8192);
}
