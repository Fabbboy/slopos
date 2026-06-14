//! Regression tests for the `MetaSlot` type-identity gate.
//!
//! `Frame::from_in_use` must hand back a `Frame<M>` only when the slot
//! actually holds an `M`. It decides this from the canonical `TypeId` the
//! slot's vtable carries, NOT from the vtable pointer — a `const`-promoted
//! vtable static has no unique address across crates/codegen units, so in
//! release builds the `mm` crate's copy diverged from the `slopos-ostd` copy
//! stored by `from_unused`, making `from_in_use` spuriously report a live
//! `RingMeta` slot as a type mismatch and wedging desktop bring-up. A
//! `TypeId` value is identical in every crate by language guarantee. These
//! tests pin the gate's behaviour: a wrong type is rejected, the right type
//! is accepted (bumping the ref-count), and a type mismatch is reported as
//! `StateMismatch`.
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
        // Leak the slot array so OSTD's `'static` view stays valid for the
        // lifetime of the test binary. `from_unused`/`from_in_use` only
        // touch the slot array (not the backing pages), so this test needs
        // no phys-virt offset.
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
    // Install a KernelMeta slot.
    let kernel = Frame::<KernelMeta>::from_unused(pa, KernelMeta).unwrap();
    assert_eq!(kernel.reference_count(), 1);

    // A from_in_use for a *different* meta type must be refused — this is
    // the gate that, when it (wrongly) tripped across the mm/ostd crate
    // boundary in release builds, killed the SlopRing map path.
    assert_eq!(
        Frame::<AnonymousMeta>::from_in_use(pa).err(),
        Some(FrameError::StateMismatch)
    );
    // The refused call must not have bumped the count.
    assert_eq!(kernel.reference_count(), 1);

    drop(kernel);
}

#[test]
fn from_in_use_accepts_matching_type_and_bumps() {
    let _g = setup();
    let pa = Paddr::new(0x2000);
    let first = Frame::<KernelMeta>::from_unused(pa, KernelMeta).unwrap();
    assert_eq!(first.reference_count(), 1);

    // Same type → succeeds and bumps the ref-count.
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
    // A never-installed slot is UNUSED → from_in_use must refuse, not
    // fabricate a frame over uninitialised storage.
    let pa = Paddr::new(0x3000);
    assert_eq!(
        Frame::<KernelMeta>::from_in_use(pa).err(),
        Some(FrameError::StateMismatch)
    );
}
