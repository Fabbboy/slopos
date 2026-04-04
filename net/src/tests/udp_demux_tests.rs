use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::types::{Ipv4Addr, NetError, Port};
use crate::udp::UDP_DEMUX;

fn reset() {
    UDP_DEMUX.lock().clear();
}

pub fn test_udp_demux_register_lookup() -> TestResult {
    reset();

    let mut demux = UDP_DEMUX.lock();
    let rc = demux.register(Ipv4Addr([10, 0, 0, 1]), Port(5000), 3, false);
    assert_test!(rc.is_ok(), "register succeeds");

    assert_eq_test!(
        demux.lookup(Ipv4Addr([10, 0, 0, 1]), Port(5000)),
        Some(3),
        "lookup returns socket index"
    );
    assert_eq_test!(
        demux.lookup(Ipv4Addr([10, 0, 0, 1]), Port(5001)),
        None,
        "lookup misses wrong port"
    );

    pass!()
}

pub fn test_udp_demux_inaddr_any() -> TestResult {
    reset();

    let mut demux = UDP_DEMUX.lock();
    let rc = demux.register(Ipv4Addr::UNSPECIFIED, Port(6000), 7, false);
    assert_test!(rc.is_ok(), "wildcard register succeeds");

    assert_eq_test!(
        demux.lookup(Ipv4Addr([10, 1, 2, 3]), Port(6000)),
        Some(7),
        "wildcard match works"
    );
    assert_eq_test!(
        demux.lookup(Ipv4Addr([192, 168, 4, 9]), Port(6000)),
        Some(7),
        "wildcard matches any destination ip"
    );

    pass!()
}

pub fn test_udp_demux_exact_over_wildcard() -> TestResult {
    reset();

    let mut demux = UDP_DEMUX.lock();
    let rc_a = demux.register(Ipv4Addr([10, 0, 0, 1]), Port(7000), 11, false);
    let rc_b = demux.register(Ipv4Addr::UNSPECIFIED, Port(7000), 12, false);
    assert_test!(rc_a.is_ok() && rc_b.is_ok(), "both registrations succeed");

    assert_eq_test!(
        demux.lookup(Ipv4Addr([10, 0, 0, 1]), Port(7000)),
        Some(11),
        "exact ip wins over wildcard"
    );
    assert_eq_test!(
        demux.lookup(Ipv4Addr([10, 0, 0, 2]), Port(7000)),
        Some(12),
        "wildcard handles non-exact destination"
    );

    pass!()
}

pub fn test_udp_demux_reuse_addr() -> TestResult {
    reset();

    let mut demux = UDP_DEMUX.lock();
    let first = demux.register(Ipv4Addr([10, 0, 0, 1]), Port(8000), 20, false);
    assert_test!(first.is_ok(), "initial register succeeds");

    let second = demux.register(Ipv4Addr([10, 0, 0, 1]), Port(8000), 21, false);
    assert_eq_test!(
        second,
        Err(NetError::AddressInUse),
        "second register without reuse fails"
    );

    let third = demux.register(Ipv4Addr([10, 0, 0, 1]), Port(8000), 21, true);
    assert_test!(third.is_ok(), "second register with reuse succeeds");

    pass!()
}

pub fn test_udp_demux_unregister() -> TestResult {
    reset();

    let mut demux = UDP_DEMUX.lock();
    let rc = demux.register(Ipv4Addr([10, 0, 0, 1]), Port(9000), 30, false);
    assert_test!(rc.is_ok(), "register succeeds");

    demux.unregister(Ipv4Addr([10, 0, 0, 1]), Port(9000), 30);
    assert_eq_test!(
        demux.lookup(Ipv4Addr([10, 0, 0, 1]), Port(9000)),
        None,
        "lookup is empty after unregister"
    );

    pass!()
}

pub fn test_udp_demux_clear() -> TestResult {
    reset();

    let mut demux = UDP_DEMUX.lock();
    let _ = demux.register(Ipv4Addr([10, 0, 0, 1]), Port(9100), 31, false);
    let _ = demux.register(Ipv4Addr([10, 0, 0, 2]), Port(9101), 32, false);
    let _ = demux.register(Ipv4Addr::UNSPECIFIED, Port(9102), 33, false);

    demux.clear();

    assert_eq_test!(
        demux.lookup(Ipv4Addr([10, 0, 0, 1]), Port(9100)),
        None,
        "first entry removed"
    );
    assert_eq_test!(
        demux.lookup(Ipv4Addr([10, 0, 0, 2]), Port(9101)),
        None,
        "second entry removed"
    );
    assert_eq_test!(
        demux.lookup(Ipv4Addr([8, 8, 8, 8]), Port(9102)),
        None,
        "wildcard entry removed"
    );

    pass!()
}

pub fn test_udp_demux_overflow() -> TestResult {
    reset();

    // Fill a single hash bucket by registering entries on the same port
    // with different IP addresses (all hash to the same bucket).
    // Each bucket holds 8 entries; the 9th should overflow.
    let mut demux = UDP_DEMUX.lock();
    for idx in 0..8u32 {
        let ip = Ipv4Addr([10, 0, idx as u8, 1]);
        let rc = demux.register(ip, Port(5555), idx, false);
        if rc.is_err() {
            return fail!("register failed before bucket became full");
        }
    }

    let overflow = demux.register(Ipv4Addr([10, 0, 8, 1]), Port(5555), 999, false);
    assert_eq_test!(
        overflow,
        Err(NetError::NoBufferSpace),
        "register fails when bucket is full"
    );

    pass!()
}

slopos_testing::define_test_suite!(
    udp_demux,
    [
        test_udp_demux_register_lookup,
        test_udp_demux_inaddr_any,
        test_udp_demux_exact_over_wildcard,
        test_udp_demux_reuse_addr,
        test_udp_demux_unregister,
        test_udp_demux_clear,
        test_udp_demux_overflow,
    ]
);
