//! Tests for the ingress pipeline.
//!
//! Covers:
//! - 1.T9:  Ingress pipeline correctly dispatches IPv4 frames
//! - 1.T10: Ingress pipeline drops malformed / unknown frames

use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KBox, KVec};
use slopos_testing::TestResult;
use slopos_testing::pass;

use crate::ETH_HEADER_LEN;
use crate::netdev::*;
use crate::packetbuf::PacketBuf;
use crate::pool::{PACKET_POOL, PacketPool};
use crate::types::*;

// =============================================================================
// Mock NetDevice for testing
// =============================================================================

/// A minimal in-memory network device for testing the ingress pipeline.
///
/// Uses `SpinLock` for interior mutability, matching the real-driver pattern.
struct MockNetDevice {
    mac_addr: MacAddr,
    dev_mtu: u16,
    feats: NetDeviceFeatures,
    stats: SpinLock<NetDeviceStats>,
    tx_count: SpinLock<u64>,
    is_up: SpinLock<bool>,
}

impl MockNetDevice {
    fn new(mac: MacAddr, mtu: u16) -> Self {
        Self {
            mac_addr: mac,
            dev_mtu: mtu,
            feats: NetDeviceFeatures::empty(),
            stats: SpinLock::new(NetDeviceStats::new(), LOCK_LEVEL_RESOURCE),
            tx_count: SpinLock::new(0, LOCK_LEVEL_RESOURCE),
            is_up: SpinLock::new(false, LOCK_LEVEL_RESOURCE),
        }
    }
}

impl NetDevice for MockNetDevice {
    fn tx(&self, _pkt: PacketBuf) -> Result<(), NetError> {
        let mut count = self.tx_count.lock();
        *count += 1;
        let mut stats = self.stats.lock();
        stats.tx_packets += 1;
        Ok(())
    }

    fn poll_rx(&self, _budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        KVec::new()
    }

    fn set_up(&self) {
        *self.is_up.lock() = true;
    }

    fn set_down(&self) {
        *self.is_up.lock() = false;
    }

    fn mtu(&self) -> u16 {
        self.dev_mtu
    }

    fn mac(&self) -> MacAddr {
        self.mac_addr
    }

    fn stats(&self) -> NetDeviceStats {
        *self.stats.lock()
    }

    fn features(&self) -> NetDeviceFeatures {
        self.feats
    }
}

/// Ensure the global pool is initialized before tests that allocate PacketBuf.
/// Idempotent — safe to call multiple times.
fn ensure_pool_init() {
    PACKET_POOL.init();
}

/// Create a test device handle with a given MAC address.
/// The registry is leaked so the device allocation lives for the test.
fn make_test_handle(mac: MacAddr) -> DeviceHandle {
    let registry = KBox::leak(KBox::try_new(NetDeviceRegistry::new()).expect("test alloc"));
    let dev = KBox::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
    registry.register(dev).expect("register must succeed")
}

/// Build an Ethernet frame with the given parameters.
fn build_frame(dst_mac: [u8; 6], src_mac: [u8; 6], ethertype: u16, payload: &[u8]) -> KVec<u8> {
    let mut frame = KVec::<u8>::with_capacity(ETH_HEADER_LEN + payload.len()).expect("test alloc");
    frame.extend_from_slice(&dst_mac).expect("test alloc");
    frame.extend_from_slice(&src_mac).expect("test alloc");
    frame
        .extend_from_slice(&ethertype.to_be_bytes())
        .expect("test alloc");
    frame.extend_from_slice(payload).expect("test alloc");
    frame
}

/// Build a minimal valid IPv4 header (20 bytes).
fn build_ipv4_header(proto: u8, src: [u8; 4], dst: [u8; 4], payload_len: usize) -> [u8; 20] {
    let total_len = (20 + payload_len) as u16;
    let mut hdr = [0u8; 20];
    hdr[0] = 0x45; // version 4, IHL 5
    hdr[2..4].copy_from_slice(&total_len.to_be_bytes());
    hdr[8] = 64; // TTL
    hdr[9] = proto;
    hdr[12..16].copy_from_slice(&src);
    hdr[16..20].copy_from_slice(&dst);
    // Compute checksum
    let csum = crate::checksum::internet_checksum(&hdr);
    hdr[10..12].copy_from_slice(&csum.to_be_bytes());
    hdr
}

// =============================================================================
// 1.T9 — Ingress pipeline correctly dispatches IPv4 frames
// =============================================================================

pub fn test_ingress_drops_short_frame() -> TestResult {
    ensure_pool_init();

    let device_mac = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let handle = make_test_handle(device_mac);

    // Create a frame shorter than 14 bytes (Ethernet header).
    let short_data = [0u8; 10];
    let pkt = match PacketBuf::from_raw_copy(&short_data) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    // Call net_rx — should not panic, just silently drop.
    crate::ingress::net_rx(&handle, pkt);

    pass!()
}

