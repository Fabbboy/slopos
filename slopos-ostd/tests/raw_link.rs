//! Host-side tests for `slopos_ostd::sync::raw_link`.

use core::ptr::NonNull;

use slopos_ostd::sync::raw_link::{ByteChain, RawLink};

#[test]
fn raw_link_null_round_trip() {
    let link: RawLink<u32> = RawLink::null();
    assert!(link.is_null());
    assert!(link.load().is_none());
}

#[test]
fn raw_link_store_and_load() {
    let mut value = 42u32;
    let link = RawLink::null();
    link.store(NonNull::new(&mut value as *mut u32));
    assert!(!link.is_null());
    let got = link.load().unwrap();
    // SAFETY: `value` is alive on the stack frame.
    assert_eq!(unsafe { *got.as_ptr() }, 42);
}

#[test]
fn raw_link_with_mut_runs_closure() {
    let mut value = 7u32;
    let link = RawLink::new(&mut value as *mut u32);
    let r = link.with_mut(|v| {
        *v += 1;
        *v
    });
    assert_eq!(r, Some(8));
    assert_eq!(value, 8);
}

#[test]
fn raw_link_with_mut_returns_none_when_null() {
    let link: RawLink<u32> = RawLink::null();
    let r = link.with_mut(|v| *v);
    assert_eq!(r, None);
}

#[test]
fn raw_link_with_mut_at_explicit_pointer() {
    let mut value = 100u32;
    let p = NonNull::new(&mut value as *mut u32);
    let r = RawLink::with_mut_at(p, |v| {
        *v *= 2;
        *v
    });
    assert_eq!(r, Some(200));
    assert_eq!(value, 200);
}

#[test]
fn raw_link_clear_resets_to_null() {
    let mut value = 5u32;
    let link = RawLink::new(&mut value as *mut u32);
    link.clear();
    assert!(link.is_null());
}

#[test]
fn byte_chain_empty_pop_returns_none() {
    let chain = ByteChain::new();
    assert!(chain.is_empty());
    assert!(chain.pop_front().is_none());
}

#[test]
fn byte_chain_lifo_round_trip() {
    #[repr(align(16))]
    struct Slab([u8; 16]);
    let mut s0 = Slab([0u8; 16]);
    let mut s1 = Slab([0u8; 16]);
    let mut s2 = Slab([0u8; 16]);

    let chain = ByteChain::new();
    chain.push_front(NonNull::new(s0.0.as_mut_ptr()).unwrap());
    chain.push_front(NonNull::new(s1.0.as_mut_ptr()).unwrap());
    chain.push_front(NonNull::new(s2.0.as_mut_ptr()).unwrap());

    let p2 = chain.pop_front().unwrap();
    let p1 = chain.pop_front().unwrap();
    let p0 = chain.pop_front().unwrap();
    assert_eq!(p2.as_ptr(), s2.0.as_mut_ptr());
    assert_eq!(p1.as_ptr(), s1.0.as_mut_ptr());
    assert_eq!(p0.as_ptr(), s0.0.as_mut_ptr());
    assert!(chain.pop_front().is_none());
}

#[test]
fn byte_chain_read_write_next() {
    #[repr(align(16))]
    struct Slab([u8; 16]);
    let mut s0 = Slab([0u8; 16]);
    let mut s1 = Slab([0u8; 16]);

    let p0 = NonNull::new(s0.0.as_mut_ptr()).unwrap();
    let p1 = NonNull::new(s1.0.as_mut_ptr()).unwrap();
    ByteChain::write_next(p0, Some(p1));
    assert_eq!(ByteChain::read_next(p0), Some(p1));
    ByteChain::write_next(p0, None);
    assert!(ByteChain::read_next(p0).is_none());
}

#[test]
fn byte_chain_send_compiles() {
    fn must_be_send<T: Send>() {}
    must_be_send::<ByteChain>();
    must_be_send::<RawLink<u32>>();
}
