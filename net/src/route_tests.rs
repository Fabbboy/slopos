//! Tests for the prefix-length-bucketed routing table.
//!
//! Covers:
//! - 3.T1: `RouteTable::lookup` with connected route returns correct DevIndex
//! - 3.T2: `RouteTable::lookup` with default route returns gateway
//! - 3.T3: `RouteTable::lookup` with no routes returns `None`
//! - 3.T4: Prefix-length bucketing: /24 beats /16 for matching address
//! - 3.T5: Metric tie-breaking: lower metric wins within same prefix length

extern crate alloc;

use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, pass};

use crate::route::{RouteEntry, RouteTable};
use crate::types::{DevIndex, Ipv4Addr};

// =============================================================================
// Helpers
// =============================================================================

/// Create a fresh RouteTable for each test (avoids shared state from ROUTE_TABLE).
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

/// Build a gateway route.
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

// =============================================================================
// 3.T1 — RouteTable::lookup with connected route returns correct DevIndex
// =============================================================================

pub fn test_route_lookup_connected() -> TestResult {
    let table = fresh_table();

    // Add connected route: 10.0.0.0/24 on dev 1.
    table.add(connected_route([10, 0, 0, 0], 24, 1, 0));

    // Lookup an address on the subnet.
    let result = table.lookup(Ipv4Addr([10, 0, 0, 42]));
    assert_test!(result.is_some(), "lookup on connected subnet should match");

    let (dev, next_hop) = result.unwrap();
    assert_eq_test!(dev, DevIndex(1), "device should be DevIndex(1)");
    // Connected route: next_hop = dst itself (gateway is UNSPECIFIED).
    assert_eq_test!(
        next_hop.0,
        [10, 0, 0, 42],
        "next_hop should be dst for connected route"
    );

    pass!()
}

pub fn test_route_lookup_connected_edge_addresses() -> TestResult {
    let table = fresh_table();

    // 192.168.1.0/24 on dev 0.
    table.add(connected_route([192, 168, 1, 0], 24, 0, 0));

    // First host address.
    let r1 = table.lookup(Ipv4Addr([192, 168, 1, 1]));
    assert_test!(r1.is_some(), ".1 should match /24");

    // Last host address.
    let r254 = table.lookup(Ipv4Addr([192, 168, 1, 254]));
    assert_test!(r254.is_some(), ".254 should match /24");

    // Address outside the subnet.
    let r_out = table.lookup(Ipv4Addr([192, 168, 2, 1]));
    assert_test!(r_out.is_none(), "192.168.2.1 should NOT match /24");

    pass!()
}

// =============================================================================
// 3.T2 — RouteTable::lookup with default route returns gateway
// =============================================================================

pub fn test_route_lookup_default_gateway() -> TestResult {
    let table = fresh_table();

    // Add a default route: 0.0.0.0/0 via gateway 10.0.0.1 on dev 1.
    table.add(gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 1, 100));

    // Lookup any address — should match the default route.
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

    // Default route: 0.0.0.0/0 via 10.0.0.1 on dev 1.
    table.add(gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 1, 100));
    // Connected route: 10.0.0.0/24 on dev 1.
    table.add(connected_route([10, 0, 0, 0], 24, 1, 0));

    // Address on the connected subnet should use the /24 (more specific).
    let result = table.lookup(Ipv4Addr([10, 0, 0, 42]));
    assert_test!(result.is_some(), "should match connected route");

    let (dev, next_hop) = result.unwrap();
    assert_eq_test!(dev, DevIndex(1), "device should be DevIndex(1)");
    // Connected route: next_hop = dst itself (not the gateway).
    assert_eq_test!(
        next_hop.0,
        [10, 0, 0, 42],
        "next_hop should be dst (connected), not gateway"
    );

    // Address NOT on the connected subnet should fall through to default.
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

// =============================================================================
// 3.T3 — RouteTable::lookup with no routes returns None
// =============================================================================

pub fn test_route_lookup_empty_table() -> TestResult {
    let table = fresh_table();

    let result = table.lookup(Ipv4Addr([10, 0, 0, 1]));
    assert_test!(result.is_none(), "empty table should return None");

    pass!()
}

pub fn test_route_lookup_no_matching_route() -> TestResult {
    let table = fresh_table();

    // Only route: 192.168.1.0/24 — not a default route.
    table.add(connected_route([192, 168, 1, 0], 24, 0, 0));

    // Address outside the only route.
    let result = table.lookup(Ipv4Addr([10, 0, 0, 1]));
    assert_test!(
        result.is_none(),
        "should return None for address outside only route"
    );

    pass!()
}

// =============================================================================
// 3.T4 — Prefix-length bucketing: /24 beats /16 for matching address
// =============================================================================

