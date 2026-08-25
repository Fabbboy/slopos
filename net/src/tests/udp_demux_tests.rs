use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tests::net_scope::NetTestScope;
use crate::tests::tcp_common::{LOCAL_IP, REMOTE_IP};
use crate::types::{Ipv4Addr, NetError, Port};
use crate::udp::UDP_DEMUX;

fn reset() {
    UDP_DEMUX.lock().clear();
}

/// Clears the demux table however the test leaves, not only on the way in.
///
/// These register wildcards on ports they do not own, at socket indices that do
/// not exist. Left behind, the next inbound datagram on that port demuxes onto a
/// stranger — and unlike a PCB, nothing ages a demux entry out.
struct ClearOnExit;

impl Drop for ClearOnExit {
    fn drop(&mut self) {
        UDP_DEMUX.lock().clear();
    }
}

pub fn test_udp_demux_register_lookup() -> TestResult {
    reset();
    let _clear = ClearOnExit;

    let mut demux = UDP_DEMUX.lock();
    let rc = demux.register(Ipv4Addr(LOCAL_IP), Port(5000), 3, false);
    assert_test!(rc.is_ok(), "register succeeds");

    assert_eq_test!(
        demux.lookup(Ipv4Addr(LOCAL_IP), Port(5000)),
        Some(3),
        "lookup returns socket index"
    );
    assert_eq_test!(
        demux.lookup(Ipv4Addr(LOCAL_IP), Port(5001)),
        None,
        "lookup misses wrong port"
    );

    pass!()
}

pub fn test_udp_demux_inaddr_any() -> TestResult {
    reset();
    let _clear = ClearOnExit;

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
    let _clear = ClearOnExit;

    let mut demux = UDP_DEMUX.lock();
    let rc_a = demux.register(Ipv4Addr(LOCAL_IP), Port(7000), 11, false);
    let rc_b = demux.register(Ipv4Addr::UNSPECIFIED, Port(7000), 12, false);
    assert_test!(rc_a.is_ok() && rc_b.is_ok(), "both registrations succeed");

    assert_eq_test!(
        demux.lookup(Ipv4Addr(LOCAL_IP), Port(7000)),
        Some(11),
        "exact ip wins over wildcard"
    );
    assert_eq_test!(
        demux.lookup(Ipv4Addr(REMOTE_IP), Port(7000)),
        Some(12),
        "wildcard handles non-exact destination"
    );

    pass!()
}

pub fn test_udp_demux_reuse_addr() -> TestResult {
    reset();
    let _clear = ClearOnExit;

    let mut demux = UDP_DEMUX.lock();
    let first = demux.register(Ipv4Addr(LOCAL_IP), Port(8000), 20, false);
    assert_test!(first.is_ok(), "initial register succeeds");

    let second = demux.register(Ipv4Addr(LOCAL_IP), Port(8000), 21, false);
    assert_eq_test!(
        second,
        Err(NetError::AddressInUse),
        "second register without reuse fails"
    );

    let third = demux.register(Ipv4Addr(LOCAL_IP), Port(8000), 21, true);
    assert_test!(third.is_ok(), "second register with reuse succeeds");

    pass!()
}

pub fn test_udp_demux_unregister() -> TestResult {
    reset();
    let _clear = ClearOnExit;

    let mut demux = UDP_DEMUX.lock();
    let rc = demux.register(Ipv4Addr(LOCAL_IP), Port(9000), 30, false);
    assert_test!(rc.is_ok(), "register succeeds");

    demux.unregister(Ipv4Addr(LOCAL_IP), Port(9000), 30);
    assert_eq_test!(
        demux.lookup(Ipv4Addr(LOCAL_IP), Port(9000)),
        None,
        "lookup is empty after unregister"
    );

    pass!()
}

pub fn test_udp_demux_clear() -> TestResult {
    reset();
    let _clear = ClearOnExit;

    let mut demux = UDP_DEMUX.lock();
    let _ = demux.register(Ipv4Addr(LOCAL_IP), Port(9100), 31, false);
    let _ = demux.register(Ipv4Addr(REMOTE_IP), Port(9101), 32, false);
    let _ = demux.register(Ipv4Addr::UNSPECIFIED, Port(9102), 33, false);

    demux.clear();

    assert_eq_test!(
        demux.lookup(Ipv4Addr(LOCAL_IP), Port(9100)),
        None,
        "first entry removed"
    );
    assert_eq_test!(
        demux.lookup(Ipv4Addr(REMOTE_IP), Port(9101)),
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
    // The bucket fill is a count on a table the live stack also registers
    // into; the scope is what makes eight the whole population.
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    reset();

    // A shared port hashes every registration into one bucket, which holds 8 entries.
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

slopos_testing::stest!(name = test_udp_demux_register_lookup, suite = udp_demux);
slopos_testing::stest!(name = test_udp_demux_inaddr_any, suite = udp_demux);
slopos_testing::stest!(name = test_udp_demux_exact_over_wildcard, suite = udp_demux);
slopos_testing::stest!(name = test_udp_demux_reuse_addr, suite = udp_demux);
slopos_testing::stest!(name = test_udp_demux_unregister, suite = udp_demux);
slopos_testing::stest!(name = test_udp_demux_clear, suite = udp_demux);
slopos_testing::stest!(name = test_udp_demux_overflow, suite = udp_demux);
