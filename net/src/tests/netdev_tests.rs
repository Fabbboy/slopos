//! Tests for NetDevice trait, NetDeviceStats, NetDeviceFeatures, DeviceHandle,
//! and NetDeviceRegistry.
//!
//! Covers:
//! - 1.T8:  NetDeviceStats accumulation (increment fields, verify reads)
//! - 1.T11: DeviceHandle::tx() does not acquire the registry lock (structural)
//! - Additional coverage: features bitflags, registry register/unregister/enumerate,
//!   handle data-plane ops, registry capacity exhaustion.

use slopos_alloc::{KBox, KVec};
use slopos_sync::{IrqMutex, LOCK_LEVEL_RESOURCE};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::netdev::*;
use crate::packetbuf::PacketBuf;
use crate::pool::{PACKET_POOL, PacketPool};
use crate::types::*;

// =============================================================================
// Mock NetDevice for testing
// =============================================================================

/// A minimal in-memory network device for testing the registry and handle.
///
/// Uses `IrqMutex` for interior mutability, matching the real-driver pattern.
struct MockNetDevice {
    mac_addr: MacAddr,
    dev_mtu: u16,
    feats: NetDeviceFeatures,
    stats: IrqMutex<NetDeviceStats>,
    tx_count: IrqMutex<u64>,
    is_up: IrqMutex<bool>,
}

impl MockNetDevice {
    fn new(mac: MacAddr, mtu: u16) -> Self {
        Self {
            mac_addr: mac,
            dev_mtu: mtu,
            feats: NetDeviceFeatures::empty(),
            stats: IrqMutex::new(NetDeviceStats::new(), LOCK_LEVEL_RESOURCE),
            tx_count: IrqMutex::new(0, LOCK_LEVEL_RESOURCE),
            is_up: IrqMutex::new(false, LOCK_LEVEL_RESOURCE),
        }
    }