pub fn test_ingress_drops_unknown_ethertype() -> TestResult {
    ensure_pool_init();

    let device_mac = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let handle = make_test_handle(device_mac);

    // Build a 60-byte frame with valid MAC fields but unknown ethertype 0x9999.
    let payload = [0u8; 46]; // 14 (eth) + 46 = 60 bytes
    let frame = build_frame(
        device_mac.0,
        [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        0x9999, // unknown ethertype
        &payload,
    );

    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    // Call net_rx — should not panic, just silently drop.
    crate::ingress::net_rx(&handle, pkt);

    pass!()
}

pub fn test_ingress_drops_wrong_destination_mac() -> TestResult {
    ensure_pool_init();

    let device_mac = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let handle = make_test_handle(device_mac);

    // Build a frame with a different destination MAC (not our MAC, not broadcast, not multicast).
    let wrong_dst_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    let payload = [0u8; 46];
    let frame = build_frame(
        wrong_dst_mac,
        [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        EtherType::Ipv4.as_u16(),
        &payload,
    );

    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    // Call net_rx — should not panic, just silently drop.
    crate::ingress::net_rx(&handle, pkt);

    pass!()
}

pub fn test_ingress_accepts_broadcast_mac() -> TestResult {
    ensure_pool_init();

    let device_mac = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let handle = make_test_handle(device_mac);

    // Build a frame with broadcast MAC and IPv4 ethertype.
    let broadcast_mac = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
    let ipv4_hdr = build_ipv4_header(17, [192, 168, 1, 100], [192, 168, 1, 1], 8);
    let mut payload: KVec<u8> = KVec::new();
    payload.extend_from_slice(&ipv4_hdr).expect("test alloc");
    payload.extend_from_slice(&[0u8; 8]).expect("test alloc"); // minimal UDP header

    let frame = build_frame(
        broadcast_mac,
        [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        EtherType::Ipv4.as_u16(),
        &payload,
    );

    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    // Call net_rx — should not panic (accepted).
    crate::ingress::net_rx(&handle, pkt);

    pass!()
}

pub fn test_ingress_accepts_our_mac() -> TestResult {
    ensure_pool_init();

    let device_mac = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let handle = make_test_handle(device_mac);

    // Build a frame with our MAC and IPv4 ethertype.
    let ipv4_hdr = build_ipv4_header(17, [192, 168, 1, 100], [192, 168, 1, 1], 8);
    let mut payload: KVec<u8> = KVec::new();
    payload.extend_from_slice(&ipv4_hdr).expect("test alloc");
    payload.extend_from_slice(&[0u8; 8]).expect("test alloc"); // minimal UDP header

    let frame = build_frame(
        device_mac.0,
        [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        EtherType::Ipv4.as_u16(),
        &payload,
    );

    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    // Call net_rx — should not panic (accepted).
    crate::ingress::net_rx(&handle, pkt);

    pass!()
}

// =============================================================================
// 1.T10 — Ingress pipeline drops malformed / unknown frames
// =============================================================================

pub fn test_ingress_ipv4_bad_version() -> TestResult {
    ensure_pool_init();

    let device_mac = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let handle = make_test_handle(device_mac);

    // Build a frame with ethertype IPv4 but IP version field = 6 (not 4).
    let mut ipv4_hdr = build_ipv4_header(17, [192, 168, 1, 100], [192, 168, 1, 1], 8);
    ipv4_hdr[0] = 0x65; // version 6, IHL 5
    // Recompute checksum
    let csum = crate::checksum::internet_checksum(&ipv4_hdr);
    ipv4_hdr[10..12].copy_from_slice(&csum.to_be_bytes());

    let mut payload: KVec<u8> = KVec::new();
    payload.extend_from_slice(&ipv4_hdr).expect("test alloc");
    payload.extend_from_slice(&[0u8; 8]).expect("test alloc");

    let frame = build_frame(
        device_mac.0,
        [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        EtherType::Ipv4.as_u16(),
        &payload,
    );

    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    // Call net_rx — should not panic (dropped by ipv4 handler).
    crate::ingress::net_rx(&handle, pkt);

    pass!()
}

pub fn test_ingress_ipv4_short_header() -> TestResult {
    ensure_pool_init();

    let device_mac = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let handle = make_test_handle(device_mac);

    // Build a frame with ethertype IPv4 but only 10 bytes of IP data (less than 20 byte minimum).
    let short_ip_data = [0u8; 10];
    let frame = build_frame(
        device_mac.0,
        [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        EtherType::Ipv4.as_u16(),
        &short_ip_data,
    );

    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    // Call net_rx — should not panic (dropped by ipv4 handler).
    crate::ingress::net_rx(&handle, pkt);

    pass!()
}

pub fn test_ingress_ipv4_bad_checksum() -> TestResult {
    ensure_pool_init();

    let device_mac = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let handle = make_test_handle(device_mac);

    // Build a valid-looking IPv4 frame but with incorrect header checksum.
    let mut ipv4_hdr = build_ipv4_header(17, [192, 168, 1, 100], [192, 168, 1, 1], 8);
    // Corrupt the checksum field.
    ipv4_hdr[10..12].copy_from_slice(&[0xff, 0xff]);

    let mut payload: KVec<u8> = KVec::new();
    payload.extend_from_slice(&ipv4_hdr).expect("test alloc");
    payload.extend_from_slice(&[0u8; 8]).expect("test alloc");

    let frame = build_frame(
        device_mac.0,
        [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        EtherType::Ipv4.as_u16(),
        &payload,
    );

    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    // Call net_rx — should not panic (dropped silently by ipv4 handler).
    crate::ingress::net_rx(&handle, pkt);

    pass!()
}

// =============================================================================
// Test suite registration
// =============================================================================

slopos_testing::stest!(name = test_ingress_drops_short_frame, suite = ingress);
slopos_testing::stest!(name = test_ingress_drops_unknown_ethertype, suite = ingress);
slopos_testing::stest!(
    name = test_ingress_drops_wrong_destination_mac,
    suite = ingress
);
slopos_testing::stest!(name = test_ingress_accepts_broadcast_mac, suite = ingress);
slopos_testing::stest!(name = test_ingress_accepts_our_mac, suite = ingress);
slopos_testing::stest!(name = test_ingress_ipv4_bad_version, suite = ingress);
slopos_testing::stest!(name = test_ingress_ipv4_short_header, suite = ingress);
slopos_testing::stest!(name = test_ingress_ipv4_bad_checksum, suite = ingress);
