//! Tests for the ARP neighbor cache.
//!
//! Covers:
//! - 2.T1: `lookup` on empty cache returns `None`
//! - 2.T2: `insert_or_update` followed by `lookup` returns correct MAC
//! - 2.T3: `Incomplete` state — queued packets flushed when reply arrives
//! - 2.T4: `Failed` state — packets are dropped

extern crate alloc;

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::neighbor::{NeighborAction, NeighborCache, ResolveOutcome};
use crate::packetbuf::PacketBuf;
use crate::pool::PACKET_POOL;
use crate::types::{DevIndex, Ipv4Addr, MacAddr, NetError};

// =============================================================================
// Helpers
// =============================================================================

/// Create a fresh neighbor cache for each test (avoids shared state).
fn fresh_cache() -> NeighborCache {
    NeighborCache::new()
}

/// Ensure the global packet pool is initialized.
fn ensure_pool_init() {
    PACKET_POOL.init();
}

/// Allocate a dummy packet for testing.
fn dummy_packet() -> PacketBuf {
    let data = [0xAA_u8; 64];
    PacketBuf::from_raw_copy(&data).expect("pool should have capacity")
}

// =============================================================================
// 2.T1 — lookup on empty cache returns None
// =============================================================================

pub fn test_neighbor_lookup_empty_cache() -> TestResult {
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr([10, 0, 0, 1]);

    let result = cache.lookup(dev, ip);
    assert_test!(result.is_none(), "lookup on empty cache should return None");

    pass!()
}

// =============================================================================
// 2.T2 — insert_or_update + lookup returns correct MAC
// =============================================================================