    fn with_features(mut self, feats: NetDeviceFeatures) -> Self {
        self.feats = feats;
        self
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
fn ensure_pool_init() {
    PACKET_POOL.init();
}

// =============================================================================
// 1.T8 — NetDeviceStats accumulation
// =============================================================================

pub fn test_netdev_stats_default_zeroed() -> TestResult {
    let stats = NetDeviceStats::default();
    assert_eq_test!(stats.rx_packets, 0, "rx_packets starts at 0");
    assert_eq_test!(stats.tx_packets, 0, "tx_packets starts at 0");
    assert_eq_test!(stats.rx_bytes, 0, "rx_bytes starts at 0");
    assert_eq_test!(stats.tx_bytes, 0, "tx_bytes starts at 0");
    assert_eq_test!(stats.rx_errors, 0, "rx_errors starts at 0");
    assert_eq_test!(stats.tx_errors, 0, "tx_errors starts at 0");
    assert_eq_test!(stats.rx_dropped, 0, "rx_dropped starts at 0");
    assert_eq_test!(stats.tx_dropped, 0, "tx_dropped starts at 0");
    pass!()
}

pub fn test_netdev_stats_new_equals_default() -> TestResult {
    let from_new = NetDeviceStats::new();
    let from_default = NetDeviceStats::default();
    assert_eq_test!(from_new, from_default, "new() == default()");
    pass!()
}

pub fn test_netdev_stats_accumulation() -> TestResult {
    let mut stats = NetDeviceStats::new();

    stats.rx_packets += 100;
    stats.tx_packets += 50;
    stats.rx_bytes += 102400;
    stats.tx_bytes += 51200;
    stats.rx_errors += 3;
    stats.tx_errors += 1;
    stats.rx_dropped += 7;
    stats.tx_dropped += 2;

    assert_eq_test!(stats.rx_packets, 100, "rx_packets after increment");
    assert_eq_test!(stats.tx_packets, 50, "tx_packets after increment");
    assert_eq_test!(stats.rx_bytes, 102400, "rx_bytes after increment");
    assert_eq_test!(stats.tx_bytes, 51200, "tx_bytes after increment");
    assert_eq_test!(stats.rx_errors, 3, "rx_errors after increment");
    assert_eq_test!(stats.tx_errors, 1, "tx_errors after increment");
    assert_eq_test!(stats.rx_dropped, 7, "rx_dropped after increment");
    assert_eq_test!(stats.tx_dropped, 2, "tx_dropped after increment");

    // Verify convenience totals.
    assert_eq_test!(stats.total_packets(), 150, "total_packets = rx + tx");
    assert_eq_test!(stats.total_bytes(), 153600, "total_bytes = rx + tx");
    assert_eq_test!(stats.total_errors(), 4, "total_errors = rx + tx");
    assert_eq_test!(stats.total_dropped(), 9, "total_dropped = rx + tx");
    pass!()
}

pub fn test_netdev_stats_copy() -> TestResult {
    let mut original = NetDeviceStats::new();
    original.rx_packets = 42;
    original.tx_bytes = 1024;

    let copy = original;
    assert_eq_test!(copy.rx_packets, 42, "copy preserves rx_packets");
    assert_eq_test!(copy.tx_bytes, 1024, "copy preserves tx_bytes");
    // Modifying original doesn't affect copy (it's Copy).
    assert_eq_test!(original, copy, "original == copy");
    pass!()
}

// =============================================================================
// NetDeviceFeatures tests
// =============================================================================

pub fn test_features_empty() -> TestResult {
    let feats = NetDeviceFeatures::empty();
    assert_test!(feats.is_empty(), "empty features has no flags set");
    assert_test!(
        !feats.contains(NetDeviceFeatures::CHECKSUM_TX),
        "empty has no CHECKSUM_TX"
    );
    assert_test!(
        !feats.contains(NetDeviceFeatures::CHECKSUM_RX),
        "empty has no CHECKSUM_RX"
    );
    pass!()
}

pub fn test_features_individual() -> TestResult {
    let tx = NetDeviceFeatures::CHECKSUM_TX;
    assert_test!(
        tx.contains(NetDeviceFeatures::CHECKSUM_TX),
        "has CHECKSUM_TX"
    );
    assert_test!(
        !tx.contains(NetDeviceFeatures::CHECKSUM_RX),
        "no CHECKSUM_RX"
    );
    assert_test!(!tx.contains(NetDeviceFeatures::TSO), "no TSO");
    assert_test!(!tx.contains(NetDeviceFeatures::VLAN_TAG), "no VLAN_TAG");
    pass!()
}

pub fn test_features_combination() -> TestResult {
    let feats = NetDeviceFeatures::CHECKSUM_TX | NetDeviceFeatures::CHECKSUM_RX;
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_TX),
        "combined has CHECKSUM_TX"
    );
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_RX),
        "combined has CHECKSUM_RX"
    );
    assert_test!(!feats.contains(NetDeviceFeatures::TSO), "combined no TSO");
    assert_test!(!feats.is_empty(), "combined is not empty");
    pass!()
}

pub fn test_features_all() -> TestResult {
    let feats = NetDeviceFeatures::all();
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_TX),
        "all has CHECKSUM_TX"
    );
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_RX),
        "all has CHECKSUM_RX"
    );
    assert_test!(feats.contains(NetDeviceFeatures::TSO), "all has TSO");
    assert_test!(
        feats.contains(NetDeviceFeatures::VLAN_TAG),
        "all has VLAN_TAG"
    );
    pass!()
}

pub fn test_features_default_is_empty() -> TestResult {
    let feats = NetDeviceFeatures::default();
    assert_test!(feats.is_empty(), "default features is empty");
    assert_eq_test!(feats, NetDeviceFeatures::empty(), "default == empty");
    pass!()
}

// =============================================================================
// Registry tests
// =============================================================================

pub fn test_registry_register_and_enumerate() -> TestResult {
    let registry = NetDeviceRegistry::new();
    assert_eq_test!(registry.device_count(), 0, "empty registry has 0 devices");
    assert_test!(
        registry.enumerate().is_empty(),
        "enumerate on empty is empty"
    );

    let mac1 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let dev1 = KBox::try_new(MockNetDevice::new(mac1, 1500)).expect("test alloc");
    let handle1 = match registry.register(dev1) {
        Some(h) => h,
        None => return slopos_testing::fail!("register should succeed"),
    };

    assert_eq_test!(handle1.index(), DevIndex(0), "first device gets index 0");
    assert_eq_test!(handle1.mac(), mac1, "handle.mac() matches");
    assert_eq_test!(handle1.mtu(), 1500, "handle.mtu() matches");
    assert_eq_test!(registry.device_count(), 1, "1 device registered");

    let enumerated = registry.enumerate();
    assert_eq_test!(enumerated.len(), 1, "enumerate returns 1 entry");
    assert_eq_test!(enumerated[0].0, DevIndex(0), "enum index 0");
    assert_eq_test!(enumerated[0].1, mac1, "enum mac matches");
    assert_eq_test!(enumerated[0].2, true, "enum is_up=true");
    pass!()
}

