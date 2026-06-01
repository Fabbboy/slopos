//! Round-trip tests for the `VmReader` / `VmWriter` volatile cursors.
//!
//! Run host-side under `cargo test` and under KernMiri (`just check-miri`).
//! Mirror `uframe_round_trip.rs`'s arena setup: install a scratch `META_SLOTS`
//! array + a phys-virt offset mapping paddr `0` to a leaked page-aligned heap
//! buffer, then build real `UFrame<AnonymousMeta>`s over that arena and drive
//! the cursors across page boundaries.

use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_ostd::mm::frame::{AnonymousMeta, MetaSlot, Paddr, init_meta_slots};
use slopos_ostd::mm::phys::init_phys_virt_offset;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vmcursor::{VmReader, VmWriter};

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
        let backing_addr = backing_ptr.expose_provenance() as u64;
        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            init_meta_slots(t, slots_ptr, N_PAGES);
            init_phys_virt_offset(t, backing_addr);
        });
        Mutex::new(())
    });
    m.lock().unwrap()
}

/// Build `n` consecutive owning `UFrame`s starting at physical page `base_page`
/// within the arena. Each page is used by exactly one test (no cross-test
/// paddr reuse) so the `UNUSED → TYPED` slot transition always succeeds.
fn frames(base_page: usize, n: usize) -> Vec<UFrame<AnonymousMeta>> {
    (0..n)
        .map(|i| {
            let pa = ((base_page + i) * PAGE_SIZE) as u64;
            UFrame::<AnonymousMeta>::from_unused(Paddr::new(pa), AnonymousMeta::default()).unwrap()
        })
        .collect()
}

fn ramp(seed: u8) -> [u8; PAGE_SIZE] {
    let mut p = [0u8; PAGE_SIZE];
    for (i, b) in p.iter_mut().enumerate() {
        *b = ((i + seed as usize) % 251) as u8;
    }
    p
}

#[test]
fn reader_crosses_page_boundary() {
    let _g = setup();
    let fr = frames(0, 2);
    let s0 = ramp(0);
    let s1 = ramp(7);
    fr[0].copy_in_volatile(0, &s0).unwrap();
    fr[1].copy_in_volatile(0, &s1).unwrap();

    // Span the last 100 bytes of page 0 + the first 200 bytes of page 1.
    let mut r = VmReader::new(&fr, PAGE_SIZE - 100, 300).unwrap();
    assert_eq!(r.remain(), 300);
    assert!(r.has_remain());

    let mut out = [0u8; 300];
    assert_eq!(r.read(&mut out), 300);
    assert_eq!(r.remain(), 0);
    assert!(!r.has_remain());

    assert_eq!(&out[..100], &s0[PAGE_SIZE - 100..]);
    assert_eq!(&out[100..], &s1[..200]);

    // A dry cursor yields nothing more.
    let mut tail = [0u8; 8];
    assert_eq!(r.read(&mut tail), 0);
}

#[test]
fn writer_crosses_page_boundary() {
    let _g = setup();
    let fr = frames(2, 2);

    let payload: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
    let mut w = VmWriter::new(&fr, PAGE_SIZE - 100, 300).unwrap();
    assert_eq!(w.write(&payload), 300);
    assert_eq!(w.remain(), 0);

    let mut tail0 = [0u8; 100];
    fr[0]
        .copy_out_volatile(PAGE_SIZE - 100, &mut tail0)
        .unwrap();
    assert_eq!(&tail0[..], &payload[..100]);

    let mut head1 = [0u8; 200];
    fr[1].copy_out_volatile(0, &mut head1).unwrap();
    assert_eq!(&head1[..], &payload[100..]);
}

#[test]
fn reader_spans_three_pages_in_small_chunks() {
    let _g = setup();
    let fr = frames(4, 3);
    for (i, f) in fr.iter().enumerate() {
        f.copy_in_volatile(0, &ramp((i * 13) as u8)).unwrap();
    }

    // Whole three-page range, drained 37 bytes at a time (resumable, advancing
    // across both page boundaries).
    let total = 3 * PAGE_SIZE;
    let mut r = VmReader::new(&fr, 0, total).unwrap();
    let mut collected = Vec::with_capacity(total);
    let mut chunk = [0u8; 37];
    loop {
        let n = r.read(&mut chunk);
        if n == 0 {
            break;
        }
        collected.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(collected.len(), total);
    for i in 0..fr.len() {
        assert_eq!(
            &collected[i * PAGE_SIZE..(i + 1) * PAGE_SIZE],
            &ramp((i * 13) as u8)[..]
        );
    }
}

#[test]
fn read_is_capped_by_remaining_then_dst() {
    let _g = setup();
    let fr = frames(7, 1);
    fr[0].copy_in_volatile(0, &ramp(3)).unwrap();

    // Only 10 bytes in the logical range, but a larger dst.
    let mut r = VmReader::new(&fr, 5, 10).unwrap();
    let mut out = [0xFFu8; 64];
    assert_eq!(r.read(&mut out), 10);
    assert_eq!(r.remain(), 0);
    let s = ramp(3);
    assert_eq!(&out[..10], &s[5..15]);
    assert_eq!(out[10], 0xFF); // untouched past the copied region
}

#[test]
fn new_rejects_out_of_range() {
    let _g = setup();
    let fr = frames(0, 1); // re-uses page 0; serialized by the setup mutex
    // abs_start + len > n_frames * PAGE_SIZE.
    assert!(VmReader::new(&fr, 0, PAGE_SIZE + 1).is_none());
    assert!(VmWriter::new(&fr, PAGE_SIZE, 1).is_none());
    // Exactly the chain length is fine.
    assert!(VmReader::new(&fr, 0, PAGE_SIZE).is_some());
}