pub fn test_neighbor_insert_then_lookup() -> TestResult {
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr([10, 0, 0, 1]);
    let mac = MacAddr([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);
    let tick = 1000;

    let action = cache.insert_or_update(dev, ip, mac, tick);
    assert_test!(
        matches!(action, NeighborAction::None),
        "insert into empty cache should return None action"
    );

    let result = cache.lookup(dev, ip);
    assert_test!(result.is_some(), "lookup after insert should return Some");
    assert_eq_test!(
        result.unwrap().0,
        mac.0,
        "lookup should return the inserted MAC"
    );

    // Verify a different IP still returns None.
    let other_ip = Ipv4Addr([10, 0, 0, 2]);
    assert_test!(
        cache.lookup(dev, other_ip).is_none(),
        "lookup for different IP should return None"
    );

    // Verify a different device still returns None.
    let other_dev = DevIndex(1);
    assert_test!(
        cache.lookup(other_dev, ip).is_none(),
        "lookup for different device should return None"
    );

    pass!()
}

pub fn test_neighbor_update_overwrites_mac() -> TestResult {
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr([10, 0, 0, 1]);
    let mac1 = MacAddr([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    let mac2 = MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

    cache.insert_or_update(dev, ip, mac1, 100);
    cache.insert_or_update(dev, ip, mac2, 200);

    let result = cache.lookup(dev, ip);
    assert_test!(result.is_some(), "lookup after update should return Some");
    assert_eq_test!(
        result.unwrap().0,
        mac2.0,
        "lookup should return the updated MAC"
    );

    pass!()
}

// =============================================================================
// 2.T3 — Incomplete state: queued packets flushed when reply arrives
// =============================================================================

pub fn test_neighbor_incomplete_to_reachable_flush() -> TestResult {
    ensure_pool_init();
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr([10, 0, 0, 1]);
    let mac = MacAddr([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);

    // First resolve creates an Incomplete entry and queues the packet.
    let pkt1 = dummy_packet();
    let outcome1 = cache.resolve(dev, ip, pkt1);
    assert_test!(
        matches!(outcome1, ResolveOutcome::ArpNeeded(_)),
        "first resolve should create Incomplete entry and request ARP"
    );

    // Second resolve queues another packet.
    let pkt2 = dummy_packet();
    let outcome2 = cache.resolve(dev, ip, pkt2);
    assert_test!(
        matches!(outcome2, ResolveOutcome::Queued),
        "second resolve should queue (ARP already in progress)"
    );

    // lookup should return None while Incomplete.
    assert_test!(
        cache.lookup(dev, ip).is_none(),
        "lookup while Incomplete should return None"
    );

    // Simulate ARP reply: insert_or_update should flush pending packets.
    let action = cache.insert_or_update(dev, ip, mac, 500);
    match action {
        NeighborAction::FlushPending {
            packets, dst_mac, ..
        } => {
            assert_eq_test!(packets.len(), 2, "should flush 2 queued packets");
            assert_eq_test!(dst_mac.0, mac.0, "flush MAC should match");
        }
        _ => return slopos_testing::fail!("expected FlushPending action after ARP reply"),
    }

    // Now lookup should return the MAC (Reachable state).
    let result = cache.lookup(dev, ip);
    assert_test!(result.is_some(), "lookup after reply should succeed");
    assert_eq_test!(
        result.unwrap().0,
        mac.0,
        "lookup should return resolved MAC"
    );

    pass!()
}

// =============================================================================
// 2.T4 — Failed state: packets are dropped
// =============================================================================

pub fn test_neighbor_failed_drops_packets() -> TestResult {
    ensure_pool_init();
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr([10, 0, 0, 1]);

    // Create an Incomplete entry by resolving.
    let pkt = dummy_packet();
    let outcome = cache.resolve(dev, ip, pkt);
    assert_test!(
        matches!(outcome, ResolveOutcome::ArpNeeded(_)),
        "first resolve should create Incomplete entry"
    );

    let entry_id = 1u32;

    for _ in 1..=3 {
        let (action, dropped) = cache.on_retransmit(entry_id);
        assert_test!(
            action.is_some(),
            "retransmit should return SendArpRequest while retries < MAX"
        );
        assert_test!(dropped.is_empty(), "no packets dropped during retransmit");
    }

    // 4th call: retries exhausted, transition to Failed.
    let (action, dropped) = cache.on_retransmit(entry_id);
    assert_test!(
        action.is_none(),
        "should return None after transitioning to Failed"
    );
    assert_eq_test!(
        dropped.len(),
        1,
        "should drop 1 pending packet on Failed transition"
    );

    // Now resolving should return Failed.
    let pkt2 = dummy_packet();
    let outcome = cache.resolve(dev, ip, pkt2);
    assert_test!(
        matches!(outcome, ResolveOutcome::Failed(NetError::HostUnreachable)),
        "resolve on Failed entry should return Failed(HostUnreachable)"
    );

    // Note: W/L currency adjustment is intentionally NOT tested here.
    // Per AGENTS.md: "Internal subsystems (drivers, boot sequences) must not
    // adjust the W/L balance directly." The scheduler reads the balance on
    // context switches. Neighbor cache failures surface through the return
    // type, not through direct W/L calls.

    pass!()
}

// =============================================================================
// Additional edge case tests
// =============================================================================

pub fn test_neighbor_resolve_reachable_returns_mac() -> TestResult {
    ensure_pool_init();
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr([10, 0, 0, 1]);
    let mac = MacAddr([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);

    // Insert a Reachable entry.
    cache.insert_or_update(dev, ip, mac, 100);

    // Resolve should return Resolved with the MAC and give back the packet.
    let pkt = dummy_packet();
    let outcome = cache.resolve(dev, ip, pkt);
    match outcome {
        ResolveOutcome::Resolved {
            mac: resolved_mac,
            pkt: _,
            action,
        } => {
            assert_eq_test!(resolved_mac.0, mac.0, "should return correct MAC");
            assert_test!(
                action.is_none(),
                "Reachable resolve should not need re-probe"
            );
        }
        _ => return slopos_testing::fail!("expected Resolved outcome for Reachable entry"),
    }

    pass!()
}

pub fn test_neighbor_expire_reachable_to_stale() -> TestResult {
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr([10, 0, 0, 1]);
    let mac = MacAddr([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);

    // Insert as Reachable.
    cache.insert_or_update(dev, ip, mac, 100);
    assert_eq_test!(cache.entry_count(), 1, "one entry after insert");

    // Simulate ArpExpire timer firing — entry_id is 1 (first entry created).
    cache.on_expire(1);

    // Entry should still be resolvable (Stale is usable).
    let result = cache.lookup(dev, ip);
    assert_test!(result.is_some(), "lookup should succeed on Stale entry");
    assert_eq_test!(
        result.unwrap().0,
        mac.0,
        "Stale entry should still have correct MAC"
    );

    pass!()
}

// =============================================================================
// Test suite registration
// =============================================================================

slopos_testing::define_test_suite!(
    neighbor,
    [
        test_neighbor_lookup_empty_cache,
        test_neighbor_insert_then_lookup,
        test_neighbor_update_overwrites_mac,
        test_neighbor_incomplete_to_reachable_flush,
        test_neighbor_failed_drops_packets,
        test_neighbor_resolve_reachable_returns_mac,
        test_neighbor_expire_reachable_to_stale,
    ]
);