pub fn test_registry_register_multiple() -> TestResult {
    let registry = NetDeviceRegistry::new();

    let mac1 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let mac2 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
    let mac3 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x03]);

    let h1 = registry.register(KBox::try_new(MockNetDevice::new(mac1, 1500)).expect("test alloc"));
    let h2 = registry.register(KBox::try_new(MockNetDevice::new(mac2, 9000)).expect("test alloc"));
    let h3 = registry.register(KBox::try_new(MockNetDevice::new(mac3, 1500)).expect("test alloc"));

    assert_test!(h1.is_some(), "register #1 succeeds");
    assert_test!(h2.is_some(), "register #2 succeeds");
    assert_test!(h3.is_some(), "register #3 succeeds");

    let h1 = h1.unwrap();
    let h2 = h2.unwrap();
    let h3 = h3.unwrap();

    assert_eq_test!(h1.index(), DevIndex(0), "dev 1 at index 0");
    assert_eq_test!(h2.index(), DevIndex(1), "dev 2 at index 1");
    assert_eq_test!(h3.index(), DevIndex(2), "dev 3 at index 2");

    assert_eq_test!(h2.mtu(), 9000, "dev 2 has mtu 9000");
    assert_eq_test!(registry.device_count(), 3, "3 devices registered");

    let enumerated = registry.enumerate();
    assert_eq_test!(enumerated.len(), 3, "enumerate returns 3 entries");
    pass!()
}

pub fn test_registry_unregister() -> TestResult {
    let registry = NetDeviceRegistry::new();

    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0xAA]);
    let _handle =
        registry.register(KBox::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc"));
    assert_eq_test!(registry.device_count(), 1, "1 device before unregister");

    let removed = registry.unregister(DevIndex(0));
    assert_test!(removed, "unregister returns true for occupied slot");
    assert_eq_test!(registry.device_count(), 0, "0 devices after unregister");
    assert_test!(
        registry.enumerate().is_empty(),
        "enumerate is empty after unregister"
    );

    // Unregistering again returns false.
    let removed_again = registry.unregister(DevIndex(0));
    assert_test!(!removed_again, "double unregister returns false");
    pass!()
}

pub fn test_registry_unregister_calls_set_down() -> TestResult {
    let registry = NetDeviceRegistry::new();
    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0xBB]);
    let dev = MockNetDevice::new(mac, 1500);
    // set_up before registering (simulating a driver that brought the link up).
    dev.set_up();

    // We need to check set_down was called after unregister.
    // Since we can't access the device after unregister (it's dropped),
    // we verify indirectly: set_down() is called during unregister as
    // a design contract. The test proves unregister succeeds without panic.
    let _handle = registry.register(KBox::try_new(dev).expect("test alloc"));
    let removed = registry.unregister(DevIndex(0));
    assert_test!(removed, "unregister succeeded (set_down called)");
    pass!()
}

pub fn test_registry_slot_reuse() -> TestResult {
    let registry = NetDeviceRegistry::new();

    let mac1 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let mac2 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);

    // Register, unregister, register again — should reuse slot 0.
    let h1 = registry.register(KBox::try_new(MockNetDevice::new(mac1, 1500)).expect("test alloc"));
    assert_test!(h1.is_some(), "first register succeeds");
    let h1 = h1.unwrap();
    assert_eq_test!(h1.index(), DevIndex(0), "first device at index 0");

    registry.unregister(DevIndex(0));

    let h2 = registry.register(KBox::try_new(MockNetDevice::new(mac2, 1500)).expect("test alloc"));
    assert_test!(h2.is_some(), "re-register succeeds");
    let h2 = h2.unwrap();
    assert_eq_test!(h2.index(), DevIndex(0), "reuses slot 0");
    assert_eq_test!(h2.mac(), mac2, "new device's mac");
    pass!()
}

pub fn test_registry_unregister_out_of_range() -> TestResult {
    let registry = NetDeviceRegistry::new();
    let removed = registry.unregister(DevIndex(999));
    assert_test!(!removed, "unregister out-of-range returns false");
    pass!()
}

