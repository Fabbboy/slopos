//! Tests for the prefix-length-bucketed routing table.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::route::{RouteEntry, RouteTable};
use crate::types::{DevIndex, Ipv4Addr};

/// A per-test table rather than the shared `ROUTE_TABLE`.
fn fresh_table() -> RouteTable {
    RouteTable::new()
}

/// Build a connected-subnet route (gateway = UNSPECIFIED).
fn connected_route(prefix: [u8; 4], prefix_len: u8, dev: usize, metric: u32) -> RouteEntry {
    RouteEntry {
        prefix: Ipv4Addr(prefix),
        prefix_len,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev: DevIndex(dev),
        metric,
    }
}

fn gateway_route(
    prefix: [u8; 4],
    prefix_len: u8,
    gateway: [u8; 4],
    dev: usize,
    metric: u32,
) -> RouteEntry {
    RouteEntry {
        prefix: Ipv4Addr(prefix),
        prefix_len,
        gateway: Ipv4Addr(gateway),
        dev: DevIndex(dev),
        metric,
    }
}

pub fn test_route_lookup_connected() -> TestResult {
    let table = fresh_table();

    table.add(connected_route([10, 0, 0, 0], 24, 1, 0));

    let result = table.lookup(Ipv4Addr([10, 0, 0, 42]));
    assert_test!(result.is_some(), "lookup on connected subnet should match");

    let (dev, next_hop) = result.unwrap();
    assert_eq_test!(dev, DevIndex(1), "device should be DevIndex(1)");
    assert_eq_test!(
        next_hop.0,
        [10, 0, 0, 42],
        "next_hop should be dst for connected route"
    );

    pass!()
}

pub fn test_route_lookup_connected_edge_addresses() -> TestResult {
    let table = fresh_table();

    table.add(connected_route([192, 168, 1, 0], 24, 0, 0));

    let r1 = table.lookup(Ipv4Addr([192, 168, 1, 1]));
    assert_test!(r1.is_some(), ".1 should match /24");

    let r254 = table.lookup(Ipv4Addr([192, 168, 1, 254]));
    assert_test!(r254.is_some(), ".254 should match /24");

    let r_out = table.lookup(Ipv4Addr([192, 168, 2, 1]));
    assert_test!(r_out.is_none(), "192.168.2.1 should NOT match /24");

    pass!()
}

pub fn test_route_lookup_default_gateway() -> TestResult {
    let table = fresh_table();

    table.add(gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 1, 100));

    let result = table.lookup(Ipv4Addr([8, 8, 8, 8]));
    assert_test!(result.is_some(), "default route should match any address");

    let (dev, next_hop) = result.unwrap();
    assert_eq_test!(dev, DevIndex(1), "device should be DevIndex(1)");
    assert_eq_test!(
        next_hop.0,
        [10, 0, 0, 1],
        "next_hop should be the gateway (not dst)"
    );

    pass!()
}

pub fn test_route_lookup_connected_preferred_over_default() -> TestResult {
    let table = fresh_table();

    table.add(gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 1, 100));
    table.add(connected_route([10, 0, 0, 0], 24, 1, 0));

    let result = table.lookup(Ipv4Addr([10, 0, 0, 42]));
    assert_test!(result.is_some(), "should match connected route");

    let (dev, next_hop) = result.unwrap();
    assert_eq_test!(dev, DevIndex(1), "device should be DevIndex(1)");
    assert_eq_test!(
        next_hop.0,
        [10, 0, 0, 42],
        "next_hop should be dst (connected), not gateway"
    );

    let result2 = table.lookup(Ipv4Addr([1, 1, 1, 1]));
    assert_test!(result2.is_some(), "should match default route");
    let (_, next_hop2) = result2.unwrap();
    assert_eq_test!(
        next_hop2.0,
        [10, 0, 0, 1],
        "next_hop should be gateway for non-local dst"
    );

    pass!()
}

