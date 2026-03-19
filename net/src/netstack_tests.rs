//! Tests for the per-interface IPv4 configuration and NetStack.
//!
//! Covers:
//! - 3A.T1: `IfaceConfig::broadcast()` returns correct broadcast address
//! - 3A.T2: `IfaceConfig::is_local()` classifies IPs correctly
//! - 3A.T3: `IfaceConfig::prefix_len()` counts leading ones
//! - 3A.T4: `NetStack::configure()` creates new interface entry
//! - 3A.T5: `NetStack::configure()` reconfigures existing interface in place
//! - 3A.T6: `NetStack::iface_for_dev()` returns None for unknown device
//! - 3A.T7: `NetStack::is_our_addr()` matches configured interfaces
//! - 3A.T8: `NetStack::first_ipv4()` returns first up+configured address

extern crate alloc;

use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, pass};

use crate::netstack::{IfaceConfig, NetStack};
use crate::types::{DevIndex, Ipv4Addr};

// =============================================================================
// Helpers
// =============================================================================

/// Create a fresh NetStack for each test (avoids shared state).
fn fresh_netstack() -> NetStack {
    NetStack::new()
}

/// Build a typical /24 IfaceConfig for testing.
fn typical_iface(dev: usize, last_octet: u8) -> IfaceConfig {
    IfaceConfig {
        dev_index: DevIndex(dev),
        ipv4_addr: Ipv4Addr([10, 0, 0, last_octet]),
        netmask: Ipv4Addr([255, 255, 255, 0]),
        gateway: Ipv4Addr([10, 0, 0, 1]),
        dns: [Ipv4Addr([8, 8, 8, 8]), Ipv4Addr([8, 8, 4, 4])],
        up: true,
    }
}

// =============================================================================
// 3A.T1 — IfaceConfig::broadcast()
// =============================================================================

pub fn test_iface_config_broadcast_24() -> TestResult {
    let iface = typical_iface(0, 50);
    let bcast = iface.broadcast();
    assert_eq_test!(
        bcast.0,
        [10, 0, 0, 255],
        "/24 broadcast should be 10.0.0.255"
    );
    pass!()
}

pub fn test_iface_config_broadcast_16() -> TestResult {
    let iface = IfaceConfig {
        dev_index: DevIndex(0),
        ipv4_addr: Ipv4Addr([172, 16, 5, 10]),
        netmask: Ipv4Addr([255, 255, 0, 0]),
        gateway: Ipv4Addr([172, 16, 0, 1]),
        dns: [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED],
        up: true,
    };
    let bcast = iface.broadcast();
    assert_eq_test!(
        bcast.0,
        [172, 16, 255, 255],
        "/16 broadcast should be 172.16.255.255"
    );
    pass!()
}

pub fn test_iface_config_broadcast_32() -> TestResult {
    let iface = IfaceConfig {
        dev_index: DevIndex(0),
        ipv4_addr: Ipv4Addr([192, 168, 1, 1]),
        netmask: Ipv4Addr([255, 255, 255, 255]),
        gateway: Ipv4Addr::UNSPECIFIED,
        dns: [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED],
        up: true,
    };
    let bcast = iface.broadcast();
    // /32: broadcast == address itself
    assert_eq_test!(
        bcast.0,
        [192, 168, 1, 1],
        "/32 broadcast should equal the address"
    );
    pass!()
}

// =============================================================================
// 3A.T2 — IfaceConfig::is_local()
// =============================================================================

pub fn test_iface_config_is_local_same_subnet() -> TestResult {
    let iface = typical_iface(0, 50);
    assert_test!(
        iface.is_local(Ipv4Addr([10, 0, 0, 1])),
        "10.0.0.1 should be local on 10.0.0.50/24"
    );
    assert_test!(
        iface.is_local(Ipv4Addr([10, 0, 0, 254])),
        "10.0.0.254 should be local on 10.0.0.50/24"
    );
    assert_test!(
        iface.is_local(Ipv4Addr([10, 0, 0, 50])),
        "own IP should be local"
    );
    pass!()
}

pub fn test_iface_config_is_local_different_subnet() -> TestResult {
    let iface = typical_iface(0, 50);
    assert_test!(
        !iface.is_local(Ipv4Addr([10, 0, 1, 1])),
        "10.0.1.1 should NOT be local on 10.0.0.50/24"
    );
    assert_test!(
        !iface.is_local(Ipv4Addr([192, 168, 1, 1])),
        "192.168.1.1 should NOT be local on 10.0.0.50/24"
    );
    pass!()
}

// =============================================================================
// 3A.T3 — IfaceConfig::prefix_len()
// =============================================================================

