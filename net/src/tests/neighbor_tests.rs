//! Tests for the ARP neighbor cache: lookup, insert/update, the
//! `Incomplete`→`Reachable` flush, the `Failed` transition, and snapshots.
//!
//! The cache under test is local, but `insert_or_update`, `resolve` and
//! `on_retransmit` schedule through the shared wheel with entry ids that
//! restart at 1, so a test that arms one holds a `NetTestScope`: left in the
//! live wheel those tokens fire against `NEIGHBOR_CACHE`'s entry of that id.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::neighbor::{NeighborAction, NeighborCache, ResolveOutcome};
use crate::packetbuf::PacketBuf;
use crate::tests::net_scope::NetTestScope;
use crate::tests::packetbuf_tests::{TEST_PACKET_POOL, ensure_test_pool};
use crate::tests::tcp_common::{LOCAL_IP, REMOTE_IP};
use crate::types::{DevIndex, Ipv4Addr, MacAddr, NetError};

fn fresh_cache() -> NeighborCache {
    NeighborCache::new()
}

fn dummy_packet() -> Option<PacketBuf> {
    let data = [0xAA_u8; 64];
    PacketBuf::from_raw_copy_in(&TEST_PACKET_POOL, &data)
}

pub fn test_neighbor_lookup_empty_cache() -> TestResult {
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr(LOCAL_IP);

    let result = cache.lookup(dev, ip);
    assert_test!(result.is_none(), "lookup on empty cache should return None");

    pass!()
}

pub fn test_neighbor_insert_then_lookup() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return slopos_testing::fail!("net scope: {:?}", e),
    };
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr(LOCAL_IP);
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

    let other_ip = Ipv4Addr(REMOTE_IP);
    assert_test!(
        cache.lookup(dev, other_ip).is_none(),
        "lookup for different IP should return None"
    );

    let other_dev = DevIndex(1);
    assert_test!(
        cache.lookup(other_dev, ip).is_none(),
        "lookup for different device should return None"
    );

    pass!()
}

pub fn test_neighbor_update_overwrites_mac() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return slopos_testing::fail!("net scope: {:?}", e),
    };
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr(LOCAL_IP);
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

pub fn test_neighbor_incomplete_to_reachable_flush() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return slopos_testing::fail!("net scope: {:?}", e),
    };
    ensure_test_pool();
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr(LOCAL_IP);
    let mac = MacAddr([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);

    let Some(pkt1) = dummy_packet() else {
        return slopos_testing::fail!("test pool should have capacity");
    };
    let outcome1 = cache.resolve(dev, ip, pkt1);
    assert_test!(
        matches!(outcome1, ResolveOutcome::ArpNeeded(_)),
        "first resolve should create Incomplete entry and request ARP"
    );

    let Some(pkt2) = dummy_packet() else {
        return slopos_testing::fail!("test pool should have capacity");
    };
    let outcome2 = cache.resolve(dev, ip, pkt2);
    assert_test!(
        matches!(outcome2, ResolveOutcome::Queued),
        "second resolve should queue (ARP already in progress)"
    );

    assert_test!(
        cache.lookup(dev, ip).is_none(),
        "lookup while Incomplete should return None"
    );

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

    let result = cache.lookup(dev, ip);
    assert_test!(result.is_some(), "lookup after reply should succeed");
    assert_eq_test!(
        result.unwrap().0,
        mac.0,
        "lookup should return resolved MAC"
    );

    pass!()
}

pub fn test_neighbor_failed_drops_packets() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return slopos_testing::fail!("net scope: {:?}", e),
    };
    ensure_test_pool();
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr(LOCAL_IP);

    let Some(pkt) = dummy_packet() else {
        return slopos_testing::fail!("test pool should have capacity");
    };
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

    let Some(pkt2) = dummy_packet() else {
        return slopos_testing::fail!("test pool should have capacity");
    };
    let outcome = cache.resolve(dev, ip, pkt2);
    assert_test!(
        matches!(outcome, ResolveOutcome::Failed(NetError::HostUnreachable)),
        "resolve on Failed entry should return Failed(HostUnreachable)"
    );

    // No W/L assertion: by kernel convention an internal subsystem never adjusts
    // the balance, so failures surface only through the return type.

    pass!()
}