pub fn test_route_lookup_empty_table() -> TestResult {
    let table = fresh_table();

    let result = table.lookup(Ipv4Addr([10, 0, 0, 1]));
    assert_test!(result.is_none(), "empty table should return None");

    pass!()
}

pub fn test_route_lookup_no_matching_route() -> TestResult {
    let table = fresh_table();

    table.add(connected_route([192, 168, 1, 0], 24, 0, 0));

    let result = table.lookup(Ipv4Addr([10, 0, 0, 1]));
    assert_test!(
        result.is_none(),
        "should return None for address outside only route"
    );

    pass!()
}

pub fn test_route_prefix_length_priority() -> TestResult {
    let table = fresh_table();

    table.add(connected_route([10, 0, 0, 0], 16, 0, 0));
    table.add(connected_route([10, 0, 1, 0], 24, 1, 0));

    let result = table.lookup(Ipv4Addr([10, 0, 1, 50]));
    assert_test!(result.is_some(), "should match the /24 route");

    let (dev, _) = result.unwrap();
    assert_eq_test!(
        dev,
        DevIndex(1),
        "/24 (dev 1) should beat /16 (dev 0) via longest prefix match"
    );

    let result2 = table.lookup(Ipv4Addr([10, 0, 2, 50]));
    assert_test!(result2.is_some(), "should match the /16 route");

    let (dev2, _) = result2.unwrap();
    assert_eq_test!(
        dev2,
        DevIndex(0),
        "10.0.2.50 should match only the /16 on dev 0"
    );

    pass!()
}

pub fn test_route_host_route_beats_subnet() -> TestResult {
    let table = fresh_table();

    table.add(connected_route([10, 0, 0, 0], 24, 0, 0));
    table.add(gateway_route([10, 0, 0, 42], 32, [10, 0, 0, 1], 1, 0));

    let result = table.lookup(Ipv4Addr([10, 0, 0, 42]));
    assert_test!(result.is_some(), "should match the /32 host route");

    let (dev, next_hop) = result.unwrap();
    assert_eq_test!(
        dev,
        DevIndex(1),
        "/32 host route should beat /24 subnet route"
    );
    assert_eq_test!(
        next_hop.0,
        [10, 0, 0, 1],
        "next_hop should be the gateway from /32 route"
    );

    let result2 = table.lookup(Ipv4Addr([10, 0, 0, 43]));
    let (dev2, _) = result2.unwrap();
    assert_eq_test!(
        dev2,
        DevIndex(0),
        "10.0.0.43 should match only /24 on dev 0"
    );

    pass!()
}

pub fn test_route_metric_tiebreak() -> TestResult {
    let table = fresh_table();

    table.add(connected_route([10, 0, 0, 0], 24, 0, 200));
    table.add(connected_route([10, 0, 0, 0], 24, 1, 50));

    let result = table.lookup(Ipv4Addr([10, 0, 0, 42]));
    assert_test!(result.is_some(), "should match a route");

    let (dev, _) = result.unwrap();
    assert_eq_test!(
        dev,
        DevIndex(1),
        "lower metric (50, dev 1) should win over higher metric (200, dev 0)"
    );

    pass!()
}

pub fn test_route_metric_update() -> TestResult {
    let table = fresh_table();

    table.add(connected_route([10, 0, 0, 0], 24, 0, 100));
    table.add(connected_route([10, 0, 0, 0], 24, 1, 50));

    let (dev, _) = table.lookup(Ipv4Addr([10, 0, 0, 1])).unwrap();
    assert_eq_test!(dev, DevIndex(1), "lower metric should win");

    table.add(RouteEntry {
        prefix: Ipv4Addr([10, 0, 0, 0]),
        prefix_len: 24,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev: DevIndex(1),
        metric: 200,
    });

    let (dev2, _) = table.lookup(Ipv4Addr([10, 0, 0, 1])).unwrap();
    assert_eq_test!(
        dev2,
        DevIndex(0),
        "after metric update, dev 0 (100) should beat dev 1 (200)"
    );

    pass!()
}