pub fn test_route_prefix_length_priority() -> TestResult {
    let table = fresh_table();

    // Broader route: 10.0.0.0/16 on dev 0 (metric 0).
    table.add(connected_route([10, 0, 0, 0], 16, 0, 0));
    // Narrower route: 10.0.1.0/24 on dev 1 (metric 0).
    table.add(connected_route([10, 0, 1, 0], 24, 1, 0));

    // Address 10.0.1.50 matches both /16 and /24 — /24 should win (LPM).
    let result = table.lookup(Ipv4Addr([10, 0, 1, 50]));
    assert_test!(result.is_some(), "should match the /24 route");

    let (dev, _) = result.unwrap();
    assert_eq_test!(
        dev,
        DevIndex(1),
        "/24 (dev 1) should beat /16 (dev 0) via longest prefix match"
    );

    // Address 10.0.2.50 matches only /16.
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

    // Subnet route: 10.0.0.0/24 on dev 0.
    table.add(connected_route([10, 0, 0, 0], 24, 0, 0));
    // Host route: 10.0.0.42/32 via gateway 10.0.0.1 on dev 1.
    table.add(gateway_route([10, 0, 0, 42], 32, [10, 0, 0, 1], 1, 0));

    // Address 10.0.0.42 matches both /24 and /32 — /32 should win.
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

    // Address 10.0.0.43 only matches /24.
    let result2 = table.lookup(Ipv4Addr([10, 0, 0, 43]));
    let (dev2, _) = result2.unwrap();
    assert_eq_test!(
        dev2,
        DevIndex(0),
        "10.0.0.43 should match only /24 on dev 0"
    );

    pass!()
}

// =============================================================================
// 3.T5 — Metric tie-breaking: lower metric wins within same prefix length
// =============================================================================

pub fn test_route_metric_tiebreak() -> TestResult {
    let table = fresh_table();

    // Two /24 routes to the same prefix, different metrics, different devices.
    table.add(connected_route([10, 0, 0, 0], 24, 0, 200)); // higher metric
    table.add(connected_route([10, 0, 0, 0], 24, 1, 50)); // lower metric

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

    // Add a route with metric 100.
    table.add(connected_route([10, 0, 0, 0], 24, 0, 100));

    // Add a second route with metric 50 (same prefix, different dev).
    table.add(connected_route([10, 0, 0, 0], 24, 1, 50));

    // Lower metric (dev 1, metric 50) should win.
    let (dev, _) = table.lookup(Ipv4Addr([10, 0, 0, 1])).unwrap();
    assert_eq_test!(dev, DevIndex(1), "lower metric should win");

    // Now update dev 1's metric to 200 (re-add same prefix+dev triggers update).
    table.add(RouteEntry {
        prefix: Ipv4Addr([10, 0, 0, 0]),
        prefix_len: 24,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev: DevIndex(1),
        metric: 200,
    });

    // Now dev 0 (metric 100) should win.
    let (dev2, _) = table.lookup(Ipv4Addr([10, 0, 0, 1])).unwrap();
    assert_eq_test!(
        dev2,
        DevIndex(0),
        "after metric update, dev 0 (100) should beat dev 1 (200)"
    );

    pass!()
}

// =============================================================================
// Additional route table tests
// =============================================================================

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

    // Add routes on two devices.
    table.add(connected_route([10, 0, 0, 0], 24, 0, 0));
    table.add(gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 0, 100));
    table.add(connected_route([192, 168, 1, 0], 24, 1, 0));
    assert_eq_test!(table.route_count(), 3, "should have 3 routes");

    // Remove all routes for dev 0.
    table.remove_device_routes(DevIndex(0));
    assert_eq_test!(
        table.route_count(),
        1,
        "should have 1 route after removing dev 0 routes"
    );

    // Only dev 1's route should remain.
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

    // Default route matches everything.
    let default_entry = gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 0, 100);
    assert_test!(
        default_entry.matches(Ipv4Addr([1, 2, 3, 4])),
        "default route should match any address"
    );

    pass!()
}

pub fn test_route_entry_next_hop() -> TestResult {
    // Connected route: next_hop should be dst.
    let connected = connected_route([10, 0, 0, 0], 24, 0, 0);
    let hop = connected.next_hop(Ipv4Addr([10, 0, 0, 42]));
    assert_eq_test!(
        hop.0,
        [10, 0, 0, 42],
        "connected route next_hop should be dst"
    );

    // Gateway route: next_hop should be the gateway.
    let gw = gateway_route([0, 0, 0, 0], 0, [10, 0, 0, 1], 0, 100);
    let hop = gw.next_hop(Ipv4Addr([8, 8, 8, 8]));
    assert_eq_test!(
        hop.0,
        [10, 0, 0, 1],
        "gateway route next_hop should be the gateway"
    );

    pass!()
}

// =============================================================================
// Test suite registration
// =============================================================================

slopos_lib::define_test_suite!(
    route,
    [
        // 3.T1 — connected route lookup
        test_route_lookup_connected,
        test_route_lookup_connected_edge_addresses,
        // 3.T2 — default route lookup
        test_route_lookup_default_gateway,
        test_route_lookup_connected_preferred_over_default,
        // 3.T3 — empty table / no matching route
        test_route_lookup_empty_table,
        test_route_lookup_no_matching_route,
        // 3.T4 — prefix-length bucketing (LPM)
        test_route_prefix_length_priority,
        test_route_host_route_beats_subnet,
        // 3.T5 — metric tie-breaking
        test_route_metric_tiebreak,
        test_route_metric_update,
        // Additional coverage
        test_route_add_and_remove,
        test_route_remove_device_routes,
        test_route_entry_matches,
        test_route_entry_next_hop,
    ]
);