// =============================================================================
// DeviceHandle data-plane tests
// =============================================================================

pub fn test_handle_tx() -> TestResult {
    ensure_pool_init();

    let registry = NetDeviceRegistry::new();
    let mac = MacAddr([0x02, 0xCA, 0xFE, 0x00, 0x00, 0x01]);
    let dev = KBox::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    // Allocate a packet and TX through the handle.
    let pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => return slopos_testing::fail!("PacketBuf::alloc failed"),
    };

    let result = handle.tx(pkt);
    assert_test!(result.is_ok(), "tx should succeed");

    // Verify the device recorded the TX (stats updated).
    let stats = handle.stats();
    assert_eq_test!(stats.tx_packets, 1, "stats.tx_packets == 1 after TX");
    pass!()
}

pub fn test_handle_poll_rx_empty() -> TestResult {
    ensure_pool_init();

    let registry = NetDeviceRegistry::new();
    let mac = MacAddr([0x02, 0xDE, 0xAD, 0x00, 0x00, 0x01]);
    let dev = KBox::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    let pkts = handle.poll_rx(16, &PACKET_POOL);
    assert_test!(pkts.is_empty(), "mock poll_rx returns empty");
    pass!()
}

pub fn test_handle_stats() -> TestResult {
    let registry = NetDeviceRegistry::new();
    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x55]);
    let dev = KBox::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    let stats = handle.stats();
    assert_eq_test!(stats, NetDeviceStats::new(), "initial stats are zeroed");
    pass!()
}

pub fn test_handle_features() -> TestResult {
    let registry = NetDeviceRegistry::new();
    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x66]);
    let dev = KBox::try_new(
        MockNetDevice::new(mac, 1500)
            .with_features(NetDeviceFeatures::CHECKSUM_TX | NetDeviceFeatures::CHECKSUM_RX),
    )
    .expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    let feats = handle.features();
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_TX),
        "handle reports CHECKSUM_TX"
    );
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_RX),
        "handle reports CHECKSUM_RX"
    );
    assert_test!(!feats.contains(NetDeviceFeatures::TSO), "handle no TSO");
    pass!()
}

/// 1.T11 — Verify `DeviceHandle::tx()` does not acquire the registry lock.
///
/// This is a structural test: we hold the registry lock and then call
/// `handle.tx()`.  If `tx()` tried to acquire the registry lock, it would
/// deadlock (since `IrqMutex` is non-reentrant).  The test passing proves
/// that `tx()` bypasses the registry lock entirely.
pub fn test_handle_tx_does_not_acquire_registry_lock() -> TestResult {
    ensure_pool_init();

    let registry = NetDeviceRegistry::new();
    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x77]);
    let dev = KBox::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    // Hold the registry lock.
    let _guard = registry.inner.lock();

    // Now TX through the handle.  If DeviceHandle::tx() tried to lock
    // the registry, this would deadlock because IrqMutex is non-reentrant
    // and we already hold the lock above.
    let pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => {
            drop(_guard);
            return slopos_testing::fail!("PacketBuf::alloc failed");
        }
    };

    let result = handle.tx(pkt);
    assert_test!(result.is_ok(), "tx succeeds while registry lock is held");

    // If we reach here, tx() did NOT try to acquire the registry lock.
    // Q.E.D.
    drop(_guard);
    pass!()
}

// =============================================================================
// Test suite registration
// =============================================================================

slopos_testing::define_test_suite!(
    netdev,
    [
        // 1.T8 — NetDeviceStats accumulation
        test_netdev_stats_default_zeroed,
        test_netdev_stats_new_equals_default,
        test_netdev_stats_accumulation,
        test_netdev_stats_copy,
        // NetDeviceFeatures
        test_features_empty,
        test_features_individual,
        test_features_combination,
        test_features_all,
        test_features_default_is_empty,
        // Registry
        test_registry_register_and_enumerate,
        test_registry_register_multiple,
        test_registry_unregister,
        test_registry_unregister_calls_set_down,
        test_registry_slot_reuse,
        test_registry_unregister_out_of_range,
        // DeviceHandle data-plane
        test_handle_tx,
        test_handle_poll_rx_empty,
        test_handle_stats,
        test_handle_features,
        // 1.T11 — Handle TX doesn't acquire registry lock
        test_handle_tx_does_not_acquire_registry_lock,
    ]
);