pub fn test_route_add_and_remove() -> TestResult {
    let table = fresh_table();

    table.add(connected_route([10, 0, 0, 0], 24, 0, 0));
    assert_eq_test!(table.route_count(), 1, "should have 1 route after add");

    let removed = table.remove(Ipv4Addr([10, 0, 0, 0]), 24);
    assert_test!(removed, "remove should return true");
    assert_eq_test!(table.route_count(), 0, "should have 0 routes after remove");

    let result = table.lookup(Ipv4Addr([10, 0, 0, 1]));
    assert_test!(result.is_none(), "lookup should fail after remove");

    pass!()
}

pub fn test_route_remove_device_routes() -> TestResult {
    let table = fresh_table();

    table.add(connected_route([10, 0, 0, 0], 24, 0, 0));
    table.add(gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 0, 100));
    table.add(connected_route([192, 168, 1, 0], 24, 1, 0));
    assert_eq_test!(table.route_count(), 3, "should have 3 routes");

    table.remove_device_routes(DevIndex(0));
    assert_eq_test!(
        table.route_count(),
        1,
        "should have 1 route after removing dev 0 routes"
    );

    let result = table.lookup(Ipv4Addr([192, 168, 1, 50]));
    assert_test!(result.is_some(), "dev 1 route should still exist");
    let (dev, _) = result.unwrap();
    assert_eq_test!(dev, DevIndex(1), "remaining route should be on dev 1");

    pass!()
}

pub fn test_route_entry_matches() -> TestResult {
    let entry = connected_route([10, 0, 0, 0], 24, 0, 0);

    assert_test!(
        entry.matches(Ipv4Addr([10, 0, 0, 1])),
        "10.0.0.1 should match 10.0.0.0/24"
    );
    assert_test!(
        entry.matches(Ipv4Addr([10, 0, 0, 255])),
        "10.0.0.255 should match 10.0.0.0/24"
    );
    assert_test!(
        !entry.matches(Ipv4Addr([10, 0, 1, 0])),
        "10.0.1.0 should NOT match 10.0.0.0/24"
    );

    let default_entry = gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 0, 100);
    assert_test!(
        default_entry.matches(Ipv4Addr([1, 2, 3, 4])),
        "default route should match any address"
    );

    pass!()
}

pub fn test_route_entry_next_hop() -> TestResult {
    let connected = connected_route([10, 0, 0, 0], 24, 0, 0);
    let hop = connected.next_hop(Ipv4Addr([10, 0, 0, 42]));
    assert_eq_test!(
        hop.0,
        [10, 0, 0, 42],
        "connected route next_hop should be dst"
    );

    let gw = gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 0, 100);
    let hop = gw.next_hop(Ipv4Addr([8, 8, 8, 8]));
    assert_eq_test!(
        hop.0,
        [10, 0, 0, 1],
        "gateway route next_hop should be the gateway"
    );

    pass!()
}

slopos_testing::stest!(name = test_route_lookup_connected, suite = route);
slopos_testing::stest!(
    name = test_route_lookup_connected_edge_addresses,
    suite = route
);
slopos_testing::stest!(name = test_route_lookup_default_gateway, suite = route);
slopos_testing::stest!(
    name = test_route_lookup_connected_preferred_over_default,
    suite = route
);
slopos_testing::stest!(name = test_route_lookup_empty_table, suite = route);
slopos_testing::stest!(name = test_route_lookup_no_matching_route, suite = route);
slopos_testing::stest!(name = test_route_prefix_length_priority, suite = route);
slopos_testing::stest!(name = test_route_host_route_beats_subnet, suite = route);
slopos_testing::stest!(name = test_route_metric_tiebreak, suite = route);
slopos_testing::stest!(name = test_route_metric_update, suite = route);
slopos_testing::stest!(name = test_route_add_and_remove, suite = route);
slopos_testing::stest!(name = test_route_remove_device_routes, suite = route);
slopos_testing::stest!(name = test_route_entry_matches, suite = route);
slopos_testing::stest!(name = test_route_entry_next_hop, suite = route);