pub fn test_iface_config_prefix_len() -> TestResult {
    // /24
    let iface24 = typical_iface(0, 1);
    assert_eq_test!(iface24.prefix_len(), 24, "/24 prefix_len should be 24");

    // /16
    let iface16 = IfaceConfig {
        netmask: Ipv4Addr([255, 255, 0, 0]),
        ..typical_iface(0, 1)
    };
    assert_eq_test!(iface16.prefix_len(), 16, "/16 prefix_len should be 16");

    // /8
    let iface8 = IfaceConfig {
        netmask: Ipv4Addr([255, 0, 0, 0]),
        ..typical_iface(0, 1)
    };
    assert_eq_test!(iface8.prefix_len(), 8, "/8 prefix_len should be 8");

    // /32
    let iface32 = IfaceConfig {
        netmask: Ipv4Addr([255, 255, 255, 255]),
        ..typical_iface(0, 1)
    };
    assert_eq_test!(iface32.prefix_len(), 32, "/32 prefix_len should be 32");

    // /0 (all zeros mask)
    let iface0 = IfaceConfig {
        netmask: Ipv4Addr([0, 0, 0, 0]),
        ..typical_iface(0, 1)
    };
    assert_eq_test!(iface0.prefix_len(), 0, "/0 prefix_len should be 0");

    pass!()
}

// =============================================================================
// 3A.T4 — NetStack::configure() creates new entry
// =============================================================================

pub fn test_netstack_configure_new_iface() -> TestResult {
    let ns = fresh_netstack();

    assert_eq_test!(ns.iface_count(), 0, "empty netstack has 0 ifaces");

    ns.configure(
        DevIndex(0),
        Ipv4Addr([10, 0, 0, 50]),
        Ipv4Addr([255, 255, 255, 0]),
        Ipv4Addr([10, 0, 0, 1]),
        [Ipv4Addr([8, 8, 8, 8]), Ipv4Addr([8, 8, 4, 4])],
    );

    assert_eq_test!(ns.iface_count(), 1, "should have 1 iface after configure");

    let ip = ns.our_ip(DevIndex(0));
    assert_test!(ip.is_some(), "our_ip should return Some after configure");
    assert_eq_test!(
        ip.unwrap().0,
        [10, 0, 0, 50],
        "our_ip should return configured address"
    );

    pass!()
}

// =============================================================================
// 3A.T5 — NetStack::configure() reconfigures existing interface
// =============================================================================

pub fn test_netstack_reconfigure_iface() -> TestResult {
    let ns = fresh_netstack();

    // Initial config
    ns.configure(
        DevIndex(0),
        Ipv4Addr([10, 0, 0, 50]),
        Ipv4Addr([255, 255, 255, 0]),
        Ipv4Addr([10, 0, 0, 1]),
        [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED],
    );

    // Reconfigure same device with new address
    ns.configure(
        DevIndex(0),
        Ipv4Addr([192, 168, 1, 100]),
        Ipv4Addr([255, 255, 255, 0]),
        Ipv4Addr([192, 168, 1, 1]),
        [Ipv4Addr([1, 1, 1, 1]), Ipv4Addr::UNSPECIFIED],
    );

    assert_eq_test!(
        ns.iface_count(),
        1,
        "reconfigure should NOT add a second entry"
    );

    let ip = ns.our_ip(DevIndex(0));
    assert_eq_test!(
        ip.unwrap().0,
        [192, 168, 1, 100],
        "our_ip should return updated address"
    );

    // Verify full config via iface_for_dev
    let cfg = ns.iface_for_dev(DevIndex(0)).unwrap();
    assert_eq_test!(cfg.gateway.0, [192, 168, 1, 1], "gateway should be updated");
    assert_eq_test!(cfg.dns[0].0, [1, 1, 1, 1], "dns[0] should be updated");

    pass!()
}

// =============================================================================
// 3A.T6 — NetStack::iface_for_dev() returns None for unknown device
// =============================================================================

pub fn test_netstack_lookup_unknown_device() -> TestResult {
    let ns = fresh_netstack();

    // Configure dev 0 only
    ns.configure(
        DevIndex(0),
        Ipv4Addr([10, 0, 0, 50]),
        Ipv4Addr([255, 255, 255, 0]),
        Ipv4Addr([10, 0, 0, 1]),
        [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED],
    );

    assert_test!(
        ns.iface_for_dev(DevIndex(1)).is_none(),
        "iface_for_dev(1) should be None"
    );
    assert_test!(ns.our_ip(DevIndex(1)).is_none(), "our_ip(1) should be None");
    assert_test!(
        ns.iface_for_dev(DevIndex(99)).is_none(),
        "iface_for_dev(99) should be None"
    );

    pass!()
}

// =============================================================================
// 3A.T7 — NetStack::is_our_addr() checks all interfaces
// =============================================================================

pub fn test_netstack_is_our_addr() -> TestResult {
    let ns = fresh_netstack();

    // Configure two interfaces
    ns.configure(
        DevIndex(0),
        Ipv4Addr([10, 0, 0, 50]),
        Ipv4Addr([255, 255, 255, 0]),
        Ipv4Addr([10, 0, 0, 1]),
        [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED],
    );
    ns.configure(
        DevIndex(1),
        Ipv4Addr([192, 168, 1, 100]),
        Ipv4Addr([255, 255, 255, 0]),
        Ipv4Addr([192, 168, 1, 1]),
        [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED],
    );

    assert_test!(
        ns.is_our_addr(Ipv4Addr([10, 0, 0, 50])),
        "10.0.0.50 is our address (dev0)"
    );
    assert_test!(
        ns.is_our_addr(Ipv4Addr([192, 168, 1, 100])),
        "192.168.1.100 is our address (dev1)"
    );
    assert_test!(
        !ns.is_our_addr(Ipv4Addr([10, 0, 0, 1])),
        "10.0.0.1 (gateway) is NOT our address"
    );
    assert_test!(
        !ns.is_our_addr(Ipv4Addr([172, 16, 0, 1])),
        "172.16.0.1 is NOT our address"
    );

    pass!()
}

