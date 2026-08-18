//! Host-side tests for `slopos_ostd::dev::FromRawPtr`.

use slopos_ostd::dev::FromRawPtr;

#[derive(Debug, PartialEq, Eq)]
struct Probe {
    counter: u32,
    label: u32,
}

#[test]
fn from_ptr_returns_some_for_valid_pointer() {
    let p = Probe {
        counter: 42,
        label: 0xCAFE,
    };
    let r = Probe::from_ptr(&p as *const Probe).expect("non-null pointer");
    assert_eq!(r.counter, 42);
    assert_eq!(r.label, 0xCAFE);
}

#[test]
fn from_ptr_returns_none_for_null() {
    let null: *const Probe = core::ptr::null();
    assert!(Probe::from_ptr(null).is_none());
}

#[test]
fn from_ptr_works_for_heap_box() {
    let boxed: Box<u64> = Box::new(0xDEAD_BEEF_DEAD_BEEF);
    let raw = Box::into_raw(boxed);
    let r = u64::from_ptr(raw).expect("box pointer is non-null");
    assert_eq!(*r, 0xDEAD_BEEF_DEAD_BEEF);
    // SAFETY: `raw` came from `Box::into_raw` and no other live
    // borrow of *r remains (the &u64 above is dropped).
    let _ = unsafe { Box::from_raw(raw) };
}
