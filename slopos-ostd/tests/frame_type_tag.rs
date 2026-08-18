//! Regression tests for the `MetaSlot` type-identity gate.
//!
//! `Frame::from_in_use` decides identity from the canonical `TypeId` in the
//! slot's vtable, not from the vtable pointer: a `const`-promoted vtable
//! static has no unique address across crates or codegen units, while a
//! `TypeId` is identical in every crate by language guarantee.
//!
//! Same single-process / shared-static isolation discipline as
//! `uframe_round_trip.rs`.

use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_ostd::mm::frame::{
    AnonymousMeta, Frame, FrameError, KernelMeta, MetaSlot, Paddr, init_meta_slots,
};

const N_PAGES: usize = 8;

static SETUP: OnceLock<Mutex<()>> = OnceLock::new();

fn setup() -> MutexGuard<'static, ()> {
    let m = SETUP.get_or_init(|| {
        let mut slots: Vec<MetaSlot> = (0..N_PAGES).map(|_| MetaSlot::new_unused()).collect();
        let slots_ptr: *mut MetaSlot = slots.as_mut_ptr();
        // Leaked so OSTD's `'static` view stays valid for the test binary;
        // only the slot array is touched, so no phys-virt offset is needed.
        Box::leak(slots.into_boxed_slice());
        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            init_meta_slots(t, slots_ptr, N_PAGES);
        });
        Mutex::new(())
    });
    m.lock().unwrap()
}

#[test]
fn from_in_use_rejects_wrong_type() {
    let _g = setup();
    let pa = Paddr::new(0x1000);
    let kernel = Frame::<KernelMeta>::from_unused(pa, KernelMeta).unwrap();
    assert_eq!(kernel.reference_count(), 1);

    assert_eq!(
        Frame::<AnonymousMeta>::from_in_use(pa).err(),
        Some(FrameError::StateMismatch)
    );
    assert_eq!(kernel.reference_count(), 1);

    drop(kernel);
}

#[test]
fn from_in_use_accepts_matching_type_and_bumps() {
    let _g = setup();
    let pa = Paddr::new(0x2000);
    let first = Frame::<KernelMeta>::from_unused(pa, KernelMeta).unwrap();
    assert_eq!(first.reference_count(), 1);

    let second = Frame::<KernelMeta>::from_in_use(pa).unwrap();
    assert_eq!(first.reference_count(), 2);
    assert_eq!(second.reference_count(), 2);

    drop(second);
    assert_eq!(first.reference_count(), 1);
    drop(first);
}

#[test]
fn from_in_use_rejects_unused_slot() {
    let _g = setup();
    let pa = Paddr::new(0x3000);
    assert_eq!(
        Frame::<KernelMeta>::from_in_use(pa).err(),
        Some(FrameError::StateMismatch)
    );
}