// =============================================================================
// 3A.T8 — NetStack::first_ipv4() returns first up+configured address
// =============================================================================

pub fn test_netstack_first_ipv4_empty() -> TestResult {
    let ns = fresh_netstack();
    assert_test!(
        ns.first_ipv4().is_none(),
        "first_ipv4 on empty stack should be None"
    );
    pass!()
}

pub fn test_netstack_first_ipv4_with_ifaces() -> TestResult {
    let ns = fresh_netstack();

    ns.configure(
        DevIndex(0),
        Ipv4Addr([10, 0, 0, 50]),
        Ipv4Addr([255, 255, 255, 0]),
        Ipv4Addr([10, 0, 0, 1]),
        [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED],
    );
    ns.configure(
        DevIndex(1),
        Ipv4Addr([192, 168, 1, 100]),
        Ipv4Addr([255, 255, 255, 0]),
        Ipv4Addr([192, 168, 1, 1]),
        [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED],
    );

    let first = ns.first_ipv4();
    assert_test!(first.is_some(), "first_ipv4 should return Some");
    assert_eq_test!(
        first.unwrap().0,
        [10, 0, 0, 50],
        "first_ipv4 should return dev0's address"
    );

    pass!()
}

// =============================================================================
// Additional edge cases
// =============================================================================

pub fn test_netstack_multiple_devices() -> TestResult {
    let ns = fresh_netstack();

    // Configure 3 devices
    for i in 0..3usize {
        ns.configure(
            DevIndex(i),
            Ipv4Addr([10, 0, i as u8, 50]),
            Ipv4Addr([255, 255, 255, 0]),
            Ipv4Addr([10, 0, i as u8, 1]),
            [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED],
        );
    }

    assert_eq_test!(ns.iface_count(), 3, "should have 3 interfaces");

    // Each device has its own IP
    for i in 0..3usize {
        let ip = ns.our_ip(DevIndex(i));
        assert_test!(ip.is_some(), "our_ip should return Some for each device");
        assert_eq_test!(
            ip.unwrap().0[2],
            i as u8,
            "third octet should match device index"
        );
    }

    pass!()
}

pub fn test_netstack_first_iface() -> TestResult {
    let ns = fresh_netstack();

    assert_test!(
        ns.first_iface().is_none(),
        "first_iface on empty stack should be None"
    );

    ns.configure(
        DevIndex(0),
        Ipv4Addr([10, 0, 0, 50]),
        Ipv4Addr([255, 255, 255, 0]),
        Ipv4Addr([10, 0, 0, 1]),
        [Ipv4Addr([8, 8, 8, 8]), Ipv4Addr([8, 8, 4, 4])],
    );

    let iface = ns.first_iface();
    assert_test!(iface.is_some(), "first_iface should return Some");
    let cfg = iface.unwrap();
    assert_eq_test!(cfg.ipv4_addr.0, [10, 0, 0, 50], "ip matches");
    assert_eq_test!(cfg.netmask.0, [255, 255, 255, 0], "netmask matches");
    assert_eq_test!(cfg.gateway.0, [10, 0, 0, 1], "gateway matches");
    assert_eq_test!(cfg.dns[0].0, [8, 8, 8, 8], "dns[0] matches");
    assert_eq_test!(cfg.dns[1].0, [8, 8, 4, 4], "dns[1] matches");
    assert_test!(cfg.up, "interface should be up");

    pass!()
}

// =============================================================================
// Test suite registration
// =============================================================================

slopos_lib::define_test_suite!(
    netstack,
    [
        // 3A.T1 — broadcast
        test_iface_config_broadcast_24,
        test_iface_config_broadcast_16,
        test_iface_config_broadcast_32,
        // 3A.T2 — is_local
        test_iface_config_is_local_same_subnet,
        test_iface_config_is_local_different_subnet,
        // 3A.T3 — prefix_len
        test_iface_config_prefix_len,
        // 3A.T4 — configure new
        test_netstack_configure_new_iface,
        // 3A.T5 — reconfigure
        test_netstack_reconfigure_iface,
        // 3A.T6 — unknown device
        test_netstack_lookup_unknown_device,
        // 3A.T7 — is_our_addr
        test_netstack_is_our_addr,
        // 3A.T8 — first_ipv4
        test_netstack_first_ipv4_empty,
        test_netstack_first_ipv4_with_ifaces,
        // Edge cases
        test_netstack_multiple_devices,
        test_netstack_first_iface,
    ]
);
