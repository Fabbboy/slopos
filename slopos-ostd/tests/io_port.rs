//! Host-side integration tests for `IoPort` / `IoPortRegistry`.
//!
//! Port-I/O asm cannot run host-side (would fault outside ring 0), so
//! these tests cover only the registry gate and `IoPort`'s
//! address/offset arithmetic.

use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_ostd::io::port::{
    self, IoPort, IoPortError, IoPortRegistry, PortRange, register_io_port_registry,
};

const COM1_RANGE: PortRange = PortRange {
    start: 0x3F8,
    end: 0x400,
};
const PS2_RANGE: PortRange = PortRange {
    start: 0x60,
    end: 0x65,
};

static SETUP: OnceLock<Mutex<()>> = OnceLock::new();

fn ranges_static() -> &'static [PortRange] {
    Box::leak(Box::new([COM1_RANGE, PS2_RANGE]))
}

fn setup() -> MutexGuard<'static, ()> {
    let m = SETUP.get_or_init(|| {
        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            register_io_port_registry(t, ranges_static());
        });
        Mutex::new(())
    });
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn reserve_succeeds_for_registered_port() {
    let _g = setup();
    let p: IoPort<u8> = IoPortRegistry::reserve(0x3F8).expect("reserve");
    assert_eq!(p.address(), 0x3F8);
    let p: IoPort<u8> = IoPortRegistry::reserve(0x60).expect("reserve");
    assert_eq!(p.address(), 0x60);
}

#[test]
fn reserve_rejects_unregistered_port() {
    let _g = setup();
    let r: Result<IoPort<u8>, IoPortError> = IoPortRegistry::reserve(0x20);
    assert_eq!(r.unwrap_err(), IoPortError::NotReserved);
    let r: Result<IoPort<u8>, IoPortError> = IoPortRegistry::reserve(0xA0);
    assert_eq!(r.unwrap_err(), IoPortError::NotReserved);
    let r: Result<IoPort<u8>, IoPortError> = IoPortRegistry::reserve(0x400);
    assert_eq!(r.unwrap_err(), IoPortError::NotReserved);
}

#[test]
fn reserve_rejects_overrun_at_range_end() {
    let _g = setup();
    // 0x3FF + size_of::<u16>() = 0x401 — past 0x400 end.
    let r: Result<IoPort<u16>, IoPortError> = IoPortRegistry::reserve(0x3FF);
    assert_eq!(r.unwrap_err(), IoPortError::NotReserved);
}

#[test]
fn reserve_rejects_when_uninitialised() {
    let _g = setup();
    port::reset_for_test();
    let r: Result<IoPort<u8>, IoPortError> = IoPortRegistry::reserve(0x3F8);
    assert_eq!(r.unwrap_err(), IoPortError::Uninitialised);
    // Re-install for subsequent tests.
    slopos_ostd::sync::run_bsp_init_for_test(|t| {
        register_io_port_registry(t, ranges_static());
    });
}

#[test]
fn offset_advances_address() {
    let _g = setup();
    let p: IoPort<u8> = IoPortRegistry::reserve(0x3F8).expect("reserve");
    let q = p.offset(5);
    assert_eq!(q.address(), 0x3FD);
}

#[test]
fn u16_reservation_in_two_byte_range() {
    let _g = setup();
    let p: Result<IoPort<u16>, IoPortError> = IoPortRegistry::reserve(0x3F8);
    assert!(p.is_ok());
}