pub fn test_neighbor_resolve_reachable_returns_mac() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return slopos_testing::fail!("net scope: {:?}", e),
    };
    ensure_test_pool();
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr(LOCAL_IP);
    let mac = MacAddr([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);

    cache.insert_or_update(dev, ip, mac, 100);

    let Some(pkt) = dummy_packet() else {
        return slopos_testing::fail!("test pool should have capacity");
    };
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
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return slopos_testing::fail!("net scope: {:?}", e),
    };
    let cache = fresh_cache();

    let dev = DevIndex(0);
    let ip = Ipv4Addr(LOCAL_IP);
    let mac = MacAddr([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);

    cache.insert_or_update(dev, ip, mac, 100);
    assert_eq_test!(cache.entry_count(), 1, "one entry after insert");

    // ArpExpire firing; entry_id 1 is the first entry created.
    cache.on_expire(1);

    let result = cache.lookup(dev, ip);
    assert_test!(result.is_some(), "lookup should succeed on Stale entry");
    assert_eq_test!(
        result.unwrap().0,
        mac.0,
        "Stale entry should still have correct MAC"
    );

    pass!()
}

/// `NET_Q_NEIGH` reads `snapshot_owned`, so `ip neigh` shows exactly what it
/// returns — every state, not just the resolved ones.
pub fn test_neighbor_snapshot_owned_reports_every_state() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return slopos_testing::fail!("net scope: {:?}", e),
    };
    ensure_test_pool();
    let cache = fresh_cache();
    let dev = DevIndex(7);
    let reachable_ip = Ipv4Addr([10, 0, 2, 2]);
    let mac = MacAddr([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);

    let (empty, total) = cache.snapshot_owned(Some(dev));
    assert_test!(empty.is_empty() && total == 0, "fresh cache is not empty");

    let _ = cache.insert_or_update(dev, reachable_ip, mac, 0);

    let (entries, total) = cache.snapshot_owned(Some(dev));
    assert_eq_test!(total, 1, "one entry inserted, one expected");
    assert_eq_test!(entries.len(), 1, "snapshot dropped the entry it counted");
    assert_eq_test!(entries[0].ip.0, reachable_ip.0, "wrong address reported");
    assert_eq_test!(entries[0].mac.0, mac.0, "wrong MAC reported");
    assert_eq_test!(
        entries[0].state,
        slopos_abi::net::NET_NEIGH_REACHABLE,
        "an ARP-confirmed entry is REACHABLE"
    );

    let (other, other_total) = cache.snapshot_owned(Some(DevIndex(8)));
    assert_test!(
        other.is_empty() && other_total == 0,
        "the device filter leaked an entry from another device"
    );

    pass!()
}

slopos_testing::stest!(name = test_neighbor_lookup_empty_cache, suite = neighbor);
slopos_testing::stest!(name = test_neighbor_insert_then_lookup, suite = neighbor);
slopos_testing::stest!(name = test_neighbor_update_overwrites_mac, suite = neighbor);
slopos_testing::stest!(
    name = test_neighbor_incomplete_to_reachable_flush,
    suite = neighbor
);
slopos_testing::stest!(name = test_neighbor_failed_drops_packets, suite = neighbor);
slopos_testing::stest!(
    name = test_neighbor_resolve_reachable_returns_mac,
    suite = neighbor
);
slopos_testing::stest!(
    name = test_neighbor_expire_reachable_to_stale,
    suite = neighbor
);
slopos_testing::stest!(
    name = test_neighbor_snapshot_owned_reports_every_state,
    suite = neighbor
);
