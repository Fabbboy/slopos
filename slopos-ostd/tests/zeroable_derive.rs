//! Smoke tests for the `#[derive(Zeroable)]` proc macro.

use slopos_ostd::{Zeroable, init_zeroed};

#[derive(Zeroable)]
#[repr(C)]
struct Plain {
    a: u32,
    b: u64,
    c: [u16; 4],
}

#[derive(Zeroable)]
#[repr(transparent)]
struct Wrapped(u32);

#[derive(Zeroable)]
#[repr(C)]
struct UnitFields;

fn assert_zeroable<T: Zeroable>() {}

#[test]
fn derive_emits_zeroable_for_named_struct() {
    assert_zeroable::<Plain>();
}

#[test]
fn derive_emits_zeroable_for_transparent_newtype() {
    assert_zeroable::<Wrapped>();
}

#[test]
fn derive_emits_zeroable_for_unit_struct() {
    assert_zeroable::<UnitFields>();
}

#[test]
fn init_zeroed_works_with_derived_zeroable() {
    // `init_zeroed::<T>()` requires `T: Zeroable`; if the derive is
    // sound the bound resolves and the recipe constructor compiles.
    let _ = init_zeroed::<Plain>();
    let _ = init_zeroed::<Wrapped>();
    let _ = init_zeroed::<UnitFields>();
}
