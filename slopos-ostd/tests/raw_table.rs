//! Host-side tests for `slopos_ostd::sync::raw_table`.

use slopos_ostd::sync::raw_table::RawTable;

fn install_static_buf(table: &RawTable<u32>, n: usize) {
    let buf: Box<[u32]> = (0..n as u32).collect::<Vec<_>>().into_boxed_slice();
    let leaked: &'static mut [u32] = Box::leak(buf);
    table.install(leaked);
}

#[test]
fn empty_table_is_uninstalled() {
    let t: RawTable<u32> = RawTable::empty();
    assert!(!t.is_installed());
    assert_eq!(t.len(), 0);
    assert!(t.is_empty());
    assert!(t.get(0).is_none());
    assert!(t.get_mut(0).is_none());
}

#[test]
fn install_then_len() {
    let t: RawTable<u32> = RawTable::empty();
    install_static_buf(&t, 4);
    assert!(t.is_installed());
    assert_eq!(t.len(), 4);
}

#[test]
fn get_returns_initial_values() {
    let t: RawTable<u32> = RawTable::empty();
    install_static_buf(&t, 8);
    for i in 0..8 {
        assert_eq!(t.get(i).copied(), Some(i as u32));
    }
    assert!(t.get(8).is_none());
}

#[test]
fn get_mut_round_trip() {
    let t: RawTable<u32> = RawTable::empty();
    install_static_buf(&t, 4);
    *t.get_mut(2).unwrap() = 99;
    assert_eq!(t.get(2).copied(), Some(99));
}

#[test]
fn with_mut_returns_closure_value() {
    let t: RawTable<u32> = RawTable::empty();
    install_static_buf(&t, 4);
    let r = t.with_mut(1, |v| {
        *v = 42;
        *v
    });
    assert_eq!(r, Some(42));
    assert_eq!(t.get(1).copied(), Some(42));
}

#[test]
fn with_mut_out_of_range() {
    let t: RawTable<u32> = RawTable::empty();
    install_static_buf(&t, 2);
    let r = t.with_mut(10, |v| *v = 1);
    assert!(r.is_none());
}

#[test]
fn double_install_panics() {
    let t: RawTable<u32> = RawTable::empty();
    install_static_buf(&t, 1);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        install_static_buf(&t, 2);
    }));
    assert!(result.is_err());
}

#[test]
fn send_sync_compile() {
    fn must_be_send_sync<T: Send + Sync>() {}
    must_be_send_sync::<RawTable<u32>>();
}
